//! SessionHost (docs/013 §1 host.rs) — the verb surface the GUI calls.
//!
//! Owns the session table and mints `SessionId`s. The GUI holds one
//! `Arc<SessionHost>`, lists sessions, spawns into a project, and routes keys.
//! Every method is a plain verb over plain data — at M-daemon this struct is
//! wrapped by a socket server and the GUI swaps in a remote client with the
//! same surface (docs/013 §1 extraction rule).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::decision::PendingDecision;
use crate::emulator::GridSnapshot;
use crate::ingress::{HookIngress, HookMessage};
use crate::input::KeyInput;
use crate::protocol::BridgeStatus;
use crate::pty::SpawnSpec;
use crate::session::{CliKind, HostedSession, SessionId};

/// A flat, render-ready view of one session for the GUI strip. Serde-plain so
/// it can cross the daemon socket unchanged (docs/013 §1 extraction rule).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub kind: CliKind,
    pub project_slug: String,
    pub title: String,
    pub phase: crate::session::Phase,
    pub alive: bool,
    /// the current blocking decision, if any (was read off `Arc<HostedSession>`).
    pub pending: Option<crate::decision::PendingDecision>,
    /// repaint-coalescing counter (the GUI/daemon-tick change signal).
    pub dirty: u64,
    /// the CLI resume handle when known (crash-recovery reconcile).
    pub cli_session_id: Option<String>,
    /// last assistant message (Stop hook) — "what did this agent just finish",
    /// the identity line for idle rows (review #12).
    pub last_message: String,
    /// when the current phase was ENTERED (wall-clock ms, mutation-stamped) —
    /// age chips + oldest-first NEEDS-YOU sorting (#4).
    pub phase_since_ms: u64,
    /// chip-grade trouble (rate limit / API error in the live tail), if any.
    pub trouble: Option<crate::session::Trouble>,
    /// claude's usage-limit footer lifted off the live grid, if present —
    /// "used 92% · resets 4:30pm" / "limit hit · resets 4:30pm" (docs/019).
    pub usage_limit: Option<crate::session::UsageLimit>,
}

pub struct SessionHost {
    next_id: AtomicU64,
    sessions: Mutex<BTreeMap<u64, Arc<HostedSession>>>,
    /// hook ingress (lazily started on the first Claude spawn). `OnceLock` so a
    /// host with no agent sessions never binds a socket.
    ingress: OnceLock<Arc<HookIngress>>,
    /// back-reference so the ingress accept thread can route to sessions.
    self_ref: Mutex<Weak<SessionHost>>,
    /// cli ids with a resume IN FLIGHT — closes guard_not_live's check-then-act
    /// window (two clients racing a resume both saw "not live" while the slow
    /// pty fork ran; adversarial review). Entries live from guard to insert.
    resuming: Mutex<std::collections::HashSet<String>>,
    /// wall-clock ms of the last codex rate-limit poll — self-throttles
    /// `poll_codex_limits` to ~10s so the 1s daemon sweep doesn't thrash disk
    /// (codex only rewrites `rate_limits` per `token_count`). 0 = never polled.
    last_codex_poll_ms: AtomicU64,
    /// the global auto-continue-on-limit-reset flag. The daemon is STORAGE-FREE,
    /// so the GUI (which owns the store) pushes this over the wire and re-pushes
    /// on attach; cached here for the later reset scheduler to read. Default OFF.
    auto_continue: AtomicBool,
    /// config (default OFF): fire auto-continue on the resolved reset INSTANT
    /// rather than the (unreachable-when-idle) banner-cleared edge. Pushed with
    /// the master flag; read by the sweep in `auto_continue_step`.
    ac_fire_on_reset: AtomicBool,
}

/// The argv for a claude spawn (`--session-id <new>`) or resume (`--resume
/// <id>`), in the one order that survives claude's parser.
///
/// `caller` is whatever the GUI staged on `spec.args` — today a profile's
/// `--model` and its `extra_args` (gui/spawn.rs `apply_profile_argv`). These
/// used to be DROPPED: both claude legs assigned `spec.args = vec![…]`, so
/// every flag the caller staged vanished before the process started and the
/// model only landed because it has an `ANTHROPIC_MODEL` twin. Extra args had
/// no surviving channel at all.
///
/// Caller args ride BETWEEN the host's own flags and `--add-dir` because
/// `--add-dir` is VARIADIC: anything appended after its value that doesn't
/// start with `-` is swallowed as another directory (same parser quirk that ate
/// the dispatch prompt, measured on claude 2.1.201). Keeping it last, adjacent
/// to its own value, means no caller arg can be mistaken for a directory.
/// `prompt` (dispatch delivery, docs/011 WIRE) rides last behind `--`, which
/// ends option parsing outright — the only way a bare positional survives.
fn claude_args(
    handle_flag: &str,
    handle: &str,
    settings: &Path,
    cwd: &Path,
    caller: &[String],
    prompt: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        handle_flag.to_string(),
        handle.to_string(),
        "--settings".to_string(),
        settings.to_string_lossy().into_owned(),
    ];
    args.extend(caller.iter().cloned());
    args.push("--add-dir".to_string());
    args.push(cwd.to_string_lossy().into_owned());
    if let Some(p) = prompt.filter(|p| !p.is_empty()) {
        args.push("--".to_string());
        args.push(p.to_string());
    }
    args
}

/// The argv for `codex resume` — caller args ride BEFORE the subcommand,
/// because codex's global flags (`-c`, `-m`, …) are only parsed there; anything
/// after `resume` belongs to the subcommand. Same drop bug as the claude legs:
/// this used to assign over `spec.args`.
fn codex_resume_args(session_id: &str, caller: &[String]) -> Vec<String> {
    let mut args = vec![
        "-c".to_string(),
        "check_for_update_on_startup=false".to_string(),
    ];
    args.extend(caller.iter().cloned());
    args.push("resume".to_string());
    args.push(session_id.to_string());
    args
}

