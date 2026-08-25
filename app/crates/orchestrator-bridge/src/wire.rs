//! The PHONE-facing protocol. Plain JSON, one message per WebSocket text frame,
//! deliberately nothing like the daemon's bincode wire.
//!
//! WHY A SECOND PROTOCOL AT ALL: `orchestrator_host::protocol` is a *frozen*,
//! version-locked wire whose whole point is that a rebuilt client must never
//! talk to an older daemon (see `client.rs`). That is exactly the wrong contract
//! for a phone, which ships on the App Store's clock and will always be some
//! unknown number of versions behind. So the phone gets a protocol that versions
//! WITHOUT lockstep, on two rules:
//!
//!   1. **Unknown object fields are ignored on both sides.** Adding a field is
//!      therefore always backward compatible; an old phone simply doesn't see it.
//!   2. **An unknown `t` from the phone answers `err` and KEEPS the connection;
//!      an unknown `t` at the phone is dropped silently.** A new message type is
//!      never a disconnect.
//!
//! Everything here is pure data + pure functions — no socket, no clock, no hub —
//! so the entire shape of the wire is unit-tested against literal JSON.
//!
//! THE PHONE CAN NOW TYPE, AND THE AUTHORIZATION STORY IS NOT IN THIS FILE.
//! `input` and `key` carry a `sid` and nothing else — the phone never says what
//! KIND of session it is typing into, so it cannot claim a shell is a claude
//! session. The daemon resolves the kind from its own state and refuses shells,
//! dead sessions and ids it has never heard of (`ClientRole::Phone` +
//! `dispatch_checked`). Everything here is the COURIER for that decision and
//! never the decision: `caps.input` and `can_input` exist so a phone can grey a
//! composer out early, and neither one grants anything.

use serde::{Deserialize, Deserializer, Serialize};

use orchestrator_host::host::SessionInfo;
use orchestrator_host::protocol::PhoneKey;
use orchestrator_host::session::{CliKind, Phase, TroubleKind};

/// The only protocol version this bridge speaks. A phone announcing anything else
/// is refused at hello rather than half-understood.
///
/// BUMPED 1 → 2 FOR INPUT, and that bump is a deliberate hard cut rather than a
/// consequence of the two rules above. Those rules cover MESSAGES; they do not
/// cover a promise. proto 1 shipped with `caps.input` documented as permanently
/// false and no `input_result` on the wire at all, so a proto-1 phone has no code
/// that could read the answer to an input attempt. Refusing it at hello costs one
/// `bad_proto` and an "update the app"; admitting it means a phone told
/// `caps.input: true` by a bridge whose answers it will silently drop. The cost is
/// real: a read-only phone built against proto 1 must update before it reconnects.
pub const PROTO: u32 = 2;

/// Hard cap on one inbound frame, enforced BEFORE any parsing (see
/// [`decode_frame`]). A phone has no business sending 64 KiB, so anything larger
/// is either a bug or someone probing; either way it must not reach serde.
pub const MAX_FRAME: usize = 65536;

// ---------------------------------------------------------------- phone → bridge

/// What the phone may say. Deserialize-only on purpose: the bridge never
/// *produces* one of these, so there is no code path that could accidentally
/// echo phone input back onto the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum PhoneMsg {
    Hello { proto: u32, token: String },
    Ping,
    /// Type a whole composed prompt into a session. `sid` and text: no session
    /// KIND, because the phone does not get to describe what it is typing into.
    Input { sid: u64, text: String, #[serde(default)] rid: u64 },
    /// One control key, for answering a prompt an agent is already showing.
    Key { sid: u64, key: PhoneKeyName, #[serde(default)] rid: u64 },
    /// Any `t` this build does not know. This variant is what makes rule 2
    /// above *structural*: an unrecognized type is a value we can answer, not a
    /// parse failure that would tempt the loop into dropping the socket.
    #[serde(other)]
    Unknown,
}

/// The keys a phone may press, as the phone spells them.
///
/// A separate enum from the daemon's [`PhoneKey`] for the same reason [`Cli`] is
/// separate from `CliKind`: this one is a shape a shipped app encodes, and
/// renaming a variant in the host crate must not silently change what that app
/// has to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhoneKeyName {
    Enter,
    Escape,
    Up,
    Down,
    Tab,
    /// A key name this build has never heard of — rule 2, one level down. A
    /// newer phone's key must be a VALUE the loop can answer, not a deserialize
    /// error that costs the whole frame (and, before the handshake, the socket).
    Unknown,
}

