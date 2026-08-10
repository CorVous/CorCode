//! An adapter that answers from a script, for tests that care about the
//! protocol rather than about a container.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};

use super::{AcpChannel, AcpError, AcpTransport, NO_SUCH_METHOD};

/// The methods a real adapter answers, spelled as it hears them. The fake
/// stands in for the adapter, not for the client, so it names them itself: a
/// script written from the client's own constants would answer to whatever the
/// client asked for and could never catch it asking for the wrong thing.
const RESUME_SESSION: &str = "session/resume";
const LOAD_SESSION: &str = "session/load";
const NEW_SESSION: &str = "session/new";
const PROMPT: &str = "session/prompt";

/// A notification the fake volunteers before every answer, as a real adapter
/// does: no client may assume the next line is the one it is waiting for.
const CHATTER: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"…"}}}"#;

/// What the script says to one method.
#[derive(Clone)]
enum Answer {
    Result(Value),
    Refusal(String),
    /// A method this adapter has never heard of, answered as JSON-RPC says.
    Unheard,
    /// A method this adapter takes and never answers, so that a client's
    /// patience can be proved rather than assumed.
    Silence,
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

/// Which of a fake's channels close without answering the turn they are in.
/// `Once` is a container that outlived one broken pipe: everything opened
/// after the first channel works.
#[derive(Clone, Copy)]
enum Dies {
    Never,
    Always,
    Once,
}

impl Dies {
    /// Whether the `nth` channel this adapter opens will close on a turn.
    const fn takes(self, nth: usize) -> bool {
        match self {
            Self::Never => false,
            Self::Always => true,
            Self::Once => nth == 1,
        }
    }
}

/// How a channel ends: at the first thing it is asked, in the middle of the
/// turn it is taking, or not at all.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Death {
    None,
    MidTurn,
    AtOnce,
}

/// Answers ACP calls from a canned script and remembers what it was asked.
///
/// A clone is the same adapter, not another one: a test that has handed its
/// fake to the thing under test keeps a copy to read what it heard.
#[derive(Clone)]
pub struct ScriptedAdapter {
    answers: Arc<Script>,
    unscripted: Answer,
    asks: Arc<Vec<Value>>,
    replay: Arc<Vec<Value>>,
    updates: Arc<Vec<Value>>,
    heard: Arc<Mutex<Heard>>,
    stall: Stall,
    dies: Dies,
    replay_trails: bool,
    dawdle: Duration,
}

/// Everything the fake was told, for the assertions to read back.
#[derive(Default)]
struct Heard {
    containers: Vec<String>,
    requests: Vec<Value>,
    chattered: bool,
    channels: usize,
    killed: bool,
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

    /// An adapter whose new session opens in `mode` rather than in whatever
    /// the managed settings asked for, as one that clamped the default does.
    #[must_use]
    pub fn opening_in_mode(session_id: &str, mode: &str) -> Self {
        let mut script = working_script(session_id);
        script.insert(
            NEW_SESSION.to_owned(),
            Answer::Result(json!({
                "sessionId": session_id,
                "modes": {"currentModeId": mode},
            })),
        );
        Self::reading(script)
    }

