//! The ACP client: newline-delimited JSON-RPC to the adapter inside a chat's
//! container (ADR-0001, ADR-0006).

mod docker;
mod error;
mod scripted;

use std::future::Future;
use std::time::Duration;

use log::debug;
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
        let channel = timeout(self.patience, self.transport.open(container))
            .await
            .map_err(|_| AcpError::Unstarted {
                container: container.to_owned(),
                patience: self.patience,
            })??;
        let mut calls = Calls {
            channel,
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
        timeout(self.patience, self.exchange(&request, id, method))
            .await
            .map_err(|_| AcpError::Silent {
                method: method.to_owned(),
                patience: self.patience,
            })?
    }

    /// One request handed over and its answer waited for. Handing it over can
    /// block as long as answering can: the adapter's stdin is a pipe.
    async fn exchange(
        &mut self,
        request: &Value,
        id: u64,
        method: &str,
    ) -> Result<Value, AcpError> {
        self.channel.send(&request.to_string()).await?;
        self.answer_to(id, method).await
    }

    /// The answer to request `id`. Notifications and answers to anything else
    /// are the adapter talking about its own business; they are read past.
    async fn answer_to(&mut self, id: u64, method: &str) -> Result<Value, AcpError> {
        loop {
            let line = self.channel.receive().await?;
            let message: Value = match serde_json::from_str(&line) {
                Ok(message) => message,
                Err(nonsense) => {
                    debug!("adapter said something that is not json-rpc: {nonsense}");
                    continue;
                }
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

    /// One `session/update` notification's params, as a real adapter sends
    /// them: the session it belongs to, wrapped around the update itself.
    fn update(session_id: &str, said: &str) -> Value {
        json!({
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": said},
            },
        })
    }

    /// The update alone, as ADR-0006 writes it down.
    fn recorded(said: &str) -> Value {
        json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": said},
        })
    }

    #[tokio::test]
    async fn a_new_session_is_handed_back_by_the_adapter() {
        let adapter = Adapter::new(ScriptedAdapter::opening(SESSION));

        let connection = adapter
            .open_session(CONTAINER)
            .await
            .expect("the scripted adapter should open a session");

        assert_eq!(connection.session_id(), SESSION);
        assert_eq!(adapter.transport().containers(), [CONTAINER]);
    }

    #[tokio::test]
    async fn a_turn_records_the_prompt_it_sent_and_then_the_updates_that_answered() {
        let adapter = Adapter::new(ScriptedAdapter::answering(
            SESSION,
            &[update(SESSION, "on it"), update(SESSION, " — done")],
        ));
        let mut connection = adapter
            .open_session(CONTAINER)
            .await
            .expect("the scripted adapter should open a session");
        let mut record = Vec::new();

        connection
            .take_turn("ship the ladder", &mut |payload| {
                record.push(payload.clone());
                Ok(())
            })
            .await
            .expect("the scripted turn should end");

        assert_eq!(
            record,
            [
                json!({
                    "sessionId": SESSION,
                    "prompt": [{"type": "text", "text": "ship the ladder"}],
                }),
                recorded("on it"),
                recorded(" — done"),
            ]
        );
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

        let connection = adapter
            .open_session(CONTAINER)
            .await
            .expect("notifications between answers should not derail the handshake");

        assert_eq!(connection.session_id(), SESSION);
        assert!(
            adapter.transport().chattered(),
            "the fake never spoke out of turn, so nothing was proved"
        );
    }

    #[tokio::test]
    async fn a_turn_asks_the_adapter_over_the_session_the_handshake_opened() {
        let adapter = Adapter::new(ScriptedAdapter::answering(SESSION, &[]));
        let mut connection = adapter
            .open_session(CONTAINER)
            .await
            .expect("the scripted adapter should open a session");

        connection
            .take_turn("ship the ladder", &mut |_| Ok(()))
            .await
            .expect("the scripted turn should end");

        assert_eq!(
            adapter.transport().requests().last(),
            Some(&json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "session/prompt",
                "params": {
                    "sessionId": SESSION,
                    "prompt": [{"type": "text", "text": "ship the ladder"}],
                },
            }))
        );
    }

    #[tokio::test]
    async fn an_update_belonging_to_another_session_is_not_recorded() {
        let adapter = Adapter::new(ScriptedAdapter::answering(
            SESSION,
            &[
                update("3f2b1c4d-0000-4000-8000-000000000002", "not for you"),
                update(SESSION, "on it"),
            ],
        ));
        let mut connection = adapter
            .open_session(CONTAINER)
            .await
            .expect("the scripted adapter should open a session");
        let mut record = Vec::new();

        connection
            .take_turn("ship the ladder", &mut |payload| {
                record.push(payload.clone());
                Ok(())
            })
            .await
            .expect("the scripted turn should end");

        assert_eq!(
            record.len(),
            2,
            "another session's words landed in this one: {record:?}"
        );
        assert_eq!(record[1], recorded("on it"));
    }

    #[tokio::test]
    async fn an_adapter_that_dies_mid_turn_fails_with_what_it_had_already_said_recorded() {
        let adapter = Adapter::new(ScriptedAdapter::dying_mid_turn(
            SESSION,
            &[update(SESSION, "on i")],
        ));
        let mut connection = adapter
            .open_session(CONTAINER)
            .await
            .expect("the scripted adapter should open a session");
        let mut record = Vec::new();

        let error = connection
            .take_turn("ship the ladder", &mut |payload| {
                record.push(payload.clone());
                Ok(())
            })
            .await
            .expect_err("a turn the adapter never ends should fail");

        assert!(
            matches!(error, AcpError::Closed),
            "a broken pipe should read as one, got: {error}"
        );
        assert_eq!(record, [
            json!({
                "sessionId": SESSION,
                "prompt": [{"type": "text", "text": "ship the ladder"}],
            }),
            recorded("on i"),
        ]);
    }

    #[tokio::test]
    async fn a_turn_the_adapter_never_ends_gives_up_rather_than_hangs() {
        let adapter = Adapter::waiting(ScriptedAdapter::opening(SESSION), IMPATIENT);
        let mut connection = adapter
            .open_session(CONTAINER)
            .await
            .expect("the scripted adapter should open a session");

        let error = connection
            .take_turn("ship the ladder", &mut |_| Ok(()))
            .await
            .expect_err("silence should end in a failure");

        assert!(
            format!("{error}").contains("session/prompt"),
            "error should name the call that hung, got: {error}"
        );
    }

    #[tokio::test]
    async fn a_turn_whose_record_cannot_be_written_fails_rather_than_going_on() {
        let adapter = Adapter::new(ScriptedAdapter::answering(SESSION, &[]));
        let mut connection = adapter
            .open_session(CONTAINER)
            .await
            .expect("the scripted adapter should open a session");

        let error = connection
            .take_turn("ship the ladder", &mut |_| {
                Err(anyhow::anyhow!("the dataset is not mounted"))
            })
            .await
            .expect_err("a turn that cannot be recorded should fail");

        assert!(
            format!("{error:#}").contains("the dataset is not mounted"),
            "error should carry why the record failed, got: {error:#}"
        );
        assert_eq!(
            adapter.transport().requests().len(),
            2,
            "the prompt went out although it was never recorded"
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
    async fn an_adapter_that_never_starts_gives_up_rather_than_hangs() {
        let adapter = Adapter::waiting(ScriptedAdapter::never_starting(), IMPATIENT);

        let error = adapter
            .open_session(CONTAINER)
            .await
            .expect_err("a start that never finishes should fail");

        assert!(
            format!("{error}").contains(CONTAINER),
            "error should name the container that would not start an adapter, got: {error}"
        );
    }

    #[tokio::test]
    async fn an_adapter_that_never_takes_a_message_gives_up_rather_than_hangs() {
        let adapter = Adapter::waiting(ScriptedAdapter::never_taking(), IMPATIENT);

        let error = adapter
            .open_session(CONTAINER)
            .await
            .expect_err("a message that cannot be handed over should fail");

        assert!(
            format!("{error}").contains("initialize"),
            "error should name the call that hung, got: {error}"
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