impl SessionHost {
    pub fn new() -> Arc<Self> {
        let host = Arc::new(SessionHost {
            next_id: AtomicU64::new(1),
            sessions: Mutex::new(BTreeMap::new()),
            ingress: OnceLock::new(),
            self_ref: Mutex::new(Weak::new()),
            resuming: Mutex::new(std::collections::HashSet::new()),
            last_codex_poll_ms: AtomicU64::new(0),
            auto_continue: AtomicBool::new(false),
            ac_fire_on_reset: AtomicBool::new(false),
        });
        *host.self_ref.lock().unwrap() = Arc::downgrade(&host);
        host
    }

    /// Ensure the hook ingress socket is up; route messages to sessions.
    fn ingress(&self) -> std::io::Result<&Arc<HookIngress>> {
        if let Some(i) = self.ingress.get() {
            return Ok(i);
        }
        let weak = self.self_ref.lock().unwrap().clone();
        let ing = HookIngress::start(move |msg: HookMessage| {
            if let Some(host) = weak.upgrade() {
                if let Some(s) = host.get(msg.session) {
                    s.on_hook(msg.event);
                }
            }
        })?;
        // first writer wins; a racing starter's socket drops harmlessly.
        let _ = self.ingress.set(ing);
        Ok(self.ingress.get().unwrap())
    }

    /// Spawn an interactive `claude` bound to `project_slug`, with hooks armed
    /// via a per-session `--settings` file so its permission prompts surface as
    /// decision cards (docs/014). cwd/rows/cols come from `spec`.
    /// Spawn a fresh claude. We MINT the session id (`--session-id`) so the
    /// orchestrator owns the resume handle at spawn time — returned alongside
    /// the local `SessionId` so the caller can record it for crash recovery
    /// (spike: the minted uuid becomes the transcript filename).
    pub fn spawn_claude(
        &self,
        project_slug: impl Into<String>,
        mut spec: SpawnSpec,
    ) -> std::io::Result<(SessionId, String)> {
        let id = SessionId(self.next_id.fetch_add(1, Ordering::SeqCst));
        let settings = self.ingress()?.write_session_settings(id, &spec.effort)?;
        let cli_id = crate::uuidv4::new();
        spec.program = "claude".into();
        // NO --permission-mode: claude inherits the user's GLOBAL default mode,
        // so a hosted session prompts exactly like their terminal (no extra
        // interruptions). Decision cards still surface for whatever that mode
        // genuinely gates (bash/network/non-allowlisted) via the PermissionRequest
        // hook. (Was hardcoded `default` to force cards while building M3.)
        let caller = std::mem::take(&mut spec.args);
        spec.args = claude_args(
            "--session-id",
            &cli_id,
            &settings,
            &spec.cwd,
            &caller,
            // Dispatch delivery (docs/011 WIRE): the prompt rides as the FINAL
            // positional argv element.
            Some(&spec.initial_prompt),
        );
        let session = HostedSession::spawn(
            id,
            CliKind::Claude,
            project_slug.into(),
            Some(cli_id.clone()),
            spec,
        )?;
        self.sessions.lock().unwrap().insert(id.0, session);
        Ok((id, cli_id))
    }

    /// RESUME an existing claude session by id — the import/recovery path. The
    /// transcript already exists on disk; this re-spawns claude --resume <id> in
    /// the session's RECORDED cwd (spike-confirmed: resume is cwd-scoped — wrong
    /// cwd is a hard failure — and drops straight at the composer with replayed
    /// history, no picker). Hooks are armed like a fresh spawn so decisions
    /// still surface. `spec.cwd` MUST be the session's recorded cwd.
    pub fn resume_claude(
        &self,
        project_slug: impl Into<String>,
        session_id: &str,
        mut spec: SpawnSpec,
    ) -> std::io::Result<SessionId> {
        self.guard_not_live(session_id)?;
        let out = (|| {
            let id = SessionId(self.next_id.fetch_add(1, Ordering::SeqCst));
            let settings = self.ingress()?.write_session_settings(id, &spec.effort)?;
            spec.program = "claude".into();
            let caller = std::mem::take(&mut spec.args);
            // deliberately IGNORES spec.initial_prompt — resume replays history.
            spec.args = claude_args("--resume", session_id, &settings, &spec.cwd, &caller, None);
            // (no --permission-mode — inherit the user's global mode, see spawn_claude)
            let session = HostedSession::spawn(
                id,
                CliKind::Claude,
                project_slug.into(),
                Some(session_id.to_string()),
                spec,
            )?;
            self.sessions.lock().unwrap().insert(id.0, session);
            Ok(id)
        })();
        self.unguard(session_id);
        out
    }

    /// RESUME an existing codex session by id, in its recorded cwd (spike: codex
    /// resume <id> re-renders + appends to the same rollout in place). Uses the
    /// user's real ~/.codex (where the rollout lives) — no custom CODEX_HOME.
    /// Guards: disable the update modal; omit -m so the account-default model is
    /// used (a forced -m 400s on a ChatGPT plan).
    pub fn resume_codex(
        &self,
        project_slug: impl Into<String>,
        session_id: &str,
        mut spec: SpawnSpec,
    ) -> std::io::Result<SessionId> {
        self.guard_not_live(session_id)?;
        let out = (|| {
            let id = SessionId(self.next_id.fetch_add(1, Ordering::SeqCst));
            spec.program = "codex".into();
            let caller = std::mem::take(&mut spec.args);
            spec.args = codex_resume_args(session_id, &caller);
            let session = HostedSession::spawn(
                id,
                CliKind::Codex,
                project_slug.into(),
                Some(session_id.to_string()),
                spec,
            )?;
            self.sessions.lock().unwrap().insert(id.0, session);
            Ok(id)
        })();
        self.unguard(session_id);
        out
    }

