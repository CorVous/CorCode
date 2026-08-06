//! The ACP connections this core holds open, one per chat.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tokio::sync::{Mutex as TurnLock, OwnedMutexGuard};

use super::Connection;

/// A chat's connection while a turn is being taken over it. One turn at a
/// time: the adapter's stdio is a single conversation, so a second prompt
/// waits for nothing and is told so.
pub type Held<C> = Arc<TurnLock<Connection<C>>>;

/// A chat's connection while this core has it to itself. Whoever holds one is
/// the turn in progress, and the pool and the archive gate both read that.
pub type Turn<C> = OwnedMutexGuard<Connection<C>>;

/// Which chats can be prompted right now. Connections live only in memory:
/// a core that restarts has none, and ADR-0007's ladder is what puts them
/// back.
pub struct Connections<C> {
    held: Mutex<HashMap<String, Held<C>>>,
}

impl<C> Default for Connections<C> {
    fn default() -> Self {
        Self {
            held: Mutex::new(HashMap::new()),
        }
    }
}

impl<C> Connections<C> {
    /// Keep `connection` as the one prompts to `chat_id` go over, in place of
    /// whatever was there before.
    pub fn hold(&self, chat_id: &str, connection: Connection<C>) {
        self.held
            .lock()
            .expect("no holder of the lock panics")
            .insert(chat_id.to_owned(), Arc::new(TurnLock::new(connection)));
    }

    /// The connection `chat_id` can be prompted over, if it has one.
    #[must_use]
    pub fn of(&self, chat_id: &str) -> Option<Held<C>> {
        self.held
            .lock()
            .expect("no holder of the lock panics")
            .get(chat_id)
            .map(Arc::clone)
    }

    /// The chats whose connection is in use right now, which is to say the
    /// chats taking a turn.
    #[must_use]
    pub fn busy_chat_ids(&self) -> HashSet<String> {
        self.held
            .lock()
            .expect("no holder of the lock panics")
            .iter()
            .filter(|(_, connection)| connection.try_lock().is_err())
            .map(|(chat_id, _)| chat_id.clone())
            .collect()
    }

    /// Let go of `chat_id`'s connection: whatever was on the other end of it
    /// is not answering any more.
    pub fn forget(&self, chat_id: &str) {
        self.held
            .lock()
            .expect("no holder of the lock panics")
            .remove(chat_id);
    }
}
