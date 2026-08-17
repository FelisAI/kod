//! HostedSession (docs/013 §1 session.rs) — one type for every CLI kind.
//!
//! Binds the three I/O planes around one project-bound process:
//!   - PTY bytes in/out (pty.rs)
//!   - the emulator (grid state + query replies)        [render + WHEN]
//!   - the OSC tap (approval/progress/title signals)    [WHEN]
//! Semantic events from hooks / app-server attach later at the host layer.
//!
//! Lock discipline (docs/013 §1): the reader thread takes `inner` to advance
//! the emulator and feed the OSC tap, writing query replies straight back to
//! the PTY; the GUI takes `inner` only for a cheap snapshot read. The PTY
//! writer has its own lock inside `PtyProcess`, never held with `inner`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::decision::{self, PendingDecision};
use crate::emulator::{EmuEvent, Emulator, GridSnapshot};
use crate::events::{SessionEvent, SessionEventKind};
use crate::hooks::HookEvent;
use crate::input::{self, KeyInput};
use crate::osc::{OscSignal, OscTap};
use crate::pty::{PtyProcess, SpawnSpec};

// The usage-limit banner parse + reset clock live in their own module now
// (usage_limit.rs), but the whole workspace — and the daemon wire, via
// `protocol.rs` — has always known these two by their `session::` path. The
// split is an internal file boundary, NOT an API change, so re-export them here
// rather than churn every call site (and the wire) for a pure move.
pub use crate::usage_limit::{parse_usage_limit, UsageLimit};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CliKind {
    Claude,
    Codex,
    Shell,
}

impl CliKind {
    pub fn label(self) -> &'static str {
        match self {
            CliKind::Claude => "claude",
            CliKind::Codex => "codex",
            CliKind::Shell => "shell",
        }
    }
}

/// Coarse session phase, derived from live signals (docs/013 §1 state model).
/// `AwaitingDecision` is set by the WHEN layer (OSC approval / waiting); the
/// full typed PendingDecision lands with the M3 hook/app-server wiring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Spawning,
    Idle,
    Busy,
    AwaitingDecision,
    Dead,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Spawning => "spawning",
            Phase::Idle => "idle",
            Phase::Busy => "busy",
            Phase::AwaitingDecision => "needs you",
            Phase::Dead => "dead",
        }
    }
}

struct Inner {
    emu: Emulator,
    osc: OscTap,
    /// Last title the TUI set (claude's activity channel) — for the strip.
    title: String,
    /// Latched WHEN-state from OSC: an approval/waiting signal is outstanding.
    awaiting: bool,
    /// The outstanding approval was raised by codex's OSC-0 "Action Required"
    /// TITLE (its only approval signal in interactive TUI mode — it emits no
    /// OSC 9 there). Records provenance so (a) the ~2Hz `[ ! ]`↔`[ . ]` blink
    /// only raises ONCE per block and (b) only a resolving title clears the
    /// codex latch — a claude/hook-backed `pending` is never touched by the
    /// title path.
    awaiting_from_title: bool,
    /// progress busy/done from OSC 9;4 (claude) — drives the busy spinner.
    busy: bool,
    /// the real decision the agent is blocked on (M3, from the hook) — the
    /// authoritative AwaitingDecision signal, replacing the OSC heuristic.
    pending: Option<PendingDecision>,
    /// last assistant message from the Stop hook (feed/DID, M4).
    last_message: String,
    /// the curated event timeline (#9 rich) — a bounded ring; deltas key off
    /// `event_seq`, never an index, so eviction can't corrupt the cursor.
    events: std::collections::VecDeque<SessionEvent>,
    event_seq: u64,
    /// current phase + when it was ENTERED — stamped at MUTATION sites (hooks,
    /// OSC edges, keys, output) plus a periodic sweep for the 2s output-decay
    /// edge, NOT lazily at observation (a detached daemon observes nothing, so
    /// ages would be attach-relative fiction — design critique, #4).
    phase_now: Phase,
    phase_since_ms: u64,
    /// the phase before `phase_now` + its stamp — a bounce back within
    /// PHASE_HYSTERESIS_MS restores the original stamp, so a repaint blip that
    /// flips Idle→Busy→Idle can't zero a real "idle 40m" age.
    phase_prev: Phase,
    phase_prev_since_ms: u64,
    /// chip-grade trouble (rate limit / API error anchors in the live tail) —
    /// Busy-gated, self-healing (clears when the anchor leaves the tail or the
    /// turn ends). Tier-grade detectors wait on measurement (task #15).
    trouble: Option<Trouble>,
    /// claude's usage-limit FOOTER lifted off the grid bottom (docs/019) — NOT
    /// Busy-gated and NOT latching (mirrors the live banner: Some while present,
    /// None the moment it clears — the natural reset edge). Carries the reset
    /// clock+zone auto-continue (slice 2) will wake on.
    usage_limit: Option<UsageLimit>,
    /// last hysteresis restore — restores are rate-limited (≥30s apart) so
    /// sparse-output work (a line every ~3s) can't CHAIN restores and keep an
    /// hours-old Idle stamp alive on an active session (review: proven lie).
    last_restore_ms: u64,
    /// scan_trouble won't LATCH before this — a Stop/turn-complete clear would
    /// otherwise be undone on the next tick while the old banner is still in
    /// the tail (review: the re-latch flash).
    trouble_muted_until_ms: u64,
    // ── auto-continue-on-limit-reset (docs/019 slice 2, hardened by the
    // adversarial safety review) — ALL IN-MEMORY. The daemon is storage-free;
    // walk-away keeps the daemon (and these) alive, and a daemon retire kills the
    // sessions anyway, so there is no store column. ──
    /// The composer DRAFT — the line the user is currently typing, rebuilt from
    /// key events (Char/Paste append, Backspace pop, Enter/Ctrl-C/Esc clear). Its
    /// ONLY consumer is the composer-EMPTY safety gate: a non-empty draft BLOCKS a
    /// fire so auto-continue never types over half-typed input. It need not be
    /// perfectly faithful — erring toward "non-empty" only ever blocks a fire (the
    /// safe direction). It is NO LONGER the source of the replayed prompt.
    input_buffer: String,
    /// unix-ms of the last NON-BLANK composer submit (Enter). 0 = never. Drives
    /// the manual-resume disarm (a submit AFTER we armed means the user is
    /// driving again, so back off) and the first-turn fallback gate. Not text.
    last_submit_ms: u64,
    /// The GROUND-TRUTH prompt to replay at reset — the LAST user message read
    /// from the CLI's OWN session transcript when we ARM (never a keystroke
    /// reconstruction: the review proved that replays stale/wrong/truncated text)
    /// + the unix-ms it was resolved. `None` until an arm edge resolves it. This
    /// is the ONLY text auto-continue may ever replay.
    ac_prompt: Option<(String, u64)>,
    /// auto-continue arm state (see `ac_decide`). Armed on a fresh limit-HIT
    /// edge; fired only once the banner CLEARS at the reset. All gated behind
    /// the global `SessionHost::auto_continue()` flag.
    ac_armed: bool,
    /// the reset instant (unix seconds) we armed against — the wake target.
    ac_reset_at: Option<i64>,
    /// wall-clock ms we armed — a user prompt SUBMITTED after this means they're
    /// driving again (manual-resume disarm, so we never type over them).
    ac_armed_at_ms: u64,
    /// fired-once latch: stays true after a replay until a fresh not-hit
    /// observation clears it, so one unbroken block is auto-continued AT MOST
    /// once (a later NEW hit re-arms).
    ac_fired: bool,
}

/// A bounce back to the previous phase within this window restores its stamp.
const PHASE_HYSTERESIS_MS: u64 = 5000;

/// Structured trouble on the wire: the GUI renders + ticks the age locally
/// (timestamps over prose — design critique, #4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TroubleKind {
    RateLimit,
    ApiError,
    Overloaded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Trouble {
    pub kind: TroubleKind,
    pub since_ms: u64,
}

/// Cap the per-session event ring (older events fall off the front).
const EVENT_CAP: usize = 256;

impl Inner {
    /// Append a timeline event at a GIVEN time (used for historical backfill from
    /// transcripts, #9 slice 4), stamping the monotonic seq + evicting on overflow.
    fn push_event_at(&mut self, at_ms: u64, kind: SessionEventKind) {
        self.event_seq += 1;
        self.events.push_back(SessionEvent {
            seq: self.event_seq,
            at_ms,
            kind,
        });
        if self.events.len() > EVENT_CAP {
            self.events.pop_front();
        }
    }

    /// Append a live event, stamped now.
    fn push_event(&mut self, kind: SessionEventKind) {
        self.push_event_at(crate::events::now_ms(), kind);
    }

    /// Record a phase transition (mutation-stamped, #4). A bounce back to the
    /// previous phase within the hysteresis window restores its original stamp
    /// — repaint/echo blips can't zero a real age. Trouble is Busy-gated, so
    /// leaving Busy also drops the chip.
    fn note_phase(&mut self, new: Phase, now: u64) {
        if new == self.phase_now {
            return;
        }
        // Restore guards (review, both proven by execution): (1) NEVER restore
        // into AwaitingDecision — a fresh ask must get its own age, not the
        // previous ask's 40 minutes; (2) rate-limit restores so repeated
        // Busy↔Idle cycling (sparse output) can't chain an ancient stamp
        // forward forever — one blip is a glance, a blip every 3s is activity.
        if new == self.phase_prev
            && new != Phase::AwaitingDecision
            && now.saturating_sub(self.phase_since_ms) < PHASE_HYSTERESIS_MS
            && now.saturating_sub(self.last_restore_ms) >= 30_000
        {
            std::mem::swap(&mut self.phase_now, &mut self.phase_prev);
            std::mem::swap(&mut self.phase_since_ms, &mut self.phase_prev_since_ms);
            self.last_restore_ms = now;
        } else {
            self.phase_prev = self.phase_now;
            self.phase_prev_since_ms = self.phase_since_ms;
            self.phase_now = new;
            self.phase_since_ms = now;
        }
        if self.phase_now != Phase::Busy {
            self.trouble = None;
        }
    }
}

/// Compute the session's phase from its flags — callable where `self` isn't
/// (the PTY reader callback). `recently` = output within the working window.
fn compute_phase(g: &Inner, alive: bool, recently: bool) -> Phase {
    if !alive {
        Phase::Dead
    } else if g.pending.is_some() || g.awaiting {
        Phase::AwaitingDecision
    } else if g.busy || recently {
        Phase::Busy
    } else {
        Phase::Idle
    }
}

/// What the auto-continue gate decided for one session this tick. The pure
/// `ac_decide` returns one of these; `HostedSession::auto_continue_step`
/// executes it. `Fire` is the ONLY variant that types into a live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcDecision {
    /// wait — nothing to do this tick.
    Skip,
    /// arm on a fresh limit-hit edge; wake at `reset_at` (unix seconds).
    Arm { reset_at: i64 },
    /// the window reset AND the banner cleared — replay the held prompt now.
    Fire,
    /// drop the arm (flag turned off, or the user resumed manually).
    Disarm,
    /// the banner never cleared long past the reset — stop trying.
    GiveUp,
}

