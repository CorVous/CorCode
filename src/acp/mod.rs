//! The ACP client: newline-delimited JSON-RPC to the adapter inside a chat's
//! container (ADR-0001, ADR-0006).

mod docker;
mod error;
mod scripted;

use std::future::Future;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::timeout;

pub use docker::{ADAPTER, DockerExec};
pub use error::AcpError;
pub use scripted::ScriptedAdapter;

use crate::plane::WORKSPACE_MOUNT;

/// The ACP version this client speaks.
const PROTOCOL_VERSION: u32 = 1;

/// How long any one call may take. The adapter boots Node and the agent SDK
/// on the first one, so this is patience, not a deadline anyone should meet.
const PATIENCE: Duration = Duration::from_secs(120);

/// A way to reach the ACP adapter of one chat's container.
pub trait AcpTransport {
    type Channel: AcpChannel + Send;

    /// Start an adapter in `container` and hand back the channel to it.
    fn open(&self, container: &str)
    -> impl Future<Output = Result<Self::Channel, AcpError>> + Send;
}

/// A JSON-RPC conversation, one message per call in each direction. Framing
/// — a message per line — belongs to whoever implements this.
pub trait AcpChannel {
    fn send(&mut self, message: &str) -> impl Future<Output = Result<(), AcpError>> + Send;

    fn receive(&mut self) -> impl Future<Output = Result<String, AcpError>> + Send;
}

/// The agent side of a chat, spoken to over `transport`.
pub struct Adapter<T> {
    transport: T,
    patience: Duration,
}

impl<T: AcpTransport + Sync> Adapter<T> {
    pub const fn new(transport: T) -> Self {
        Self {
            transport,
            patience: PATIENCE,
        }
    }

    /// An adapter given less than the usual patience, for tests that would
    /// rather not wait two minutes to watch a call time out.
    pub const fn waiting(transport: T, patience: Duration) -> Self {
        Self {
            transport,
            patience,
        }
    }

    pub const fn transport(&self) -> &T {
        &self.transport
    }

    /// Hand shake with the adapter and open a fresh session over the chat's
    /// workspace, answering with the session id ADR-0006 stores.
    pub async fn open_session(&self, container: &str) -> Result<String, AcpError> {
        let mut calls = Calls {
            channel: self.transport.open(container).await?,
            patience: self.patience,
            next_id: 1,
        };
        calls
            .call(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "clientCapabilities": {
                        "fs": {"readTextFile": false, "writeTextFile": false},
                        "terminal": false,
                    },
                }),
            )
            .await?;
        let session = calls
            .call(
                "session/new",
                json!({"cwd": WORKSPACE_MOUNT, "mcpServers": []}),
            )
            .await?;
        session["sessionId"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| AcpError::Unreadable {
                method: "session/new".to_owned(),
                answer: session.to_string(),
            })
    }
}

/// One channel's numbered requests, each waited on for an answer of its own.
struct Calls<C> {
    channel: C,
    patience: Duration,
    next_id: u64,
}

impl<C: AcpChannel> Calls<C> {
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, AcpError> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.channel.send(&request.to_string()).await?;
        timeout(self.patience, self.answer_to(id, method))
            .await
            .map_err(|_| AcpError::Silent {
                method: method.to_owned(),
                patience: self.patience,
            })?
    }

    /// The answer to request `id`. Notifications and answers to anything else
    /// are the adapter talking about its own business; they are read past.
    async fn answer_to(&mut self, id: u64, method: &str) -> Result<Value, AcpError> {
        loop {
            let line = self.channel.receive().await?;
            let message: Value = match serde_json::from_str(&line) {
                Ok(message) => message,
                Err(_) => continue,
            };
            if message["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(refusal) = message.get("error") {
                return Err(AcpError::Refused {
                    method: method.to_owned(),
                    complaint: refusal["message"]
                        .as_str()
                        .unwrap_or(&refusal.to_string())
                        .to_owned(),
                });
            }
            return Ok(message["result"].clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;

    const CONTAINER: &str = "corcode-chat-01K1TESTCHATID0000000000";
    const SESSION: &str = "3f2b1c4d-0000-4000-8000-000000000001";

    /// Long enough that a scripted answer always beats it, short enough that
    /// a test waiting on nothing does not hold the suite up.
    const IMPATIENT: Duration = Duration::from_millis(200);

    #[tokio::test]
    async fn a_new_session_is_handed_back_by_the_adapter() {
        let adapter = Adapter::new(ScriptedAdapter::opening(SESSION));

        let session_id = adapter
            .open_session(CONTAINER)
            .await
            .expect("the scripted adapter should open a session");

        assert_eq!(session_id, SESSION);
        assert_eq!(adapter.transport().containers(), [CONTAINER]);
    }

    #[tokio::test]
    async fn the_handshake_comes_before_the_session_and_the_ids_climb() {
        let adapter = Adapter::new(ScriptedAdapter::opening(SESSION));

        adapter
            .open_session(CONTAINER)
            .await
            .expect("the scripted adapter should open a session");

        assert_eq!(
            adapter.transport().requests(),
            [
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": 1,
                        "clientCapabilities": {
                            "fs": {"readTextFile": false, "writeTextFile": false},
                            "terminal": false,
                        },
                    },
                }),
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session/new",
                    "params": {"cwd": "/workspace", "mcpServers": []},
                }),
            ]
        );
    }

    #[tokio::test]
    async fn a_line_answering_no_request_is_passed_over() {
        let adapter = Adapter::new(ScriptedAdapter::opening(SESSION));

        let session_id = adapter
            .open_session(CONTAINER)
            .await
            .expect("notifications between answers should not derail the handshake");

        assert_eq!(session_id, SESSION);
        assert!(
            adapter.transport().chattered(),
            "the fake never spoke out of turn, so nothing was proved"
        );
    }

    #[tokio::test]
    async fn an_adapter_that_refuses_fails_loudly() {
        let adapter = Adapter::new(ScriptedAdapter::refusing(
            "session/new",
            "not authenticated",
        ));

        let error = adapter
            .open_session(CONTAINER)
            .await
            .expect_err("a refused session should fail");

        let message = format!("{error}");
        assert!(
            message.contains("session/new") && message.contains("not authenticated"),
            "error should quote the adapter, got: {message}"
        );
    }

    #[tokio::test]
    async fn an_adapter_that_says_nothing_gives_up_rather_than_hangs() {
        let adapter = Adapter::waiting(ScriptedAdapter::silent(), IMPATIENT);

        let error = adapter
            .open_session(CONTAINER)
            .await
            .expect_err("silence should end in a failure");

        assert!(
            format!("{error}").contains("initialize"),
            "error should name the call that hung, got: {error}"
        );
    }
}
