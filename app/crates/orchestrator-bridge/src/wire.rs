//! The PHONE-facing protocol (v0). Plain JSON, one message per WebSocket text
//! frame, deliberately nothing like the daemon's bincode wire.
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
//! v0 IS READ-ONLY. `caps.input` is `false` and there is no input message. Do not
//! add one here without adding the authorization story that would have to come
//! with it.

use serde::{Deserialize, Serialize};

use orchestrator_host::host::SessionInfo;
use orchestrator_host::session::{CliKind, Phase, TroubleKind};

/// The only protocol version v0 speaks. A phone announcing anything else is
/// refused at hello rather than half-understood.
pub const PROTO: u32 = 1;

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
    /// Any `t` this build does not know. This variant is what makes rule 2
    /// above *structural*: an unrecognized type is a value we can answer, not a
    /// parse failure that would tempt the loop into dropping the socket.
    #[serde(other)]
    Unknown,
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

/// What the bridge announces it can do. v0 is read-only, so `input` is `false`
/// — it is a field rather than an omission so the phone can grey out its
/// composer on the ANSWER instead of on its own build number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caps {
    pub input: bool,
}

impl Caps {
    /// The only caps v0 ever sends.
    pub fn v0() -> Self {
        Self { input: false }
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
    Err {
        code: String,
        message: String,
    },
}

impl BridgeMsg {
    pub fn err(code: &str, message: impl Into<String>) -> Self {
        Self::Err { code: code.to_string(), message: message.into() }
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
            json!({"t":"hello_ok","proto":1,"epoch":"e1","server_time":1234,"caps":{"input":false}})
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
    fn the_wire_types_are_exactly_the_contract_and_none_of_them_is_input() {
        // The EXHAUSTIVE match is the guard: adding a bridge→phone message type
        // stops this test compiling, so v0's read-only promise cannot be broken
        // by quietly growing the enum. It also pins every serde tag at once — a
        // `rename_all` surprise on any variant lands here rather than on a phone.
        fn tag(m: &BridgeMsg) -> &'static str {
            match m {
                BridgeMsg::HelloOk { .. } => "hello_ok",
                BridgeMsg::HelloErr { .. } => "hello_err",
                BridgeMsg::Sessions { .. } => "sessions",
                BridgeMsg::Session { .. } => "session",
                BridgeMsg::Gone { .. } => "gone",
                BridgeMsg::Pong => "pong",
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
            ["err", "gone", "hello_err", "hello_ok", "pong", "session", "sessions"]
        );
        assert!(!Caps::v0().input, "v0 advertises no input channel…");
    }

    #[test]
    fn the_phone_can_say_exactly_two_things() {
        // …and there is no inbound type for it to use if it tried. Same
        // exhaustive-match guard, pointed the other way down the wire.
        fn tag(m: &PhoneMsg) -> &'static str {
            match m {
                PhoneMsg::Hello { .. } => "hello",
                PhoneMsg::Ping => "ping",
                PhoneMsg::Unknown => "<unknown>",
            }
        }
        assert_eq!(tag(&decode_frame(br#"{"t":"hello","proto":1,"token":"k"}"#).unwrap()), "hello");
        assert_eq!(tag(&decode_frame(br#"{"t":"ping"}"#).unwrap()), "ping");
        // Anything a future phone invents — including an input message — lands
        // in `Unknown`, which the server answers with `err`. It is never a verb.
        assert_eq!(tag(&decode_frame(br#"{"t":"input","text":"hi"}"#).unwrap()), "<unknown>");
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
        assert_eq!(decode_frame(br#"{"t":"input","text":"hi"}"#), Ok(PhoneMsg::Unknown));
        assert_eq!(decode_frame(br#"{"t":"whatever"}"#), Ok(PhoneMsg::Unknown));
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
