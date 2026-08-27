//! The daemon wire protocol (docs/018 §4). One stable Unix socket, `u32`-LE
//! length-prefixed bincode frames both directions. The GUI client and the
//! daemon server both depend on these types — defined ONCE here so they can
//! never drift (the host crate is the shared dependency).
//!
//! Frozen wire types: any shape change to a type that crosses the socket must
//! bump [`WIRE_VERSION`] AND update `PROTOCOL_HASH` — the `protocol_hash_is_stable`
//! test fails loudly otherwise (a serde-bounds check alone can't catch wire
//! incompatibility — docs/018 §4, codex review).

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::emulator::{GridSnapshot, SearchMatch};
use crate::events::SessionEvent;
use crate::host::SessionInfo;
use crate::input::KeyInput;
use crate::pty::SpawnSpec;
use crate::session::{CliKind, SessionId};

/// Bumped by hand whenever any wire type below changes shape. The client sends
/// it in `Hello`; the daemon rejects a mismatch so a freshly-rebuilt GUI never
/// talks to an incompatible older daemon (docs/018 §13).
pub const WIRE_VERSION: u32 = 25; // …18: UsageLimit.reset_date + reset_at_unix; 19: Command::SetAutoContinue; 20: Command::Answer removed; 21: legacy agent CLI removed; 22: SetAutoContinue.fire_on_reset; 23: Command::SetBridge/BridgeStatus + CommandReply::Bridge; 24: ClientMsg::Hello.role + Command::PhoneInput/PhoneKey; 25: BridgeStatus.fingerprint (TLS)

/// Reject absurd frame lengths (a corrupt/foreign peer) before allocating.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

// ---- client → daemon ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello {
        wire_version: u32,
        /// What this connection is allowed to ask for. Declared ONCE, at attach,
        /// and never re-negotiated — a connection cannot talk its way up.
        role: ClientRole,
    },
    Request { request_id: u64, command: Command },
}

/// The capability a connection holds for its whole life.
///
/// This exists because the mobile bridge is a SEPARATE PROCESS that a phone talks
/// to over a network. Putting the "phones may not type into shells" rule inside
/// the bridge would make it a convention: anything that compromised the bridge —
/// or any future refactor of it — could send `SendKey` to a shell and get
/// arbitrary command execution as the user. Declaring the role at attach moves
/// the rule to the far side of a process boundary, where the daemon enforces it
/// against its OWN view of what each session is.
///
/// The phone never sends this. The bridge sets it, and even if the bridge lied it
/// could only ever claim FEWER rights than `Full` — the daemon decides what each
/// role may do, and a `Phone` connection is refused everything but typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientRole {
    /// The desktop app. Every command.
    Full,
    /// The mobile bridge: typing into agent sessions, and nothing else.
    Phone,
}