    /// Move a session to another project (dogfooding #10: work often outgrows
    /// the project it was spawned in — e.g. a repo spun off mid-session).
    pub fn rebind(&self, id: SessionId, project_slug: &str) {
        if let Some(s) = self.get(id) {
            s.rebind_project(project_slug);
        }
    }

    /// Refuse to resume a CLI session that is ALREADY live in this host — a
    /// second resume would fork two PTYs over one conversation and split the
    /// transcript (the worst work-loss mode, review #12). Callers turn this
    /// into a jump-to; the guard is the backstop for every surface.
    fn guard_not_live(&self, cli_session_id: &str) -> std::io::Result<()> {
        let live = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .any(|s| s.is_alive() && s.cli_session_id().as_deref() == Some(cli_session_id));
        // reserve the id for the duration of the (slow) spawn — a concurrent
        // resume for the same id fails here instead of double-forking.
        if live
            || !self
                .resuming
                .lock()
                .unwrap()
                .insert(cli_session_id.to_string())
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "session is already running — jump to it instead of resuming",
            ));
        }
        Ok(())
    }

    /// Release guard_not_live's in-flight reservation (both outcomes).
    fn unguard(&self, cli_session_id: &str) {
        self.resuming.lock().unwrap().remove(cli_session_id);
    }

    /// Search a session's grid (⌘F). Empty when the session is gone.
    pub fn search(&self, id: SessionId, query: &str) -> Vec<crate::emulator::SearchMatch> {
        self.get(id).map(|s| s.search(query)).unwrap_or_default()
    }

    /// Pure view scroll (scroll-to-match) — never PTY input.
    pub fn scroll_view(&self, id: SessionId, delta: i32) {
        if let Some(s) = self.get(id) {
            s.scroll_view(delta);
        }
    }

    /// Clear stale decision cards whose dialog has left the grid (call each
    /// repaint tick). Also the phase/trouble sweep: stamps the time-driven
    /// Busy→Idle decay edge and runs the dirty-gated tail scan (#4). The
    /// daemon's detached sweeper calls this too, so ages stay true with no
    /// client attached.
    pub fn reconcile_pending(&self) {
        for s in self.sessions() {
            s.reconcile_pending();
            s.observe_phase();
            s.scan_trouble();
            s.scan_limit();
        }
    }

    /// Spawn a plain shell in `cwd`, bound to `project_slug`.
    pub fn spawn_shell(
        &self,
        project_slug: impl Into<String>,
        cwd: impl AsRef<Path>,
    ) -> std::io::Result<SessionId> {
        self.spawn(project_slug, CliKind::Shell, SpawnSpec::shell(cwd))
    }

    /// Spawn an interactive CLI (claude/codex) directly — never via a login
    /// shell (docs/013 §1: spawn the CLI with per-session config, not typed in).
    pub fn spawn(
        &self,
        project_slug: impl Into<String>,
        kind: CliKind,
        spec: SpawnSpec,
    ) -> std::io::Result<SessionId> {
        let id = SessionId(self.next_id.fetch_add(1, Ordering::SeqCst));
        let session = HostedSession::spawn(id, kind, project_slug.into(), None, spec)?;
        self.sessions.lock().unwrap().insert(id.0, session);
        Ok(id)
    }

    pub fn get(&self, id: SessionId) -> Option<Arc<HostedSession>> {
        self.sessions.lock().unwrap().get(&id.0).cloned()
    }

    /// All sessions, id order. Cheap — clones `Arc`s.
    pub fn sessions(&self) -> Vec<Arc<HostedSession>> {
        self.sessions.lock().unwrap().values().cloned().collect()
    }

    /// Sessions bound to a project, id order.
    pub fn sessions_for(&self, project_slug: &str) -> Vec<Arc<HostedSession>> {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.project_slug() == project_slug)
            .cloned()
            .collect()
    }

    /// Flat infos for the GUI strip — the serde-plain DTO the GUI reads instead
    /// of holding `Arc<HostedSession>` (so the same surface works over the daemon
    /// socket, docs/018).
    pub fn infos(&self) -> Vec<SessionInfo> {
        self.sessions().iter().map(|s| Self::info_of(s)).collect()
    }

    /// Infos bound to a project, id order (replaces `sessions_for` for the GUI).
    pub fn infos_for(&self, project_slug: &str) -> Vec<SessionInfo> {
        self.sessions()
            .iter()
            .filter(|s| s.project_slug() == project_slug)
            .map(|s| Self::info_of(s))
            .collect()
    }

    fn info_of(s: &Arc<HostedSession>) -> SessionInfo {
        SessionInfo {
            id: s.id,
            kind: s.kind,
            project_slug: s.project_slug(),
            title: s.title(),
            phase: s.phase(),
            alive: s.is_alive(),
            pending: s.pending(),
            dirty: s.dirty(),
            cli_session_id: s.cli_session_id(),
            last_message: s.last_message(),
            phase_since_ms: s.phase_since_ms(),
            trouble: s.trouble(),
            usage_limit: s.usage_limit(),
        }
    }

    /// A render-ready grid snapshot for one session (replaces `get(id).snapshot()`).
    pub fn snapshot(&self, id: SessionId) -> Option<GridSnapshot> {
        self.get(id).map(|s| s.snapshot())
    }

    /// The current blocking decision for one session (replaces `get(id).pending()`).
    pub fn pending(&self, id: SessionId) -> Option<PendingDecision> {
        self.get(id).and_then(|s| s.pending())
    }

    /// The full event timeline for one session (#9) — the GUI / local backend.
    pub fn events_for(&self, id: SessionId) -> Vec<crate::events::SessionEvent> {
        self.get(id).map(|s| s.events()).unwrap_or_default()
    }

    /// Events newer than `after_seq` — the daemon coalescer's delta scan.
    pub fn events_since(&self, id: SessionId, after_seq: u64) -> Vec<crate::events::SessionEvent> {
        self.get(id)
            .map(|s| s.events_since(after_seq))
            .unwrap_or_default()
    }

    /// The session's max event seq — the coalescer's cheap change check.
    pub fn max_event_seq_for(&self, id: SessionId) -> u64 {
        self.get(id).map(|s| s.max_event_seq()).unwrap_or(0)
    }

    /// Backfill a session's timeline from its on-disk transcript (#9 §4). The
    /// per-CLI difference lives HERE, in one place:
    /// - CODEX has no hooks → the transcript is its whole event source.
    /// - CLAUDE is hooks-live → backfill ONLY the pre-attach history (events
    ///   older than this session's start), so live hooks aren't double-counted.
    ///   A fresh claude has no pre-start history, so this is a no-op for it.
    pub fn backfill_transcript(&self, id: SessionId, path: &std::path::Path) {
        let Some(s) = self.get(id) else { return };
        let items = match s.kind {
            CliKind::Codex => {
                let text = match std::fs::read_to_string(path) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("backfill: read failed for {}: {e}", path.display());
                        return;
                    }
                };
                crate::transcript::codex_rollout_events(&text)
            }
            CliKind::Claude => {
                let text = match std::fs::read_to_string(path) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("backfill: read failed for {}: {e}", path.display());
                        return;
                    }
                };
                let cutoff = s.started_at_ms();
                crate::transcript::claude_transcript_events(&text)
                    .into_iter()
                    .filter(|it| it.at_ms < cutoff)
                    .collect()
            }
            CliKind::Shell => Vec::new(),
        };
        s.push_timeline_items(items);
    }

    pub fn send_key(&self, id: SessionId, key: &KeyInput) {
        if let Some(s) = self.get(id) {
            s.send_key(key);
        }
    }

    /// Scroll a session's viewport through scrollback (#9 — wheel up/down).
    pub fn scroll(&self, id: SessionId, delta: i32) {
        if let Some(s) = self.get(id) {
            s.scroll(delta);
        }
    }

    pub fn resize(&self, id: SessionId, rows: u16, cols: u16) {
        if let Some(s) = self.get(id) {
            s.resize(rows, cols);
        }
    }

    /// Associate a CLI resume handle with a live session — used when a fresh
    /// codex's rollout id is discovered after spawn (docs/018 §12), so the
    /// session's `SessionInfo.cli_session_id` is populated for restore reconcile.
    pub fn set_cli_session_id(&self, id: SessionId, cli: String) {
        if let Some(s) = self.get(id) {
            s.set_cli_session_id(cli);
        }
    }

    /// Cache the global auto-continue-on-limit-reset flag pushed from the GUI
    /// (the daemon is storage-free — docs/019 auto-continue). The later reset
    /// scheduler reads it via [`SessionHost::auto_continue`].
    pub fn set_auto_continue(&self, on: bool, fire_on_reset: bool) {
        self.auto_continue.store(on, Ordering::Relaxed);
        self.ac_fire_on_reset.store(fire_on_reset, Ordering::Relaxed);
    }

    /// The cached auto-continue flag (default OFF until the GUI pushes it).
    pub fn auto_continue(&self) -> bool {
        self.auto_continue.load(Ordering::Relaxed)
    }

    /// config (default OFF): whether FIRE keys off the reset instant vs a cleared
    /// banner. Read each sweep tick by `auto_continue_tick`.
    pub fn ac_fire_on_reset(&self) -> bool {
        self.ac_fire_on_reset.load(Ordering::Relaxed)
    }

    /// Self-poll every live Codex session's rollout for its STRUCTURED
    /// `rate_limits` telemetry and mirror it onto the SAME `UsageLimit` claude's
    /// grid scan produces — so the chip + Standup BLOCKED tier (and auto-continue)
    /// work for codex with ZERO render changes (docs/019 codex limit). Runs
    /// HOST-SIDE (not the GUI) so it fires from the daemon's detached 1s sweep
    /// with no client attached: daemon (default), local, and headless all surface
    /// codex limits. Off the claude grid-scan path entirely — `scan_limit` is
    /// gated to claude, so this value is never clobbered on a reconcile tick.
    ///
    /// SELF-THROTTLED to ~10s off `last_codex_poll_ms`: codex only rewrites
    /// `rate_limits` per `token_count`, so polling every 1s tick would just thrash
    /// disk. Reads the rollout's structured telemetry, NOT the terminal grid
    /// (codex's on-hit strings are unconfirmed binary templates).
    pub fn poll_codex_limits(&self) {
        let now = crate::events::now_ms();
        let prev = self.last_codex_poll_ms.load(Ordering::Relaxed);
        if prev != 0 && now.saturating_sub(prev) < 10_000 {
            return;
        }
        self.last_codex_poll_ms.store(now, Ordering::Relaxed);
        let local_off = local_off_secs();
        for s in self.sessions() {
            if s.kind != CliKind::Codex {
                continue;
            }
            let Some(cli) = s.cli_session_id() else {
                continue;
            };
            let Some(path) = orchestrator_core::scan::codex_rollout_path(&cli) else {
                continue;
            };
            let text = crate::transcript::read_rollout_tail(&path, 64 * 1024);
            // F6 GUARD (critical): only STORE on Some. `codex_rate_limits` is None
            // when the rollout carries no usable `rate_limits` window at all, and
            // calling `set_usage_limit(None)` here would CLEAR a real limit hit off
            // absent/empty telemetry — a false "cleared". Never write None from here.
            if let Some(ul) = crate::transcript::codex_rate_limits(&text)
                .and_then(|rl| rl.to_usage_limit(local_off))
            {
                s.set_usage_limit(Some(ul));
            }
        }
    }

    /// Auto-continue-on-limit-reset sweep (docs/019 slice 2): replay each blocked
    /// session's captured held prompt when its usage window resets — ONLY when the
    /// global flag is on. Runs HOST-side from the daemon's 1s sweep next to
    /// `poll_codex_limits`; the per-session gate (`ac_decide`) is self-cheap, so a
    /// per-tick call is fine. Returns IMMEDIATELY when the flag is off, so a
    /// user who never opted in is NEVER typed at. The pure gate lives in
    /// `session::ac_decide`; this just drives it once per session.
    pub fn auto_continue_tick(&self) {
        if !self.auto_continue() {
            return;
        }
        for s in self.sessions() {
            // #9: isolate each session's step. One panicking/poisoned session must
            // not abort the whole 1s sweep and freeze auto-continue for every OTHER
            // session (mirrors the daemon command-dispatch catch_unwind). The step
            // itself uses poison-tolerant locks so a session poisoned by an earlier
            // panic still recovers on the next tick.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                s.auto_continue_step(true, self.ac_fire_on_reset())
            }));
        }
    }

    /// Close a session: kill its process group NOW (deterministic, not waiting
    /// on the last `Arc` to drop), then forget it. Returns whether it existed.
    pub fn close(&self, id: SessionId) -> bool {
        match self.sessions.lock().unwrap().remove(&id.0) {
            Some(s) => {
                s.terminate();
                true
            }
            None => false,
        }
    }

    /// Reap sessions whose process has exited and the user hasn't kept open.
    /// (v1: the GUI decides retention; this is a helper for tests/cleanup.)
    pub fn reap_dead(&self) -> usize {
        let mut g = self.sessions.lock().unwrap();
        let before = g.len();
        g.retain(|_, s| s.is_alive());
        before - g.len()
    }
}

