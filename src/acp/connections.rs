//! The ACP connections this core holds open, one per chat.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as TurnLock;

use super::Connection;

/// A chat's connection while a turn is being taken over it. One turn at a
/// time: the adapter's stdio is a single conversation, so a second prompt
/// waits for nothing and is told so.
pub type Held<C> = Arc<TurnLock<Connection<C>>>;

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

    /// Let go of `chat_id`'s connection: whatever was on the other end of it
    /// is not answering any more.
    pub fn forget(&self, chat_id: &str) {
        self.held
            .lock()
            .expect("no holder of the lock panics")
            .remove(chat_id);
    }
}
