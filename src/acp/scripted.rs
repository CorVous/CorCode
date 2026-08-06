//! An adapter that answers from a script, for tests that care about the
//! protocol rather than about a container.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};

use super::{AcpChannel, AcpError, AcpTransport, LOAD_SESSION, NO_SUCH_METHOD, RESUME_SESSION};

/// A notification the fake volunteers before every answer, as a real adapter
/// does: no client may assume the next line is the one it is waiting for.
const CHATTER: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"…"}}}"#;

/// What the script says to one method. A method the script does not name is
/// one the fake never answers.
enum Answer {
    Result(Value),
    Refusal(String),
    /// A method this adapter has never heard of, answered as JSON-RPC says.
    Unheard,
}

/// What the fake answers to each method it knows.
type Script = BTreeMap<String, Answer>;

/// Where a fake stops responding altogether, so that a client's patience can
/// be proved rather than assumed.
#[derive(Clone, Copy)]
enum Stall {
    Nowhere,
    Starting,
    Taking,
}

/// Answers ACP calls from a canned script and remembers what it was asked.
pub struct ScriptedAdapter {
    answers: Arc<Script>,
    asks: Arc<Vec<Value>>,
    replay: Arc<Vec<Value>>,
    updates: Arc<Vec<Value>>,
    heard: Arc<Mutex<Heard>>,
    stall: Stall,
    hangs_up: bool,
    dawdle: Duration,
}

/// Everything the fake was told, for the assertions to read back.
#[derive(Default)]
struct Heard {
    containers: Vec<String>,
    requests: Vec<Value>,
    chattered: bool,
}

impl ScriptedAdapter {
    /// The adapter as it behaves when all is well: a handshake, then a
    /// session under `session_id`.
    #[must_use]
    pub fn opening(session_id: &str) -> Self {
        Self::reading(working_script(session_id))
    }

    /// An adapter that streams `updates` at every prompt and then ends the
    /// turn. Each update is the params of one `session/update` notification,
    /// so a test can address one at whichever session it likes.
    ///
    /// It knows neither rung of ADR-0007's ladder, as an adapter that has
    /// forgotten the chat does.
    #[must_use]
    pub fn answering(session_id: &str, updates: &[Value]) -> Self {
        Self {
            updates: Arc::new(updates.to_vec()),
            ..Self::reading(prompting_script(session_id))
        }
    }

    /// An adapter that hands a session back where it left off, replaying
    /// nothing (ADR-0007 rung 1).
    #[must_use]
    pub fn resuming(session_id: &str, updates: &[Value]) -> Self {
        Self::climbable(session_id, RESUME_SESSION, updates)
    }

    /// An adapter that will not resume but replays `history` as
    /// `session/update` notifications when the session is loaded, before it
    /// answers the load (rung 2).
    #[must_use]
    pub fn loading(session_id: &str, history: &[Value], updates: &[Value]) -> Self {
        Self {
            replay: Arc::new(history.to_vec()),
            ..Self::climbable(session_id, LOAD_SESSION, updates)
        }
    }

    /// An adapter that answers `rung` and then takes prompts.
    fn climbable(session_id: &str, rung: &str, updates: &[Value]) -> Self {
        let mut script = prompting_script(session_id);
        script.insert(rung.to_owned(), Answer::Result(Value::Null));
        Self {
            updates: Arc::new(updates.to_vec()),
            ..Self::reading(script)
        }
    }

    /// An adapter that asks the client for something mid-turn before it
    /// streams `updates`. Each ask is a whole JSON-RPC message, so a test can
    /// give one the very id the client is waiting on.
    #[must_use]
    pub fn asking(session_id: &str, asks: &[Value], updates: &[Value]) -> Self {
        Self {
            asks: Arc::new(asks.to_vec()),
            ..Self::answering(session_id, updates)
        }
    }

    /// An adapter that takes `dawdle` over every line it hands over, so that
    /// a turn can be caught in flight or drawn out past a client's patience.
    #[must_use]
    pub fn dawdling(session_id: &str, updates: &[Value], dawdle: Duration) -> Self {
        Self {
            dawdle,
            ..Self::answering(session_id, updates)
        }
    }

    /// An adapter that streams `updates` and then closes its channel without
    /// ending the turn, as one whose container was killed does.
    #[must_use]
    pub fn dying_mid_turn(session_id: &str, updates: &[Value]) -> Self {
        Self {
            updates: Arc::new(updates.to_vec()),
            hangs_up: true,
            ..Self::reading(working_script(session_id))
        }
    }

    /// An adapter that turns `method` down.
    #[must_use]
    pub fn refusing(method: &str, complaint: &str) -> Self {
        let mut script = working_script("never-opened");
        script.insert(method.to_owned(), Answer::Refusal(complaint.to_owned()));
        Self::reading(script)
    }

    /// An adapter that starts and then says nothing at all.
    #[must_use]
    pub fn silent() -> Self {
        Self::reading(Script::new())
    }

    /// An adapter whose start never finishes, as a wedged container's exec
    /// does.
    #[must_use]
    pub fn never_starting() -> Self {
        Self::stalling(Stall::Starting)
    }

    /// An adapter that starts and then never takes a message, as a pipe
    /// nobody is draining does.
    #[must_use]
    pub fn never_taking() -> Self {
        Self::stalling(Stall::Taking)
    }

