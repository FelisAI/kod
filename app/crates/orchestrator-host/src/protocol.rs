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
pub const WIRE_VERSION: u32 = 22; // …18: UsageLimit.reset_date + reset_at_unix; 19: Command::SetAutoContinue; 20: Command::Answer removed; 21: legacy agent CLI removed; 22: SetAutoContinue.fire_on_reset

/// Reject absurd frame lengths (a corrupt/foreign peer) before allocating.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

// ---- client → daemon ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello { wire_version: u32 },
    Request { request_id: u64, command: Command },
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
        const PROTOCOL_HASH: u64 = 0xc08f1ea2cebf8361; // WIRE_VERSION 22
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
            matches!(got_hello, ClientMsg::Hello { wire_version } if wire_version == WIRE_VERSION)
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
