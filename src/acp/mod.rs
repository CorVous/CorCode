//! The ACP client: newline-delimited JSON-RPC to the adapter inside a chat's
//! container (ADR-0001, ADR-0006).

mod connections;
mod docker;
mod error;
mod scripted;

use std::fmt::{self, Debug, Formatter};
use std::future::Future;
use std::time::Duration;

use log::debug;
use serde_json::{Value, json};
use tokio::time::timeout;

pub use connections::{Connections, Held};
pub use docker::{ADAPTER, DockerExec};
pub use error::AcpError;
pub use scripted::ScriptedAdapter;

use crate::plane::WORKSPACE_MOUNT;

/// The ACP version this client speaks.
const PROTOCOL_VERSION: u32 = 1;

/// The JSON-RPC this client speaks.
const JSONRPC: &str = "2.0";

/// How long any one call may take. The adapter boots Node and the agent SDK
/// on the first one, so this is patience, not a deadline anyone should meet.
const PATIENCE: Duration = Duration::from_secs(120);

/// How long a turn may go silent. An agent reads, edits and runs tests inside
/// a single prompt, and every line it streams meanwhile starts the wait over,
/// so what this catches is an adapter that has stopped speaking altogether.
const TURN_PATIENCE: Duration = Duration::from_secs(600);

/// The notification an adapter streams a turn over.
const SESSION_UPDATE: &str = "session/update";

/// The one thing an adapter asks a client that declares no capabilities for.
const PERMISSION_ASK: &str = "session/request_permission";

/// What a permission ask leaves in the log, since the operator is the one who
/// would have granted it (ADR-0006).
const PERMISSION_DECLINED: &str = "permission_declined";

/// JSON-RPC's code for a method the receiver does not implement.
const NO_SUCH_METHOD: i64 = -32601;

/// Where a turn's payloads go as they happen: the prompt on its way out, then
/// each update on its way in. Recording is what makes them real (ADR-0006),
/// so a record that cannot be written ends the turn.
pub type Record<'a> = dyn FnMut(&Value) -> anyhow::Result<()> + Send + 'a;

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
    turn_patience: Duration,
}

impl<T: AcpTransport + Sync> Adapter<T> {
    pub const fn new(transport: T) -> Self {
        Self {
            transport,
            patience: PATIENCE,
            turn_patience: TURN_PATIENCE,
        }
    }

    /// An adapter given less than the usual patience, for tests that would
    /// rather not wait two minutes to watch a call time out.
    pub const fn waiting(transport: T, patience: Duration) -> Self {
        Self {
            transport,
            patience,
            turn_patience: patience,
        }
    }

    pub const fn transport(&self) -> &T {
        &self.transport
    }

    /// Hand shake with the adapter and open a fresh session over the chat's
    /// workspace, answering with the connection every later turn is taken
    /// over.
    pub async fn open_session(&self, container: &str) -> Result<Connection<T::Channel>, AcpError> {
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
        let session_id = session["sessionId"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| AcpError::Unreadable {
                method: "session/new".to_owned(),
                answer: session.to_string(),
            })?;
        Ok(Connection {
            calls,
            session_id,
            turn_patience: self.turn_patience,
        })
    }
}

/// One chat's open conversation with its adapter, held across turns.
pub struct Connection<C> {
    calls: Calls<C>,
    session_id: String,
    turn_patience: Duration,
}

/// A connection is its session; the pipe underneath has nothing to say.
impl<C> Debug for Connection<C> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Connection")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl<C: AcpChannel> Connection<C> {
    /// The session the adapter opened, as ADR-0006 stores it.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Take one turn: `said` goes into `record` before it goes on the wire,
    /// then everything the adapter says about this session until it ends the
    /// turn. Another session's traffic is the adapter's own business.
    pub async fn take_turn(&mut self, said: &str, record: &mut Record<'_>) -> Result<(), AcpError> {
        let params = json!({
            "sessionId": self.session_id,
            "prompt": [{"type": "text", "text": said}],
        });
        keep(record, &params)?;
        let session_id = self.session_id.clone();
        self.calls
            .call_within(
                self.turn_patience,
                "session/prompt",
                params.clone(),
                &mut |message| {
                    worth_recording(message, &session_id)
                        .map_or_else(|| Ok(()), |payload| keep(record, &payload))
                },
            )
            .await
            .map(|_| ())
    }
}