/// The outcome of resolving a session's replay prompt from its CLI transcript
/// (docs/019 rework). `NoUserMessage` (transcript readable, no genuine user turn)
/// and `Unreadable` (no path / read error) are DISTINCT so the first-turn
/// `initial_prompt` fallback applies ONLY to `Unreadable`.
enum TranscriptPrompt {
    Found(String),
    NoUserMessage,
    Unreadable,
}

/// A snapshot of ONE session's real state, the sole input to the auto-continue
/// gate. Kept as a plain struct so `ac_decide` is PURE and every branch is
/// unit-tested WITHOUT a live session (RULE ZERO: no subprocess in tests).
#[derive(Debug, Clone, Copy)]
pub struct AcInputs {
    /// the global auto-continue flag (`SessionHost::auto_continue()`).
    pub on: bool,
    /// config (default OFF): FIRE on the resolved reset INSTANT arriving, not on
    /// the banner physically clearing. The default `cleared` edge is unreachable
    /// for an idle blocked session (no output to overwrite its footer), so without
    /// this auto-continue only ever GivesUp (audit B2 / task #31).
    pub fire_on_reset: bool,
    /// the usage-limit banner is a HARD hit (not a benign warning).
    pub hit: bool,
    /// the resolved reset instant (unix seconds), when the banner carried one.
    pub reset_at: Option<i64>,
    /// the banner is now GONE (`usage_limit.is_none()`) — the genuine reset
    /// edge we FIRE on (distinct from merely reaching the estimated clock).
    pub cleared: bool,
    /// the CLI is actively working (OSC progress or an outstanding decision).
    pub busy: bool,
    /// the session produced output within the working window (~2s).
    pub recently_working: bool,
    /// a permission/question dialog is on the visible grid.
    pub has_dialog: bool,
    /// the composer draft is EMPTY (`input_buffer` blank) — the #1/#6 safety gate:
    /// a non-empty composer BLOCKS a fire so auto-continue never types over a
    /// half-typed line. Erring toward "non-empty" only ever blocks (the safe way).
    pub composer_empty: bool,
    /// unix-ms of the last non-blank composer submit (the manual-resume check).
    pub last_submit_ms: Option<u64>,
    /// there is a resolved `ac_prompt` (from the transcript) to replay at all.
    pub has_held_prompt: bool,
    /// this session is currently armed.
    pub armed: bool,
    /// the reset instant (unix seconds) it armed against.
    pub armed_reset_at: i64,
    /// wall-clock ms it armed.
    pub armed_at_ms: u64,
    /// the fired-once latch is set (already auto-continued this block).
    pub fired: bool,
}

/// GiveUp horizon: 6h in ms past the armed reset instant. `pub(crate)` only so
/// `usage_limit`'s undated age-out can pin itself to the SAME horizon (a block
/// we've given up waking on is a block we no longer believe).
pub(crate) const AC_GIVEUP_MS: i64 = 6 * 3600 * 1000;

/// THE auto-continue gate (docs/019 slice 2) — PURE and exhaustively tested.
/// This is the safety boundary: every reason NOT to type into a live session
/// lives here, so the actuation is a trivial executor. Rules apply IN ORDER.
///
/// The key asymmetry: we ARM on `hit == true` (the block appeared), but we FIRE
/// only when `cleared == true` (the banner is GONE) — the reset genuinely
/// happened, not merely the estimated clock.
pub fn ac_decide(i: &AcInputs, now_ms: u64) -> AcDecision {
    // 1. Flag off ⇒ never act; drop any arm we were holding.
    if !i.on {
        return if i.armed { AcDecision::Disarm } else { AcDecision::Skip };
    }
    // 2. ARM on a fresh hit edge: not already armed, not still in a fired latch,
    //    a real hard hit, a captured prompt to replay, and a known reset instant.
    if !i.armed && !i.fired && i.hit && i.has_held_prompt {
        if let Some(reset_at) = i.reset_at {
            return AcDecision::Arm { reset_at };
        }
    }
    // 3. Not armed and no arm-edge above ⇒ nothing to do.
    if !i.armed {
        return AcDecision::Skip;
    }
    // 4. Manual-resume disarm: the user SUBMITTED a new prompt after we armed ⇒
    //    they're driving now; back off so we never type over them.
    if let Some(ts) = i.last_submit_ms {
        if ts > i.armed_at_ms {
            return AcDecision::Disarm;
        }
    }
    let reset_ms = i.armed_reset_at.saturating_mul(1000);
    let now = now_ms as i64;
    // 5. GiveUp: long past the reset and the banner STILL hasn't cleared (bad
    //    reset estimate / stuck session) — stop, don't spin forever.
    if now > reset_ms + AC_GIVEUP_MS {
        return AcDecision::GiveUp;
    }
    // 6. FIRE: the reset instant has arrived AND either the banner actually
    //    CLEARED or the `fire_on_reset` config is on (default OFF — the cleared
    //    edge is unreachable for an idle blocked session, so the clock is the only
    //    signal that ever fires); the session is quiet (not busy, not mid-output,
    //    no open dialog); the composer is EMPTY (#1/#6 — never type over a half line).
    if now >= reset_ms
        && (i.cleared || i.fire_on_reset)
        && !i.busy
        && !i.recently_working
        && !i.has_dialog
        && i.composer_empty
    {
        return AcDecision::Fire;
    }
    // 7. Armed but the reset instant / clear / quiet conditions aren't all met.
    AcDecision::Skip
}

/// Fold one key into the composer-draft tracking (docs/019 slice 2, hardened by
/// the safety review). PURE `Inner` mutation, unit-tested without a subprocess
/// (RULE ZERO). Maintains ONLY `input_buffer` (the composer-empty gate) and
/// `last_submit_ms` (the manual-resume disarm) — it NO LONGER captures the
/// replayed prompt: that now comes from the CLI transcript at arm time. It need
/// not model claude's own line editing; erring toward a non-empty buffer or a
/// spurious submit only ever BLOCKS/DISARMS a fire (the safe direction).
fn capture_key(g: &mut Inner, key: &KeyInput) {
    match key {
        // text-bearing keys extend the composed draft.
        KeyInput::Char(s) | KeyInput::Paste(s) => g.input_buffer.push_str(s),
        KeyInput::Backspace => {
            g.input_buffer.pop();
        }
        // Ctrl-C / Esc abandon the composed line — interrupted, not submitted.
        KeyInput::Ctrl(c) if c.eq_ignore_ascii_case(&'c') => g.input_buffer.clear(),
        KeyInput::Escape => g.input_buffer.clear(),
        // Enter SUBMITS: a non-blank draft records the submit TIME (manual-resume
        // disarm) — NO grid/dialog check (a stray dialog-answer only ever disarms,
        // the safe direction). Then reset the draft for the next line.
        KeyInput::Enter => {
            if !g.input_buffer.trim().is_empty() {
                g.last_submit_ms = now_ms();
            }
            g.input_buffer.clear();
        }
        _ => {}
    }
}

/// Apply an `AcDecision` to a session's `Inner` arm-state (docs/019 slice 2) —
/// everything EXCEPT the pty-touching Fire keystrokes, which the caller (which
/// owns the pty + liveness) actuates. Returns `Some(held_text)` when the caller
/// must replay a Fire into a LIVE session, `None` otherwise. `hit` is the current
/// banner-hit bit: a genuine not-hit observation clears the fired latch so a
/// LATER new hit edge can re-arm. PURE w.r.t. the pty, so the arm-state
/// transitions + the fired latch + the Notices are unit-tested on a bare `Inner`
/// (RULE ZERO: no subprocess in tests).
fn ac_apply(g: &mut Inner, decision: AcDecision, now: u64, hit: bool, alive: bool) -> Option<String> {
    match decision {
        AcDecision::Skip => {}
        AcDecision::Arm { reset_at } => {
            g.ac_armed = true;
            g.ac_reset_at = Some(reset_at);
            g.ac_armed_at_ms = now;
            g.ac_fired = false;
        }
        AcDecision::Disarm => {
            g.ac_armed = false;
            g.ac_reset_at = None;
            g.ac_prompt = None; // a fresh block re-reads the transcript ground truth
            // ac_fired left as-is; the not-hit clear below re-enables arming.
        }
        AcDecision::GiveUp => {
            let reset_at = g.ac_reset_at.unwrap_or(0);
            // LATCH: GiveUp is TERMINAL for this block. Without the latch it merely
            // clears the arm while the banner (and its dead reset) is still on the
            // grid — which re-opens the arm edge in `auto_continue_step` AND rule 2 of
            // `ac_decide` (which PRECEDES the GiveUp rule) on the very next 1s tick:
            // Arm → GiveUp → Arm → GiveUp forever, re-reading the whole CLI transcript
            // and pushing a Notice every ~2s until the event ring has evicted the
            // entire session timeline. The trailing `if !hit` below still RELEASES the
            // latch on a genuine not-hit observation, so a LATER new block re-arms.
            g.ac_fired = true;
            g.ac_armed = false;
            g.ac_reset_at = None;
            g.ac_prompt = None;
            g.push_event(SessionEventKind::Notice {
                text: format!("auto-continue: limit banner never cleared by reset({reset_at})+6h, gave up"),
            });
        }
        AcDecision::Fire => {
            let text = g.ac_prompt.as_ref().map(|(t, _)| t.clone()).unwrap_or_default();
            if !alive {
                // ALIVE-ONLY: a session that DIED while blocked can't be resumed by
                // typing — disarm and tell the user to resume it manually.
                g.ac_armed = false;
                g.ac_reset_at = None;
                g.ac_prompt = None;
                g.push_event(SessionEventKind::Notice {
                    text: "auto-continue: session died while blocked, resume it manually".into(),
                });
            } else {
                // Latch + disarm BEFORE the caller's pty write so a concurrent tick
                // can't double-fire; return the exact ground-truth text to replay.
                g.ac_fired = true;
                g.ac_armed = false;
                g.ac_reset_at = None;
                g.ac_prompt = None;
                // #8: record a BOUNDED PREFIX of what was injected (audit trail),
                // not merely a char count — the timeline shows WHAT was replayed.
                g.push_event(SessionEventKind::Notice {
                    text: format!("auto-continued at reset — replayed: {}", prompt_prefix(&text, 80)),
                });
                return Some(text);
            }
        }
    }
    // The fired latch clears on a genuine not-hit observation (banner gone / was
    // only a warning) — NEVER on the Fire-alive tick, which returned Some above
    // with the latch just set. `ac_prompt` is left untouched here: it is cleared
    // ONLY on Fire/Disarm/GiveUp, so a still-armed session waiting past a slightly
    // early banner-clear keeps its resolved prompt until the reset instant.
    if !hit {
        g.ac_fired = false;
    }
    None
}

/// A bounded, single-line PREFIX of a replayed prompt for the audit Notice (#8):
/// the first `max` chars (ellipsized if longer), newlines flattened — so the
/// timeline records WHAT was injected, not merely a char count.
fn prompt_prefix(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = flat.trim();
    let head: String = trimmed.chars().take(max).collect();
    if trimmed.chars().count() > max {
        format!("{head}…")
    } else {
        head
    }
}

