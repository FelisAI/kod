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
    /// wall-clock ms of the last transcript limit poll — self-throttles
    /// `poll_transcript_limits` to ~10s so the 1s daemon sweep doesn't thrash
    /// disk (both CLIs write their limit record per turn). 0 = never polled.
    last_limit_poll_ms: AtomicU64,
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
    effort: &str,
    prompt: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        handle_flag.to_string(),
        handle.to_string(),
        "--settings".to_string(),
        settings.to_string_lossy().into_owned(),
    ];
    args.extend(effort_cli_args(effort));
    args.extend(caller.iter().cloned());
    args.push("--add-dir".to_string());
    args.push(cwd.to_string_lossy().into_owned());
    if let Some(p) = prompt.filter(|p| !p.is_empty()) {
        args.push("--".to_string());
        args.push(p.to_string());
    }
    args
}

/// The effort levels the per-session settings file cannot carry.
///
/// `effortLevel` in settings.json is allowlisted to low/medium/high/xhigh (see
/// `Ingress::effort_fragment`). "max" is documented only on the CLI —
/// `--effort <level>  Effort level for the current session (low, medium, high,
/// xhigh, max)`, read straight off 2.1.261's own `--help` — so it rides argv.
///
/// Deliberately NOT moving the other four onto this flag as well. The flag is
/// documented for all five, but the settings-file path is the one currently
/// shipping and working; swapping four working levels onto a different
/// mechanism to gain a fifth trades a known-good for an untested one. One
/// verified route each.
///
/// Placed BEFORE the caller's own args so an explicit `--effort` from a profile
/// still wins.
fn effort_cli_args(effort: &str) -> Vec<String> {
    if effort == "max" {
        vec!["--effort".to_string(), "max".to_string()]
    } else {
        Vec::new()
    }
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
            last_limit_poll_ms: AtomicU64::new(0),
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
            &spec.effort,
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
            spec.args = claude_args(
                "--resume",
                session_id,
                &settings,
                &spec.cwd,
                &caller,
                &spec.effort,
                None,
            );
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
    /// resume <id> re-renders + appends to the same rollout in place). Runs under
    /// whatever account `spec.env` carries (the caller layers the row's profile
    /// on), which is the account the rollout was written in.
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

    /// Read every session's usage limit out of its OWN transcript, for both
    /// CLIs, and store it (docs/019). Never the terminal: a screen cannot tell
    /// Claude's "You've hit your session limit" banner from the same words in a
    /// message — the old claude grid scan put a false ⛔ on sessions that were
    /// only TALKING about limits (2026-09-14). Each CLI writes its limit as a
    /// structured record instead: claude a `rate_limit` refusal
    /// ([`crate::transcript::claude_limit_record`]), codex `rate_limits`
    /// telemetry. Found through the session's account (`transcript_path`), so a
    /// profiled session is read in its own config dir.
    ///
    /// Runs HOST-SIDE so the daemon's detached sweep surfaces limits with no
    /// client attached. SELF-THROTTLED to ~10s: both records change per turn,
    /// and polling every 1s tick would just thrash disk.
    pub fn poll_transcript_limits(&self) {
        let now = crate::events::now_ms();
        let prev = self.last_limit_poll_ms.load(Ordering::Relaxed);
        if prev != 0 && now.saturating_sub(prev) < 10_000 {
            return;
        }
        self.last_limit_poll_ms.store(now, Ordering::Relaxed);
        let local_off = local_off_secs();
        for s in self.sessions() {
            // a shell has no transcript; a fresh codex has no id yet.
            let Some(path) = s.transcript_path() else {
                continue;
            };
            let text = crate::transcript::read_rollout_tail(&path, LIMIT_TAIL_BYTES);
            match s.kind {
                // F6 GUARD (critical): codex telemetry only ever STORES. A rollout
                // with no usable window is absence, not a clear — writing None
                // here would drop a real hit off empty telemetry.
                CliKind::Codex => {
                    if let Some(ul) = crate::transcript::codex_rate_limits(&text)
                        .and_then(|rl| rl.to_usage_limit(local_off))
                    {
                        s.set_usage_limit(Some(ul));
                    }
                }
                // Claude's record decides BOTH ways: a refusal blocks, a real
                // response after it clears. A tail with neither decides nothing
                // and leaves the stored limit exactly as it is.
                CliKind::Claude => {
                    if let Some(rec) = crate::transcript::claude_limit_record(&text) {
                        s.set_usage_limit(rec.to_usage_limit(local_off));
                    }
                }
                CliKind::Shell => {}
            }
        }
    }

    /// Auto-continue-on-limit-reset sweep (docs/019 slice 2): replay each blocked
    /// session's captured held prompt when its usage window resets — ONLY when the
    /// global flag is on. Runs HOST-side from the daemon's 1s sweep next to
    /// `poll_transcript_limits`; the per-session gate (`ac_decide`) is self-cheap, so a
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

/// How much of a transcript's END the limit poller reads. A limit record is
/// the newest line when it matters, but a claude turn can end in a large tool
/// result; 256 KiB keeps the deciding record in view without slurping a
/// many-MB transcript every ~10s.
const LIMIT_TAIL_BYTES: u64 = 256 * 1024;

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
    /// The process hosting the sessions is gone (a daemon that exited or
    /// crashed): every session it held died with it, and none of them was
    /// watched exiting. In-process hosting can't outlive itself, so `false`.
    fn daemon_lost(&self) -> bool {
        false
    }
    /// Attached to a daemon running an OLDER build than the one on disk — it
    /// held live sessions, so it accepted instead of retiring.
    fn daemon_build_stale(&self) -> bool {
        false
    }
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
    /// Read every live session's usage limit from its own transcript
    /// (`SessionHost::poll_transcript_limits`).
    /// DEFAULTED to a no-op: `RemoteHost` (the daemon client) inherits the no-op
    /// because the DAEMON's own in-process `SessionHost` self-polls this in its 1s
    /// sweep — so the GUI's `tick_needs` calling it through the trait is a no-op in
    /// daemon (default) mode and the REAL poll in local (in-process `SessionHost`)
    /// mode. Mirrors the `reconcile_pending` split: the daemon calls the concrete
    /// `SessionHost`; the GUI reaches the backend through the trait.
    fn poll_transcript_limits(&self) {}
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
    fn poll_transcript_limits(&self) {
        SessionHost::poll_transcript_limits(self)
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

    /// 2026-09-14: a codex session under a PROFILE must get limit detection —
    /// and therefore auto-continue — exactly like an ambient one. The daemon
    /// polled `~/.codex` for every session, so a profiled rollout's 100%
    /// telemetry was never read. End to end through the real poller: a session
    /// spawned with `CODEX_HOME=<acct>` whose rollout lives only in `<acct>`.
    #[test]
    fn a_profiled_codex_sessions_limit_is_read_from_its_own_account() {
        let acct = std::env::temp_dir().join(format!(
            "kod-host-acct-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let day = acct.join("sessions/2026/09/14");
        std::fs::create_dir_all(&day).unwrap();
        let rollout_id = "01a0beef-0000-7000-8000-000000000001";
        // resets far in the future so the view accessor cannot expire it.
        std::fs::write(
            day.join(format!("rollout-2026-09-14T10-00-00-{rollout_id}.jsonl")),
            concat!(
                r#"{"timestamp":"2026-09-14T17:00:00.000Z","type":"event_msg","payload":{"type":"token_count","#,
                r#""rate_limits":{"primary":{"used_percent":100.0,"window_minutes":300,"resets_at":4102444800}}}}"#,
                "\n"
            ),
        )
        .unwrap();

        let host = SessionHost::new();
        let mut spec = SpawnSpec::program("cat", std::env::temp_dir());
        spec.env.push(("CODEX_HOME".to_string(), acct.to_string_lossy().into_owned()));
        let id = host.spawn("proj", CliKind::Codex, spec).unwrap();
        host.set_cli_session_id(id, rollout_id.to_string());

        let session = host.get(id).unwrap();
        assert_eq!(
            session.home().map(|h| h.root().to_path_buf()),
            Some(acct.clone()),
            "the session must remember the account it was spawned under"
        );

        host.poll_transcript_limits();
        let ul = session.usage_limit().expect("the profiled rollout's telemetry was read");
        assert!(ul.hit, "100% used is a hit: {ul:?}");
        assert_eq!(ul.reset_at_unix, Some(4102444800), "auto-continue's wake target");

        session.terminate();
        let _ = std::fs::remove_dir_all(&acct);
    }

    fn tmp_account(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "kod-host-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 2026-09-14 BUG: a ⛔ on a session that was only TALKING about limits. The
    /// terminal shows Claude's banner words — quoted, pasted, discussed — while
    /// the transcript holds nothing but ordinary conversation. No limit, ever:
    /// the screen is not a source.
    #[test]
    fn banner_words_on_screen_are_never_a_limit() {
        let acct = tmp_account("claude-screen");
        let cli = "5c3e0000-0000-4000-8000-00000000c1a0";
        let proj = acct.join("projects/-fixture");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join(format!("{cli}.jsonl")),
            concat!(
                r#"{"type":"user","timestamp":"2026-09-14T21:00:00.000Z","message":{"role":"user","content":"is this right? You've hit your session limit · resets 7:30pm (America/Los_Angeles)"}}"#, "\n",
                r#"{"type":"assistant","timestamp":"2026-09-14T21:00:05.000Z","message":{"role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"That banner reads /upgrade or /usage-credits to finish what you're working on."}]}}"#, "\n",
            ),
        )
        .unwrap();

        let host = SessionHost::new();
        let mut spec = SpawnSpec::program("printf", std::env::temp_dir()).arg(
            "You've hit your session limit · resets 7:30pm (America/Los_Angeles)\n/upgrade or /usage-credits to finish what you're working on.\n",
        );
        spec.env.push(("CLAUDE_CONFIG_DIR".into(), acct.to_string_lossy().into_owned()));
        let id = host.spawn("proj", CliKind::Claude, spec).unwrap();
        host.set_cli_session_id(id, cli.to_string());
        let session = host.get(id).unwrap();
        assert!(
            wait_until(
                || session.snapshot().rows.iter().any(|r| r.iter().map(|run| run.text.as_str()).collect::<String>().contains("usage-credits")),
                2000
            ),
            "the banner words must actually be on the grid, or this proves nothing"
        );

        host.reconcile_pending();
        host.poll_transcript_limits();
        assert_eq!(session.usage_limit(), None, "banner words on screen are not a limit");
        let _ = std::fs::remove_dir_all(&acct);
    }

    /// A refusal from long ago — the newest record of a session nobody has
    /// touched since — must neither show nor write a note stamped today.
    /// Measured on a real live session: a model cap from July 12, nothing after.
    #[test]
    fn a_long_dead_refusal_is_not_reported_as_a_new_block() {
        let acct = tmp_account("claude-stale");
        let cli = "5c3e0000-0000-4000-8000-0000000057a1";
        let proj = acct.join("projects/-fixture");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join(format!("{cli}.jsonl")),
            concat!(
                r#"{"type":"assistant","timestamp":"2026-07-12T18:41:00.000Z","isApiErrorMessage":true,"error":"rate_limit","message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"You've reached your Fable 5 limit. Run /usage-credits to continue."}]}}"#,
                "\n"
            ),
        )
        .unwrap();
        let host = SessionHost::new();
        let mut spec = SpawnSpec::program("cat", std::env::temp_dir());
        spec.env.push(("CLAUDE_CONFIG_DIR".into(), acct.to_string_lossy().into_owned()));
        let id = host.spawn("proj", CliKind::Claude, spec).unwrap();
        host.set_cli_session_id(id, cli.to_string());
        let session = host.get(id).unwrap();

        host.poll_transcript_limits();
        assert_eq!(session.usage_limit(), None, "aged out in the view");
        assert!(
            session
                .events_since(0)
                .iter()
                .all(|e| !matches!(&e.kind, crate::events::SessionEventKind::Notice { text } if text.starts_with("usage limit hit"))),
            "no note for a block that ended months ago"
        );
        session.terminate();
        let _ = std::fs::remove_dir_all(&acct);
    }

    /// The real thing, through the real poller: Claude's structured refusal in a
    /// PROFILED account blocks with its exact reset, is recorded once, and a
    /// real response afterwards clears it.
    #[test]
    fn a_claude_refusal_blocks_until_a_real_response_clears_it() {
        let acct = tmp_account("claude-refusal");
        let cli = "5c3e0000-0000-4000-8000-00000000beef";
        let proj = acct.join("projects/-fixture");
        std::fs::create_dir_all(&proj).unwrap();
        let transcript = proj.join(format!("{cli}.jsonl"));
        // resets far in the future so the view accessor cannot expire it.
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"assistant","timestamp":"2026-09-14T20:01:26.059Z","isApiErrorMessage":true,"error":"rate_limit","apiErrorStatus":429,"quotaLimits":{"status":"rejected","resetsAt":4102444800,"rateLimitType":"five_hour"},"message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"You've hit your session limit · resets 2pm (America/Los_Angeles)"}]}}"#,
                "\n"
            ),
        )
        .unwrap();

        let host = SessionHost::new();
        let mut spec = SpawnSpec::program("cat", std::env::temp_dir());
        spec.env.push(("CLAUDE_CONFIG_DIR".into(), acct.to_string_lossy().into_owned()));
        let id = host.spawn("proj", CliKind::Claude, spec).unwrap();
        host.set_cli_session_id(id, cli.to_string());
        let session = host.get(id).unwrap();

        host.poll_transcript_limits();
        let ul = session.usage_limit().expect("the refusal record is a limit");
        assert!(ul.hit);
        assert_eq!(ul.reset_at_unix, Some(4102444800), "auto-continue's wake target");
        let notes = |s: &HostedSession| {
            s.events_since(0)
                .into_iter()
                .filter(|e| matches!(&e.kind, crate::events::SessionEventKind::Notice { text } if text.starts_with("usage limit hit")))
                .count()
        };
        assert_eq!(notes(&session), 1, "the block is recorded");

        // re-read next poll: the same block, not a second record.
        host.last_limit_poll_ms.store(0, Ordering::Relaxed);
        host.poll_transcript_limits();
        assert_eq!(notes(&session), 1, "one record per block, not per poll");

        // a real response arrives — the block is over.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&transcript).unwrap();
        writeln!(f, "{}", r#"{"type":"assistant","timestamp":"2026-09-15T02:10:04.000Z","message":{"role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"Resuming."}]}}"#).unwrap();
        host.last_limit_poll_ms.store(0, Ordering::Relaxed);
        host.poll_transcript_limits();
        assert_eq!(session.usage_limit(), None, "a real response clears it");

        session.terminate();
        let _ = std::fs::remove_dir_all(&acct);
    }

    // ── argv construction (pure; the spawn legs themselves need a real CLI) ──
    // These assert on what the HOST builds, not on the SpawnSpec the GUI hands
    // it: the caller's args used to be assigned over here, so a GUI-side test
    // could pass while the flag never reached the process.

    #[test]
    /// "max" is the one level the settings FILE cannot carry, so it has to
    /// reach claude on argv — and it must not disturb anything else.
    #[test]
    fn max_effort_rides_argv_and_the_others_do_not() {
        let build = |eff: &str| {
            claude_args(
                "--session-id",
                "s",
                Path::new("/s.json"),
                Path::new("/repo"),
                &[],
                eff,
                None,
            )
        };
        let max = build("max");
        let i = max.iter().position(|a| a == "--effort").expect("--effort missing");
        assert_eq!(max[i + 1], "max");
        // BEFORE --add-dir, and before any caller args, so a profile's own
        // --effort still wins by coming later.
        assert!(i < max.iter().position(|a| a == "--add-dir").unwrap());

        for eff in ["", "high", "xhigh", "ultracode", "junk"] {
            assert!(
                !build(eff).contains(&"--effort".to_string()),
                "{eff:?} must stay on the settings-file path"
            );
        }
    }

    #[test]
    fn claude_argv_keeps_caller_args_and_pins_add_dir_last() {
        let args = claude_args(
            "--session-id",
            "sess-1",
            Path::new("/s.json"),
            Path::new("/repo"),
            &["--model".to_string(), "opus".to_string(), "-v".to_string()],
            "",
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
            "",
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
            "",
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
            "",
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