/// Seconds EAST of UTC (the local offset), read once from `date +%z` — std has
/// no tz and the app is macOS-only. Added to a UTC unix instant to render a
/// reset clock in the user's local zone (docs/019 codex limit).
///
/// Public because the GUI renders the SAME wall clock from it (Standup day
/// headers and HH:MM stamps, the map's truth-meter time). It used to hold a
/// byte-identical private copy — a second `OnceLock` over the same `date +%z`
/// that any fix made here (a `%z` parse edge, a DST refresh) would have
/// silently skipped. One implementation, so the two sides cannot disagree.
pub fn local_off_secs() -> i64 {
    static OFF: OnceLock<i64> = OnceLock::new();
    *OFF.get_or_init(|| {
        std::process::Command::new("date")
            .arg("+%z")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| {
                let s = s.trim();
                if s.len() < 5 {
                    return None;
                }
                let sign = if s.starts_with('-') { -1 } else { 1 };
                let h: i64 = s[1..3].parse().ok()?;
                let m: i64 = s[3..5].parse().ok()?;
                Some(sign * (h * 3600 + m * 60))
            })
            .unwrap_or(0)
    })
}

/// The backend surface the GUI depends on — object-safe so the GUI holds
/// `Arc<dyn SessionBackend>` and swaps backend by config with ZERO render
/// changes (docs/018 §3). Implemented in-process by `SessionHost` (Local) and,
/// at the daemon milestone, by a `RemoteHost` client over the socket (Remote).
/// All reads return serde-plain DTOs / never an `Arc<HostedSession>`; in `Remote`
/// they read a local cache fed by the event stream, never a blocking RPC.
pub trait SessionBackend: Send + Sync {
    // reads (latest known state)
    fn infos(&self) -> Vec<SessionInfo>;
    fn infos_for(&self, project_slug: &str) -> Vec<SessionInfo>;
    fn snapshot(&self, id: SessionId) -> Option<GridSnapshot>;
    fn pending(&self, id: SessionId) -> Option<PendingDecision>;
    /// The session's curated event timeline (#9) — the Sessions stream.
    fn events_for(&self, id: SessionId) -> Vec<crate::events::SessionEvent>;
    /// Backfill a session's timeline from its on-disk transcript (#9 §4).
    fn backfill_transcript(&self, id: SessionId, path: &std::path::Path);
    fn reconcile_pending(&self);
    // commands
    fn spawn_claude(
        &self,
        project_slug: String,
        spec: SpawnSpec,
    ) -> std::io::Result<(SessionId, String)>;
    fn resume_claude(
        &self,
        project_slug: String,
        session_id: &str,
        spec: SpawnSpec,
    ) -> std::io::Result<SessionId>;
    fn resume_codex(
        &self,
        project_slug: String,
        session_id: &str,
        spec: SpawnSpec,
    ) -> std::io::Result<SessionId>;
    fn spawn(
        &self,
        project_slug: String,
        kind: CliKind,
        spec: SpawnSpec,
    ) -> std::io::Result<SessionId>;
    fn spawn_shell(
        &self,
        project_slug: String,
        cwd: std::path::PathBuf,
    ) -> std::io::Result<SessionId>;
    fn send_key(&self, id: SessionId, key: &KeyInput);
    /// Scroll a session's viewport through scrollback (#9 — wheel up/down).
    fn scroll(&self, id: SessionId, delta: i32);
    /// Pure VIEW scroll (search jump) — never routed to the PTY.
    fn scroll_view(&self, id: SessionId, delta: i32);
    /// Move a session to another project.
    fn rebind(&self, id: SessionId, project_slug: &str);
    /// Search the session's grid incl. scrollback (⌘F).
    fn search(&self, id: SessionId, query: &str) -> Vec<crate::emulator::SearchMatch>;
    fn resize(&self, id: SessionId, rows: u16, cols: u16);
    fn close(&self, id: SessionId) -> bool;
    /// Tell the backend a fresh codex's discovered rollout id (docs/018 §12).
    fn set_cli_session_id(&self, id: SessionId, cli: String);
    /// Poll live Codex sessions' rollouts for their STRUCTURED `rate_limits`
    /// telemetry and store any hit onto the session (docs/019 codex limit).
    /// DEFAULTED to a no-op: `RemoteHost` (the daemon client) inherits the no-op
    /// because the DAEMON's own in-process `SessionHost` self-polls this in its 1s
    /// sweep — so the GUI's `tick_needs` calling it through the trait is a no-op in
    /// daemon (default) mode and the REAL poll in local (in-process `SessionHost`)
    /// mode. Mirrors the `reconcile_pending` split: the daemon calls the concrete
    /// `SessionHost`; the GUI reaches the backend through the trait.
    fn poll_codex_limits(&self) {}
    /// Push the global auto-continue-on-limit-reset flag (docs/019). DEFAULTED to
    /// a no-op so only the two backends that need it act: `RemoteHost` fires it
    /// over the wire; the in-process `SessionHost` caches it. The daemon is
    /// storage-free, so the GUI is the source of truth and re-pushes on attach.
    fn set_auto_continue(&self, _on: bool, _fire_on_reset: bool) {}
    /// Configure the mobile bridge and report what it did (docs/020 mobile).
    /// DEFAULTED to `unavailable` — and `SessionHost` deliberately does NOT
    /// override it. The bridge is an ordinary daemon CLIENT: it already depends on
    /// this crate for the protocol types, so hosting it inside `SessionHost` would
    /// mean orchestrator-host → bridge → orchestrator-host, a crate cycle rustc
    /// rejects outright. Only the DAEMON can own the bridge; `RemoteHost` forwards
    /// there over the socket, and a GUI running in-process (local mode) honestly
    /// answers "no bridge here" rather than pretending.
    fn set_bridge(&self, _on: bool, _port: u16, _bind: &str, _token: &str) -> BridgeStatus {
        BridgeStatus::unavailable("no daemon: the bridge is hosted by the daemon, not in-process")
    }
    /// Poll the bridge. DEFAULTED for the same reason as
    /// [`SessionBackend::set_bridge`] — an in-process backend has none to poll.
    fn bridge_status(&self) -> BridgeStatus {
        BridgeStatus::unavailable("no daemon: the bridge is hosted by the daemon, not in-process")
    }
}