/// A live, project-bound terminal session. Cheap to snapshot; the GUI holds an
/// `Arc<HostedSession>` and never touches alacritty/pty types.
pub struct HostedSession {
    pub id: SessionId,
    pub kind: CliKind,
    project_slug: Mutex<String>,
    pub cwd: std::path::PathBuf,
    /// The dispatch prompt this session was spawned with (claude's final argv
    /// positional), retained so auto-continue can fall back to it ONLY when the
    /// transcript is unreadable AND the session is still on its first turn (no
    /// user submit yet). "" for shells / composer-launched sessions.
    initial_prompt: String,
    /// The CLI's own resume handle (claude --session-id / --resume id, codex
    /// rollout id) when known — None for a shell or a not-yet-discovered fresh
    /// codex. Surfaced in `SessionInfo` for crash-recovery reconcile. Interior-
    /// mutable so a fresh codex's id can be set once discovered (docs/018 §12).
    cli_session_id: Mutex<Option<String>>,
    inner: Arc<Mutex<Inner>>,
    pty: PtyProcess,
    /// Bumped on every output advance — the GUI compares it to coalesce
    /// repaints without locking the grid.
    dirty: Arc<AtomicU64>,
    /// `dirty` at the last `reconcile_pending` grid-walk — lets it skip the full
    /// snapshot when no PTY bytes arrived since (the dialog can't have moved if
    /// the grid didn't change). Per the perf audit: while a decision card is open
    /// this otherwise rebuilds the grid every 16ms tick (daemon + GUI).
    last_reconcile_dirty: AtomicU64,
    /// wall-clock ms of the last PTY output — the OUTPUT-ACTIVITY signal behind
    /// the "working" phase, so a session actively producing output reads as Busy
    /// even when the CLI doesn't emit OSC 9;4 progress (claude fullscreen / codex).
    last_output_ms: Arc<AtomicU64>,
    /// wall-clock ms when the current pending decision was raised (hook). The
    /// hook races the PTY paint: PermissionRequest often lands BEFORE the dialog
    /// is drawn, so reconcile_pending's "dialog left the grid" check must not
    /// clear a decision younger than the grace window (review #12).
    pending_set_ms: Arc<AtomicU64>,
    /// `dirty` at the last trouble scan — its own gate; sharing
    /// `last_reconcile_dirty` would have the two scans eating each other's
    /// edges (both swap; design critique #4).
    last_trouble_dirty: AtomicU64,
    /// wall-clock of the last tail walk — the scan's 500ms time floor.
    last_trouble_scan_ms: AtomicU64,
    /// `dirty` + wall-clock at the last usage-limit scan — its OWN gate (a
    /// shared gate would eat the trouble scan's edges). 1s floor: the footer is
    /// static, so a coarse cadence loses nothing (docs/019).
    last_limit_dirty: AtomicU64,
    last_limit_scan_ms: AtomicU64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl HostedSession {
    pub fn spawn(
        id: SessionId,
        kind: CliKind,
        project_slug: String,
        cli_session_id: Option<String>,
        spec: SpawnSpec,
    ) -> std::io::Result<Arc<HostedSession>> {
        let t0 = now_ms();
        let inner = Arc::new(Mutex::new(Inner {
            emu: Emulator::new(spec.rows, spec.cols),
            osc: OscTap::new(),
            title: String::new(),
            awaiting: false,
            awaiting_from_title: false,
            busy: false,
            pending: None,
            last_message: String::new(),
            events: std::collections::VecDeque::new(),
            event_seq: 0,
            phase_now: Phase::Idle,
            phase_since_ms: t0,
            phase_prev: Phase::Idle,
            phase_prev_since_ms: t0,
            trouble: None,
            usage_limit: None,
            last_restore_ms: 0,
            trouble_muted_until_ms: 0,
            input_buffer: String::new(),
            last_submit_ms: 0,
            ac_prompt: None,
            ac_armed: false,
            ac_reset_at: None,
            ac_armed_at_ms: 0,
            ac_fired: false,
        }));
        inner.lock().unwrap().push_event(SessionEventKind::Started);
        let dirty = Arc::new(AtomicU64::new(0));
        let last_output = Arc::new(AtomicU64::new(0));
        let pending_set_ms = Arc::new(AtomicU64::new(0));
        let cwd = spec.cwd.clone();
        let initial_prompt = spec.initial_prompt.clone();

        let inner_r = inner.clone();
        let dirty_r = dirty.clone();
        let last_output_r = last_output.clone();
        let pending_set_ms_r = pending_set_ms.clone();
        // The reader callback is the only path from PTY bytes to the emulator.
        // It returns terminal-query replies (CPR/DA1/color), which pty.rs
        // writes straight back on the reader thread — zero latency, no pump
        // thread, no construction-order cycle.
        let pty = PtyProcess::spawn(&spec, move |bytes| {
            let mut replies: Vec<u8> = Vec::new();
            let mut g = inner_r.lock().unwrap();
            // OSC tap runs on the raw stream first (the fork drops 9/9;4/777).
            let sigs = g.osc.scan(bytes);
            for s in sigs {
                // `kind` is Copy — the move-closure copies it in, leaving the
                // outer binding for the HostedSession ctor below.
                if apply_osc(&mut g, kind, s) {
                    pending_set_ms_r.store(now_ms(), Ordering::Relaxed);
                }
            }
            // then the emulator; collect query replies + titles.
            for e in g.emu.advance(bytes) {
                match e {
                    EmuEvent::PtyReply(b) => replies.extend_from_slice(&b),
                    EmuEvent::Title(t) => g.title = t,
                    EmuEvent::ResetTitle => g.title.clear(),
                    EmuEvent::Bell => {}
                }
            }
            // output is arriving, so the session is alive + working by
            // definition — stamp any phase edge this output caused (OSC busy/
            // awaiting flips, or entering Busy from Idle). The decay edge back
            // to Idle is the sweep's job.
            let ph = compute_phase(&g, true, true);
            g.note_phase(ph, now_ms());
            drop(g);
            dirty_r.fetch_add(1, Ordering::SeqCst);
            last_output_r.store(now_ms(), Ordering::Relaxed);
            replies
        })?;

        Ok(Arc::new(HostedSession {
            id,
            kind,
            project_slug: Mutex::new(project_slug),
            cwd,
            initial_prompt,
            cli_session_id: Mutex::new(cli_session_id),
            inner,
            pty,
            dirty,
            last_reconcile_dirty: AtomicU64::new(0),
            last_output_ms: last_output,
            pending_set_ms,
            last_trouble_dirty: AtomicU64::new(0),
            last_trouble_scan_ms: AtomicU64::new(0),
            last_limit_dirty: AtomicU64::new(0),
            last_limit_scan_ms: AtomicU64::new(0),
        }))
    }

    /// Kill this session's process group now (explicit close) — reaped exactly
    /// once via the CAS in `PtyProcess` (docs/018 §10).
    pub fn terminate(&self) {
        self.pty.terminate();
    }

    /// The CLI resume handle, if known (the crash-recovery reconcile key).
    pub fn cli_session_id(&self) -> Option<String> {
        self.cli_session_id.lock().unwrap().clone()
    }

    /// Set the CLI resume handle once discovered (fresh codex — docs/018 §12).
    pub fn set_cli_session_id(&self, v: String) {
        *self.cli_session_id.lock().unwrap() = Some(v);
    }

    /// A render-ready snapshot.
    pub fn snapshot(&self) -> GridSnapshot {
        let g = self.inner.lock().unwrap();
        g.emu.snapshot()
    }

    /// Latest window title the TUI set (claude busy/idle activity).
    pub fn title(&self) -> String {
        self.inner.lock().unwrap().title.clone()
    }

    /// Producing output within the last ~2s — the output-activity "working"
    /// signal (a live session streaming a response / running a tool), robust to
    /// CLIs that don't emit OSC 9;4 (claude fullscreen, codex).
    fn recently_working(&self) -> bool {
        let last = self.last_output_ms.load(Ordering::Relaxed);
        last > 0 && now_ms().saturating_sub(last) < 2000
    }

    pub fn phase(&self) -> Phase {
        let g = self.inner.lock().unwrap();
        compute_phase(&g, self.pty.is_alive(), self.recently_working())
    }

    /// Observe the current phase and stamp a transition if one happened —
    /// covers the time-driven edge (the 2s output-decay Busy→Idle) that no
    /// mutation site sees. Called by the host sweep; cheap when nothing moved.
    pub fn observe_phase(&self) {
        let alive = self.pty.is_alive();
        let recently = self.recently_working();
        let mut g = self.inner.lock().unwrap();
        let ph = compute_phase(&g, alive, recently);
        g.note_phase(ph, now_ms());
    }

    /// When the current phase was entered (wall-clock ms) — the age primitive
    /// behind NEEDS-YOU/IDLE chips and oldest-first sorting (#4).
    pub fn phase_since_ms(&self) -> u64 {
        self.inner.lock().unwrap().phase_since_ms
    }

    /// The chip-grade trouble latch, if any.
    pub fn trouble(&self) -> Option<Trouble> {
        self.inner.lock().unwrap().trouble
    }

    /// Scan the LIVE tail (offset-0 bottom rows, immune to scroll position) for
    /// failure anchors — rate limit / API error / overloaded. Dirty-gated on
    /// its OWN counter (sharing reconcile's would eat its edges). Busy-gated:
    /// pending/awaiting outrank, idle text is history. Self-healing: the latch
    /// clears when the anchor scrolls off or the turn ends.
    pub fn scan_trouble(&self) {
        // time floor FIRST (before the dirty swap, so the edge isn't consumed):
        // while streaming, dirty changes every tick — without the floor the
        // tail walk would run at 60Hz per busy session (review).
        let now = now_ms();
        if now.saturating_sub(self.last_trouble_scan_ms.load(Ordering::Relaxed)) < 500 {
            return;
        }
        let dirty = self.dirty.load(Ordering::SeqCst);
        if self.last_trouble_dirty.swap(dirty, Ordering::SeqCst) == dirty {
            return;
        }
        self.last_trouble_scan_ms.store(now, Ordering::Relaxed);
        let alive = self.pty.is_alive();
        let recently = self.recently_working();
        let mut g = self.inner.lock().unwrap();
        if compute_phase(&g, alive, recently) != Phase::Busy {
            g.trouble = None;
            return;
        }
        // NO "usage limit" anchor: claude pins a benign "Approaching usage
        // limit · resets at Xpm" status line that would latch a permanent
        // false chip (review). Underscore variants catch the raw error ids
        // (rate_limit_error / api_error).
        let tail = g.emu.tail_plain(15).to_lowercase();
        let kind = if tail.contains("rate limit") || tail.contains("rate_limit") {
            Some(TroubleKind::RateLimit)
        } else if tail.contains("overloaded") {
            Some(TroubleKind::Overloaded)
        } else if tail.contains("api error") || tail.contains("api_error") {
            Some(TroubleKind::ApiError)
        } else {
            None
        };
        let muted = now < g.trouble_muted_until_ms;
        match (kind, g.trouble) {
            (Some(k), None) if !muted => {
                g.trouble = Some(Trouble {
                    kind: k,
                    since_ms: now,
                })
            }
            (Some(k), Some(t)) if t.kind != k => {
                g.trouble = Some(Trouble {
                    kind: k,
                    since_ms: now,
                })
            }
            (None, Some(_)) => g.trouble = None,
            _ => {}
        }
    }

    /// Scan the live GRID BOTTOM for claude's usage-limit footer and mirror it
    /// into `usage_limit`. Deliberately NOT the `scan_trouble` path: that one is
    /// Busy-gated, latching, and explicitly SKIPS this banner (it would pin a
    /// false red chip). This one is non-Busy-gated (the footer persists while
    /// idle) and non-latching (Some while present, None the moment it clears —
    /// the natural reset edge). The banner sits BELOW the composer/cursor, so
    /// `tail_plain` (cursor-anchored) would miss it — `bottom_plain` reads UP
    /// from the grid bottom, scroll-immune (docs/019).
    pub fn scan_limit(&self) {
        // CODEX (and every non-claude kind) owns its usage limit via the rollout
        // `rate_limits` telemetry (`set_usage_limit`, driven by the summaries
        // poll). This claude GRID scan must NEVER run on such a session or it
        // would parse the codex grid (no claude footer there → None) and clobber
        // the rollout value on every reconcile tick.
        if self.kind != CliKind::Claude {
            return;
        }
        let now = now_ms();
        if now.saturating_sub(self.last_limit_scan_ms.load(Ordering::Relaxed)) < 1000 {
            return;
        }
        let dirty = self.dirty.load(Ordering::SeqCst);
        if self.last_limit_dirty.swap(dirty, Ordering::SeqCst) == dirty {
            return;
        }
        self.last_limit_scan_ms.store(now, Ordering::Relaxed);
        let mut g = self.inner.lock().unwrap();
        let parsed = parse_usage_limit(&g.emu.bottom_plain(6), now);
        let old = g.usage_limit.take();
        g.usage_limit = parsed.map(|new| UsageLimit::carry_forward(new, old.as_ref()));
    }

    /// The usage-limit banner lifted off the live grid, if present (docs/019) —
    /// CLOCK-EXPIRED here via [`UsageLimit::is_expired`]. This accessor IS the view
    /// boundary: its only caller is `SessionHost::info_of`, i.e. everything it
    /// returns is `SessionInfo`. Expiry lives HERE, not in the stored value, because
    /// neither CLI ever retracts a limit — see `is_expired`.
    ///
    /// Auto-continue deliberately does NOT read through this accessor
    /// (`auto_continue_step` reads `g.usage_limit` RAW) — a clock-expired limit
    /// would forge its FIRE gate. Keep it that way.
    pub fn usage_limit(&self) -> Option<UsageLimit> {
        let now = now_ms();
        let g = self.inner.lock().unwrap();
        g.usage_limit.as_ref().filter(|u| !u.is_expired(now)).cloned()
    }

    /// Store a usage limit computed OFF-GRID — codex's rollout `rate_limits`
    /// telemetry, mapped via [`crate::transcript::CodexRateLimits::to_usage_limit`]
    /// — into the same `usage_limit` slot the claude footer scan fills, so it
    /// rides `SessionInfo` uniformly (docs/019 codex limit). Folds onto the stored
    /// banner via [`UsageLimit::carry_forward`] (as `scan_limit` does), so the chip
    /// age ticks from first observation rather than from each poll — an IDLE codex
    /// session re-derives the SAME stale `token_count` telemetry every ~10s forever,
    /// which is exactly why the view expires it. Bumps `dirty` only on a real change
    /// (a repaint per ~12s poll
    /// when nothing moved would be noise). Never clobbered by `scan_limit`, which
    /// is gated to claude.
    pub fn set_usage_limit(&self, ul: Option<UsageLimit>) {
        let mut g = self.inner.lock().unwrap();
        let old = g.usage_limit.take();
        let next = ul.map(|new| UsageLimit::carry_forward(new, old.as_ref()));
        let changed = next != old;
        g.usage_limit = next;
        drop(g);
        if changed {
            self.dirty.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// One auto-continue tick for THIS session (docs/019 slice 2). Builds the
    /// gate inputs from live state, runs the PURE `ac_decide`, and EXECUTES its
    /// decision. `on` is the global flag threaded down from
    /// `SessionHost::auto_continue_tick` (which already short-circuits when off).
    ///
    /// This is the ONLY code path that types into a session UNATTENDED — so every
    /// guard lives in `ac_decide`; here we merely execute what it returns. Returns
    /// the decision (for the daemon-sweep caller / tests).
    pub fn auto_continue_step(&self, on: bool, fire_on_reset: bool) -> AcDecision {
        // pty liveness + output-activity read OUTSIDE the lock (atomics).
        let alive = self.pty.is_alive();
        let recently = self.recently_working();

        // PHASE 1 (short lock): is this the fresh ARM edge? If so we must source the
        // GROUND-TRUTH prompt from the CLI transcript — a filesystem read we do
        // OUTSIDE the lock so a large transcript can't stall the reader thread. The
        // read runs AT MOST ONCE per limit block (arming OR the no-prompt latch both
        // close this edge). Poison-tolerant lock (#9): a session poisoned by an
        // earlier panic must still be swept, not permanently wedged.
        let arm_edge = {
            let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let ul = g.usage_limit.as_ref();
            let hit = ul.map(|u| u.hit).unwrap_or(false);
            let reset_known = ul.and_then(|u| u.reset_at_unix).is_some();
            on && hit && reset_known && !g.ac_armed && !g.ac_fired && g.ac_prompt.is_none()
        };
        let resolved = arm_edge.then(|| self.transcript_last_user_message());

        // PHASE 2 (main lock): apply the resolved prompt (arm edge only), run the
        // PURE gate, and actuate a Fire ATOMICALLY under THIS lock (#2 TOCTOU).
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_ms();
        if let Some(resolved) = resolved {
            // re-check the edge under the lock (state may have moved during the read).
            let hit_now = g.usage_limit.as_ref().map(|u| u.hit).unwrap_or(false);
            if hit_now && !g.ac_armed && !g.ac_fired && g.ac_prompt.is_none() {
                match resolved {
                    TranscriptPrompt::Found(p) => g.ac_prompt = Some((p, now)),
                    TranscriptPrompt::Unreadable
                        if g.last_submit_ms == 0 && !self.initial_prompt.is_empty() =>
                    {
                        // first-turn dispatch whose transcript isn't on disk yet —
                        // the ONLY sanctioned fallback (docs/019 rework).
                        g.ac_prompt = Some((self.initial_prompt.clone(), now));
                    }
                    _ => {
                        // no recoverable prompt (empty / tool-only transcript, or an
                        // unreadable one past turn 1) → LATCH this block so we don't
                        // re-read every tick, surface it ONCE, and do NOT arm: we
                        // never replay a fabricated prompt.
                        g.ac_fired = true;
                        g.push_event(SessionEventKind::Notice {
                            text: "auto-continue: blocked — no recoverable prompt to auto-continue"
                                .into(),
                        });
                    }
                }
            }
        }

        // ── build the gate inputs from real session state ──
        // RAW read of `g.usage_limit`, NEVER the `usage_limit()` accessor: that one
        // CLOCK-EXPIRES a limit for the VIEW (`UsageLimit::is_expired`), and reading
        // it here would make `cleared` true the moment the estimated reset passed.
        // `cleared` must stay the "banner is actually GONE from the grid" proof — the
        // independent evidence the FIRE gate is built on. Routing it through the
        // accessor would collapse it into the very timer check it exists to
        // corroborate, and auto-continue would type into a session still blocked on a
        // bad estimate. Keep this raw.
        let ul = g.usage_limit.as_ref();
        let hit = ul.map(|u| u.hit).unwrap_or(false);
        let cleared = ul.is_none();
        let reset_at = ul.and_then(|u| u.reset_at_unix);
        // A session AWAITING a decision (claude hook card / codex approval) must
        // NEVER be fired into even when grid_has_dialog can't corroborate it — fold
        // AwaitingDecision into `busy` so the FIRE gate blocks on it too. `g.busy`
        // is the OSC 9;4 progress flag.
        let phase = compute_phase(&g, alive, recently);
        let busy = g.busy || phase == Phase::AwaitingDecision;
        let has_dialog = grid_has_dialog(&g.emu.snapshot());
        let composer_empty = g.input_buffer.trim().is_empty();
        let last_submit_ms = (g.last_submit_ms != 0).then_some(g.last_submit_ms);
        let has_held_prompt = g.ac_prompt.is_some();
        let inputs = AcInputs {
            on,
            hit,
            reset_at,
            cleared,
            busy,
            recently_working: recently,
            has_dialog,
            composer_empty,
            last_submit_ms,
            has_held_prompt,
            armed: g.ac_armed,
            armed_reset_at: g.ac_reset_at.unwrap_or(0),
            armed_at_ms: g.ac_armed_at_ms,
            fired: g.ac_fired,
            fire_on_reset,
        };
        let mut decision = ac_decide(&inputs, now);
        // #2 TOCTOU: the composer check and the Fire write must be ATOMIC. We hold
        // `g` from the input build through the write below, so `composer_empty` /
        // `cleared` / `armed` cannot have moved (every writer takes `inner`); this
        // recheck is defense-in-depth. Any mismatch downgrades Fire→Skip so we stay
        // armed and retry — never a fire past a stale check.
        if decision == AcDecision::Fire
            && !(g.input_buffer.trim().is_empty() && g.usage_limit.is_none() && g.ac_armed)
        {
            decision = AcDecision::Skip;
        }
        // Everything except the pty keystrokes is a pure `Inner` mutation, so it's
        // unit-tested on a bare `Inner` (RULE ZERO). `ac_apply` returns the exact
        // ground-truth text ONLY when a live session must be fired into.
        let fire_text = ac_apply(&mut g, decision, now, hit, alive);
        if let Some(text) = fire_text {
            // Replay EXACTLY the resolved ground-truth prompt (never fabricated): a
            // bracketed paste of the line, then Enter. Written WHILE STILL HOLDING
            // `g` (lock-held raw-write path, #2) so a client keystroke can't
            // interleave between the composer check and the write. `pty.write` locks
            // only its own writer mutex, and no path locks `writer` then `inner`, so
            // this `inner`→`writer` ordering cannot deadlock.
            let mode = g.emu.mode_flags();
            if let Some(bytes) = input::encode(&KeyInput::Paste(text), mode) {
                self.pty.write(&bytes);
            }
            if let Some(bytes) = input::encode(&KeyInput::Enter, mode) {
                self.pty.write(&bytes);
            }
        }
        drop(g);
        decision
    }

    /// Read the LAST user-submitted message from this session's on-disk CLI
    /// transcript — the GROUND-TRUTH prompt to replay at a usage-limit reset
    /// (docs/019 rework: the source is the transcript, never keystroke capture,
    /// which the review proved replays stale/wrong/truncated prompts). Resolves the
    /// path by the SAME id-glob the app already uses (`orchestrator_core::scan`),
    /// reads it, and parses the last real user turn (mirroring the app's standup
    /// extractors). Distinguishes "readable but no user turn" from "unreadable" so
    /// the caller applies the first-turn `initial_prompt` fallback ONLY on the
    /// latter. Runs OUTSIDE the `inner` lock (a large transcript read must not stall
    /// the reader thread).
    fn transcript_last_user_message(&self) -> TranscriptPrompt {
        let Some(cli) = self.cli_session_id() else {
            return TranscriptPrompt::Unreadable;
        };
        let (path, is_codex) = match self.kind {
            CliKind::Claude => (orchestrator_core::scan::claude_transcript_path(&cli), false),
            CliKind::Codex => (orchestrator_core::scan::codex_rollout_path(&cli), true),
            // shells never hit a subscription usage limit → no prompt.
            _ => return TranscriptPrompt::Unreadable,
        };
        let Some(path) = path else {
            return TranscriptPrompt::Unreadable;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return TranscriptPrompt::Unreadable;
        };
        let found = if is_codex {
            crate::transcript::codex_last_user_message(&text)
        } else {
            crate::transcript::claude_last_user_message(&text)
        };
        match found {
            Some(p) => TranscriptPrompt::Found(p),
            None => TranscriptPrompt::NoUserMessage,
        }
    }

    /// The project this session is filed under (rebindable — a session can be
    /// MOVED to the project the work actually belongs to, dogfooding #10).
    pub fn project_slug(&self) -> String {
        self.project_slug.lock().unwrap().clone()
    }

    /// Re-home the session under another project.
    pub fn rebind_project(&self, slug: &str) {
        *self.project_slug.lock().unwrap() = slug.to_string();
        self.dirty.fetch_add(1, Ordering::SeqCst);
    }

    /// The decision the agent is currently blocked on, if any (M3).
    pub fn pending(&self) -> Option<PendingDecision> {
        self.inner.lock().unwrap().pending.clone()
    }

    /// Last assistant message (Stop hook) — feed/DID input (M4).
    pub fn last_message(&self) -> String {
        self.inner.lock().unwrap().last_message.clone()
    }

    /// Route a hook event for this session (docs/014). PermissionRequest raises
    /// a pending decision; Stop clears it + records the message.
    pub fn on_hook(&self, event: HookEvent) {
        let mut g = self.inner.lock().unwrap();
        // record the timeline event (#9) before the semantic handling below —
        // a parallel, source-agnostic sink; the consent path stays untouched.
        if let Some(kind) = crate::events::reduce(&event) {
            g.push_event(kind);
        }
        match &event {
            HookEvent::PermissionRequest(_) => {
                if let Some(d) = decision::extract(&event) {
                    g.pending = Some(d);
                    let now = now_ms();
                    self.pending_set_ms.store(now, Ordering::Relaxed);
                    // a NEW ask always gets its OWN age — even when the phase
                    // is already AwaitingDecision (pending replaced within the
                    // paint grace) note_phase would early-return and the card
                    // would inherit the previous ask's age (review, proven).
                    if g.phase_now != Phase::AwaitingDecision {
                        g.phase_prev = g.phase_now;
                        g.phase_prev_since_ms = g.phase_since_ms;
                        g.phase_now = Phase::AwaitingDecision;
                    }
                    g.phase_since_ms = now;
                }
            }
            HookEvent::Stop(p) => {
                if let Some(m) = &p.last_assistant_message {
                    g.last_message = m.clone();
                }
                g.pending = None; // turn ended → any prompt is resolved
                                  // the semantic turn-end outranks a missed OSC 9;4 done: never
                                  // leave a phantom "working" latch past a Stop (critique #4).
                g.busy = false;
                g.trouble = None;
                // mute the scan briefly: the old banner is still in the tail
                // and would re-latch on the next tick (review).
                g.trouble_muted_until_ms = now_ms() + 5000;
            }
            // PreToolUse/Notification are activity signals in M3 (no card).
            _ => {}
        }
        // hooks are mutation sites for phase — stamp the edge now (#4).
        let ph = compute_phase(&g, self.pty.is_alive(), self.recently_working());
        g.note_phase(ph, now_ms());
        drop(g);
        self.dirty.fetch_add(1, Ordering::SeqCst);
    }

    /// The full event timeline (the surviving ring) — the GUI reads this.
    pub fn events(&self) -> Vec<SessionEvent> {
        self.inner.lock().unwrap().events.iter().cloned().collect()
    }

    /// Only events newer than `after_seq` — the daemon coalescer's delta scan.
    pub fn events_since(&self, after_seq: u64) -> Vec<SessionEvent> {
        self.inner
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|e| e.seq > after_seq)
            .cloned()
            .collect()
    }

    /// The current max event seq — a CHEAP change check (no filter/clone) so the
    /// 16ms coalescer skips `events_since` entirely on event-quiet ticks.
    pub fn max_event_seq(&self) -> u64 {
        self.inner.lock().unwrap().event_seq
    }

    /// Append historical timeline items parsed from a transcript (#9 slice 4) —
    /// one batched lock + a single dirty bump. Items are already in file order.
    pub fn push_timeline_items(&self, items: Vec<crate::transcript::TimelineItem>) {
        if items.is_empty() {
            return;
        }
        let mut g = self.inner.lock().unwrap();
        for it in items {
            g.push_event_at(it.at_ms, it.kind);
        }
        drop(g);
        self.dirty.fetch_add(1, Ordering::SeqCst);
    }

    /// The first event's timestamp = ~attach/spawn time — the cutoff a claude
    /// backfill uses to keep only pre-attach history (hooks own the rest).
    pub fn started_at_ms(&self) -> u64 {
        self.inner
            .lock()
            .unwrap()
            .events
            .front()
            .map(|e| e.at_ms)
            .unwrap_or(0)
    }

    /// Clear a stale pending decision if its dialog has left the grid (the
    /// human answered in-terminal; rejection is hook-invisible, S1) OR the
    /// session died at the dialog (no Stop hook will ever come). Call from the
    /// repaint tick. Returns true if it cleared one.
    pub fn reconcile_pending(&self) -> bool {
        // a dead session can't resolve a dialog — clear regardless of the
        // frozen grid (else the card zombies on a dead session).
        let dead = !self.pty.is_alive();
        // HOOK-BEATS-DIALOG grace: the PermissionRequest hook usually lands
        // before the dialog paints, so "no dialog in the grid" is meaningless
        // for a young decision. Return BEFORE consuming the dirty gate below,
        // so bytes that arrive during the grace still trigger the post-grace
        // walk (consuming them here would zombie a stale pending forever).
        // Death still clears immediately (no Stop hook will ever come).
        const PAINT_GRACE_MS: u64 = 2000;
        let young =
            now_ms().saturating_sub(self.pending_set_ms.load(Ordering::Relaxed)) < PAINT_GRACE_MS;
        if young && !dead {
            return false;
        }
        // skip the grid walk when no PTY bytes arrived since the last check: an
        // unchanged grid means the dialog hasn't moved or closed (perf audit #3).
        let dirty = self.dirty.load(Ordering::SeqCst);
        if !dead && self.last_reconcile_dirty.swap(dirty, Ordering::SeqCst) == dirty {
            return false;
        }
        let mut g = self.inner.lock().unwrap();
        // re-check the grace UNDER the lock: a PermissionRequest can land
        // between the early check above and this lock (on_hook stamps while
        // holding `inner`), and the pre-lock read would judge the fresh
        // decision by the previous one's stamp (adversarial review).
        let young =
            now_ms().saturating_sub(self.pending_set_ms.load(Ordering::Relaxed)) < PAINT_GRACE_MS;
        if g.pending.is_some() && (dead || (!young && !grid_has_dialog(&g.emu.snapshot()))) {
            g.pending = None;
            // if this was a codex title-raised latch, tear down awaiting + the
            // title marker too (review F1) so it can't zombie as a card-less
            // AwaitingDecision and so the latch re-arms for the next approval.
            if g.awaiting_from_title {
                g.awaiting = false;
                g.awaiting_from_title = false;
            }
            // clearing pending is a phase mutation site (#4).
            let ph = compute_phase(&g, !dead, self.recently_working());
            g.note_phase(ph, now_ms());
            drop(g);
            self.dirty.fetch_add(1, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    /// The repaint coalescing counter (GUI compares across frames).
    pub fn dirty(&self) -> u64 {
        self.dirty.load(Ordering::SeqCst)
    }

    /// Send a neutral key event (encoded mode-aware). Reads only the mode bits
    /// (no full-grid snapshot on the keystroke hot path).
    pub fn send_key(&self, key: &KeyInput) {
        let mut g = self.inner.lock().unwrap();
        // Any keystroke into the session IS the in-terminal answer to an
        // OSC-level approval (codex has no hooks): approve/deny/Esc all arrive
        // here, so drop the awaiting latch. The `pending` card is left alone on
        // purpose — a stray key that ISN'T the answer would else dismiss a live
        // codex card and let the next blink re-raise it (flicker + dup event).
        // The codex title-raised card clears when codex leaves the approval (the
        // resolving OSC-0 title) or via grid-reconcile; a claude hook card
        // clears via injection/Stop/grid-reconcile.
        g.awaiting = false;
        // also drop the title-owned marker so the latch fully re-arms for the
        // NEXT codex approval — else awaiting_from_title stays true and the
        // raise-once guard silently suppresses it (review F1/F7).
        g.awaiting_from_title = false;
        // ── COMPOSER-DRAFT CAPTURE (docs/019 slice 2, safety-review rework):
        // maintain the composer-empty gate + last-submit time. NO grid/dialog check
        // — the replayed prompt now comes from the CLI transcript at arm time, not
        // keystrokes. See the `input_buffer` / `last_submit_ms` field docs. ──
        let alive = self.pty.is_alive();
        let recently = self.recently_working();
        capture_key(&mut g, key);
        // a key is a mutation site for phase (awaiting → busy/idle edge, #4).
        let ph = compute_phase(&g, alive, recently);
        g.note_phase(ph, now_ms());
        let mode = g.emu.mode_flags();
        // typing jumps the view back to the live bottom (standard terminal UX).
        g.emu.scroll_to_bottom();
        drop(g);
        if let Some(bytes) = input::encode(key, mode) {
            self.pty.write(&bytes);
        }
    }

    /// Scroll the session's viewport. On the MAIN screen this moves our own
    /// scrollback; inside a fullscreen (alt-screen) app the scrollback is empty,
    /// so FORWARD the wheel to the app — mouse-wheel events if it captures the
    /// mouse, else arrow keys (alternate scroll) — exactly like a real terminal,
    /// so `claude --tui fullscreen` / vim / htop scroll inside our terminal (#9).
    pub fn scroll(&self, delta: i32) {
        let mode = self.inner.lock().unwrap().emu.scroll_mode();
        let bytes = if mode.mouse {
            input::encode_mouse_scroll(delta, mode.sgr)
        } else if mode.alt_screen && mode.alt_scroll {
            input::encode_arrow_scroll(delta, mode.app_cursor)
        } else {
            self.inner.lock().unwrap().emu.scroll(delta);
            self.dirty.fetch_add(1, Ordering::SeqCst);
            return;
        };
        if !bytes.is_empty() {
            self.pty.write(&bytes);
        }
    }

    /// Search the whole grid incl. scrollback (⌘F) — wrapline-aware via the
    /// fork's RegexSearch (see Emulator::search_grid).
    pub fn search(&self, query: &str) -> Vec<crate::emulator::SearchMatch> {
        self.inner.lock().unwrap().emu.search_grid(query)
    }

    /// PURE view scroll (scroll-to-match): always moves the viewport, NEVER
    /// routes to the PTY — `scroll()` would inject mouse/arrow bytes into
    /// alt-screen apps (design critique: searching in vim must not type into
    /// vim). Alt-screen has no history, so the emu scroll is a no-op there.
    pub fn scroll_view(&self, delta: i32) {
        self.inner.lock().unwrap().emu.scroll(delta);
        self.dirty.fetch_add(1, Ordering::SeqCst);
    }

    /// Inject raw bytes (used by the answer path / tests).
    pub fn send_raw(&self, bytes: &[u8]) {
        self.pty.write(bytes);
    }

    /// Resize the session: emulator Term AND the PTY together, so the child
    /// and the rendered grid agree on geometry (docs/013 §4). Returns whether
    /// the geometry actually changed (caller can skip a redundant ioctl).
    pub fn resize(&self, rows: u16, cols: u16) {
        {
            let mut g = self.inner.lock().unwrap();
            g.emu.resize(rows, cols);
        }
        self.pty.resize(rows, cols);
        self.dirty.fetch_add(1, Ordering::SeqCst);
    }

    pub fn is_alive(&self) -> bool {
        self.pty.is_alive()
    }
}

/// Does the visible grid show a permission/question dialog? (anchor-based
/// corroboration for re-verify-before-inject and stale-card reconcile.)
fn grid_has_dialog(snap: &GridSnapshot) -> bool {
    let lines = snap.plain_lines();
    let has = |needle: &str| lines.iter().any(|l| l.contains(needle));
    (has("Do you want") || has("Enter to select") || has("Would you like"))
        && (has("❯") || has("Esc to cancel") || has("esc to"))
}

fn apply_osc(inner: &mut Inner, kind: CliKind, sig: OscSignal) -> bool {
    // NOTE (docs/013 §1 "semantics first"): real decision detection is the M3
    // job (hooks / app-server). Here OSC only drives the calm busy/idle state.
    // `awaiting` is set ONLY by codex's explicit approval announcement and is
    // CLEARED by any subsequent activity, so it can never latch on (the OSC-777
    // idle-prompt is NOT treated as a decision — that was the claude latch bug).
    let mut raised_pending = false;
    match sig {
        OscSignal::Notify9(text) => {
            if text.starts_with("Approval requested")
                || text.contains("wants to edit")
                || text.contains("Action Required")
            {
                inner.awaiting = true;
                if let Some(pending) = codex_pending_from_notify(&text) {
                    inner.pending = Some(pending);
                    inner.push_event(SessionEventKind::Awaiting {
                        tool: "Codex".into(),
                    });
                    let now = now_ms();
                    if inner.phase_now != Phase::AwaitingDecision {
                        inner.phase_prev = inner.phase_now;
                        inner.phase_prev_since_ms = inner.phase_since_ms;
                        inner.phase_now = Phase::AwaitingDecision;
                    }
                    inner.phase_since_ms = now;
                    raised_pending = true;
                }
            } else if text.contains("turn complete") {
                inner.awaiting = false;
                inner.awaiting_from_title = false;
                inner.pending = None;
                inner.busy = false;
                inner.trouble = None; // turn ended cleanly → any latched chip is stale
                inner.trouble_muted_until_ms = crate::events::now_ms() + 5000;
            }
        }
        OscSignal::Progress { state, .. } => match state {
            0 => inner.busy = false, // done
            1 | 2 | 3 => {
                // activity resumed → busy, and any stale approval is cleared
                inner.busy = true;
                inner.awaiting = false;
                inner.awaiting_from_title = false;
            }
            _ => {}
        },
        // OSC 777 "waiting for your input" is an idle prompt, NOT a decision —
        // ignore for phase (it would otherwise latch claude into needs-you).
        OscSignal::Notify777 { .. } => {}
        // The OSC-0 window title. Codex signals an approval block HERE, not via
        // OSC 9: in interactive TUI mode it emits zero OSC 9 and instead swaps
        // the title to `[ ! ] Action Required | <proj>`, blinking `[ ! ]`↔`[ . ]`
        // (BOTH frames carry "Action Required"). Claude also sets titles ("✳ …")
        // but never writes "Action Required", so gating on CliKind::Codex AND the
        // marker keeps claude from ever latching this. Otherwise a retitle is just
        // activity narration and must NOT clear awaiting (codex retitles while its
        // dialog is up — review #12 signal-eater).
        OscSignal::Title(t) => {
            let action_required = kind == CliKind::Codex && t.contains("Action Required");
            inner.title = t;
            if action_required {
                // Raise ONCE per block. The title blinks ~2Hz and both frames
                // match, so re-stamping the phase / re-pushing the event every
                // frame would reset the card age and spam the timeline ring.
                // `awaiting_from_title` gates the blink to a single raise and
                // marks the latch as title-owned so only a resolving title
                // (the else-branch) clears it.
                if !inner.awaiting_from_title {
                    inner.awaiting = true;
                    inner.awaiting_from_title = true;
                    inner.pending = Some(codex_pending_from_title());
                    inner.push_event(SessionEventKind::Awaiting {
                        tool: "Codex".into(),
                    });
                    let now = now_ms();
                    if inner.phase_now != Phase::AwaitingDecision {
                        inner.phase_prev = inner.phase_now;
                        inner.phase_prev_since_ms = inner.phase_since_ms;
                        inner.phase_now = Phase::AwaitingDecision;
                    }
                    inner.phase_since_ms = now;
                    raised_pending = true;
                }
            } else if inner.awaiting_from_title {
                // A title WITHOUT "Action Required" arrived while a title-raised
                // approval was outstanding → codex left the approval (reverted
                // to a spinner/idle title). Clear ONLY the title-raised latch;
                // the caller recomputes the phase. A hook/Notify9 pending
                // (claude) has `awaiting_from_title == false` and is untouched.
                inner.awaiting = false;
                inner.awaiting_from_title = false;
                inner.pending = None;
            }
        }
    }
    raised_pending
}

/// The codex approval card raised from the OSC-0 "Action Required" title. The
/// title carries no command (the real command is on the grid + reachable via
/// the card's expand), so this is a generic "needs approval" summary.
fn codex_pending_from_title() -> PendingDecision {
    PendingDecision {
        tool_use_id: None,
        tool_name: "Codex".into(),
        view: crate::decision::DecisionView::Other {
            tool: "Codex".into(),
            summary: "Codex needs your approval".into(),
        },
    }
}

fn codex_pending_from_notify(text: &str) -> Option<PendingDecision> {
    let view = if let Some(cmd) = text.strip_prefix("Approval requested:") {
        crate::decision::DecisionView::Bash {
            command: cmd.trim().to_string(),
            description: String::new(),
        }
    } else if text.contains("wants to edit") || text.contains("Action Required") {
        crate::decision::DecisionView::Other {
            tool: "Codex".into(),
            summary: text.trim().to_string(),
        }
    } else {
        return None;
    };
    Some(PendingDecision {
        tool_use_id: None,
        tool_name: "Codex".into(),
        view,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn test_inner() -> Inner {
        Inner {
            emu: Emulator::new(10, 40),
            osc: OscTap::new(),
            title: String::new(),
            awaiting: false,
            awaiting_from_title: false,
            busy: false,
            pending: None,
            last_message: String::new(),
            events: std::collections::VecDeque::new(),
            event_seq: 0,
            phase_now: Phase::Idle,
            phase_since_ms: 0,
            phase_prev: Phase::Idle,
            phase_prev_since_ms: 0,
            trouble: None,
            usage_limit: None,
            last_restore_ms: 0,
            trouble_muted_until_ms: 0,
            input_buffer: String::new(),
            last_submit_ms: 0,
            ac_prompt: None,
            ac_armed: false,
            ac_reset_at: None,
            ac_armed_at_ms: 0,
            ac_fired: false,
        }
    }

    #[test]
    fn event_ring_caps_and_seq_stays_monotonic_across_eviction() {
        let mut inner = test_inner();
        for _ in 0..300 {
            inner.push_event(SessionEventKind::Started);
        }
        // the ring is bounded, but seq keeps counting (front is evicted, not reset).
        assert_eq!(inner.events.len(), EVENT_CAP);
        assert_eq!(inner.events.back().unwrap().seq, 300);
        assert_eq!(
            inner.events.front().unwrap().seq,
            300 - EVENT_CAP as u64 + 1
        );
        // a delta scan keyed on seq is correct even after front-eviction.
        let cursor = 297;
        let since: Vec<u64> = inner
            .events
            .iter()
            .filter(|e| e.seq > cursor)
            .map(|e| e.seq)
            .collect();
        assert_eq!(since, vec![298, 299, 300]);
    }

    fn wait_until(mut f: impl FnMut() -> bool, ms: u64) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(ms) {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        f()
    }

    #[test]
    fn shell_session_renders_output_on_the_grid() {
        let spec = SpawnSpec::program("printf", std::env::temp_dir()).arg("hello-grid");
        let s =
            HostedSession::spawn(SessionId(1), CliKind::Shell, "test".into(), None, spec).unwrap();
        assert!(
            wait_until(|| s.snapshot().contains("hello-grid"), 2000),
            "printed text never reached the grid"
        );
    }

    #[test]
    fn send_key_reaches_the_process() {
        let spec = SpawnSpec::program("cat", std::env::temp_dir());
        let s =
            HostedSession::spawn(SessionId(2), CliKind::Shell, "test".into(), None, spec).unwrap();
        s.send_key(&KeyInput::Char("ping".into()));
        s.send_key(&KeyInput::Enter);
        assert!(
            wait_until(|| s.snapshot().contains("ping"), 2000),
            "keystrokes never echoed back via cat"
        );
    }

    #[test]
    fn dirty_counter_advances_with_output() {
        let spec = SpawnSpec::program("printf", std::env::temp_dir()).arg("x");
        let s =
            HostedSession::spawn(SessionId(3), CliKind::Shell, "test".into(), None, spec).unwrap();
        assert!(wait_until(|| s.dirty() > 0, 2000), "dirty never advanced");
    }

    #[test]
    fn dead_process_reports_dead_phase() {
        let spec = SpawnSpec::program("true", std::env::temp_dir());
        let s =
            HostedSession::spawn(SessionId(4), CliKind::Shell, "test".into(), None, spec).unwrap();
        assert!(
            wait_until(|| s.phase() == Phase::Dead, 2000),
            "phase never went Dead"
        );
    }

    #[test]
    fn resize_grows_the_emulator_grid_and_bumps_dirty() {
        let spec = SpawnSpec::program("cat", std::env::temp_dir());
        let s =
            HostedSession::spawn(SessionId(5), CliKind::Shell, "test".into(), None, spec).unwrap();
        assert_eq!(s.snapshot().rows.len(), 30); // SpawnSpec default
        let before = s.dirty();
        s.resize(38, 100);
        assert_eq!(
            s.snapshot().rows.len(),
            38,
            "emulator grid did not resize with the PTY"
        );
        assert!(
            s.dirty() > before,
            "resize did not bump the repaint counter"
        );
    }

    #[test]
    fn codex_osc_approval_creates_pending_decision() {
        let mut inner = test_inner();
        let raised = apply_osc(
            &mut inner,
            CliKind::Codex,
            OscSignal::Notify9("Approval requested: rm /tmp/x".into()),
        );
        assert!(raised);
        assert!(inner.awaiting);
        assert_eq!(inner.phase_now, Phase::AwaitingDecision);
        let pending = inner
            .pending
            .expect("codex approval should create card data");
        assert_eq!(pending.tool_name, "Codex");
        assert_eq!(
            pending.view,
            crate::decision::DecisionView::Bash {
                command: "rm /tmp/x".into(),
                description: String::new()
            }
        );
        assert!(inner.events.iter().any(|e| e.kind
            == SessionEventKind::Awaiting {
                tool: "Codex".into()
            }));
    }

    #[test]
    fn codex_turn_complete_clears_osc_pending_decision() {
        let mut inner = test_inner();
        assert!(apply_osc(
            &mut inner,
            CliKind::Codex,
            OscSignal::Notify9("Approval requested: rm /tmp/x".into())
        ));
        apply_osc(
            &mut inner,
            CliKind::Codex,
            OscSignal::Notify9("Agent turn complete".into()),
        );
        assert!(!inner.awaiting);
        assert!(inner.pending.is_none());
        assert!(!inner.busy);
    }

    #[test]
    fn codex_title_action_required_raises_then_clears() {
        // Codex's ACTUAL approval signal in interactive TUI: the OSC-0 title
        // becomes "[ ! ] Action Required | proj" (no OSC 9 is emitted). The old
        // Notify9 matcher never fired here — this is the bug in issue #3.
        let mut inner = test_inner();
        let raised = apply_osc(
            &mut inner,
            CliKind::Codex,
            OscSignal::Title("[ ! ] Action Required | proj".into()),
        );
        assert!(raised, "an Action Required title must raise the codex approval");
        assert!(inner.awaiting);
        assert!(inner.awaiting_from_title);
        assert_eq!(inner.phase_now, Phase::AwaitingDecision);
        let pending = inner.pending.clone().expect("title approval creates card data");
        assert_eq!(pending.tool_name, "Codex");
        assert!(inner.events.iter().any(|e| e.kind
            == SessionEventKind::Awaiting {
                tool: "Codex".into()
            }));

        // The blink frame "[ . ] Action Required" still matches — it must be a
        // no-op: no re-raise, no new timeline event, no card-age reset.
        let events_before = inner.events.len();
        let since_before = inner.phase_since_ms;
        let raised_blink = apply_osc(
            &mut inner,
            CliKind::Codex,
            OscSignal::Title("[ . ] Action Required | proj".into()),
        );
        assert!(!raised_blink, "the blink frame must not re-raise");
        assert_eq!(inner.events.len(), events_before, "blink spammed the event ring");
        assert_eq!(inner.phase_since_ms, since_before, "blink reset the card age");
        assert!(inner.awaiting);

        // Codex leaves the approval → the title reverts to a spinner/idle frame.
        // The latch clears; the caller's compute_phase then leaves needs-you.
        apply_osc(
            &mut inner,
            CliKind::Codex,
            OscSignal::Title("⠋ proj".into()),
        );
        assert!(!inner.awaiting);
        assert!(!inner.awaiting_from_title);
        assert!(inner.pending.is_none());
        assert_eq!(compute_phase(&inner, true, false), Phase::Idle);
    }

    #[test]
    fn claude_title_never_latches_needs_you() {
        // Claude sets OSC-0 titles too ("✳ …") but never writes "Action
        // Required"; and even if some title did, the CliKind gate blocks it.
        let mut inner = test_inner();
        let raised = apply_osc(
            &mut inner,
            CliKind::Claude,
            OscSignal::Title("✳ Cogitating…".into()),
        );
        assert!(!raised);
        assert!(!inner.awaiting);
        assert!(!inner.awaiting_from_title);
        assert!(inner.pending.is_none());
        assert_eq!(inner.title, "✳ Cogitating…");
        // A non-codex "Action Required" title must still be inert (gate on kind).
        assert!(!apply_osc(
            &mut inner,
            CliKind::Claude,
            OscSignal::Title("[ ! ] Action Required | proj".into()),
        ));
        assert!(!inner.awaiting);
        assert!(inner.pending.is_none());
    }

    /// The view expires a limit on the clock; auto-continue must NOT. Its `cleared`
    /// input is the "banner is GONE from the grid" proof — evidence INDEPENDENT of the
    /// reset estimate we armed against. Grep-proof that it cannot come from the
    /// expiring accessor: `usage_limit()` has exactly ONE caller in the workspace,
    /// `SessionHost::info_of` (host.rs), and `auto_continue_step` reads `g.usage_limit`
    /// RAW under the lock. If someone "cleans that up" to the accessor, this fails.
    #[test]
    fn auto_continue_fires_on_a_gone_banner_not_on_a_passed_clock() {
        // armed, long past the reset, quiet, composer empty — but the banner is STILL
        // on the grid. A clock-expired view would call this "cleared" and fire into a
        // session that is still blocked.
        let mut still_blocked = armed_ready();
        still_blocked.hit = true;
        still_blocked.cleared = false;
        assert_eq!(
            ac_decide(&still_blocked, R_MS + 1000),
            AcDecision::Skip,
            "clock passed but the banner is still painted ⇒ NOT a reset"
        );
        // the genuine edge: the banner is gone.
        assert_eq!(ac_decide(&armed_ready(), R_MS), AcDecision::Fire);
    }

    #[test]
    fn fire_on_reset_config_fires_on_the_clock_without_a_cleared_banner() {
        // audit B2 / task #31: the default `cleared` edge never comes for an idle
        // blocked session, so the `fire_on_reset` config lets FIRE key off the
        // resolved reset CLOCK instead. Default OFF stays conservative.
        let mut i = armed_ready();
        i.hit = true;
        i.cleared = false; // banner still painted (the idle-session reality)
        assert_eq!(
            ac_decide(&i, R_MS),
            AcDecision::Skip,
            "config OFF ⇒ a still-painted banner never fires (current behavior)"
        );
        // config ON: the SAME state fires once the reset instant arrives.
        i.fire_on_reset = true;
        assert_eq!(
            ac_decide(&i, R_MS),
            AcDecision::Fire,
            "config ON ⇒ fire on the reset clock"
        );
        // ...but every OTHER safety gate still binds with the config on.
        i.composer_empty = false; // a half-typed line
        assert_eq!(
            ac_decide(&i, R_MS),
            AcDecision::Skip,
            "config on but composer non-empty ⇒ never type over a half line"
        );
        i.composer_empty = true;
        assert_eq!(ac_decide(&i, R_MS - 1), AcDecision::Skip, "before the reset ⇒ wait");
    }

    // ════════ auto-continue-on-limit-reset (docs/019 slice 2) — SAFETY GATE ════════
    // The pure `ac_decide` is the entire safety boundary (it decides whether to type
    // into a live session unattended), so EVERY branch is pinned here. The actuation
    // `ac_apply` + the `capture_key` reconstruction are tested on a bare `Inner`.
    // No subprocess anywhere (RULE ZERO).

    const R: i64 = 1_000_000; // an arbitrary reset instant (unix seconds)
    const R_MS: u64 = 1_000_000_000; // R * 1000, as u64 ms

    /// Base gate input: flag ON, a hard HIT with a captured prompt + a known reset,
    /// NOT yet armed, session quiet. Each test flips exactly the field under test.
    fn base_inputs() -> AcInputs {
        AcInputs {
            on: true,
            hit: true,
            reset_at: Some(R),
            cleared: false,
            busy: false,
            recently_working: false,
            has_dialog: false,
            composer_empty: true,
            last_submit_ms: Some(500),
            has_held_prompt: true,
            armed: false,
            armed_reset_at: R,
            armed_at_ms: 1000,
            fired: false,
            fire_on_reset: false,
        }
    }

    /// State AFTER arming, at/after the reset with the banner CLEARED and the
    /// session quiet — i.e. every FIRE precondition met.
    fn armed_ready() -> AcInputs {
        AcInputs {
            hit: false, // banner CLEARED — the reset edge we fire on
            cleared: true,
            armed: true,
            ..base_inputs()
        }
    }

    #[test]
    fn gate_off_disarms_when_armed_else_skips() {
        let mut armed = armed_ready();
        armed.on = false;
        assert_eq!(ac_decide(&armed, R_MS), AcDecision::Disarm, "off ⇒ drop the arm");
        let mut idle = base_inputs();
        idle.on = false;
        assert_eq!(ac_decide(&idle, R_MS), AcDecision::Skip, "off + unarmed ⇒ nothing");
    }

    #[test]
    fn gate_warning_never_arms() {
        // A benign warning (hit=false) must NEVER arm, even with a held prompt +
        // reset present — auto-continue gates strictly on a HARD block.
        let mut i = base_inputs();
        i.hit = false;
        assert_eq!(ac_decide(&i, 0), AcDecision::Skip);
    }

    #[test]
    fn gate_arms_on_fresh_hit_edge() {
        assert_eq!(ac_decide(&base_inputs(), 0), AcDecision::Arm { reset_at: R });
    }

    #[test]
    fn gate_hit_without_held_prompt_never_arms() {
        // Nothing to replay ⇒ never arm (we never fabricate a prompt). With the
        // rework this means the transcript yielded no user message → ac_prompt None.
        let mut i = base_inputs();
        i.has_held_prompt = false;
        i.last_submit_ms = None;
        assert_eq!(ac_decide(&i, 0), AcDecision::Skip);
    }

    #[test]
    fn gate_hit_without_reset_instant_never_arms() {
        // No resolved wake instant ⇒ never arm (can't know when to fire).
        let mut i = base_inputs();
        i.reset_at = None;
        assert_eq!(ac_decide(&i, 0), AcDecision::Skip);
    }

    #[test]
    fn gate_no_fire_before_reset_instant() {
        // Armed + cleared + quiet, but the reset clock hasn't arrived yet.
        assert_eq!(ac_decide(&armed_ready(), R_MS - 1000), AcDecision::Skip);
    }

    #[test]
    fn gate_no_fire_while_banner_still_present() {
        // Past the reset clock, but the banner has NOT cleared — the reset didn't
        // genuinely happen yet. This is the ARM-on-hit / FIRE-on-clear asymmetry.
        let mut i = armed_ready();
        i.hit = true;
        i.cleared = false;
        assert_eq!(ac_decide(&i, R_MS), AcDecision::Skip);
    }

    #[test]
    fn gate_no_fire_when_not_quiet() {
        // Any of busy / recently-working / open-dialog blocks the fire.
        let tweaks: [fn(&mut AcInputs); 3] = [
            |i| i.busy = true,
            |i| i.recently_working = true,
            |i| i.has_dialog = true,
        ];
        for tweak in tweaks {
            let mut i = armed_ready();
            tweak(&mut i);
            assert_eq!(ac_decide(&i, R_MS), AcDecision::Skip);
        }
    }

    #[test]
    fn gate_no_fire_when_composer_nonempty() {
        // #1/#6: a half-typed composer BLOCKS the fire (never type over the user)
        // but stays ARMED to retry once the draft clears — the safe direction.
        let mut i = armed_ready();
        i.composer_empty = false;
        assert_eq!(ac_decide(&i, R_MS), AcDecision::Skip);
        // and the moment the composer is empty again (every other cond still met)
        // it fires — proving the gate is the ONLY thing that was holding it.
        i.composer_empty = true;
        assert_eq!(ac_decide(&i, R_MS), AcDecision::Fire);
    }

    #[test]
    fn gate_fires_when_reset_reached_and_cleared_and_quiet() {
        assert_eq!(ac_decide(&armed_ready(), R_MS), AcDecision::Fire);
        // still fires shortly after the reset instant.
        assert_eq!(ac_decide(&armed_ready(), R_MS + 60_000), AcDecision::Fire);
    }

    #[test]
    fn gate_disarms_on_newer_user_prompt() {
        // A prompt SUBMITTED after we armed ⇒ the user is driving; back off even
        // though every fire condition is otherwise met (rule 4 precedes fire).
        let mut i = armed_ready();
        i.last_submit_ms = Some(i.armed_at_ms + 1);
        assert_eq!(ac_decide(&i, R_MS), AcDecision::Disarm);
    }

    #[test]
    fn gate_gives_up_6h_past_reset() {
        let now = R_MS + AC_GIVEUP_MS as u64 + 1;
        // banner never cleared, long past reset ⇒ stop trying.
        let mut stuck = armed_ready();
        stuck.hit = true;
        stuck.cleared = false;
        assert_eq!(ac_decide(&stuck, now), AcDecision::GiveUp);
        // GiveUp precedes a late Fire (rule 5 before rule 6): even cleared + quiet,
        // 6h past the reset we give up rather than type into a long-idle session.
        assert_eq!(ac_decide(&armed_ready(), now), AcDecision::GiveUp);
    }

    #[test]
    fn gate_fired_latch_prevents_rearm_until_fresh_hit() {
        // fire-once: the latch blocks a re-arm while still hit...
        let mut i = base_inputs();
        i.fired = true;
        assert_eq!(ac_decide(&i, 0), AcDecision::Skip);
        // ...and only a fresh hit edge (latch cleared by a prior not-hit tick) arms.
        i.fired = false;
        assert_eq!(ac_decide(&i, 0), AcDecision::Arm { reset_at: R });
    }

    // ── actuation (`ac_apply`) on a bare Inner — no pty ──

    #[test]
    fn apply_arm_sets_arm_state() {
        let mut g = test_inner();
        let out = ac_apply(&mut g, AcDecision::Arm { reset_at: R }, 5000, true, true);
        assert_eq!(out, None);
        assert!(g.ac_armed);
        assert_eq!(g.ac_reset_at, Some(R));
        assert_eq!(g.ac_armed_at_ms, 5000);
        assert!(!g.ac_fired);
    }

    #[test]
    fn apply_fire_alive_returns_text_latches_and_logs() {
        let mut g = test_inner();
        g.ac_prompt = Some(("finish the refactor".into(), 100));
        g.ac_armed = true;
        g.ac_reset_at = Some(R);
        let out = ac_apply(&mut g, AcDecision::Fire, 9000, false, true);
        assert_eq!(
            out.as_deref(),
            Some("finish the refactor"),
            "replays the EXACT transcript prompt"
        );
        assert!(g.ac_fired, "fired latch set before the pty write (no double-fire)");
        assert!(!g.ac_armed, "disarmed after firing");
        assert!(g.ac_reset_at.is_none());
        assert!(g.ac_prompt.is_none(), "the resolved prompt is consumed on fire");
        assert!(
            g.events.iter().any(|e| matches!(&e.kind,
                SessionEventKind::Notice { text }
                    if text.contains("auto-continued at reset") && text.contains("finish the refactor"))),
            "#8: the Notice logs the replayed prompt PREFIX, not just a count"
        );
    }

    #[test]
    fn prompt_prefix_bounds_and_flattens() {
        // short prompt: verbatim (newlines flattened, trimmed).
        assert_eq!(prompt_prefix("  fix\nthe bug  ", 80), "fix the bug");
        // long prompt: truncated to `max` chars + an ellipsis (audit trail #8).
        let long = "x".repeat(200);
        let pfx = prompt_prefix(&long, 80);
        assert_eq!(pfx.chars().count(), 81, "80 chars + the ellipsis");
        assert!(pfx.ends_with('…'));
    }

    #[test]
    fn apply_fire_dead_skips_and_logs_manual_resume() {
        let mut g = test_inner();
        g.ac_prompt = Some(("x".into(), 100));
        g.ac_armed = true;
        g.ac_reset_at = Some(R);
        let out = ac_apply(&mut g, AcDecision::Fire, 9000, false, false);
        assert_eq!(out, None, "MUST NOT type into a dead session");
        assert!(!g.ac_armed, "disarmed");
        assert!(!g.ac_fired, "no fire latch on a dead skip");
        assert!(g.events.iter().any(|e| matches!(&e.kind,
            SessionEventKind::Notice { text } if text.contains("died while blocked"))));
    }

    #[test]
    fn apply_giveup_disarms_and_logs() {
        let mut g = test_inner();
        g.ac_armed = true;
        g.ac_reset_at = Some(R);
        let out = ac_apply(&mut g, AcDecision::GiveUp, 9000, true, true);
        assert_eq!(out, None);
        assert!(!g.ac_armed);
        assert!(
            g.ac_fired,
            "GiveUp LATCHES — it must never re-arm against the same dead reset"
        );
        assert!(g.events.iter().any(|e| matches!(&e.kind,
            SessionEventKind::Notice { text } if text.contains("gave up"))));
    }

    /// REGRESSION (the GiveUp oscillation): a banner whose reset is already >6h past
    /// (a bad estimate, or a limit re-observed long after its clock) sits on the grid
    /// FOREVER — no CLI ever retracts it. If GiveUp only DISARMED, the next 1s daemon
    /// tick would find the arm edge open again (`!armed && !fired && hit`), re-read the
    /// whole CLI transcript, re-Arm (rule 2 precedes the GiveUp rule), and give up
    /// again — Arm→GiveUp→Arm forever, one Notice every ~2s until the 256-slot event
    /// ring has evicted every real turn from the timeline. Drives the REAL
    /// `ac_decide`+`ac_apply` pair over 20 ticks: the loop must settle after ONE.
    #[test]
    fn giveup_is_terminal_and_never_oscillates_back_into_arm() {
        let mut g = test_inner();
        let start = R_MS + AC_GIVEUP_MS as u64 + 1; // already past the GiveUp horizon
        let (mut arms, mut giveups) = (0, 0);
        for tick in 0..20u64 {
            let now = start + tick * 1000; // the 1s daemon sweep
            // mirror `auto_continue_step`'s arm edge: whenever it is OPEN, the step
            // does a full `read_to_string` of the CLI transcript and installs a prompt.
            if !g.ac_armed && !g.ac_fired && g.ac_prompt.is_none() {
                g.ac_prompt = Some(("continue the refactor".into(), now));
            }
            let inputs = AcInputs {
                hit: true,      // the banner is STILL on the grid…
                cleared: false, // …so it never clears
                last_submit_ms: None,
                has_held_prompt: g.ac_prompt.is_some(),
                armed: g.ac_armed,
                armed_reset_at: g.ac_reset_at.unwrap_or(R),
                armed_at_ms: g.ac_armed_at_ms,
                fired: g.ac_fired,
                ..base_inputs()
            };
            let decision = ac_decide(&inputs, now);
            match decision {
                AcDecision::Arm { .. } => arms += 1,
                AcDecision::GiveUp => giveups += 1,
                _ => {}
            }
            ac_apply(&mut g, decision, now, true, true);
        }
        assert_eq!(giveups, 1, "GiveUp is TERMINAL — exactly one, not one every other tick");
        assert_eq!(arms, 1, "and it must not re-arm against the same dead reset");
        assert!(g.ac_fired, "the latch holds while the banner is still hit");
        assert!(g.ac_prompt.is_none(), "no transcript re-read once the block is given up");
        let notices = g
            .events
            .iter()
            .filter(|e| matches!(&e.kind, SessionEventKind::Notice { text } if text.contains("gave up")))
            .count();
        assert_eq!(notices, 1, "ONE Notice — not a timeline-evicting stream of them");
    }

    #[test]
    fn apply_not_hit_clears_fired_latch_but_hit_preserves_it() {
        // A genuine not-hit observation re-enables arming for the NEXT hit.
        let mut cleared = test_inner();
        cleared.ac_fired = true;
        assert_eq!(ac_apply(&mut cleared, AcDecision::Skip, 9000, false, true), None);
        assert!(!cleared.ac_fired, "not-hit must clear the fired latch");
        // While STILL hit the latch must persist (one auto-continue per block).
        let mut still_hit = test_inner();
        still_hit.ac_fired = true;
        ac_apply(&mut still_hit, AcDecision::Skip, 9000, true, true);
        assert!(still_hit.ac_fired, "fired latch persists while still hit");
    }

    // ── composer-draft capture (`capture_key`) on a bare Inner — no pty ──
    // (the replayed prompt now comes from the transcript, so these only pin the
    //  composer-empty gate + the manual-resume submit clock.)

    #[test]
    fn capture_tracks_composer_draft_and_submit_time() {
        let mut g = test_inner();
        for ch in ["fix ", "the ", "bug"] {
            capture_key(&mut g, &KeyInput::Char(ch.into()));
        }
        assert_eq!(g.input_buffer, "fix the bug");
        assert_eq!(g.last_submit_ms, 0, "no submit recorded until Enter");
        capture_key(&mut g, &KeyInput::Enter);
        assert!(g.last_submit_ms > 0, "a non-blank Enter records the submit time");
        assert_eq!(g.input_buffer, "", "draft resets after submit");
    }

    #[test]
    fn capture_backspace_and_paste_and_interrupts() {
        let mut g = test_inner();
        capture_key(&mut g, &KeyInput::Char("abc".into()));
        capture_key(&mut g, &KeyInput::Backspace);
        assert_eq!(g.input_buffer, "ab");
        capture_key(&mut g, &KeyInput::Paste("XY".into()));
        assert_eq!(g.input_buffer, "abXY");
        // Ctrl-C abandons the line without submitting.
        capture_key(&mut g, &KeyInput::Ctrl('c'));
        assert_eq!(g.input_buffer, "");
        assert_eq!(g.last_submit_ms, 0, "an interrupt is not a submit");
        // Esc also clears; uppercase Ctrl-C is honored too.
        capture_key(&mut g, &KeyInput::Char("z".into()));
        capture_key(&mut g, &KeyInput::Ctrl('C'));
        assert_eq!(g.input_buffer, "");
        capture_key(&mut g, &KeyInput::Char("q".into()));
        capture_key(&mut g, &KeyInput::Escape);
        assert_eq!(g.input_buffer, "");
    }

    #[test]
    fn capture_nonblank_enter_records_submit_even_for_a_digit() {
        // No dialog check in capture anymore: a stray "1"+Enter records a submit,
        // which only ever DISARMS auto-continue (backs off) — the safe direction.
        let mut g = test_inner();
        capture_key(&mut g, &KeyInput::Char("1".into()));
        capture_key(&mut g, &KeyInput::Enter);
        assert!(g.last_submit_ms > 0);
        assert_eq!(g.input_buffer, "");
    }

    #[test]
    fn capture_blank_enter_does_not_record_submit() {
        // A bare/whitespace Enter (no real content) never records a submit.
        let mut g = test_inner();
        capture_key(&mut g, &KeyInput::Enter);
        assert_eq!(g.last_submit_ms, 0);
        capture_key(&mut g, &KeyInput::Char("   ".into()));
        capture_key(&mut g, &KeyInput::Enter);
        assert_eq!(g.last_submit_ms, 0, "whitespace-only is blank");
        assert_eq!(g.input_buffer, "");
    }
}