/// Hand-written because `#[serde(other)]` is not available here: serde allows it
/// only on a unit variant of an internally- or adjacently-tagged enum, and this
/// one is deserialized from a bare string. Deserialize-only, like [`PhoneMsg`] —
/// the bridge reads keys, it never mints one.
impl<'de> Deserialize<'de> for PhoneKeyName {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(d)?.as_str() {
            "enter" => Self::Enter,
            "escape" => Self::Escape,
            "up" => Self::Up,
            "down" => Self::Down,
            "tab" => Self::Tab,
            _ => Self::Unknown,
        })
    }
}

impl PhoneKeyName {
    /// The daemon's word for this key, or `None` when there is not one.
    ///
    /// `None` is answered by the connection loop with a refusal; it must never
    /// become a guess, because the nearest key to one the phone did not name is
    /// still a key the user did not press.
    pub fn to_daemon(self) -> Option<PhoneKey> {
        Some(match self {
            Self::Enter => PhoneKey::Enter,
            Self::Escape => PhoneKey::Escape,
            Self::Up => PhoneKey::Up,
            Self::Down => PhoneKey::Down,
            Self::Tab => PhoneKey::Tab,
            Self::Unknown => return None,
        })
    }
}

/// Why a frame never became a [`PhoneMsg`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// Over [`MAX_FRAME`] — rejected without parsing.
    TooLarge { len: usize },
    BadJson(String),
}

impl FrameError {
    /// The stable `code` for the `err` / `hello_err` this becomes on the wire.
    pub fn code(&self) -> &'static str {
        match self {
            Self::TooLarge { .. } => "frame_too_large",
            Self::BadJson(_) => "bad_json",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::TooLarge { len } => format!("frame is {len} bytes; the limit is {MAX_FRAME}"),
            Self::BadJson(e) => e.clone(),
        }
    }
}

/// Decode one inbound frame.
///
/// THE LENGTH CHECK COMES FIRST, and that ordering is the point: a size limit
/// applied after `serde_json` has already walked the bytes is not a limit, it is
/// a description of what was allocated. The test
/// `an_oversized_frame_is_rejected_before_it_is_parsed` pins it with a payload
/// that is perfectly valid JSON — if it ever parses, the check has moved.
pub fn decode_frame(bytes: &[u8]) -> Result<PhoneMsg, FrameError> {
    if bytes.len() > MAX_FRAME {
        return Err(FrameError::TooLarge { len: bytes.len() });
    }
    serde_json::from_slice(bytes).map_err(|e| FrameError::BadJson(e.to_string()))
}

// ---------------------------------------------------------------- bridge → phone

/// What the bridge announces it can do. It is a field rather than an omission so
/// the phone can decide on the ANSWER instead of on its own build number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caps {
    /// THE FLAG THE iOS APP READS TO DECIDE WHETHER TO SHOW A COMPOSER AT ALL.
    /// It says this bridge understands `input`/`key` and will answer them — not
    /// that any particular session will accept one. That is per-session
    /// ([`WireSession::can_input`]) and, for real, the daemon's call.
    pub input: bool,
}

impl Caps {
    /// The only caps this bridge ever sends.
    pub fn v0() -> Self {
        Self { input: true }
    }
}

/// What the bridge may say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum BridgeMsg {
    HelloOk {
        proto: u32,
        epoch: String,
        server_time: u64,
        caps: Caps,
    },
    HelloErr {
        code: String,
        message: String,
    },
    /// The ONE full snapshot, sent immediately after `hello_ok`. Everything
    /// after it is a delta.
    Sessions {
        epoch: String,
        sessions: Vec<WireSession>,
    },
    /// An upsert. The phone applies it iff `rev` is greater than the rev it
    /// already holds for this `(epoch, sid)`.
    Session {
        epoch: String,
        rev: u64,
        session: WireSession,
    },
    Gone {
        epoch: String,
        sid: u64,
    },
    Pong,
    /// The answer to exactly one `input` or `key`, ok or not.
    ///
    /// ALWAYS sent, including on success: an input whose fate the phone cannot
    /// see is an input the user retypes. `reason` carries the DAEMON's own
    /// sentence wherever there is one ("Kod does not let a phone type into a
    /// shell…"), because this bridge is not what decided and must not paraphrase
    /// a refusal it did not make.
    InputResult {
        /// Echoes the `rid` the phone sent.
        ///
        /// Without it a result can only be matched by session, and a LATE answer
        /// to one send settles a DIFFERENT, newer one — dispatching an Enter
        /// against a paste that never landed, i.e. submitting the wrong text at
        /// an agent. `sid` alone cannot tell those apart; a phone that is
        /// backgrounded mid-send hits it on ordinary use.
        #[serde(default)]
        rid: u64,
        sid: u64,
        ok: bool,
        reason: Option<String>,
    },
    Err {
        code: String,
        message: String,
    },
}