    /// An adapter that answers a load with the mode the session came back in,
    /// which a clamped session is in just as a new one can be.
    #[must_use]
    pub fn loading_in_mode(session_id: &str, mode: &str) -> Self {
        let mut script = prompting_script(session_id);
        script.insert(
            LOAD_SESSION.to_owned(),
            Answer::Result(json!({"modes": {"currentModeId": mode}})),
        );
        Self::reading(script)
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

    /// An adapter that replays `history` *after* it answers the load, as one
    /// whose replay and answer cross on the wire does. ACP fixes no order
    /// between them, so display-silence may not lean on one.
    #[must_use]
    pub fn loading_late(session_id: &str, history: &[Value], updates: &[Value]) -> Self {
        Self {
            replay_trails: true,
            ..Self::loading(session_id, history, updates)
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
        Self::deathly(session_id, updates, Dies::Always)
    }

    /// An adapter that loses its first channel mid-turn and answers on every
    /// channel after it, as a container that outlived one broken pipe does.
    #[must_use]
    pub fn dying_once(session_id: &str, updates: &[Value]) -> Self {
        Self::deathly(session_id, updates, Dies::Once)
    }

    /// Leave every channel opened from here on closed from the first word, as
    /// an agent that has died inside a container it is still holding does.
    pub fn dies(&self) {
        self.heard().killed = true;
    }

    /// An adapter that takes prompts on every channel `dies` spares.
    fn deathly(session_id: &str, updates: &[Value], dies: Dies) -> Self {
        Self {
            updates: Arc::new(updates.to_vec()),
            dies,
            ..Self::reading(prompting_script(session_id))
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
        Self {
            unscripted: Answer::Silence,
            ..Self::reading(Script::new())
        }
    }

    /// An adapter that opens a session, takes a prompt and never ends the
    /// turn, as one whose agent has wandered off does.
    #[must_use]
    pub fn never_ending_a_turn(session_id: &str) -> Self {
        let mut script = working_script(session_id);
        script.insert(PROMPT.to_owned(), Answer::Silence);
        Self::reading(script)
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
            unscripted: Answer::Unheard,
            asks: Arc::new(Vec::new()),
            replay: Arc::new(Vec::new()),
            updates: Arc::new(Vec::new()),
            heard: Arc::new(Mutex::new(Heard::default())),
            stall: Stall::Nowhere,
            dies: Dies::Never,
            replay_trails: false,
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
/// handshake and a new session. Neither rung of ADR-0007's ladder is in it, so
/// an adapter that has not been given one has never heard of it.
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
            NEW_SESSION.to_owned(),
            Answer::Result(json!({"sessionId": session_id})),
        ),
    ])
}

/// The working script, plus a turn that ends.
fn prompting_script(session_id: &str) -> Script {
    let mut script = working_script(session_id);
    script.insert(
        PROMPT.to_owned(),
        Answer::Result(json!({"stopReason": "end_turn"})),
    );
    script
}

impl AcpTransport for ScriptedAdapter {
    type Channel = ScriptedChannel;

    async fn open(&self, container: &str) -> Result<Self::Channel, AcpError> {
        let death = {
            let mut heard = self.heard();
            heard.containers.push(container.to_owned());
            heard.channels += 1;
            if heard.killed {
                Death::AtOnce
            } else if self.dies.takes(heard.channels) {
                Death::MidTurn
            } else {
                Death::None
            }
        };
        if matches!(self.stall, Stall::Starting) {
            std::future::pending::<()>().await;
        }
        Ok(ScriptedChannel {
            answers: Arc::clone(&self.answers),
            unscripted: self.unscripted.clone(),
            asks: Arc::clone(&self.asks),
            replay: Arc::clone(&self.replay),
            updates: Arc::clone(&self.updates),
            heard: Arc::clone(&self.heard),
            unread: VecDeque::new(),
            stall: self.stall,
            death,
            replay_trails: self.replay_trails,
            dawdle: self.dawdle,
        })
    }
}

/// One scripted conversation.
pub struct ScriptedChannel {
    answers: Arc<Script>,
    unscripted: Answer,
    asks: Arc<Vec<Value>>,
    replay: Arc<Vec<Value>>,
    updates: Arc<Vec<Value>>,
    heard: Arc<Mutex<Heard>>,
    unread: VecDeque<String>,
    stall: Stall,
    death: Death,
    replay_trails: bool,
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