impl ClientRole {
    /// Whether a connection holding this role may issue `cmd`.
    ///
    /// A allowlist, not a denylist: a command added later is refused for phones
    /// until someone deliberately lists it. The opposite default would mean every
    /// new capability silently reaches the network.
    pub fn may(self, cmd: &Command) -> bool {
        match self {
            ClientRole::Full => true,
            ClientRole::Phone => matches!(
                cmd,
                Command::PhoneInput { .. }
                    | Command::PhoneKey { .. }
                    | Command::PhoneClients { .. }
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    SpawnClaude {
        project_slug: String,
        spec: SpawnSpec,
    },
    ResumeClaude {
        project_slug: String,
        session_id: String,
        spec: SpawnSpec,
    },
    ResumeCodex {
        project_slug: String,
        session_id: String,
        spec: SpawnSpec,
    },
    Spawn {
        project_slug: String,
        kind: CliKind,
        spec: SpawnSpec,
    },
    SpawnShell {
        project_slug: String,
        cwd: std::path::PathBuf,
    },
    SendKey {
        id: SessionId,
        key: KeyInput,
    },
    /// scroll the viewport through scrollback (#9) — fire-and-forget.
    Scroll {
        id: SessionId,
        delta: i32,
    },
    /// PURE view scroll (search jump) — never becomes PTY input (⌘F).
    ScrollView {
        id: SessionId,
        delta: i32,
    },
    /// search the session grid incl. scrollback (⌘F) — request/reply.
    Search {
        id: SessionId,
        query: String,
    },
    /// move a session to another project (#10) — fire-and-forget.
    Rebind {
        id: SessionId,
        project_slug: String,
    },
    Resize {
        id: SessionId,
        rows: u16,
        cols: u16,
    },
    Close {
        id: SessionId,
    },
    /// associate a fresh codex's discovered rollout id (docs/018 §12) — fire-and-
    /// forget; the daemon updates the session so reconcile recognizes it live.
    SetCliId {
        id: SessionId,
        cli_session_id: String,
    },
    /// backfill a session's timeline from its on-disk transcript (#9 §4) — the
    /// GUI resolves the path (it has core); the daemon parses + pushes (the
    /// per-CLI parser + claude cutoff are chosen host-side by the session kind).
    BackfillTranscript {
        id: SessionId,
        path: String,
    },
    Quit,
    /// push the global auto-continue-on-limit-reset flag to the daemon (which is
    /// storage-free) — fire-and-forget. Cached in a `SessionHost` AtomicBool; the
    /// GUI re-pushes on every attach so a fresh/retired-respawned daemon learns
    /// the user's choice. Trailing variant (positional wire): a safe add.
    SetAutoContinue {
        on: bool,
        /// FIRE auto-continue on the resolved reset INSTANT (config, default OFF).
        /// The default gate waits for the banner to physically clear, which an idle
        /// blocked session never produces — so without this the feature only ever
        /// GivesUp. Trailing field (positional wire): a conscious add.
        fire_on_reset: bool,
    },
    /// Configure the mobile bridge the DAEMON hosts (docs/020 mobile) — REQUEST/
    /// REPLY, answered with [`CommandReply::Bridge`]. `on: false` stops it but
    /// still records the config, so the reply can say Off rather than
    /// "never configured". `bind` and `token` stay strings on the wire: the
    /// daemon owns parsing/validating them and reports a bad one back as
    /// [`BridgeStatus::error`] instead of the GUI guessing. The daemon is
    /// storage-free, so the GUI re-pushes this on every attach. Trailing variant
    /// (positional wire): a safe add.
    SetBridge {
        on: bool,
        port: u16,
        bind: String,
        token: String,
    },
    /// Poll the daemon-hosted bridge (the settings pane's live line) — request/
    /// reply. Trailing variant (positional wire): a safe add.
    BridgeStatus,
    /// Type `text` into an AGENT session, from a phone.
    ///
    /// Deliberately NOT `SendKey`: a separate command is what lets
    /// `ClientRole::may` allowlist typing without also handing a network peer the
    /// key path the desktop uses for everything else. The daemon resolves the
    /// session's `CliKind` ITSELF and refuses a shell — the phone sends only an
    /// id, so it cannot misrepresent what it is typing into.
    PhoneInput { id: SessionId, text: String },
    /// One control key into an AGENT session, from a phone. Enough to answer a
    /// permission prompt (arrow to a choice, Enter) without giving the phone a
    /// general keyboard.
    PhoneKey { id: SessionId, key: PhoneKey },
    /// The bridge telling the daemon how many phones are connected.
    ///
    /// Moving the bridge out of this process cost the daemon its direct view of
    /// the connection table, and the settings line would otherwise have to guess.
    /// Allowed for `Phone` because it carries no capability at all — it is a
    /// number the daemon displays, and the worst a lying bridge achieves is a
    /// wrong count on its owner's own screen.
    PhoneClients { n: u32 },
}

/// The only keys a phone may press. An explicit, tiny set rather than the
/// desktop's `KeyInput`: everything here is navigating a prompt an agent is
/// already showing, and nothing here can start work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PhoneKey {
    Enter,
    Escape,
    Up,
    Down,
    Tab,
}

/// The daemon-hosted mobile bridge's live state — the whole answer to "how is it
/// doing", so the GUI never has to infer it from a bare ack.
///
/// Plain serde data (lib.rs `public_api_is_plain_data`): it crosses the socket
/// inside [`CommandReply::Bridge`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BridgeStatus {
    pub running: bool,
    /// every URL a phone can actually reach it on (one per bound address).
    pub endpoints: Vec<String>,
    /// phones currently connected.
    pub clients: u32,
    /// why it is NOT running (bind refused, bad token, …). `None` while healthy.
    pub error: Option<String>,
    /// whether this daemon has EVER been told the bridge config. Distinguishes
    /// "off because the user turned it off" (`configured`) from "this daemon has
    /// not been told yet" (a fresh or respawned daemon before the GUI attaches
    /// and re-pushes) — the UI must render those differently (Off vs waiting for
    /// Kod), and the daemon is storage-free so the second case is REAL, not
    /// hypothetical.
    pub configured: bool,
    /// wall-clock ms when the current running/stopped state was entered (uptime
    /// chip). 0 = never.
    pub since_ms: u64,
    /// base64url SHA-256 of the server's public key (SPKI), when serving over
    /// TLS. This is the phone's ONLY notion of who it is talking to: no CA is
    /// involved, the certificate is self-signed, and the pairing QR carries this
    /// value out of band. `None` means plaintext, which the bind policy only
    /// permits on loopback.
    #[serde(default)]
    pub fingerprint: Option<String>,
}

/// What the UI should actually SAY, derived from the three fields that can
/// otherwise be combined into a sentence that lies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BridgePhase {
    /// Serving. `endpoints` names where.
    Running,
    /// Tried and failed, or cannot host one at all. Show the reason verbatim.
    Failed,
    /// This daemon has never been told the config — a fresh or respawned daemon
    /// before the GUI attaches. NOT the same as off, and must not read as off.
    Waiting,
    /// Configured, no error, deliberately not running.
    Off,
}