/// Local (in-process) backend — forwards to the inherent `SessionHost` verbs.
/// (`Type::method(self)` paths resolve to the inherent method, not the trait, so
/// there is no recursion.)
impl SessionBackend for SessionHost {
    fn infos(&self) -> Vec<SessionInfo> {
        SessionHost::infos(self)
    }
    fn infos_for(&self, project_slug: &str) -> Vec<SessionInfo> {
        SessionHost::infos_for(self, project_slug)
    }
    fn snapshot(&self, id: SessionId) -> Option<GridSnapshot> {
        SessionHost::snapshot(self, id)
    }
    fn pending(&self, id: SessionId) -> Option<PendingDecision> {
        SessionHost::pending(self, id)
    }
    fn events_for(&self, id: SessionId) -> Vec<crate::events::SessionEvent> {
        SessionHost::events_for(self, id)
    }
    fn backfill_transcript(&self, id: SessionId, path: &std::path::Path) {
        SessionHost::backfill_transcript(self, id, path)
    }
    fn reconcile_pending(&self) {
        SessionHost::reconcile_pending(self)
    }
    fn spawn_claude(
        &self,
        project_slug: String,
        spec: SpawnSpec,
    ) -> std::io::Result<(SessionId, String)> {
        SessionHost::spawn_claude(self, project_slug, spec)
    }
    fn resume_claude(
        &self,
        project_slug: String,
        session_id: &str,
        spec: SpawnSpec,
    ) -> std::io::Result<SessionId> {
        SessionHost::resume_claude(self, project_slug, session_id, spec)
    }
    fn resume_codex(
        &self,
        project_slug: String,
        session_id: &str,
        spec: SpawnSpec,
    ) -> std::io::Result<SessionId> {
        SessionHost::resume_codex(self, project_slug, session_id, spec)
    }
    fn spawn(
        &self,
        project_slug: String,
        kind: CliKind,
        spec: SpawnSpec,
    ) -> std::io::Result<SessionId> {
        SessionHost::spawn(self, project_slug, kind, spec)
    }
    fn spawn_shell(
        &self,
        project_slug: String,
        cwd: std::path::PathBuf,
    ) -> std::io::Result<SessionId> {
        SessionHost::spawn_shell(self, project_slug, cwd)
    }
    fn send_key(&self, id: SessionId, key: &KeyInput) {
        SessionHost::send_key(self, id, key)
    }
    fn scroll(&self, id: SessionId, delta: i32) {
        SessionHost::scroll(self, id, delta)
    }
    fn scroll_view(&self, id: SessionId, delta: i32) {
        SessionHost::scroll_view(self, id, delta)
    }
    fn rebind(&self, id: SessionId, project_slug: &str) {
        SessionHost::rebind(self, id, project_slug)
    }
    fn search(&self, id: SessionId, query: &str) -> Vec<crate::emulator::SearchMatch> {
        SessionHost::search(self, id, query)
    }
    fn resize(&self, id: SessionId, rows: u16, cols: u16) {
        SessionHost::resize(self, id, rows, cols)
    }
    fn close(&self, id: SessionId) -> bool {
        SessionHost::close(self, id)
    }
    fn set_cli_session_id(&self, id: SessionId, cli: String) {
        SessionHost::set_cli_session_id(self, id, cli)
    }
    fn poll_codex_limits(&self) {
        SessionHost::poll_codex_limits(self)
    }
    fn set_auto_continue(&self, on: bool, fire_on_reset: bool) {
        SessionHost::set_auto_continue(self, on, fire_on_reset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

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

    // ── argv construction (pure; the spawn legs themselves need a real CLI) ──
    // These assert on what the HOST builds, not on the SpawnSpec the GUI hands
    // it: the caller's args used to be assigned over here, so a GUI-side test
    // could pass while the flag never reached the process.

    #[test]
    fn claude_argv_keeps_caller_args_and_pins_add_dir_last() {
        let args = claude_args(
            "--session-id",
            "sess-1",
            Path::new("/s.json"),
            Path::new("/repo"),
            &["--model".to_string(), "opus".to_string(), "-v".to_string()],
            None,
        );
        assert_eq!(
            args,
            vec![
                "--session-id",
                "sess-1",
                "--settings",
                "/s.json",
                "--model",
                "opus",
                "-v",
                "--add-dir",
                "/repo"
            ],
            "caller args must survive, and --add-dir must stay next to its value"
        );
    }

    #[test]
    fn claude_argv_puts_the_dispatch_prompt_last_behind_a_double_dash() {
        let args = claude_args(
            "--session-id",
            "sess-1",
            Path::new("/s.json"),
            Path::new("/repo"),
            &["--model".to_string(), "opus".to_string()],
            Some("go build it"),
        );
        assert_eq!(
            &args[args.len() - 4..],
            &["--add-dir", "/repo", "--", "go build it"],
            "--add-dir is variadic: only `--` keeps it from eating the prompt"
        );
        // an empty prompt is "spawn at the composer" — never a bare `--`.
        let plain = claude_args(
            "--session-id",
            "sess-1",
            Path::new("/s.json"),
            Path::new("/repo"),
            &[],
            Some(""),
        );
        assert!(!plain.iter().any(|a| a == "--"));
    }

    #[test]
    fn claude_resume_argv_carries_the_handle_and_caller_args() {
        let args = claude_args(
            "--resume",
            "abc",
            Path::new("/s.json"),
            Path::new("/repo"),
            &["--model".to_string(), "opus".to_string()],
            None,
        );
        assert_eq!(&args[..2], &["--resume", "abc"]);
        // the model must survive the resume leg too — the asymmetry that made a
        // session change model between spawn and resume.
        assert!(args.windows(2).any(|w| w == ["--model", "opus"]));
    }

    #[test]
    fn codex_resume_argv_keeps_caller_args_before_the_subcommand() {
        let args = codex_resume_args("roll-1", &["-c".to_string(), "x=1".to_string()]);
        assert_eq!(
            args,
            vec![
                "-c",
                "check_for_update_on_startup=false",
                "-c",
                "x=1",
                "resume",
                "roll-1"
            ],
            "codex parses global flags only BEFORE the subcommand"
        );
        // the update-modal guard is never lost, even with no caller args.
        let plain = codex_resume_args("roll-1", &[]);
        assert_eq!(plain, vec!["-c", "check_for_update_on_startup=false", "resume", "roll-1"]);
    }

    #[test]
    fn spawn_list_and_route_keys_per_project() {
        let host = SessionHost::new();
        let a = host
            .spawn(
                "alpha",
                CliKind::Shell,
                SpawnSpec::program("cat", std::env::temp_dir()),
            )
            .unwrap();
        let _b = host.spawn_shell("beta", std::env::temp_dir()).unwrap();

        assert_eq!(host.sessions().len(), 2);
        assert_eq!(host.sessions_for("alpha").len(), 1);

        host.send_key(a, &KeyInput::Char("route-test".into()));
        host.send_key(a, &KeyInput::Enter);
        let sa = host.get(a).unwrap();
        assert!(
            wait_until(|| sa.snapshot().contains("route-test"), 2000),
            "key did not route to the right session"
        );
    }

    #[test]
    fn infos_carry_the_dto_fields() {
        let host = SessionHost::new();
        // a cat session that echoes — produces output so `dirty` advances.
        let a = host
            .spawn(
                "alpha",
                CliKind::Shell,
                SpawnSpec::program("cat", std::env::temp_dir()),
            )
            .unwrap();
        let _b = host.spawn_shell("beta", std::env::temp_dir()).unwrap();
        host.send_key(a, &KeyInput::Char("hi".into()));
        host.send_key(a, &KeyInput::Enter);

        // infos_for replaces sessions_for and carries the flat DTO.
        assert_eq!(host.infos_for("alpha").len(), 1);
        let info = host.infos().into_iter().find(|i| i.id == a).unwrap();
        assert_eq!(info.project_slug, "alpha");
        assert_eq!(info.pending, None); // a shell has no decision
        assert_eq!(info.cli_session_id, None); // shell: no resume handle
        assert!(
            wait_until(
                || host.infos().iter().find(|i| i.id == a).unwrap().dirty > 0,
                2000
            ),
            "dirty never advanced"
        );
        // the DTO snapshot/pending verbs replace get(id).snapshot()/.pending().
        assert!(host.snapshot(a).is_some());
        assert!(host.pending(a).is_none());
    }

    #[test]
    fn set_cli_session_id_populates_infos() {
        // a fresh codex spawns with cli_session_id=None; once the GUI discovers
        // the rollout id it sets it, so restore-reconcile recognizes it live (#1).
        let host = SessionHost::new();
        let id = host
            .spawn(
                "p",
                CliKind::Shell,
                SpawnSpec::program("cat", std::env::temp_dir()),
            )
            .unwrap();
        assert_eq!(host.infos()[0].cli_session_id, None);
        host.set_cli_session_id(id, "rollout-discovered".into());
        assert_eq!(
            host.infos()[0].cli_session_id.as_deref(),
            Some("rollout-discovered")
        );
    }

    #[test]
    fn hook_events_land_on_the_session_timeline() {
        use crate::events::SessionEventKind;
        let host = SessionHost::new();
        let id = host
            .spawn(
                "p",
                CliKind::Shell,
                SpawnSpec::program("cat", std::env::temp_dir()),
            )
            .unwrap();
        // a fresh session opens with the synthesized Started event (#9).
        assert!(matches!(
            host.events_for(id).first().map(|e| &e.kind),
            Some(SessionEventKind::Started)
        ));
        // feed it a PreToolUse hook → a Tool event appends to the timeline.
        let s = host.get(id).unwrap();
        s.on_hook(crate::hooks::parse_hook_payload(r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}"#).unwrap());
        let evs = host.events_for(id);
        assert!(matches!(
            evs.last().map(|e| &e.kind),
            Some(SessionEventKind::Tool { .. })
        ));
        // events_since respects the per-session cursor (the daemon delta scan).
        let cursor = evs[evs.len() - 2].seq;
        assert_eq!(host.events_since(id, cursor).len(), 1);
    }

    #[test]
    fn max_event_seq_is_a_cheap_change_check() {
        let host = SessionHost::new();
        let id = host
            .spawn(
                "p",
                CliKind::Shell,
                SpawnSpec::program("cat", std::env::temp_dir()),
            )
            .unwrap();
        let s0 = host.max_event_seq_for(id); // the synthesized Started event
        assert!(s0 >= 1);
        // a quiet session: max is unchanged → the coalescer skips events_since.
        assert_eq!(host.max_event_seq_for(id), s0);
        assert!(host.events_since(id, s0).is_empty());
        // a hook bumps it by exactly one and surfaces exactly one delta.
        host.get(id).unwrap().on_hook(crate::hooks::parse_hook_payload(r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}"#).unwrap());
        assert_eq!(host.max_event_seq_for(id), s0 + 1);
        assert_eq!(host.events_since(id, s0).len(), 1);
    }

    #[test]
    fn backfill_codex_parses_a_rollout_into_the_timeline() {
        use crate::events::{SessionEventKind, ToolVerb};
        let dir = std::env::temp_dir().join(format!("orch-roll-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("rollout.jsonl");
        // a real-shaped codex rollout (session_meta + exec_command).
        std::fs::write(&path, r#"{"timestamp":"2026-05-19T18:11:00.000Z","type":"session_meta","payload":{}}
{"timestamp":"2026-05-19T18:11:32.424Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"npm install\"}","call_id":"c1"}}"#).unwrap();
        let host = SessionHost::new();
        let id = host
            .spawn(
                "p",
                CliKind::Codex,
                SpawnSpec::program("cat", std::env::temp_dir()),
            )
            .unwrap();
        host.backfill_transcript(id, &path);
        // the rollout's exec_command landed on the session timeline as a Ran row.
        assert!(host.events_for(id).iter().any(|e| e.kind
            == SessionEventKind::Tool {
                verb: ToolVerb::Ran,
                target: "npm install".into()
            }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_backfill_keeps_only_pre_attach_history() {
        use crate::events::SessionEventKind;
        let dir = std::env::temp_dir().join(format!("orch-cltx-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("claude.jsonl");
        // one tool_use in the distant PAST (pre-attach) + one in the FUTURE (which
        // a live session's hooks would own). started_at ≈ now sits between them.
        std::fs::write(&path, r#"{"type":"assistant","timestamp":"2020-01-01T00:00:00.000Z","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"old-cmd"}}]}}
{"type":"assistant","timestamp":"2099-01-01T00:00:00.000Z","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"future-cmd"}}]}}"#).unwrap();
        let host = SessionHost::new();
        let id = host
            .spawn(
                "p",
                CliKind::Claude,
                SpawnSpec::program("cat", std::env::temp_dir()),
            )
            .unwrap();
        host.backfill_transcript(id, &path);
        let cmds: Vec<String> = host
            .events_for(id)
            .iter()
            .filter_map(|e| match &e.kind {
                SessionEventKind::Tool { target, .. } => Some(target.clone()),
                _ => None,
            })
            .collect();
        assert!(
            cmds.iter().any(|c| c == "old-cmd"),
            "pre-attach history is backfilled"
        );
        assert!(
            !cmds.iter().any(|c| c == "future-cmd"),
            "post-attach events are hook territory, never backfilled"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cli_session_id_is_stored_on_the_session() {
        // HostedSession records its resume handle (claude/codex id) — surfaced in
        // SessionInfo for crash-recovery reconcile (docs/018 §12). A shell is None.
        use crate::session::HostedSession;
        let s = HostedSession::spawn(
            SessionId(1),
            CliKind::Codex,
            "p".into(),
            Some("rollout-uuid-abc".into()),
            SpawnSpec::program("cat", std::env::temp_dir()),
        )
        .unwrap();
        assert_eq!(s.cli_session_id().as_deref(), Some("rollout-uuid-abc"));
    }

    #[test]
    fn ids_are_unique_and_monotonic() {
        let host = SessionHost::new();
        let a = host.spawn_shell("p", std::env::temp_dir()).unwrap();
        let b = host.spawn_shell("p", std::env::temp_dir()).unwrap();
        assert!(b.0 > a.0);
    }

    #[test]
    fn close_removes_and_kills() {
        let host = SessionHost::new();
        let id = host
            .spawn(
                "p",
                CliKind::Shell,
                SpawnSpec::program("sleep", std::env::temp_dir()).arg("30"),
            )
            .unwrap();
        assert!(host.close(id));
        assert!(!host.close(id)); // gone
        assert!(host.get(id).is_none());
    }

    #[test]
    fn reap_dead_collects_exited() {
        let host = SessionHost::new();
        host.spawn(
            "p",
            CliKind::Shell,
            SpawnSpec::program("true", std::env::temp_dir()),
        )
        .unwrap();
        assert!(
            wait_until(|| host.reap_dead() == 1, 2000),
            "dead session not reaped"
        );
    }
}