/// What one thing the adapter says mid-turn leaves in this chat's log: the
/// update it streamed, or a line saying its permission ask was turned down.
fn worth_recording(message: &Value, session_id: &str) -> Option<Value> {
    if message["params"]["sessionId"].as_str() != Some(session_id) {
        return None;
    }
    match message["method"].as_str()? {
        SESSION_UPDATE => Some(streamed(&message["params"])),
        PERMISSION_ASK => Some(declined(&message["params"])),
        _ => None,
    }
}

/// The update inside a notification, or the whole notification when the
/// adapter carries the fields itself: a shape this build does not know passes
/// through unchanged rather than being written down as nothing (ADR-0006).
fn streamed(params: &Value) -> Value {
    params.get("update").unwrap_or(params).clone()
}

/// The core's own line for a permission ask (ADR-0006).
fn declined(params: &Value) -> Value {
    let asked_for = params["toolCall"]["title"]
        .as_str()
        .unwrap_or("a tool call");
    json!({
        "corcode": PERMISSION_DECLINED,
        "text": format!("Declined a permission request: {asked_for}."),
    })
}

/// The answer we owe something the adapter said. A notification is owed
/// nothing; a request is owed a response even when we cannot do what it asks,
/// or the adapter waits on us for the rest of the turn. We declared no
/// capabilities, so the one ask we can meet is a permission ask, and the
/// answer to that is no: nothing outside the container is ours to grant
/// (ADR-0001).
fn owed(message: &Value) -> Option<Value> {
    let id = message.get("id").filter(|id| !id.is_null())?;
    Some(if message["method"] == PERMISSION_ASK {
        json!({
            "jsonrpc": JSONRPC,
            "id": id,
            "result": {"outcome": no_permission(&message["params"]["options"])},
        })
    } else {
        json!({
            "jsonrpc": JSONRPC,
            "id": id,
            "error": {
                "code": NO_SUCH_METHOD,
                "message": format!(
                    "{} is not one this client answers",
                    message["method"].as_str().unwrap_or("that"),
                ),
            },
        })
    })
}

/// A permission ask turned down: the adapter's own reject option where it
/// offered one, and a cancelled outcome where it offered none.
fn no_permission(options: &Value) -> Value {
    options
        .as_array()
        .into_iter()
        .flatten()
        .find(|option| {
            option["kind"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("reject"))
        })
        .and_then(|option| option["optionId"].as_str())
        .map_or_else(
            || json!({"outcome": "cancelled"}),
            |chosen| json!({"outcome": "selected", "optionId": chosen}),
        )
}

fn keep(record: &mut Record<'_>, payload: &Value) -> Result<(), AcpError> {
    record(payload).map_err(|source| AcpError::Unrecorded { source })
}

fn silent(method: &str, patience: Duration) -> AcpError {
    AcpError::Silent {
        method: method.to_owned(),
        patience,
    }
}

/// A look at every message that answers no request of ours, so a caller can
/// pick its own out of the stream.
type Overhear<'a> = dyn FnMut(&Value) -> Result<(), AcpError> + Send + 'a;

/// One channel's numbered requests, each waited on for an answer of its own.
struct Calls<C> {
    channel: C,
    patience: Duration,
    next_id: u64,
}

impl<C: AcpChannel> Calls<C> {
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, AcpError> {
        self.call_within(self.patience, method, params, &mut |_| Ok(()))
            .await
    }