impl BridgeMsg {
    pub fn err(code: &str, message: impl Into<String>) -> Self {
        Self::Err { code: code.to_string(), message: message.into() }
    }

    pub fn input_ok(rid: u64, sid: u64) -> Self {
        Self::InputResult { rid, sid, ok: true, reason: None }
    }

    pub fn input_refused(rid: u64, sid: u64, reason: impl Into<String>) -> Self {
        Self::InputResult { rid, sid, ok: false, reason: Some(reason.into()) }
    }

    pub fn hello_err(code: &str, message: impl Into<String>) -> Self {
        Self::HelloErr { code: code.to_string(), message: message.into() }
    }

    /// Serialize to the one text frame that carries it.
    pub fn to_frame(&self) -> String {
        // Every variant is a plain struct of owned data, so this cannot fail;
        // `unwrap` would still be a panic on the connection thread, so fall back
        // to a frame the phone can at least classify.
        serde_json::to_string(self).unwrap_or_else(|e| {
            format!(r#"{{"t":"err","code":"encode","message":{}}}"#, escape(&e.to_string()))
        })
    }
}

fn escape(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

// ---------------------------------------------------------------- the session view

/// Which CLI a session runs. A separate enum from [`CliKind`] on purpose: this
/// one is a wire shape the phone parses, and it must not silently change because
/// someone renamed a variant in the host crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cli {
    Claude,
    Codex,
    Shell,
}

impl From<CliKind> for Cli {
    fn from(k: CliKind) -> Self {
        match k {
            CliKind::Claude => Cli::Claude,
            CliKind::Codex => Cli::Codex,
            CliKind::Shell => Cli::Shell,
        }
    }
}

/// The coarse phase, as the phone spells it.
///
/// `AwaitingDecision` becomes `awaiting`: the desktop word is about the decision
/// *object* (which v0 cannot show and cannot answer), while the phone's word is
/// about the human — the only thing a read-only client can act on is "this one
/// is waiting for you".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WirePhase {
    Spawning,
    Idle,
    Busy,
    Awaiting,
    Dead,
}

impl From<Phase> for WirePhase {
    fn from(p: Phase) -> Self {
        match p {
            Phase::Spawning => WirePhase::Spawning,
            Phase::Idle => WirePhase::Idle,
            Phase::Busy => WirePhase::Busy,
            Phase::AwaitingDecision => WirePhase::Awaiting,
            Phase::Dead => WirePhase::Dead,
        }
    }
}

/// The stable slug for a trouble chip. Slugs, not prose: the phone owns its own
/// wording and its own localisation, and a server-rendered sentence would freeze
/// both.
fn trouble_slug(k: TroubleKind) -> &'static str {
    match k {
        TroubleKind::RateLimit => "rate_limit",
        TroubleKind::ApiError => "api_error",
        TroubleKind::Overloaded => "overloaded",
    }
}