impl BridgeStatus {
    /// "Nothing to report, and here is why" — the answer from a backend that
    /// cannot host a bridge at all, or when the daemon never answered. Leaves
    /// `configured` false: an unavailable bridge is exactly the case the UI must
    /// NOT render as a user-chosen Off.
    pub fn unavailable(reason: &str) -> Self {
        Self {
            error: Some(reason.to_string()),
            ..Self::default()
        }
    }

    /// The precedence rule, in ONE place.
    ///
    /// It lives here rather than in the view because there are three independent
    /// booleans and only four legal readings, and the wrong combination is not a
    /// cosmetic bug: rendering a daemon that failed to bind as "waiting for Kod"
    /// tells the user to do nothing when the truth is that their phone will never
    /// connect. `error` outranks `configured` for exactly that reason.
    pub fn phase(&self) -> BridgePhase {
        if self.running {
            BridgePhase::Running
        } else if self.error.is_some() {
            BridgePhase::Failed
        } else if !self.configured {
            BridgePhase::Waiting
        } else {
            BridgePhase::Off
        }
    }
}

#[cfg(test)]
mod bridge_status_tests {
    use super::{BridgePhase, BridgeStatus};

    #[test]
    fn a_failure_outranks_never_having_been_configured() {
        // The regression this exists for: `unavailable` leaves `configured` false,
        // so a naive `if !configured { Waiting }` renders a daemon that could not
        // bind — or one that is gone — as "waiting for Kod", i.e. as something the
        // user should simply wait out. They would wait forever.
        let s = BridgeStatus::unavailable("lost the daemon");
        assert!(!s.configured);
        assert_eq!(s.phase(), BridgePhase::Failed);
    }

    #[test]
    fn a_configured_bridge_that_failed_to_bind_reads_as_failed_not_off() {
        let s = BridgeStatus {
            configured: true,
            error: Some("port 8787 is already in use".into()),
            ..Default::default()
        };
        assert_eq!(s.phase(), BridgePhase::Failed);
    }

    #[test]
    fn the_three_quiet_states_stay_distinguishable() {
        let fresh = BridgeStatus::default();
        assert_eq!(fresh.phase(), BridgePhase::Waiting, "a daemon nobody told yet");

        let off = BridgeStatus { configured: true, ..Default::default() };
        assert_eq!(off.phase(), BridgePhase::Off, "the user turned it off");

        let on = BridgeStatus {
            configured: true,
            running: true,
            endpoints: vec!["127.0.0.1:8787".into()],
            ..Default::default()
        };
        assert_eq!(on.phase(), BridgePhase::Running);
    }

    #[test]
    fn running_wins_over_a_stale_error() {
        // A restart that succeeded after a failure must not keep showing the old
        // failure — the status line would contradict the phone that is connected.
        let s = BridgeStatus {
            configured: true,
            running: true,
            error: Some("stale".into()),
            ..Default::default()
        };
        assert_eq!(s.phase(), BridgePhase::Running);
    }
}

