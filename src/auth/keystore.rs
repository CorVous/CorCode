//! The signing key on disk: generated at first boot, persisted on the data
//! dataset, and rotated to log every device out (ADR-0003).

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anyhow::Result;

use super::session::SigningKey;

/// File under the data directory holding the raw key material.
pub const KEY_FILE: &str = "signing_key";

/// The live signing key, backed by a file under the data directory.
pub struct KeyStore {
    path: PathBuf,
    current: RwLock<SigningKey>,
}

impl KeyStore {
    /// Load the key stored under `data_dir`, generating and persisting one
    /// if this is the first boot.
    pub fn open(_data_dir: &Path) -> Result<Self> {
        todo!()
    }

    /// The key currently in force.
    #[must_use]
    pub fn current(&self) -> SigningKey {
        todo!()
    }

    /// Replace the key with fresh material, invalidating every cookie.
    pub fn rotate(&self) -> Result<()> {
        todo!()
    }
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