    /// What the script says to `method`, unless this channel dies before it
    /// gets there. A method the script does not name is one this adapter has
    /// never heard of, as a real one says rather than falling quiet; a
    /// notification, carrying no id, is owed nothing at all.
    fn answer_to(&self, method: &str, id: &Value) -> Option<String> {
        if id.is_null() {
            return None;
        }
        let withheld = match self.death {
            Death::None => false,
            Death::MidTurn => method == PROMPT,
            Death::AtOnce => true,
        };
        if withheld {
            return None;
        }
        let answer = match self.answers.get(method).unwrap_or(&self.unscripted) {
            Answer::Result(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Answer::Refusal(complaint) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": complaint}})
            }
            Answer::Unheard => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": NO_SUCH_METHOD, "message": format!("no method {method}")},
            }),
            Answer::Silence => return None,
        };
        Some(answer.to_string())
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
        if method == PROMPT {
            for ask in self.asks.iter() {
                self.unread.push_back(ask.to_string());
            }
            self.stream(&self.updates.clone());
        }
        let replaying = method == LOAD_SESSION;
        if replaying && !self.replay_trails {
            self.stream(&self.replay.clone());
        }
        if let Some(answer) = self.answer_to(&method, &id) {
            self.unread.push_back(answer);
        }
        if replaying && self.replay_trails {
            self.stream(&self.replay.clone());
        }
        Ok(())
    }

    async fn receive(&mut self) -> Result<String, AcpError> {
        tokio::time::sleep(self.dawdle).await;
        match self.unread.pop_front() {
            Some(line) => Ok(line),
            None if self.death != Death::None => Err(AcpError::Closed),
            None => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::timeout;

    use super::*;

    const CONTAINER: &str = "corcode-chat-01K1TESTCHATID0000000000";
    const SESSION: &str = "3f2b1c4d-0000-4000-8000-000000000001";

    /// Long enough that a queued line is always there, short enough that a
    /// test proving the fake said nothing does not hold the suite up.
    const BRIEF: Duration = Duration::from_millis(50);

    /// The next thing the fake says that answers a request rather than
    /// volunteering something of its own.
    async fn answer_from(channel: &mut ScriptedChannel) -> Value {
        loop {
            let line = timeout(BRIEF, channel.receive())
                .await
                .expect("the fake should answer a request it was sent")
                .expect("the channel should stay open");
            let message: Value = serde_json::from_str(&line).expect("the fake should speak JSON");
            if message.get("method").is_none() {
                return message;
            }
        }
    }

    async fn channel_to(adapter: &ScriptedAdapter) -> ScriptedChannel {
        adapter
            .open(CONTAINER)
            .await
            .expect("the scripted adapter should open a channel")
    }

    async fn say(channel: &mut ScriptedChannel, message: &Value) {
        channel
            .send(&message.to_string())
            .await
            .expect("the fake should take a message");
    }

    #[tokio::test]
    async fn a_method_the_script_does_not_name_is_refused_with_the_code_json_rpc_reserves() {
        let mut channel = channel_to(&ScriptedAdapter::opening(SESSION)).await;

        say(
            &mut channel,
            &json!({"jsonrpc": "2.0", "id": 7, "method": "session/wander", "params": {}}),
        )
        .await;

        let answer = answer_from(&mut channel).await;
        assert_eq!(answer["id"], json!(7));
        assert_eq!(
            answer["error"]["code"].as_i64(),
            Some(-32601),
            "an unheard method is JSON-RPC's method-not-found, got: {answer}"
        );
    }

    #[tokio::test]
    async fn a_notification_is_taken_and_left_unanswered() {
        let mut channel = channel_to(&ScriptedAdapter::opening(SESSION)).await;

        say(
            &mut channel,
            &json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {}}),
        )
        .await;

        let mut said = Vec::new();
        while let Ok(line) = timeout(BRIEF, channel.receive()).await {
            said.push(line.expect("the channel should stay open"));
        }
        assert!(
            said.iter().all(|line| line.contains("\"method\"")),
            "a notification is owed no answer, got: {said:?}"
        );
    }
}