// ---- daemon → client ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMsg {
    /// version OK — initial session list follows as a snapshot replay.
    Welcome {
        wire_version: u32,
        infos: Vec<SessionInfo>,
    },
    /// version mismatch — the client decides (restart daemon vs run in-process).
    VersionMismatch { daemon_version: u32 },
    /// the attach snapshot-replay is complete; live events follow (docs/018 §7).
    ReplayDone,
    /// an unsolicited live event (grid/phase/decision/title/closed).
    Event(ServerEvent),
    /// the reply to a `Request`, correlated by `request_id`.
    Reply {
        request_id: u64,
        reply: CommandReply,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerEvent {
    /// monotonic per-client ordering (semantic events are reliable; the writer
    /// never drops them — docs/018 §5).
    pub seq: u64,
    pub session_id: SessionId,
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventKind {
    /// full session info — emitted for a NEW session (so a client that spawned
    /// after attach sees it) AND on any info change (phase / pending / title /
    /// alive). Upserted into the client cache. Collapses the old per-field
    /// events and fixes the spawn-after-attach blind spot (review #3/#8/#11).
    Info(SessionInfo),
    /// a full coalesced grid (latest-wins; never one-per-chunk — docs/018 §6).
    Grid(GridSnapshot),
    /// new timeline events since the client's last cursor — the DELTA, appended
    /// to the client cache (the Sessions curated stream, #9). Keyed off each
    /// event's monotonic per-session `seq`, never an index.
    Events(Vec<SessionEvent>),
    /// the session ended (reaped / closed).
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CommandReply {
    /// a spawn/resume result. `cli_session_id` is `Some` for a fresh claude
    /// (the minted handle the GUI records); the GUI ignores it for resume.
    Spawned {
        id: SessionId,
        cli_session_id: Option<String>,
    },
    /// answer / close result.
    Bool(bool),
    /// fire-and-forget ack (send_key / resize / quit).
    Ok,
    /// the command failed (e.g. CLI not on PATH) — carries the error text.
    Error(String),
    /// search results (⌘F). `total` counts ALL hits — when it exceeds
    /// `matches.len()` the cap truncated and the bar shows "n/300+".
    Matches {
        matches: Vec<SearchMatch>,
        total: u32,
    },
    /// the bridge's state — the reply to BOTH `SetBridge` and `BridgeStatus`, so
    /// a config click and a poll take the same rendering path. Trailing variant
    /// (positional wire): a safe add.
    Bridge(BridgeStatus),
}

// ---- framing: u32-LE length-prefixed bincode ----

/// Write one length-prefixed bincode frame and flush.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let bytes = bincode::serialize(msg).map_err(io::Error::other)?;
    if bytes.len() > MAX_FRAME {
        return Err(io::Error::other("frame exceeds MAX_FRAME"));
    }
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(&bytes)?;
    w.flush()
}

/// Read one length-prefixed bincode frame. Blocks; returns `UnexpectedEof` when
/// the peer closes the connection cleanly between frames.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::other("frame exceeds MAX_FRAME"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    bincode::deserialize(&buf).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::{DecisionView, PendingDecision};
    use crate::session::Phase;

    fn sample_grid() -> GridSnapshot {
        // a populated StyleRun (incl. uri) so the hash actually covers the run shape.
        let run = crate::emulator::StyleRun {
            text: "x".into(),
            fg: 1,
            bg: 2,
            bold: true,
            italic: false,
            underline: false,
            uri: Some("https://x".into()),
        };
        GridSnapshot {
            seq: 7,
            rows: vec![vec![run]],
            cursor: (0, 0),
            cursor_visible: true,
            bracketed_paste: false,
            kitty_keys: false,
            display_offset: 3,
            history_size: 100,
            alt_screen: true,
        }
    }

    /// Every `DecisionView` variant — so a field/shape change to any of them
    /// changes the protocol hash and forces a WIRE_VERSION bump (review #1).
    fn all_views() -> Vec<DecisionView> {
        vec![
            DecisionView::Edit {
                path: "/a/b.rs".into(),
                old: "x".into(),
                new: "y".into(),
            },
            DecisionView::Write {
                path: "/a/c.rs".into(),
                content: "z".into(),
            },
            DecisionView::Bash {
                command: "ls".into(),
                description: "list".into(),
            },
            DecisionView::Question {
                header: "h".into(),
                prompt: "p?".into(),
                options: vec!["a".into(), "b".into()],
            },
            DecisionView::Other {
                tool: "T".into(),
                summary: "s".into(),
            },
        ]
    }

    fn info_with(view: Option<DecisionView>) -> SessionInfo {
        SessionInfo {
            id: SessionId(3),
            kind: CliKind::Codex,
            project_slug: "p".into(),
            title: "t".into(),
            phase: Phase::Idle,
            alive: true,
            pending: view.map(|v| PendingDecision {
                tool_use_id: Some("tu".into()),
                tool_name: "Edit".into(),
                view: v,
            }),
            dirty: 5,
            cli_session_id: Some("abc".into()),
            last_message: "done".into(),
            phase_since_ms: 7000,
            // trouble/usage_limit stay None here; sample_info() carries the
            // Some arm so BOTH shapes of each Option are hashed.
            trouble: None,
            usage_limit: None,
        }
    }

    fn sample_info() -> SessionInfo {
        let mut i = info_with(Some(DecisionView::Bash {
            command: "x".into(),
            description: String::new(),
        }));
        i.trouble = Some(crate::session::Trouble {
            kind: crate::session::TroubleKind::RateLimit,
            since_ms: 9000,
        });
        i.usage_limit = Some(crate::session::UsageLimit {
            hit: true,
            percent: Some(92),
            reset_clock: "4:30pm".into(),
            reset_date: String::new(),
            reset_tz: "America/Los_Angeles".into(),
            reset_at_unix: Some(1_700_000_000),
            since_ms: 9000,
        });
        i
    }

    /// One of EVERY wire shape — the protocol-hash sample. Must instantiate each
    /// enum variant + payload type so an uncovered shape change can't slip past
    /// the frozen-wire guard (review #1).
    fn all_events() -> Vec<SessionEvent> {
        use crate::events::{SessionEventKind as K, ToolVerb as V};
        let mut out = vec![
            SessionEvent {
                seq: 1,
                at_ms: 1000,
                kind: K::Started,
            },
            SessionEvent {
                seq: 2,
                at_ms: 1001,
                kind: K::Awaiting {
                    tool: "Edit".into(),
                },
            },
            SessionEvent {
                seq: 3,
                at_ms: 1002,
                kind: K::TurnEnd {
                    summary: "did it".into(),
                },
            },
            SessionEvent {
                seq: 4,
                at_ms: 1003,
                kind: K::Notice {
                    text: "waiting".into(),
                },
            },
        ];
        for (i, verb) in [V::Edited, V::Created, V::Ran, V::Read, V::Used]
            .into_iter()
            .enumerate()
        {
            out.push(SessionEvent {
                seq: 5 + i as u64,
                at_ms: 1004 + i as u64,
                kind: K::Tool {
                    verb,
                    target: "t".into(),
                },
            });
        }
        out
    }

    fn protocol_corpus() -> Vec<u8> {
        let mut msgs: Vec<ServerMsg> = vec![
            ServerMsg::Welcome {
                wire_version: WIRE_VERSION,
                infos: vec![sample_info()],
            },
            ServerMsg::VersionMismatch { daemon_version: 99 },
            ServerMsg::ReplayDone,
            ServerMsg::Event(ServerEvent {
                seq: 1,
                session_id: SessionId(3),
                kind: EventKind::Grid(sample_grid()),
            }),
            ServerMsg::Event(ServerEvent {
                seq: 2,
                session_id: SessionId(3),
                kind: EventKind::Closed,
            }),
            ServerMsg::Reply {
                request_id: 1,
                reply: CommandReply::Spawned {
                    id: SessionId(4),
                    cli_session_id: Some("u".into()),
                },
            },
            ServerMsg::Reply {
                request_id: 90,
                reply: CommandReply::Matches {
                    matches: vec![],
                    total: 0,
                },
            },
            ServerMsg::Reply {
                request_id: 91,
                reply: CommandReply::Matches {
                    matches: vec![SearchMatch {
                        lines_above_bottom: 12,
                        start: 3,
                        end: 9,
                    }],
                    total: 400,
                },
            },
            ServerMsg::Reply {
                request_id: 2,
                reply: CommandReply::Bool(true),
            },
            ServerMsg::Reply {
                request_id: 3,
                reply: CommandReply::Ok,
            },
            ServerMsg::Reply {
                request_id: 4,
                reply: CommandReply::Error("boom".into()),
            },
            ServerMsg::Reply {
                request_id: 5,
                reply: CommandReply::Bridge(BridgeStatus {
                    running: true,
                    endpoints: vec!["ws://192.168.1.4:8765".into()],
                    clients: 2,
                    error: None,
                    configured: true,
                    since_ms: 12_000,
            fingerprint: None,
                }),
            },
            // the failed arm too, so BOTH shapes of `error: Option<String>` are
            // hashed (same reason sample_info() carries the Some arms).
            ServerMsg::Reply {
                request_id: 6,
                reply: CommandReply::Bridge(BridgeStatus::unavailable("bind refused")),
            },
        ];
        // an Info event per DecisionView variant (and a None) — covers
        // SessionInfo/PendingDecision/DecisionView entirely.
        msgs.push(ServerMsg::Event(ServerEvent {
            seq: 10,
            session_id: SessionId(3),
            kind: EventKind::Info(info_with(None)),
        }));
        for (i, v) in all_views().into_iter().enumerate() {
            msgs.push(ServerMsg::Event(ServerEvent {
                seq: 11 + i as u64,
                session_id: SessionId(3),
                kind: EventKind::Info(info_with(Some(v))),
            }));
        }
        // an Events delta covering EVERY SessionEventKind + ToolVerb, so a shape
        // change to the timeline wire can't slip past the frozen-wire guard (#9).
        msgs.push(ServerMsg::Event(ServerEvent {
            seq: 30,
            session_id: SessionId(3),
            kind: EventKind::Events(all_events()),
        }));

        let mut spec = SpawnSpec::program("cat", "/tmp");
        // non-empty so the hash covers the dispatch-delivery field's payload.
        spec.initial_prompt = "do the thing".into();
        let cmds: Vec<ClientMsg> = vec![
            ClientMsg::Hello {
                wire_version: WIRE_VERSION,
                role: ClientRole::Full,
            },
            ClientMsg::Request {
                request_id: 1,
                command: Command::SpawnClaude {
                    project_slug: "p".into(),
                    spec: spec.clone(),
                },
            },
            ClientMsg::Request {
                request_id: 2,
                command: Command::ResumeClaude {
                    project_slug: "p".into(),
                    session_id: "s".into(),
                    spec: spec.clone(),
                },
            },
            ClientMsg::Request {
                request_id: 3,
                command: Command::ResumeCodex {
                    project_slug: "p".into(),
                    session_id: "s".into(),
                    spec: spec.clone(),
                },
            },
            ClientMsg::Request {
                request_id: 4,
                command: Command::Spawn {
                    project_slug: "p".into(),
                    kind: CliKind::Shell,
                    spec: spec.clone(),
                },
            },
            ClientMsg::Request {
                request_id: 5,
                command: Command::SpawnShell {
                    project_slug: "p".into(),
                    cwd: "/tmp".into(),
                },
            },
            ClientMsg::Request {
                request_id: 6,
                command: Command::SendKey {
                    id: SessionId(1),
                    key: KeyInput::Enter,
                },
            },
            ClientMsg::Request {
                request_id: 7,
                command: Command::SendKey {
                    id: SessionId(1),
                    key: KeyInput::Char("hi".into()),
                },
            },
            ClientMsg::Request {
                request_id: 11,
                command: Command::Resize {
                    id: SessionId(1),
                    rows: 30,
                    cols: 110,
                },
            },
            ClientMsg::Request {
                request_id: 110,
                command: Command::Scroll {
                    id: SessionId(1),
                    delta: -3,
                },
            },
            ClientMsg::Request {
                request_id: 12,
                command: Command::Close { id: SessionId(1) },
            },
            ClientMsg::Request {
                request_id: 13,
                command: Command::SetCliId {
                    id: SessionId(1),
                    cli_session_id: "rollout-x".into(),
                },
            },
            ClientMsg::Request {
                request_id: 14,
                command: Command::BackfillTranscript {
                    id: SessionId(1),
                    path: "/x/rollout.jsonl".into(),
                },
            },
            ClientMsg::Request {
                request_id: 15,
                command: Command::Quit,
            },
            ClientMsg::Request {
                request_id: 16,
                command: Command::ScrollView {
                    id: SessionId(1),
                    delta: -40,
                },
            },
            ClientMsg::Request {
                request_id: 17,
                command: Command::Search {
                    id: SessionId(1),
                    query: "needle".into(),
                },
            },
            ClientMsg::Request {
                request_id: 18,
                command: Command::Rebind {
                    id: SessionId(1),
                    project_slug: "p2".into(),
                },
            },
            ClientMsg::Request {
                request_id: 19,
                command: Command::SetAutoContinue { on: true, fire_on_reset: true },
            },
            ClientMsg::Request {
                request_id: 20,
                command: Command::SetBridge {
                    on: true,
                    port: 8765,
                    bind: "lan".into(),
                    token: "s3cret".into(),
                },
            },
            ClientMsg::Request {
                request_id: 21,
                command: Command::BridgeStatus,
            },
            // A NEW COMMAND MUST BE ADDED HERE. This corpus is hand-maintained,
            // so the guard only sees what it is given: adding a variant and not
            // listing it leaves the hash unchanged and the "wire protocol
            // changed" alarm silent. Found exactly that way — PhoneInput and
            // PhoneKey initially slipped through, and the hash only moved because
            // Hello gained a field in the same edit.
            ClientMsg::Request {
                request_id: 22,
                command: Command::PhoneInput {
                    id: SessionId(1),
                    text: "hello".into(),
                },
            },
            ClientMsg::Request {
                request_id: 23,
                command: Command::PhoneKey {
                    id: SessionId(1),
                    key: PhoneKey::Enter,
                },
            },
            ClientMsg::Request {
                request_id: 24,
                command: Command::PhoneClients { n: 2 },
            },
        ];
        let mut bytes = bincode::serialize(&msgs).unwrap();
        bytes.extend(bincode::serialize(&cmds).unwrap());
        bytes
    }

    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    /// Frozen-wire guard: a shape change to any wire type changes this hash, so
    /// the change must be a CONSCIOUS act paired with a WIRE_VERSION bump.
    #[test]
    fn protocol_hash_is_stable() {
        const PROTOCOL_HASH: u64 = 0xdf8263516e4e6d5f; // WIRE_VERSION 25
        let got = fnv1a(&protocol_corpus());
        assert_eq!(
            got, PROTOCOL_HASH,
            "wire protocol changed — if intentional, bump WIRE_VERSION and set PROTOCOL_HASH = {got:#018x}"
        );
    }

    #[test]
    fn frames_round_trip_both_directions() {
        let mut buf: Vec<u8> = Vec::new();
        let hello = ClientMsg::Hello {
            wire_version: WIRE_VERSION,
            role: ClientRole::Phone,
        };
        let welcome = ServerMsg::Welcome {
            wire_version: WIRE_VERSION,
            infos: vec![sample_info()],
        };
        write_frame(&mut buf, &hello).unwrap();
        write_frame(&mut buf, &welcome).unwrap();

        let mut r = &buf[..];
        let got_hello: ClientMsg = read_frame(&mut r).unwrap();
        let got_welcome: ServerMsg = read_frame(&mut r).unwrap();
        assert!(
            matches!(got_hello, ClientMsg::Hello { wire_version, role }
                     if wire_version == WIRE_VERSION && role == ClientRole::Phone),
            "the role must survive the socket — it is the whole access-control decision"
        );
        match got_welcome {
            ServerMsg::Welcome { infos, .. } => {
                assert_eq!(infos.len(), 1);
                assert_eq!(infos[0].cli_session_id.as_deref(), Some("abc"));
            }
            _ => panic!("wrong msg"),
        }
        // clean EOF between frames surfaces as UnexpectedEof, not a panic.
        assert_eq!(
            read_frame::<_, ServerMsg>(&mut r).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    fn variant_tag<T: Serialize>(v: &T) -> u32 {
        let bytes = bincode::serialize(v).unwrap();
        u32::from_le_bytes(bytes[..4].try_into().unwrap())
    }

    /// The enum tag on the wire is the variant's POSITION, so inserting a variant
    /// anywhere but the end silently re-points every later one: an older GUI's
    /// `Quit` would arrive at the daemon as `SetAutoContinue`. `protocol_hash_is_stable`
    /// notices such an edit but reports it as "the wire changed"; this pins the
    /// indices so the failure names the actual damage.
    #[test]
    fn trailing_additions_did_not_renumber_existing_variants() {
        assert_eq!(variant_tag(&Command::Quit), 14, "Quit moved");
        assert_eq!(
            variant_tag(&Command::SetAutoContinue {
                on: true,
                fire_on_reset: false
            }),
            15,
            "SetAutoContinue moved"
        );
        // the wire-23 adds, at the END where they cannot disturb the above.
        assert_eq!(
            variant_tag(&Command::SetBridge {
                on: true,
                port: 1,
                bind: String::new(),
                token: String::new()
            }),
            16
        );
        assert_eq!(variant_tag(&Command::BridgeStatus), 17);
        assert_eq!(variant_tag(&CommandReply::Ok), 2, "CommandReply::Ok moved");
        assert_eq!(
            variant_tag(&CommandReply::Bridge(BridgeStatus::default())),
            5
        );
    }

    /// `configured` is the field the settings pane branches on, and it must
    /// survive the socket independently of `running`: stopped-but-configured is
    /// "Off" (the user's choice), stopped-and-unconfigured is "waiting for Kod"
    /// (a respawned daemon nobody has pushed config to yet). Collapse the two and
    /// the pane lies about a bridge that is merely un-pushed.
    #[test]
    fn bridge_status_round_trips_with_configured_independent_of_running() {
        let off_by_choice = BridgeStatus {
            running: false,
            endpoints: vec![],
            clients: 0,
            error: None,
            configured: true,
            since_ms: 4242,
            fingerprint: None,
        };
        let mut buf: Vec<u8> = Vec::new();
        write_frame(
            &mut buf,
            &ServerMsg::Reply {
                request_id: 8,
                reply: CommandReply::Bridge(off_by_choice),
            },
        )
        .unwrap();
        write_frame(
            &mut buf,
            &ServerMsg::Reply {
                request_id: 9,
                reply: CommandReply::Bridge(BridgeStatus {
                    running: true,
                    endpoints: vec!["ws://10.0.0.2:8765".into(), "ws://127.0.0.1:8765".into()],
                    clients: 3,
                    error: None,
                    configured: true,
                    since_ms: 1,
            fingerprint: None,
                }),
            },
        )
        .unwrap();

        let mut r = &buf[..];
        let a: ServerMsg = read_frame(&mut r).unwrap();
        let b: ServerMsg = read_frame(&mut r).unwrap();
        match (a, b) {
            (
                ServerMsg::Reply {
                    reply: CommandReply::Bridge(off),
                    ..
                },
                ServerMsg::Reply {
                    reply: CommandReply::Bridge(up),
                    ..
                },
            ) => {
                assert!(!off.running && off.configured, "Off must stay configured");
                assert_eq!(off.since_ms, 4242);
                assert!(up.running);
                assert_eq!(up.clients, 3);
                // every endpoint, in order — the phone needs the LAN one, and the
                // pane shows them all.
                assert_eq!(
                    up.endpoints,
                    vec!["ws://10.0.0.2:8765", "ws://127.0.0.1:8765"]
                );
            }
            _ => panic!("wrong msg"),
        }
    }

    /// `unavailable` is the "we cannot know" answer, NOT a user-chosen Off: it
    /// must leave `configured` false so the pane renders "waiting for Kod" and
    /// carry the reason, so a bind failure is readable instead of a silent stop.
    #[test]
    fn unavailable_is_never_mistaken_for_a_configured_off() {
        let s = BridgeStatus::unavailable("bind refused: port 8765 in use");
        assert!(!s.running);
        assert!(!s.configured);
        assert_eq!(s.error.as_deref(), Some("bind refused: port 8765 in use"));
        assert!(s.endpoints.is_empty());
        assert_eq!(s.clients, 0);
        assert_eq!(s.since_ms, 0);
    }

    #[test]
    fn grid_event_round_trips() {
        let mut buf: Vec<u8> = Vec::new();
        let ev = ServerMsg::Event(ServerEvent {
            seq: 9,
            session_id: SessionId(2),
            kind: EventKind::Grid(sample_grid()),
        });
        write_frame(&mut buf, &ev).unwrap();
        let got: ServerMsg = read_frame(&mut &buf[..]).unwrap();
        match got {
            ServerMsg::Event(ServerEvent {
                seq,
                kind: EventKind::Grid(g),
                ..
            }) => {
                assert_eq!(seq, 9);
                assert_eq!(g.seq, 7);
            }
            _ => panic!("wrong msg"),
        }
    }
}

#[cfg(test)]
mod role_tests {
    use super::*;

    fn sid() -> SessionId {
        SessionId(1)
    }

    #[test]
    fn a_phone_may_type_and_may_do_nothing_else() {
        let phone = ClientRole::Phone;
        assert!(phone.may(&Command::PhoneInput { id: sid(), text: "hi".into() }));
        assert!(phone.may(&Command::PhoneKey { id: sid(), key: PhoneKey::Enter }));

        // The ones that matter. SendKey is arbitrary keystrokes into ANY session
        // including a shell — that is remote command execution, and it is exactly
        // what a compromised bridge would reach for.
        assert!(!phone.may(&Command::SendKey {
            id: sid(),
            key: KeyInput::Paste("rm -rf /".into()),
        }));
        assert!(!phone.may(&Command::SpawnShell {
            project_slug: "p".into(),
            cwd: std::path::PathBuf::from("/tmp"),
        }));
        assert!(!phone.may(&Command::SetBridge {
            on: false,
            port: 1,
            bind: String::new(),
            token: String::new(),
        }));
    }

    #[test]
    fn the_desktop_keeps_every_capability() {
        let full = ClientRole::Full;
        assert!(full.may(&Command::SendKey { id: sid(), key: KeyInput::Paste("x".into()) }));
        assert!(full.may(&Command::SpawnShell {
            project_slug: "p".into(),
            cwd: std::path::PathBuf::from("/tmp"),
        }));
        assert!(full.may(&Command::PhoneInput { id: sid(), text: "x".into() }));
    }

    /// The allowlist must stay an ALLOWLIST. If someone adds a command and this
    /// test still passes without them touching `may`, the new capability is
    /// already refused for phones — which is the safe direction. This test exists
    /// to state that intent so nobody "fixes" it into a denylist.
    #[test]
    fn an_unlisted_command_is_refused_rather_than_allowed() {
        assert!(!ClientRole::Phone.may(&Command::BridgeStatus));
        assert!(!ClientRole::Phone.may(&Command::Scroll { id: sid(), delta: 1 }));
    }
}