    /// One request, with `patience` for each thing the adapter says or fails
    /// to say, and everything it says meanwhile offered to `overhear`.
    async fn call_within(
        &mut self,
        patience: Duration,
        method: &str,
        params: Value,
        overhear: &mut Overhear<'_>,
    ) -> Result<Value, AcpError> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc": JSONRPC, "id": id, "method": method, "params": params});
        self.say(&request, patience, method).await?;
        self.answer_to(id, method, patience, overhear).await
    }

    /// One message handed over. Handing it over can block as long as
    /// answering can: the adapter's stdin is a pipe.
    async fn say(
        &mut self,
        message: &Value,
        patience: Duration,
        method: &str,
    ) -> Result<(), AcpError> {
        timeout(patience, self.channel.send(&message.to_string()))
            .await
            .map_err(|_| silent(method, patience))?
    }

    /// The answer to request `id`. Nothing carrying a `method` is that answer,
    /// whatever id it wears: JSON-RPC numbers each direction separately, and
    /// the adapter counts from one as well. So a message of the adapter's own
    /// is offered to `overhear` and answered where it asks something of us,
    /// and any other answer than ours is read past.
    ///
    /// The channel is only read while a call is outstanding: anything the
    /// adapter volunteers between turns waits in the pipe for the next one.
    async fn answer_to(
        &mut self,
        id: u64,
        method: &str,
        patience: Duration,
        overhear: &mut Overhear<'_>,
    ) -> Result<Value, AcpError> {
        loop {
            let line = timeout(patience, self.channel.receive())
                .await
                .map_err(|_| silent(method, patience))??;
            let message: Value = match serde_json::from_str(&line) {
                Ok(message) => message,
                Err(nonsense) => {
                    debug!("adapter said something that is not json-rpc: {nonsense}");
                    continue;
                }
            };
            if message.get("method").is_some() {
                overhear(&message)?;
                if let Some(answer) = owed(&message) {
                    self.say(&answer, patience, method).await?;
                }
                continue;
            }
            if message["id"].as_u64() != Some(id) {
                debug!("adapter answered a request that is not ours: {message}");
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

    /// One `session/request_permission` request, as the adapter makes it:
    /// numbered from the adapter's own id space, which overlaps ours.
    fn permission_ask(id: u64, session_id: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/request_permission",
            "params": {
                "sessionId": session_id,
                "toolCall": {"toolCallId": "call_1", "title": "git push"},
                "options": [
                    {"optionId": "yes", "name": "Allow", "kind": "allow_once"},
                    {"optionId": "no", "name": "Reject", "kind": "reject_once"},
                ],
            },
        })
    }

    #[tokio::test]
    async fn a_request_of_the_adapters_own_is_not_mistaken_for_the_answer_to_ours() {
        let adapter = Adapter::new(ScriptedAdapter::asking(
            SESSION,
            &[permission_ask(3, SESSION)],
            &[update(SESSION, "on it")],
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
            .expect("a request wearing our id should not end the turn");

        assert_eq!(
            record.last(),
            Some(&recorded("on it")),
            "the turn ended on the adapter's own request: {record:?}"
        );
    }

    #[tokio::test]
    async fn a_permission_ask_is_turned_down_rather_than_left_to_block_the_turn() {
        let adapter = Adapter::new(ScriptedAdapter::asking(
            SESSION,
            &[permission_ask(1, SESSION)],
            &[],
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
            .expect("a permission ask should not hold the turn up");

        assert_eq!(
            adapter.transport().requests().last(),
            Some(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"outcome": {"outcome": "selected", "optionId": "no"}},
            })),
            "the adapter is still waiting on an answer it will never get"
        );
        assert_eq!(
            record.last().and_then(|line| line["corcode"].as_str()),
            Some("permission_declined"),
            "the operator is never told their agent asked: {record:?}"
        );
    }

    #[tokio::test]
    async fn an_update_that_carries_its_own_fields_is_written_down_whole() {
        let carried = json!({"sessionId": SESSION, "sessionUpdate": "plan", "entries": []});
        let adapter = Adapter::new(ScriptedAdapter::answering(
            SESSION,
            std::slice::from_ref(&carried),
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
            record.last(),
            Some(&carried),
            "an update this build does not know was written down as nothing"
        );
    }

    #[tokio::test]
    async fn a_turn_that_keeps_speaking_outlives_the_patience_for_silence() {
        let said: Vec<Value> = (0..8)
            .map(|nth| update(SESSION, &nth.to_string()))
            .collect();
        let adapter = Adapter::waiting(
            ScriptedAdapter::dawdling(SESSION, &said, IMPATIENT / 5),
            IMPATIENT,
        );
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
            .expect("a turn that never falls silent should not be given up on");

        assert_eq!(
            record.len(),
            said.len() + 1,
            "a steadily streaming turn was cut short: {record:?}"
        );
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
        assert_eq!(
            record,
            [
                json!({
                    "sessionId": SESSION,
                    "prompt": [{"type": "text", "text": "ship the ladder"}],
                }),
                recorded("on i"),
            ]
        );
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

        let logged = format!("{:#}", anyhow::Error::new(error));
        assert!(
            logged.contains("the dataset is not mounted"),
            "error should carry why the record failed, got: {logged}"
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
