//! The signing key on disk: generated at first boot, persisted on the data
//! dataset, and rotated to log every device out (ADR-0003).

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anyhow::{Context as _, Result};

use super::session::SigningKey;

/// File under the data directory holding the raw key material.
pub const KEY_FILE: &str = "signing_key";

/// Sibling file a new key lands in before it is moved into place, so a
/// crash mid-write cannot leave an unbootable half key.
const PENDING_KEY_FILE: &str = "signing_key.tmp";

/// The live signing key, backed by a file under the data directory.
pub struct KeyStore {
    path: PathBuf,
    current: RwLock<SigningKey>,
}

impl KeyStore {
    /// Load the key stored under `data_dir`, generating and persisting one
    /// if this is the first boot.
    pub fn open(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join(KEY_FILE);
        let key = match fs::read(&path) {
            Ok(bytes) => adopt(&bytes, &path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let key = SigningKey::random()?;
                persist(&key, &path)?;
                key
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        Ok(Self {
            path,
            current: RwLock::new(key),
        })
    }

    /// The key currently in force.
    #[must_use]
    pub fn current(&self) -> SigningKey {
        self.current.read().expect(POISONED).clone()
    }

    /// Replace the key with fresh material, invalidating every cookie.
    pub fn rotate(&self) -> Result<()> {
        let key = SigningKey::random()?;
        persist(&key, &self.path)?;
        *self.current.write().expect(POISONED) = key;
        Ok(())
    }
}

/// Nothing held across the key lock can panic, so it cannot be poisoned.
const POISONED: &str = "the signing key lock is never poisoned";

/// Interpret stored bytes as key material.
fn adopt(bytes: &[u8], path: &Path) -> Result<SigningKey> {
    let bytes: [u8; 32] = bytes.try_into().with_context(|| {
        format!(
            "{} holds {} bytes, not a 32-byte signing key",
            path.display(),
            bytes.len()
        )
    })?;
    Ok(SigningKey::from_bytes(bytes))
}

/// Put key material in place in one step: a crash leaves either the old
/// key or the new one, never a half-written file.
fn persist(key: &SigningKey, path: &Path) -> Result<()> {
    let pending = path.with_file_name(PENDING_KEY_FILE);
    write_owner_only(key, &pending)?;
    fs::rename(&pending, path).with_context(|| {
        format!(
            "failed to move {} onto {}",
            pending.display(),
            path.display()
        )
    })
}

/// Write key material where only its owner can read it back, and make sure
/// it has reached the disk.
fn write_owner_only(key: &SigningKey, path: &Path) -> Result<()> {
    let mut options = File::options();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(path)
        .and_then(|mut file| {
            file.write_all(key.as_bytes())
                .and_then(|()| file.sync_all())
        })
        .with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn first_open_generates_and_persists_a_key() {
        let dir = tempdir().expect("temp dir should be creatable");

        let store = KeyStore::open(dir.path()).expect("first open should generate a key");

        let persisted = fs::read(dir.path().join(KEY_FILE)).expect("key file should exist");
        assert_eq!(persisted, store.current().as_bytes());
    }

    #[test]
    fn reopening_keeps_the_same_key() {
        let dir = tempdir().expect("temp dir should be creatable");
        let first = KeyStore::open(dir.path()).expect("first open should generate a key");

        let second = KeyStore::open(dir.path()).expect("second open should load the key");

        assert_eq!(first.current().as_bytes(), second.current().as_bytes());
    }

    #[test]
    fn rotation_replaces_the_key_on_disk() {
        let dir = tempdir().expect("temp dir should be creatable");
        let store = KeyStore::open(dir.path()).expect("first open should generate a key");
        let before = *store.current().as_bytes();

        store.rotate().expect("rotation should persist a new key");

        assert_ne!(*store.current().as_bytes(), before);
        let persisted = fs::read(dir.path().join(KEY_FILE)).expect("key file should exist");
        assert_eq!(persisted, store.current().as_bytes());
    }

    #[test]
    fn a_truncated_key_file_names_the_path() {
        let dir = tempdir().expect("temp dir should be creatable");
        fs::write(dir.path().join(KEY_FILE), b"too short").expect("key file should be writable");

        let error = KeyStore::open(dir.path())
            .err()
            .expect("a short key should not load");

        assert!(
            format!("{error:#}").contains(KEY_FILE),
            "error should name the key file, got: {error:#}"
        );
    }

    #[test]
    fn rotation_leaves_no_half_written_key_behind() {
        let dir = tempdir().expect("temp dir should be creatable");
        let store = KeyStore::open(dir.path()).expect("first open should generate a key");

        store.rotate().expect("rotation should persist a new key");

        assert!(!dir.path().join(PENDING_KEY_FILE).exists());
        let reopened = KeyStore::open(dir.path()).expect("the rotated key should load");
        assert_eq!(reopened.current().as_bytes(), store.current().as_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn a_rotated_key_file_stays_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempdir().expect("temp dir should be creatable");
        let store = KeyStore::open(dir.path()).expect("first open should generate a key");

        store.rotate().expect("rotation should persist a new key");

        let mode = fs::metadata(dir.path().join(KEY_FILE))
            .expect("key file should exist")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn the_key_file_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempdir().expect("temp dir should be creatable");
        KeyStore::open(dir.path()).expect("first open should generate a key");

        let mode = fs::metadata(dir.path().join(KEY_FILE))
            .expect("key file should exist")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