/// One session as the phone sees it — `<S>` in the contract.
///
/// This is a LOSSY projection of [`SessionInfo`] and that is the design: no
/// grid, no scrollback, no decision body, no resume handle. A phone on a hotel
/// wifi wants to know which sessions are waiting on it, and shipping anything
/// more would be shipping the terminal to a screen that cannot drive it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSession {
    pub sid: u64,
    pub cli: Cli,
    /// `project_slug` — named `project` here because the phone never sees any
    /// other kind of project identifier.
    pub project: String,
    pub title: String,
    pub phase: WirePhase,
    /// wall-clock ms the current phase was ENTERED. The phone ticks the age
    /// locally rather than being told "3m ago" once and then lying about it.
    pub phase_since: u64,
    pub alive: bool,
    /// Whether the phone should offer a composer for this session — true for
    /// claude and codex, false for a shell.
    ///
    /// A COURTESY, NOT THE ENFORCEMENT. It exists so the composer is greyed out
    /// BEFORE the user types a paragraph into a session that was never going to
    /// take it. THIS VALUE BEING TRUE DOES NOT MAKE AN INPUT LEGAL: the daemon
    /// re-decides on every single one, against its own view of the session, and a
    /// phone that ignores this field is refused there rather than here. `alive`
    /// stays a separate field for the same reason — the daemon refuses dead
    /// sessions too, and the phone can combine the two without either one
    /// pretending to be the whole rule.
    pub can_input: bool,
    /// May be `""` — a session that has not finished a turn has no last message,
    /// and an empty string is honest where a null would tempt a placeholder.
    pub last_message: String,
    /// A one-line headline of what the agent is blocked on, or null. v0 sends
    /// only the headline: the full decision body is the thing you must not
    /// approve from a phone, so it is not on the wire to be approved.
    pub pending_headline: Option<String>,
    pub trouble: Option<String>,
    pub limit_hit: bool,
    pub limit_percent: Option<u8>,
    pub limit_reset: Option<String>,
}