    /// The containers an adapter was started in, in order.
    #[must_use]
    pub fn containers(&self) -> Vec<String> {
        self.heard().containers.clone()
    }

    /// The JSON-RPC requests the client sent, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<Value> {
        self.heard().requests.clone()
    }

    /// Whether the fake ever spoke out of turn, so that a test relying on
    /// interleaved notifications knows it proved something.
    #[must_use]
    pub fn chattered(&self) -> bool {
        self.heard().chattered
    }

    fn reading(script: Script) -> Self {
        Self {
            answers: Arc::new(script),
            asks: Arc::new(Vec::new()),
            replay: Arc::new(Vec::new()),
            updates: Arc::new(Vec::new()),
            heard: Arc::new(Mutex::new(Heard::default())),
            stall: Stall::Nowhere,
            hangs_up: false,
            dawdle: Duration::ZERO,
        }
    }

    fn stalling(stall: Stall) -> Self {
        Self {
            stall,
            ..Self::reading(Script::new())
        }
    }

    fn heard(&self) -> MutexGuard<'_, Heard> {
        self.heard.lock().expect("no holder of the lock panics")
    }
}

/// What every scripted adapter answers before anything goes wrong: a
/// handshake, a new session, and neither rung of ADR-0007's ladder.
fn working_script(session_id: &str) -> Script {
    Script::from([
        (
            "initialize".to_owned(),
            Answer::Result(json!({
                "protocolVersion": 1,
                "agentInfo": {"name": "scripted", "version": "0.0.0"},
            })),
        ),
        (
            "session/new".to_owned(),
            Answer::Result(json!({"sessionId": session_id})),
        ),
        (RESUME_SESSION.to_owned(), Answer::Unheard),
        (LOAD_SESSION.to_owned(), Answer::Unheard),
    ])
}

/// The working script, plus a turn that ends.
fn prompting_script(session_id: &str) -> Script {
    let mut script = working_script(session_id);
    script.insert(
        "session/prompt".to_owned(),
        Answer::Result(json!({"stopReason": "end_turn"})),
    );
    script
}

impl AcpTransport for ScriptedAdapter {
    type Channel = ScriptedChannel;

    async fn open(&self, container: &str) -> Result<Self::Channel, AcpError> {
        self.heard().containers.push(container.to_owned());
        if matches!(self.stall, Stall::Starting) {
            std::future::pending::<()>().await;
        }
        Ok(ScriptedChannel {
            answers: Arc::clone(&self.answers),
            asks: Arc::clone(&self.asks),
            replay: Arc::clone(&self.replay),
            updates: Arc::clone(&self.updates),
            heard: Arc::clone(&self.heard),
            unread: VecDeque::new(),
            stall: self.stall,
            hangs_up: self.hangs_up,
            dawdle: self.dawdle,
        })
    }
}

/// One scripted conversation.
pub struct ScriptedChannel {
    answers: Arc<Script>,
    asks: Arc<Vec<Value>>,
    replay: Arc<Vec<Value>>,
    updates: Arc<Vec<Value>>,
    heard: Arc<Mutex<Heard>>,
    unread: VecDeque<String>,
    stall: Stall,
    hangs_up: bool,
    dawdle: Duration,
}

impl ScriptedChannel {
    /// Line up `params` as `session/update` notifications, each one a whole
    /// message the client has to read past to reach the answer it waits for.
    fn stream(&mut self, params: &[Value]) {
        for update in params {
            self.unread.push_back(
                json!({"jsonrpc": "2.0", "method": "session/update", "params": update}).to_string(),
            );
        }
    }
}

impl AcpChannel for ScriptedChannel {
    async fn send(&mut self, message: &str) -> Result<(), AcpError> {
        if matches!(self.stall, Stall::Taking) {
            std::future::pending::<()>().await;
        }
        let request: Value = serde_json::from_str(message).expect("the client should send JSON");
        let method = request["method"].as_str().unwrap_or_default().to_owned();
        let id = request["id"].clone();
        let mut heard = self.heard.lock().expect("no holder of the lock panics");
        heard.requests.push(request);
        heard.chattered = true;
        drop(heard);
        if method.is_empty() {
            return Ok(());
        }
        self.unread.push_back(CHATTER.to_owned());
        if method == "session/prompt" {
            for ask in self.asks.iter() {
                self.unread.push_back(ask.to_string());
            }
            self.stream(&self.updates.clone());
        }
        if method == LOAD_SESSION {
            self.stream(&self.replay.clone());
        }
        match self.answers.get(&method) {
            Some(Answer::Result(result)) => self
                .unread
                .push_back(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()),
            Some(Answer::Refusal(complaint)) => self.unread.push_back(
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": complaint}})
                    .to_string(),
            ),
            Some(Answer::Unheard) => self.unread.push_back(
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": NO_SUCH_METHOD, "message": format!("no method {method}")},
                })
                .to_string(),
            ),
            None => {}
        }
        Ok(())
    }

    async fn receive(&mut self) -> Result<String, AcpError> {
        tokio::time::sleep(self.dawdle).await;
        match self.unread.pop_front() {
            Some(line) => Ok(line),
            None if self.hangs_up => Err(AcpError::Closed),
            None => std::future::pending().await,
        }
    }
}