impl From<&SessionInfo> for WireSession {
    fn from(i: &SessionInfo) -> Self {
        let limit = i.usage_limit.as_ref();
        Self {
            sid: i.id.0,
            cli: i.kind.into(),
            project: i.project_slug.clone(),
            title: i.title.clone(),
            phase: i.phase.into(),
            phase_since: i.phase_since_ms,
            alive: i.alive,
            can_input: i.kind != CliKind::Shell,
            last_message: i.last_message.clone(),
            pending_headline: i.pending.as_ref().map(|p| p.view.summary()),
            trouble: i.trouble.map(|t| trouble_slug(t.kind).to_string()),
            // A session with NO banner is not "not limited" in some third state:
            // `limit_hit` is false and every limit_* field is null, which is the
            // same shape a phone would compute anyway.
            limit_hit: limit.is_some_and(|u| u.hit),
            limit_percent: limit.and_then(|u| u.percent),
            // `reset_label` returns "" for a banner that carried no parseable
            // clock. An empty string would render as a blank chip that looks
            // like a rendering bug, so it becomes null — the absence it is.
            limit_reset: limit.map(|u| u.reset_label()).filter(|s| !s.is_empty()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator_host::decision::{DecisionView, PendingDecision};
    use orchestrator_host::session::{SessionId, Trouble, UsageLimit};
    use serde_json::{json, Value};

    fn v(m: &BridgeMsg) -> Value {
        serde_json::from_str(&m.to_frame()).expect("bridge frames are valid json")
    }

    /// A SessionInfo with every optional emptied — the common case.
    fn bare_info() -> SessionInfo {
        SessionInfo {
            id: SessionId(7),
            kind: CliKind::Claude,
            project_slug: "kod".into(),
            title: "kod — main".into(),
            phase: Phase::Idle,
            alive: true,
            pending: None,
            dirty: 42,
            cli_session_id: Some("abc-123".into()),
            last_message: "".into(),
            phase_since_ms: 1_700_000_000_000,
            trouble: None,
            usage_limit: None,
        }
    }

    // ---- message shapes ----

    #[test]
    fn hello_ok_json_shape() {
        let m = BridgeMsg::HelloOk {
            proto: PROTO,
            epoch: "e1".into(),
            server_time: 1234,
            caps: Caps::v0(),
        };
        assert_eq!(
            v(&m),
            json!({"t":"hello_ok","proto":2,"epoch":"e1","server_time":1234,"caps":{"input":true}})
        );
    }

    #[test]
    fn hello_err_json_shape() {
        let m = BridgeMsg::hello_err("unauthorized", "bad token");
        assert_eq!(v(&m), json!({"t":"hello_err","code":"unauthorized","message":"bad token"}));
    }

    #[test]
    fn sessions_snapshot_json_shape() {
        let m = BridgeMsg::Sessions { epoch: "e1".into(), sessions: vec![] };
        assert_eq!(v(&m), json!({"t":"sessions","epoch":"e1","sessions":[]}));
    }

    #[test]
    fn session_upsert_json_shape() {
        let m = BridgeMsg::Session {
            epoch: "e1".into(),
            rev: 3,
            session: WireSession::from(&bare_info()),
        };
        assert_eq!(
            v(&m),
            json!({
                "t": "session",
                "epoch": "e1",
                "rev": 3,
                "session": {
                    "sid": 7,
                    "cli": "claude",
                    "project": "kod",
                    "title": "kod — main",
                    "phase": "idle",
                    "phase_since": 1_700_000_000_000u64,
                    "alive": true,
                    "can_input": true,
                    "last_message": "",
                    "pending_headline": null,
                    "trouble": null,
                    "limit_hit": false,
                    "limit_percent": null,
                    "limit_reset": null
                }
            })
        );
    }

    #[test]
    fn gone_json_shape() {
        let m = BridgeMsg::Gone { epoch: "e1".into(), sid: 9 };
        assert_eq!(v(&m), json!({"t":"gone","epoch":"e1","sid":9}));
    }

    #[test]
    fn pong_and_err_json_shape() {
        assert_eq!(v(&BridgeMsg::Pong), json!({"t":"pong"}));
        assert_eq!(
            v(&BridgeMsg::err("unknown_type", "no such t")),
            json!({"t":"err","code":"unknown_type","message":"no such t"})
        );
    }

    #[test]
    fn input_result_json_shape() {
        // `rid` echoes what the phone sent. It is the field that lets a LATE
        // answer be discarded instead of settling a newer send — matching on sid
        // alone dispatches an Enter against a paste that never landed.
        assert_eq!(
            v(&BridgeMsg::input_ok(11, 7)),
            json!({"t":"input_result","rid":11,"sid":7,"ok":true,"reason":null})
        );
        assert_eq!(
            v(&BridgeMsg::input_refused(11, 7, "that session has ended")),
            json!({"t":"input_result","rid":11,"sid":7,"ok":false,
                   "reason":"that session has ended"})
        );
    }

    /// The rid must survive the round trip in BOTH directions, since a mismatch
    /// in either one silently reintroduces the bug it exists to prevent.
    #[test]
    fn the_request_id_round_trips() {
        let decoded = decode_frame(br#"{"t":"input","sid":3,"text":"hi","rid":99}"#);
        assert_eq!(decoded, Ok(PhoneMsg::Input { sid: 3, text: "hi".into(), rid: 99 }));
        // Absent rid is 0, so an older phone still works — it simply cannot tell
        // two answers apart, which is what it does today anyway.
        let decoded = decode_frame(br#"{"t":"input","sid":3,"text":"hi"}"#);
        assert_eq!(decoded, Ok(PhoneMsg::Input { sid: 3, text: "hi".into(), rid: 0 }));
        assert_eq!(v(&BridgeMsg::input_ok(99, 3))["rid"], json!(99));
    }

    #[test]
    fn the_wire_types_are_exactly_the_contract() {
        // The EXHAUSTIVE match is the guard: adding a bridge→phone message type
        // stops this test compiling, so the set a phone must handle cannot grow
        // by accident. It also pins every serde tag at once — a `rename_all`
        // surprise on any variant lands here rather than on a phone.
        //
        // UPDATED DELIBERATELY when `input_result` was added: this is the answer
        // to an input attempt, and it is listed because a phone that can type has
        // to be told what happened. Adding one here is a protocol decision, which
        // is exactly why it has to be typed out by hand.
        fn tag(m: &BridgeMsg) -> &'static str {
            match m {
                BridgeMsg::HelloOk { .. } => "hello_ok",
                BridgeMsg::HelloErr { .. } => "hello_err",
                BridgeMsg::Sessions { .. } => "sessions",
                BridgeMsg::Session { .. } => "session",
                BridgeMsg::Gone { .. } => "gone",
                BridgeMsg::Pong => "pong",
                BridgeMsg::InputResult { .. } => "input_result",
                BridgeMsg::Err { .. } => "err",
            }
        }
        let all = [
            BridgeMsg::HelloOk {
                proto: PROTO,
                epoch: "e".into(),
                server_time: 0,
                caps: Caps::v0(),
            },
            BridgeMsg::hello_err("c", "m"),
            BridgeMsg::Sessions { epoch: "e".into(), sessions: vec![] },
            BridgeMsg::Session {
                epoch: "e".into(),
                rev: 1,
                session: WireSession::from(&bare_info()),
            },
            BridgeMsg::Gone { epoch: "e".into(), sid: 1 },
            BridgeMsg::Pong,
            BridgeMsg::input_ok(1, 1),
            BridgeMsg::err("c", "m"),
        ];
        let mut seen: Vec<&str> = Vec::new();
        for m in &all {
            assert_eq!(v(m)["t"], json!(tag(m)), "wrong tag for {m:?}");
            seen.push(tag(m));
        }
        seen.sort_unstable();
        assert_eq!(
            seen,
            [
                "err",
                "gone",
                "hello_err",
                "hello_ok",
                "input_result",
                "pong",
                "session",
                "sessions"
            ]
        );
        assert!(Caps::v0().input, "the composer flag the iOS app reads went dark");
    }

    #[test]
    fn the_phone_can_say_exactly_these_things() {
        // The same exhaustive-match guard, pointed the other way down the wire.
        fn tag(m: &PhoneMsg) -> &'static str {
            match m {
                PhoneMsg::Hello { .. } => "hello",
                PhoneMsg::Ping => "ping",
                PhoneMsg::Input { .. } => "input",
                PhoneMsg::Key { .. } => "key",
                PhoneMsg::Unknown => "<unknown>",
            }
        }
        assert_eq!(tag(&decode_frame(br#"{"t":"hello","proto":2,"token":"k"}"#).unwrap()), "hello");
        assert_eq!(tag(&decode_frame(br#"{"t":"ping"}"#).unwrap()), "ping");
        assert_eq!(tag(&decode_frame(br#"{"t":"input","sid":1,"text":"hi"}"#).unwrap()), "input");
        assert_eq!(tag(&decode_frame(br#"{"t":"key","sid":1,"key":"enter"}"#).unwrap()), "key");
        // Anything a future phone invents still lands in `Unknown`, which the
        // server answers with `err` rather than a disconnect.
        assert_eq!(tag(&decode_frame(br#"{"t":"approve","sid":1}"#).unwrap()), "<unknown>");
    }

    // ---- phone → bridge parsing ----

    #[test]
    fn phone_hello_and_ping_parse() {
        assert_eq!(
            decode_frame(br#"{"t":"hello","proto":1,"token":"s3cret"}"#),
            Ok(PhoneMsg::Hello { proto: 1, token: "s3cret".into() })
        );
        assert_eq!(decode_frame(br#"{"t":"ping"}"#), Ok(PhoneMsg::Ping));
    }

    #[test]
    fn an_unknown_type_parses_as_unknown_rather_than_failing() {
        // The whole no-lockstep story: a `t` from a NEWER phone is a value we can
        // answer, not a parse error that would look like a broken client.
        assert_eq!(decode_frame(br#"{"t":"approve","sid":1}"#), Ok(PhoneMsg::Unknown));
        assert_eq!(decode_frame(br#"{"t":"whatever"}"#), Ok(PhoneMsg::Unknown));
    }

    #[test]
    fn an_input_frame_parses() {
        assert_eq!(
            decode_frame(br#"{"t":"input","sid":7,"text":"run the tests"}"#),
            Ok(PhoneMsg::Input { sid: 7, text: "run the tests".into(), rid: 0 })
        );
        // Text is carried verbatim — newlines and all. Nothing here decides that
        // a newline means "submit"; `key` is how a phone submits, so a pasted
        // paragraph cannot become N accidental turns.
        assert_eq!(
            decode_frame(br#"{"t":"input","sid":7,"text":"a\nb"}"#),
            Ok(PhoneMsg::Input { sid: 7, text: "a\nb".into(), rid: 0 })
        );
    }

    #[test]
    fn every_key_name_parses_to_the_daemons_word_for_it() {
        for (name, daemon) in [
            ("enter", PhoneKey::Enter),
            ("escape", PhoneKey::Escape),
            ("up", PhoneKey::Up),
            ("down", PhoneKey::Down),
            ("tab", PhoneKey::Tab),
        ] {
            let frame = format!(r#"{{"t":"key","sid":3,"key":"{name}"}}"#);
            let Ok(PhoneMsg::Key { sid, key, .. }) = decode_frame(frame.as_bytes()) else {
                panic!("{name} did not parse as a key frame");
            };
            assert_eq!(sid, 3);
            assert_eq!(key.to_daemon(), Some(daemon), "{name} maps to the wrong key");
        }
    }

    #[test]
    fn an_unrecognised_key_name_is_a_value_not_a_parse_failure() {
        // Rule 2, one level down. A phone one version ahead sending a key this
        // build has never heard of must leave the loop something to ANSWER: a
        // deserialize error here would cost the whole frame, and before the
        // handshake it costs the socket.
        assert_eq!(
            decode_frame(br#"{"t":"key","sid":3,"key":"f7"}"#),
            Ok(PhoneMsg::Key { sid: 3, key: PhoneKeyName::Unknown, rid: 0 })
        );
        assert_eq!(PhoneKeyName::Unknown.to_daemon(), None, "an unknown key must not be guessed");
        // …and it must not be silently coerced to the nearest real key, which
        // would press something the user never pressed.
        assert_eq!(
            decode_frame(br#"{"t":"key","sid":3,"key":"ENTER"}"#),
            Ok(PhoneMsg::Key { sid: 3, key: PhoneKeyName::Unknown, rid: 0 }),
            "key names are exact; a case-folded match would be a guess"
        );
    }

    #[test]
    fn unknown_object_fields_are_ignored() {
        // Rule 1. A newer phone that adds a field to `hello` must still be
        // understood by this build.
        assert_eq!(
            decode_frame(br#"{"t":"hello","proto":1,"token":"k","device":"iphone","nonce":7}"#),
            Ok(PhoneMsg::Hello { proto: 1, token: "k".into() })
        );
    }

    #[test]
    fn malformed_json_is_bad_json_not_a_panic() {
        let e = decode_frame(b"{not json").unwrap_err();
        assert_eq!(e.code(), "bad_json");
    }

    #[test]
    fn a_frame_missing_its_type_is_bad_json() {
        // No `t` at all is not an unknown type — there is nothing to be unknown.
        assert_eq!(decode_frame(br#"{"proto":1}"#).unwrap_err().code(), "bad_json");
    }

    #[test]
    fn an_oversized_frame_is_rejected_before_it_is_parsed() {
        // PERFECTLY VALID JSON that would parse to `ping` if it ever reached
        // serde — so a `TooLarge` here can only mean the length check ran first.
        let pad = "a".repeat(MAX_FRAME);
        let frame = format!(r#"{{"t":"ping","pad":"{pad}"}}"#);
        assert!(frame.len() > MAX_FRAME);
        assert_eq!(
            decode_frame(frame.as_bytes()),
            Err(FrameError::TooLarge { len: frame.len() })
        );
        // …and garbage of the same size is TooLarge too, never BadJson: the
        // parser is not consulted at all.
        let junk = vec![b'{'; MAX_FRAME + 1];
        assert_eq!(decode_frame(&junk).unwrap_err().code(), "frame_too_large");
    }

    #[test]
    fn a_frame_exactly_at_the_cap_is_accepted() {
        // Off-by-one guard: the limit is inclusive, so a phone that sizes to it
        // exactly is not mysteriously rejected.
        let mut frame = String::from(r#"{"t":"ping","pad":""#);
        let tail = r#""}"#;
        let pad = MAX_FRAME - frame.len() - tail.len();
        frame.push_str(&"a".repeat(pad));
        frame.push_str(tail);
        assert_eq!(frame.len(), MAX_FRAME);
        assert_eq!(decode_frame(frame.as_bytes()), Ok(PhoneMsg::Ping));
    }

    // ---- SessionInfo → <S> ----

    #[test]
    fn every_phase_variant_maps() {
        let cases = [
            (Phase::Spawning, "spawning"),
            (Phase::Idle, "idle"),
            (Phase::Busy, "busy"),
            (Phase::AwaitingDecision, "awaiting"),
            (Phase::Dead, "dead"),
        ];
        for (phase, expect) in cases {
            let mut i = bare_info();
            i.phase = phase;
            let s = WireSession::from(&i);
            assert_eq!(serde_json::to_value(s.phase).unwrap(), json!(expect));
        }
    }

    #[test]
    fn every_cli_kind_maps() {
        for (kind, expect) in
            [(CliKind::Claude, "claude"), (CliKind::Codex, "codex"), (CliKind::Shell, "shell")]
        {
            let mut i = bare_info();
            i.kind = kind;
            let s = WireSession::from(&i);
            assert_eq!(serde_json::to_value(s.cli).unwrap(), json!(expect));
        }
    }

    #[test]
    fn can_input_is_false_for_a_shell_and_true_for_the_agents() {
        // The courtesy flag, pinned against the rule the DAEMON enforces: a shell
        // is arbitrary command execution, so typing into one from a phone is
        // remote code execution as the user. This value only greys the composer
        // out early — but if it ever said `true` for a shell, every phone would
        // offer a box for exactly the input that is about to be refused.
        for (kind, expect) in
            [(CliKind::Claude, true), (CliKind::Codex, true), (CliKind::Shell, false)]
        {
            let mut i = bare_info();
            i.kind = kind;
            assert_eq!(WireSession::from(&i).can_input, expect, "wrong can_input for {kind:?}");
        }
    }

    #[test]
    fn every_trouble_kind_maps_to_a_slug() {
        for (kind, expect) in [
            (TroubleKind::RateLimit, "rate_limit"),
            (TroubleKind::ApiError, "api_error"),
            (TroubleKind::Overloaded, "overloaded"),
        ] {
            let mut i = bare_info();
            i.trouble = Some(Trouble { kind, since_ms: 5 });
            assert_eq!(WireSession::from(&i).trouble.as_deref(), Some(expect));
        }
    }

    #[test]
    fn the_null_cases_are_null_not_empty_strings() {
        let s = WireSession::from(&bare_info());
        assert_eq!(s.pending_headline, None);
        assert_eq!(s.trouble, None);
        assert_eq!(s.limit_percent, None);
        assert_eq!(s.limit_reset, None);
        assert!(!s.limit_hit);
        // last_message is the one field that is "" rather than null — a session
        // that has not spoken yet has an empty message, not a missing one.
        assert_eq!(s.last_message, "");
    }

    #[test]
    fn a_pending_decision_becomes_its_headline_only() {
        let mut i = bare_info();
        i.phase = Phase::AwaitingDecision;
        i.pending = Some(PendingDecision {
            tool_use_id: Some("tu_1".into()),
            tool_name: "Bash".into(),
            view: DecisionView::Bash {
                command: "rm -rf build".into(),
                description: "clean".into(),
            },
        });
        let s = WireSession::from(&i);
        assert_eq!(s.pending_headline.as_deref(), Some("rm -rf build"));
        // The BODY must not be on the wire: the phone cannot answer a decision
        // in v0, so shipping the diff would only invite a screenshot-approval.
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("tool_use_id"), "decision body leaked onto the wire");
        assert!(!json.contains("\"description\""), "decision body leaked onto the wire");
    }

    fn limit(hit: bool, percent: Option<u8>, clock: &str, date: &str) -> UsageLimit {
        UsageLimit {
            hit,
            percent,
            reset_clock: clock.into(),
            reset_tz: "America/Los_Angeles".into(),
            reset_date: date.into(),
            reset_at_unix: Some(1_700_003_600),
            since_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn a_usage_limit_maps_to_the_three_limit_fields() {
        let mut i = bare_info();
        i.usage_limit = Some(limit(true, None, "4:30pm", ""));
        let s = WireSession::from(&i);
        assert!(s.limit_hit);
        assert_eq!(s.limit_percent, None);
        assert_eq!(s.limit_reset.as_deref(), Some("4:30pm"));

        i.usage_limit = Some(limit(false, Some(92), "7am", "Jun 5"));
        let s = WireSession::from(&i);
        assert!(!s.limit_hit, "a warning is not a hit");
        assert_eq!(s.limit_percent, Some(92));
        assert_eq!(s.limit_reset.as_deref(), Some("Jun 5, 7am"));
    }

    #[test]
    fn a_limit_with_no_parseable_clock_sends_a_null_reset() {
        let mut i = bare_info();
        i.usage_limit = Some(limit(true, None, "", ""));
        let s = WireSession::from(&i);
        assert!(s.limit_hit, "the block is still real without a clock");
        assert_eq!(s.limit_reset, None, "an empty label would render as a blank chip");
    }

    #[test]
    fn a_dead_session_still_reports_its_identity() {
        // A dead session is not a gone session: it stays on the wire (alive
        // false, phase dead) until the daemon actually closes it, because
        // "crashed" is the single most useful thing a phone can tell you.
        let mut i = bare_info();
        i.phase = Phase::Dead;
        i.alive = false;
        i.last_message = "build failed".into();
        let s = WireSession::from(&i);
        assert_eq!(s.phase, WirePhase::Dead);
        assert!(!s.alive);
        assert_eq!(s.last_message, "build failed");
        assert_eq!(s.project, "kod");
    }
}
