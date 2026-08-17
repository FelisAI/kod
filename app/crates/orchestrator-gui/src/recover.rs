use gpui::prelude::FluentBuilder;
use gpui::*;
use crate::*;

/// The "when" stamp on a recoverable / restorable row. Every timestamp reaching
/// this surface (`scan`, the registry) is unix SECONDS while the shared ladder
/// speaks ms, so this is the ONE place that conversion lives — the three row
/// renderers below each used to carry their own copy of the ladder.
fn rel_secs(then_secs: u64, now_secs: u64) -> String {
    crate::timefmt::ago_label(now_secs.saturating_sub(then_secs).saturating_mul(1_000))
}

impl Orchestrator {

    /// Resolve a recoverable session's HOME project, returning `(slug, cwd)`.
    /// Precedence: manual OVERRIDE > longest-ancestor path-match > Unfiled
    /// (empty slug). The cwd is ALWAYS the session's recorded cwd (resume is
    /// cwd-scoped — the slug only sets project binding, never where we spawn).
    fn resolve_home(
        &self,
        rec: &orchestrator_core::scan::RecoverableSession,
    ) -> (String, std::path::PathBuf) {
        if let Some(slug) = self.overrides.get(&rec.id) {
            return (slug.clone(), rec.cwd.clone());
        }
        let best = self
            .projects
            .iter()
            .filter(|p| p.path.as_ref().is_some_and(|pp| rec.cwd.starts_with(pp)))
            .max_by_key(|p| {
                p.path
                    .as_ref()
                    .map(|pp| pp.components().count())
                    .unwrap_or(0)
            });
        match best {
            Some(p) => (p.slug.clone(), rec.cwd.clone()),
            None => (String::new(), rec.cwd.clone()),
        }
    }

    /// Give a cwd a home project, minting an ad-hoc one if none exists (orphan
    /// import). Idempotent by the FOLDED slug — two cwds under the same project
    /// dir (and re-resuming the same orphan) reuse one project, never duplicate.
    fn ensure_home_for(&mut self, cwd: &std::path::Path) -> String {
        // `adhoc` folds the cwd, so its slug is the dedup key (a raw-path compare
        // would miss when two nested cwds share a folded home).
        let proj = Project::adhoc(cwd.to_path_buf());
        if let Some(p) = self.projects.iter().find(|p| p.slug == proj.slug) {
            return p.slug.clone();
        }
        let slug = proj.slug.clone();
        if let Ok(store) = self.store.lock() {
            let _ = store.ensure_project(&slug, &proj.name);
        }
        self.projects.push(proj);
        slug
    }

    /// Load recoverable sessions from disk on a background thread (the import
    /// feature). The scan stats all candidates but reads only the newest ~120, so
    /// a session that crashed weeks ago is still recoverable (was capped at 7
    /// days); refreshed at launch + on each registry adopt.
    pub(crate) fn load_recoverable(&self, cx: &mut Context<Self>) {
        let slot = self.recoverable.clone();
        std::thread::spawn(move || {
            // ~10y window = effectively all on-disk sessions; the newest 120 valid
            // are read (cost is ~limit, not the whole disk).
            let sessions = orchestrator_core::scan::recoverable_sessions(3650, 120);
            if let Ok(mut g) = slot.lock() {
                *g = sessions;
            }
        });
        // a light poll to pick it up once it's loaded (one-shot)
        let slot = self.recoverable.clone();
        cx.spawn(async move |this, cx| loop {
            Timer::after(std::time::Duration::from_millis(300)).await;
            let loaded = slot.lock().map(|g| !g.is_empty()).unwrap_or(false);
            let alive = this
                .update(cx, |_, cx| {
                    if loaded {
                        cx.notify();
                    }
                })
                .is_ok();
            if loaded || !alive {
                break;
            }
        })
        .detach();
    }

    /// Kick the on-demand one-line summary for a recoverable session (the
    /// user's ask). Runs `claude -p` over a head+tail digest on a background
    /// thread (quota — ONLY on explicit click), cached so a session summarizes
    /// at most once. Idempotent: re-clicking a Running/Done summary is a no-op.
    fn start_summary(
        &mut self,
        rec: &orchestrator_core::scan::RecoverableSession,
        cx: &mut Context<Self>,
    ) {
        let id = rec.id.clone();
        if matches!(
            self.summaries.get(&id),
            Some(SummaryState::Running) | Some(SummaryState::Done(_))
        ) {
            return;
        }
        self.summaries.insert(id.clone(), SummaryState::Running);
        let sink = self.summary_sink.clone();
        let (path, is_codex, idc) = (rec.path.clone(), rec.is_codex, id.clone());
        std::thread::spawn(move || {
            let r = extract::summarize_session(&path, is_codex);
            if let Ok(mut g) = sink.lock() {
                g.push((idc, r));
            }
        });
        // poll the sink until this summary resolves (mirrors load_recoverable).
        // Bounded (~30s) so a worker that panics/hangs before writing the sink
        // can't spin the poll forever — it flips to a retryable Failed instead.
        let sink = self.summary_sink.clone();
        cx.spawn(async move |this, cx| {
            for tick in 0.. {
                Timer::after(std::time::Duration::from_millis(300)).await;
                let drained: Vec<_> = sink
                    .lock()
                    .map(|mut g| g.drain(..).collect())
                    .unwrap_or_default();
                let timed_out = tick >= 100; // ~30s
                let stop = this
                    .update(cx, |o, cx| {
                        for (sid, r) in drained {
                            o.summaries.insert(
                                sid,
                                match r {
                                    Ok(s) => SummaryState::Done(s),
                                    Err(e) => SummaryState::Failed(e),
                                },
                            );
                        }
                        if timed_out && matches!(o.summaries.get(&id), Some(SummaryState::Running))
                        {
                            o.summaries
                                .insert(id.clone(), SummaryState::Failed("timed out".into()));
                        }
                        cx.notify();
                        matches!(
                            o.summaries.get(&id),
                            Some(SummaryState::Done(_)) | Some(SummaryState::Failed(_))
                        )
                    })
                    .unwrap_or(true); // view gone → stop
                if stop {
                    break;
                }
            }
        })
        .detach();
    }

    /// File a recoverable session under a project by hand (the user's ask).
    /// Persists durable intent in the store so it survives even before resume.
    fn set_attach(&mut self, cli_session_id: String, project_key: String, cx: &mut Context<Self>) {
        if let Ok(store) = self.store.lock() {
            let _ = store.set_override(&cli_session_id, &project_key);
        }
        self.overrides.insert(cli_session_id, project_key);
        self.attach_picker = None;
        cx.notify();
    }

    /// Recoverable sessions whose resolved home is the open project (override or
    /// longest-ancestor path-match — NOT raw starts_with, so manually-filed
    /// sessions group correctly), newest first.
    /// cli_session_ids of every session currently LIVE in the host, read from the
    /// per-frame infos cache (cheap, no host lock). Used to hide an already-resumed
    /// session from Recover — once it's live it isn't "recoverable" anymore (#6).
    /// CLEAN-EXIT observer (review #12, user-defined): a clean exit is a death
    /// we WATCHED — the session is still in the infos snapshot but alive=false
    /// (the CLI was quit on purpose). Close its store row so the crash banner
    /// stops crying wolf; it stays resumable from Recover (transcript on disk).
    /// A session that VANISHED from the snapshot was never observed dying
    /// (daemon crash/retire) — its row stays open and the banner offers it.
    /// Runs on the tick (NOT render: an occluded window stops painting, and a
    /// >reaper-window gap would misread a watched exit as a crash). A cli id
    /// that is ALSO alive in the same snapshot is a resumed session's dead
    /// ghost — closing it would zero the LIVE row (adversarial review), skip.
    pub(crate) fn observe_clean_exits(&mut self, infos: &[SessionInfo]) {
        let now_alive: std::collections::HashSet<String> = infos
            .iter()
            .filter(|i| i.alive)
            .filter_map(|i| i.cli_session_id.clone())
            .collect();
        for i in infos.iter().filter(|i| !i.alive) {
            if let Some(cid) = &i.cli_session_id {
                if self.prev_alive_cli.contains(cid) && !now_alive.contains(cid) {
                    if let Ok(store) = self.store.lock() {
                        let _ = store.close_session(cid);
                    }
                    // a session that dies between idle windows never got its
                    // last chapter summarized (docs/012 §4) — ENQUEUE ONE final
                    // ("end") summary on the DURABLE queue (docs/019 slice 3:
                    // crash-surviving, not an in-RAM list); the worker's end job
                    // writes the ■ note with a FRESH achievement headline. No new
                    // content → just the ■ note now (nothing to summarize).
                    let grew = self.summaries_on
                        && !matches!(i.kind, CliKind::Shell)
                        && self.latest_turn_at.get(cid).copied().unwrap_or(0)
                            > self
                                .sess_summaries
                                .get(cid)
                                .map(|r| r.thru_at_ms)
                                .unwrap_or(0);
                    if grew {
                        if let Ok(store) = self.store.lock() {
                            let _ = store.enqueue_summary_job(cid, &i.project_slug, "end");
                        }
                    } else {
                        self.note_session_end(cid);
                    }
                }
            }
        }
        // the crash banner must not contradict reality: a session resumed out-
        // of-band (now alive) or watched exiting (row just closed) is no longer
        // a "crashed, restore me" claim — prune it so the banner and the ENDED
        // strip never double-offer the same session (review).
        let prev = &self.prev_alive_cli;
        self.restore_offer.retain(|r| {
            let cid = &r.cli_session_id;
            let resumed = now_alive.contains(cid);
            let just_closed = prev.contains(cid) && !now_alive.contains(cid);
            !resumed && !just_closed
        });
        self.prev_alive_cli = now_alive;
    }

    /// The second of a dispatched session's exactly-two log lines (docs/011
    /// §C). Dedup by trail shape — resume legs re-observe the same cli id, and
    /// a second "■" for a session whose latest trail line is already an end
    /// would corrupt the node's permanent log. The headline rides along only
    /// when the cached summary is FRESH (covers the final turn) — a stale one
    /// would permanently misdescribe what the session concluded.
    fn note_session_end(&self, cid: &str) {
        let Ok(store) = self.store.lock() else { return };
        let Some((pid, pkey)) = store.dispatch_of(cid) else {
            return;
        };
        let src = format!("sess-{cid}");
        let already_ended = store
            .notes_for_part(pid)
            .unwrap_or_default()
            .iter()
            .find(|n| n.kind == "session" && n.source == src)
            .is_some_and(|n| n.text.starts_with('■'));
        if already_ended {
            return;
        }
        let head = self
            .summary_fresh
            .contains(cid)
            .then(|| self.sess_summaries.get(cid).map(|s| s.headline.clone()))
            .flatten()
            .unwrap_or_default();
        let body = if head.is_empty() {
            "■ session ended".to_string()
        } else {
            format!("■ session ended — {head}")
        };
        let _ = store.add_note(&pkey, pid, "session", &body, &src);
    }

    /// The live session (project slug + id) already running a given CLI
    /// conversation, if any — resume paths jump there instead of re-spawning
    /// (a second resume forks the transcript; review #12).
    pub(crate) fn find_live_by_cli_id(&self, cli_id: &str) -> Option<(String, SessionId)> {
        self.infos_cache
            .values()
            .flatten()
            .find(|i| i.alive && i.cli_session_id.as_deref() == Some(cli_id))
            .map(|i| (i.project_slug.clone(), i.id))
    }

    fn live_cli_ids(&self) -> std::collections::HashSet<String> {
        // ONLY truly-alive sessions dedupe a recoverable one — so a session you
        // resumed and then EXITED returns to Recover as resumable (not hidden by a
        // dead-but-cached ghost). #6.
        self.infos_cache
            .values()
            .flatten()
            .filter(|i| i.alive)
            .filter_map(|i| i.cli_session_id.clone())
            .collect()
    }

    pub(crate) fn recoverable_for_project(&self) -> Vec<orchestrator_core::scan::RecoverableSession> {
        let slug = self.project().slug.clone();
        let live = self.live_cli_ids();
        self.recoverable
            .lock()
            .map(|g| {
                g.iter()
                    .filter(|s| self.resolve_home(s).0 == slug && !live.contains(&s.id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// EVERY recoverable session (deduped against live) — the "Browse all" list,
    /// so a session mis-filed by its recorded cwd can be found and homed anywhere.
    fn all_recoverable(&self) -> Vec<orchestrator_core::scan::RecoverableSession> {
        let live = self.live_cli_ids();
        self.recoverable
            .lock()
            .map(|g| {
                g.iter()
                    .filter(|s| !live.contains(&s.id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn recoverable_by_id(
        &self,
        cli_session_id: &str,
    ) -> Option<orchestrator_core::scan::RecoverableSession> {
        self.recoverable
            .lock()
            .ok()
            .and_then(|g| g.iter().find(|s| s.id == cli_session_id).cloned())
            .or_else(|| {
                orchestrator_core::scan::recoverable_sessions(3650, 120)
                    .into_iter()
                    .find(|s| s.id == cli_session_id)
            })
    }

    /// Count of recoverables homed to the open project (excluding any already live),
    /// WITHOUT cloning the list — cheap enough for the control-bar badge every frame.
    pub(crate) fn recoverable_count_for_project(&self) -> usize {
        let slug = self.project().slug.clone();
        let live = self.live_cli_ids();
        self.recoverable
            .lock()
            .map(|g| {
                g.iter()
                    .filter(|s| self.resolve_home(s).0 == slug && !live.contains(&s.id))
                    .count()
            })
            .unwrap_or(0)
    }

    /// Resume a recoverable session into the drawer, bound to its RESOLVED home
    /// (override > path-match > ad-hoc). The single resume path shared by the
    /// drawer list and the global Recover surface.
    fn resume_session(
        &mut self,
        rec: orchestrator_core::scan::RecoverableSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // already running? jump to it — never fork a live conversation.
        if let Some((live_slug, sid)) = self.find_live_by_cli_id(&rec.id) {
            self.focus_session(&live_slug, sid, window, cx);
            return;
        }
        let (mut slug, cwd) = self.resolve_home(&rec);
        // claude --resume is scoped to the session's START cwd (where claude stored
        // it); codex resumes by id. Spawn there or claude says "No conversation
        // found" (a session started one dir up, then cd'd into the project).
        let run_cwd = if rec.is_codex {
            cwd.clone()
        } else {
            rec.start_cwd.clone()
        };
        if !run_cwd.is_dir() {
            self.term_error = Some(format!("can't resume — {} is gone", run_cwd.display()));
            cx.notify();
            return;
        }
        // re-home on an empty OR stale slug (e.g. an override pointing at a
        // project that no longer exists) so we never spawn under a phantom tile.
        if slug.is_empty() || !self.projects.iter().any(|p| p.slug == slug) {
            slug = self.ensure_home_for(&cwd);
        }
        // the kind is fixed by which resume leg this is (codex-by-id vs
        // claude-by-cwd); resume_* overrides program, but the BASE must be
        // `program` and not `shell` — shell() seeds `args: ["-l"]`, and the host
        // now KEEPS caller args, so a login-shell flag would ride into the CLI.
        let resume_kind = if rec.is_codex { CliKind::Codex } else { CliKind::Claude };
        let mut spec = self.stage_spec(SpawnSpec::program(resume_kind.label(), &run_cwd));
        // resume under the SAME account this session ran under. A RecoverableSession
        // is a disk scan and carries no profile_id, so look it up from the store by
        // the session's own cli id — never a global/current profile.
        let profile = self
            .store
            .lock()
            .ok()
            .and_then(|s| s.session_profile_id(&rec.id).and_then(|pid| s.profile(pid)));
        if let Some(p) = &profile {
            crate::spawn::apply_profile(&mut spec, resume_kind, p);
            crate::spawn::apply_profile_resume_argv(&mut spec, resume_kind, p);
        }
        self.screen = Screen::Workspace;
        self.mode = Mode::Agent;
        // land on the resolved project so the resumed drawer shows under it.
        if let Some(i) = self.projects.iter().position(|p| p.slug == slug) {
            self.selected = i;
        }
        let spawned = if rec.is_codex {
            self.host.resume_codex(slug.clone(), &rec.id, spec)
        } else {
            self.host.resume_claude(slug.clone(), &rec.id, spec)
        };
        match spawned {
            Ok(id) => {
                self.term_error = None;
                // crash-proof the resumed session: record its id so it survives a
                // crash exactly like a fresh one (the test's invariant). UPSERT,
                // so re-resuming keeps the same row alive — and re-persist the
                // profile so the account survives across resumes.
                if let Ok(store) = self.store.lock() {
                    let _ = store.record_session(
                        &rec.id,
                        &slug,
                        resume_kind.label(),
                        &run_cwd.to_string_lossy(),
                        profile.as_ref().map(|p| p.id),
                    );
                }
                self.active_session.insert(slug, id);
                self.term_focus.focus(window);
            }
            Err(e) => self.term_error = Some(format!("couldn't resume: {e}")),
        }
        cx.notify();
    }

    /// Restore-on-launch: read the prior process's still-alive (crashed) hosted
    /// sessions ONCE at construction. A crashed session keeps being offered on
    /// EVERY launch until you RECOVER or DISMISS it. Shells aren't restorable.
    pub(crate) fn load_restore_offer(&mut self) {
        // RECONCILE with the daemon before offering (docs/018 §12, codex): a
        // session the daemon already holds LIVE (matched by cli_session_id) must
        // NOT be offered for restore — it's reachable now via attach, and
        // re-resuming it would double-spawn. The not-live (truly-crashed) rows are
        // offered and STAY alive=1 until acted on (recover or dismiss). In-process
        // mode has no live sessions at launch, so this is a no-op there.
        // only truly-ALIVE daemon sessions suppress an offer — a dead-but-not-
        // yet-reaped ghost at launch must not hide its crashed row this launch
        // (adversarial review).
        let live: std::collections::HashSet<String> = self
            .host
            .infos()
            .into_iter()
            .filter(|i| i.alive)
            .filter_map(|i| i.cli_session_id)
            .collect();
        if let Ok(store) = self.store.lock() {
            let rows = store.restorable_sessions().unwrap_or_default();
            let mut offer = Vec::new();
            for r in rows {
                if live.contains(&r.cli_session_id) {
                    continue; // daemon holds it live → keep alive=1, don't offer
                }
                // KEEP the row alive=1: a crashed session must keep being offered on
                // every launch until you actually RECOVER or DISMISS it. Clearing it
                // on first sight (the old present-then-clear) meant a missed banner
                // silently dropped the session — the daemon-retire "sessions gone" bug.
                if r.kind != "shell" {
                    offer.push(r);
                }
            }
            self.restore_offer = offer;
        }
    }

    /// Re-resume one prior-process session. Binding precedence mirrors
    /// `resolve_home` but over a `HostedSessionRow` (String cwd): override >
    /// stored project_key (a previously-resolved key) > ad-hoc home for a
    /// vanished project. Returns `Some(bound_slug)` — the slug the live session
    /// was actually filed under — or `None` (with term_error) on failure.
    fn restore_one(
        &mut self,
        row: HostedSessionRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        // already running (e.g. resumed via Recover moments ago)? jump, don't fork.
        if let Some((live_slug, sid)) = self.find_live_by_cli_id(&row.cli_session_id) {
            self.focus_session(&live_slug, sid, window, cx);
            return Some(live_slug);
        }
        let restored_kind = restore_row_kind(
            &row.kind,
            orchestrator_core::scan::codex_rollout_path(&row.cli_session_id).is_some(),
        );
        if restored_kind == CliKind::Codex {
            if let Some(rec) = self.recoverable_by_id(&row.cli_session_id) {
                // carry the ROW's own profile forward — the delegate has no store
                // row of its own to read it from.
                return self.restore_recoverable_codex(rec, row.profile_id, window, cx);
            }
        }
        let cwd = std::path::PathBuf::from(&row.cwd);
        if !cwd.is_dir() {
            self.term_error = Some(format!("can't restore — {} is gone", row.cwd));
            return None;
        }
        let mut key = self
            .overrides
            .get(&row.cli_session_id)
            .cloned()
            .unwrap_or_else(|| row.project_key.clone());
        if !self.projects.iter().any(|p| p.slug == key) {
            key = self.ensure_home_for(&cwd);
        }
        // `program`, not `shell`, as the base — see resume_session: shell()'s
        // `-l` would now survive into the resumed CLI's argv.
        let mut spec = self.stage_spec(SpawnSpec::program(restored_kind.label(), &cwd));
        // resume under the ROW's own account (its recorded profile_id), applying
        // the kind THIS path resumes — never a global/current profile.
        let profile = row
            .profile_id
            .and_then(|id| self.store.lock().ok().and_then(|s| s.profile(id)));
        if let Some(p) = &profile {
            crate::spawn::apply_profile(&mut spec, restored_kind, p);
            crate::spawn::apply_profile_resume_argv(&mut spec, restored_kind, p);
        }
        let spawned = match restored_kind {
            CliKind::Codex => self
                .host
                .resume_codex(key.clone(), &row.cli_session_id, spec),
            _ => self
                .host
                .resume_claude(key.clone(), &row.cli_session_id, spec),
        };
        match spawned {
            Ok(id) => {
                if let Ok(store) = self.store.lock() {
                    let _ = store.record_session(
                        &row.cli_session_id,
                        &key,
                        restored_kind.label(),
                        &row.cwd,
                        row.profile_id,
                    );
                }
                self.active_session.insert(key.clone(), id);
                self.term_focus.focus(window);
                Some(key)
            }
            Err(e) => {
                self.term_error = Some(format!("couldn't restore: {e}"));
                None
            }
        }
    }

    fn restore_recoverable_codex(
        &mut self,
        rec: orchestrator_core::scan::RecoverableSession,
        profile_id: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let (mut slug, cwd) = self.resolve_home(&rec);
        if !cwd.is_dir() {
            self.term_error = Some(format!("can't restore — {} is gone", cwd.display()));
            return None;
        }
        if slug.is_empty() || !self.projects.iter().any(|p| p.slug == slug) {
            slug = self.ensure_home_for(&cwd);
        }
        // `program`, not `shell`, as the base — see resume_session.
        let mut spec = self.stage_spec(SpawnSpec::program("codex", &cwd));
        // this path always resumes codex → CODEX_HOME isolation, under the profile
        // handed down from the row that dispatched us (never a global one).
        let profile = profile_id.and_then(|id| self.store.lock().ok().and_then(|s| s.profile(id)));
        if let Some(p) = &profile {
            crate::spawn::apply_profile(&mut spec, CliKind::Codex, p);
            crate::spawn::apply_profile_resume_argv(&mut spec, CliKind::Codex, p);
        }
        match self.host.resume_codex(slug.clone(), &rec.id, spec) {
            Ok(id) => {
                if let Ok(store) = self.store.lock() {
                    let _ = store.record_session(
                        &rec.id,
                        &slug,
                        "codex",
                        &cwd.to_string_lossy(),
                        profile_id,
                    );
                }
                self.active_session.insert(slug.clone(), id);
                self.term_focus.focus(window);
                cx.notify();
                Some(slug)
            }
            Err(e) => {
                self.term_error = Some(format!("couldn't restore: {e}"));
                cx.notify();
                None
            }
        }
    }

    /// Restore every offered session, dedup-guarded, landing on the FIRST
    /// (newest) successfully-restored one's Workspace — by the slug it was
    /// actually bound to (which may be a freshly-minted ad-hoc home). Sessions
    /// whose restore FAILS (cwd transiently gone, spawn error) are kept in the
    /// offer so the banner can re-surface them rather than silently dropping.
    fn restore_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = std::mem::take(&mut self.restore_offer);
        let mut seen = std::collections::HashSet::new();
        let mut land: Option<String> = None;
        let mut failed: Vec<HostedSessionRow> = Vec::new();
        for row in rows {
            if !seen.insert(row.cli_session_id.clone()) {
                continue;
            }
            match self.restore_one(row.clone(), window, cx) {
                Some(slug) => {
                    if land.is_none() {
                        land = Some(slug);
                    }
                }
                None => failed.push(row),
            }
        }
        if let Some(k) = land {
            self.selected = self
                .projects
                .iter()
                .position(|p| p.slug == k)
                .unwrap_or(self.selected);
            self.screen = Screen::Workspace;
            self.mode = Mode::Agent;
            self.term_focus.focus(window);
        }
        // keep what failed; only dismiss once the offer is fully consumed.
        self.restore_offer = failed;
        self.restore_dismissed = self.restore_offer.is_empty();
        cx.notify();
    }

    fn dismiss_restore(&mut self, cx: &mut Context<Self>) {
        // dismiss = "I've handled these" → mark the rows alive=0 AND dismissed,
        // so they neither re-offer next launch NOR resurrect as tombstones in
        // the ENDED strip one tick later (review: close_session alone stamps
        // closed_at=now and the strip would show all of them, dated wrong).
        if let Ok(store) = self.store.lock() {
            for r in &self.restore_offer {
                let _ = store.close_session(&r.cli_session_id);
                let _ = store.dismiss_session(&r.cli_session_id);
            }
        }
        // unwatched deaths exit through here — their dispatch trail still gets
        // its end line (docs/011: both close sites, exactly two lines).
        let cids: Vec<String> = self
            .restore_offer
            .iter()
            .map(|r| r.cli_session_id.clone())
            .collect();
        for cid in &cids {
            self.note_session_end(cid);
        }
        self.restore_offer.clear();
        self.restore_dismissed = true;
        cx.notify();
    }

    /// Resume every recoverable session homed to the open project (the control
    /// bar's "Resume all"). The last one wins focus.
    fn resume_all_for_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for rec in self.recoverable_for_project() {
            self.resume_session(rec, window, cx);
        }
    }

    /// The import/recovery list — recoverable sessions for this project (the
    /// crashed ones), each resumable into the drawer with one click.
    /// The Agent stage's no-live-agents state — the SAME `recover_row` the
    /// dedicated Recover view uses (one component: recap line, metadata,
    /// File-under, lazy summary, Resume), so the two surfaces can never drift
    /// apart again. Framed as "pick up where you left off", with Resume-all
    /// and a jump to the full Recover view.
    pub(crate) fn render_recoverable_list(
        &self,
        recs: Vec<orchestrator_core::scan::RecoverableSession>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let now = orchestrator_core::registry::now_secs();
        let n = recs.len();
        let head = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .pb(px(4.))
            .child(
                div()
                    .text_size(px(17.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(TEXT_STRONG))
                    .child("Pick up where you left off"),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.))
                    .text_color(rgb(MUTED))
                    .child(SharedString::from(format!(
                        "{n} recent session{} in this project",
                        if n == 1 { "" } else { "s" }
                    ))),
            )
            .when(n > 1, |c| {
                c.child(
                    div()
                        .id("agent-resume-all")
                        .px(px(11.))
                        .py(px(5.))
                        .rounded(px(8.))
                        .border_1()
                        .border_color(rgb(0x2f4a3e))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(rgb(GREEN))
                        .hover(|h| h.bg(rgb(0x16231d)))
                        .child("Resume all")
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.resume_all_for_project(window, cx)
                        })),
                )
            })
            .child(
                div()
                    .id("agent-open-recover")
                    .px(px(11.))
                    .py(px(5.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(rgb(HAIR))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .hover(|h| h.text_color(rgb(ACCENT)))
                    .child("All sessions ▸")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.mode = Mode::Recover;
                        cx.notify();
                    })),
            );
        let mut list = div()
            .id("recover-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(10.))
            .px(px(22.))
            .py(px(16.))
            .child(head);
        for rec in recs.iter().take(12) {
            let when = rel_secs(rec.last_active_secs, now);
            list = list.child(self.recover_row(rec, &when, cx));
        }
        list.child(
            div()
                .pt(px(6.))
                .text_size(px(11.5))
                .text_color(rgb(MUTED2))
                .child("…or start fresh: ⌘T claude · ⇧⌘T codex · ⌥⌘T shell"),
        )
    }

    /// Restore-on-launch banner (atop the Standup): the sessions left alive by
    /// the prior process. None when nothing to offer / dismissed.
    pub(crate) fn render_restore_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.restore_dismissed || self.restore_offer.is_empty() {
            return None;
        }
        let now = orchestrator_core::registry::now_secs();
        let n = self.restore_offer.len();
        let names: Vec<String> = self
            .restore_offer
            .iter()
            .take(4)
            .map(|r| {
                std::path::Path::new(&r.cwd)
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_else(|| r.cwd.clone())
            })
            .collect();
        let mut sub = names.join(" · ");
        if n > 4 {
            sub.push_str(" …");
        }
        let mut card = div()
            .mx(px(26.))
            .mt(px(14.))
            .p(px(13.))
            .rounded(px(12.))
            .bg(rgb(CARD))
            .border_1()
            .border_color(rgb(0x4A4636))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .child(div().w(px(6.)).h(px(6.)).rounded(px(3.)).bg(rgb(AMBER)))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT_STRONG))
                            .child(SharedString::from(format!(
                                "{n} session{} {} open before you crashed",
                                if n == 1 { "" } else { "s" },
                                if n == 1 { "was" } else { "were" }
                            ))),
                    )
                    .child(
                        div()
                            .id("restore-all")
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.restore_all(window, cx)
                            }))
                            .child(accent_btn("Restore all")),
                    )
                    .child(
                        div()
                            .id("restore-review")
                            .px(px(10.))
                            .py(px(4.))
                            .rounded(px(8.))
                            .border_1()
                            .border_color(rgb(HAIR))
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .hover(|h| h.text_color(rgb(ACCENT)))
                            .child(if self.restore_expanded {
                                "Hide"
                            } else {
                                "Review"
                            })
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.restore_expanded = !this.restore_expanded;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("restore-dismiss")
                            .px(px(10.))
                            .py(px(4.))
                            .rounded(px(8.))
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .hover(|h| h.text_color(rgb(0xE68A8A)))
                            .child("Dismiss")
                            .on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.dismiss_restore(cx)),
                            ),
                    ),
            )
            .child(
                div()
                    .pl(px(16.))
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .child(SharedString::from(sub)),
            );
        if self.restore_expanded {
            let mut list = div()
                .id("restore-list")
                .max_h(px(240.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(2.))
                .pt(px(2.));
            for row in self.restore_offer.iter() {
                let glyph = if row.kind == "codex" {
                    "◆"
                } else {
                    "✦"
                };
                let color = if row.kind == "codex" {
                    MUTED
                } else {
                    ACCENT
                };
                let base = std::path::Path::new(&row.cwd)
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let r = row.clone();
                let id8 = row.cli_session_id.clone(); // full id for unique element identity
                list = list.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(9.))
                        .px(px(8.))
                        .py(px(5.))
                        .rounded(px(8.))
                        .hover(|h| h.bg(rgb(CARD2)))
                        .child(
                            div()
                                .w(px(14.))
                                .flex_none()
                                .text_size(px(12.))
                                .text_color(rgb(color))
                                .child(glyph),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_size(px(13.))
                                .text_color(rgb(TEXT))
                                .child(SharedString::from(format!(
                                    "{base} · {}",
                                    rel_secs(row.last_seen_secs, now)
                                ))),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("restore-{id8}")))
                                .px(px(10.))
                                .py(px(3.))
                                .rounded(px(7.))
                                .bg(rgb(ACCENT))
                                .cursor_pointer()
                                .text_size(px(11.))
                                .text_color(rgb(0x0C140F))
                                .child("Resume ▸")
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    // only remove from the offer on SUCCESS — a failed
                                    // restore stays so the banner can re-surface it.
                                    if this.restore_one(r.clone(), window, cx).is_some() {
                                        this.restore_offer
                                            .retain(|x| x.cli_session_id != r.cli_session_id);
                                        if this.restore_offer.is_empty() {
                                            this.restore_dismissed = true;
                                        }
                                    }
                                    cx.notify();
                                })),
                        ),
                );
            }
            card = card.child(list);
        }
        Some(card.into_any_element())
    }

    /// The global Recover surface — ALL recoverable sessions grouped by their
    /// resolved home (project / manual override / Unfiled), each resumable and
    /// (esp. orphans) manually attachable to a project.
    /// The per-project Recover view (Mode::Recover) — the rich recoverable list
    /// for the OPEN project (goal · summary · last state · Resume), reusing
    /// recover_row. Replaces the global Recover screen; reached from the control
    /// bar's ⟲ button (#9 / task #6).
    pub(crate) fn render_project_recover(&self, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let now = orchestrator_core::registry::now_secs();
        let all = self.recover_all;
        let recs = if all {
            self.all_recoverable()
        } else {
            self.recoverable_for_project()
        };
        let n = recs.len();
        let head = div().flex().flex_row().items_center().gap(px(10.)).pb(px(4.))
            .child(div().text_size(px(18.)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(TEXT_STRONG)).child("Recover"))
            .child(div().flex_1().text_size(px(13.)).text_color(rgb(MUTED)).child(SharedString::from(
                if all { format!("{n} session{} across all projects — resume, or ‘File under ▸’ to home one here", if n == 1 { "" } else { "s" }) }
                else if n == 0 { format!("nothing homed to {name}") }
                else { format!("{n} recoverable in {name}") })))
            // toggle: this project ⟷ every session (to find one mis-filed by its cwd).
            .child(div().id("recover-scope").px(px(11.)).py(px(5.)).rounded(px(8.)).border_1().border_color(rgb(HAIR)).cursor_pointer()
                .text_size(px(12.)).text_color(rgb(if all { ACCENT } else { MUTED })).hover(|h| h.text_color(rgb(ACCENT)))
                .child(if all { SharedString::from(format!("◂ {name} only")) } else { SharedString::from("Browse all sessions ▸") })
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| { this.recover_all = !this.recover_all; this.attach_picker = None; cx.notify(); })))
            .when(!all && n > 1, |c| c.child(
                div().id("recover-all").px(px(11.)).py(px(5.)).rounded(px(8.)).border_1().border_color(rgb(0x2f4a3e)).cursor_pointer()
                    .text_size(px(12.)).text_color(rgb(GREEN)).hover(|h| h.bg(rgb(0x16231d)))
                    .child("Resume all")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.resume_all_for_project(window, cx))),
            ));
        let mut body = div()
            .id("proj-recover-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(10.))
            .px(px(22.))
            .py(px(16.))
            .child(head);
        if recs.is_empty() {
            body = body.child(
                div().pt(px(16.)).flex().flex_col().gap(px(10.))
                    .child(div().text_size(px(13.)).text_color(rgb(MUTED2)).child(SharedString::from(
                        if all { "No recoverable sessions found on disk.".to_string() }
                        else { format!("Nothing homed to {name}. A session you ran here can be filed elsewhere by its recorded folder — browse all to find it.") })))
                    .when(!all, |c| c.child(div().flex().flex_row().child(
                        div().id("browse-empty").px(px(12.)).py(px(6.)).rounded(px(8.)).border_1().border_color(rgb(0x2c5246)).cursor_pointer().text_size(px(12.5)).text_color(rgb(ACCENT)).hover(|h| h.bg(rgb(0x16231d)))
                            .child("Browse all sessions ▸")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| { this.recover_all = true; cx.notify(); }))))),
            );
        } else {
            for rec in recs.into_iter().take(if all { 60 } else { 40 }) {
                let when = rel_secs(rec.last_active_secs, now);
                body = body.child(self.recover_row(&rec, &when, cx));
            }
        }
        body.into_any_element()
    }

    /// One recoverable session row (reused by the per-project Recover view):
    /// glyph, last message, when+dir, Resume, lazy summary, and a File-under
    /// picker that files an orphan under a project.
    fn recover_row(
        &self,
        rec: &orchestrator_core::scan::RecoverableSession,
        when: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let glyph = if rec.is_codex { "◆" } else { "✦" };
        let msg = rec.last_message.clone().unwrap_or_default();
        let dir = rec
            .cwd
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        // FULL id for element identity — codex uuidv7 share a timestamp prefix,
        // so an 8-char id would collide across same-hour sessions and misroute clicks.
        let eid = rec.id.as_str();
        let r_resume = rec.clone();
        let r_attach = rec.id.clone();
        let picker_open = self.attach_picker.as_deref() == Some(rec.id.as_str());

        let mut row = div()
            .flex()
            .flex_col()
            .w_full()
            .min_w_0() // a long summary line must not expand the row past the viewport
            .gap(px(4.))
            .px(px(8.))
            .py(px(6.))
            .rounded(px(8.))
            .hover(|h| h.bg(rgb(CARD2)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(9.))
                    .w_full()
                    .min_w_0()
                    .child(
                        div()
                            .w(px(14.))
                            .flex_none()
                            .text_size(px(12.))
                            .text_color(rgb(if rec.is_codex { MUTED } else { ACCENT }))
                            .child(glyph),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .flex()
                            .flex_col()
                            .child(
                                div().text_size(px(13.)).text_color(rgb(TEXT)).child(
                                    SharedString::from(
                                        orchestrator_core::recap::recap_line(&msg)
                                            .chars()
                                            .take(72)
                                            .collect::<String>(),
                                    ),
                                ),
                            )
                            .child(div().text_size(px(11.)).text_color(rgb(MUTED2)).child(
                                SharedString::from(format!(
                                    "{} → {when} · {} msgs · {} · {dir}",
                                    short_date(rec.started_secs),
                                    rec.turns,
                                    human_bytes(rec.bytes),
                                )),
                            )),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("rattach-{eid}")))
                            .px(px(10.))
                            .py(px(4.))
                            .rounded(px(7.))
                            .border_1()
                            .border_color(rgb(HAIR))
                            .cursor_pointer()
                            .text_size(px(11.5))
                            .text_color(rgb(MUTED))
                            .hover(|h| h.text_color(rgb(ACCENT)))
                            .child("File under ▸")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.attach_picker =
                                    if this.attach_picker.as_deref() == Some(r_attach.as_str()) {
                                        None
                                    } else {
                                        Some(r_attach.clone())
                                    };
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("rresume-{eid}")))
                            .px(px(11.))
                            .py(px(4.))
                            .rounded(px(7.))
                            .bg(rgb(ACCENT))
                            .cursor_pointer()
                            .text_size(px(11.5))
                            .text_color(rgb(0x0C140F))
                            .child("Resume ▸")
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.resume_session(r_resume.clone(), window, cx)
                            })),
                    ),
            )
            .child(self.summary_el(rec, eid, cx));
        if picker_open {
            let mut picker = div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(6.))
                .pl(px(23.))
                .pt(px(2.))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(MUTED2))
                        .pr(px(4.))
                        .child("file under:"),
                );
            for (i, p) in self.projects.iter().enumerate() {
                let sid = rec.id.clone();
                let pslug = p.slug.clone();
                picker = picker.child(
                    div()
                        .id(SharedString::from(format!("pick-{eid}-{i}")))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .px(px(8.))
                        .py(px(3.))
                        .rounded(px(7.))
                        .bg(rgb(CARD))
                        .border_1()
                        .border_color(rgb(HAIR))
                        .cursor_pointer()
                        .hover(|h| h.border_color(rgb(ACCENT)))
                        .child(dot(p.status.color()))
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(rgb(TEXT))
                                .child(SharedString::from(p.name.clone())),
                        )
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.set_attach(sid.clone(), pslug.clone(), cx)
                        })),
                );
            }
            row = row.child(picker);
        }
        row.into_any_element()
    }

    /// The on-demand summary line for a Recover row: a "Summarize" button, a
    /// running state, the one-line result, or a retryable failure.
    fn summary_el(
        &self,
        rec: &orchestrator_core::scan::RecoverableSession,
        eid: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pl = px(23.);
        match self.summaries.get(&rec.id) {
            Some(SummaryState::Running) => div()
                .pl(pl)
                .text_size(px(11.5))
                .text_color(rgb(MUTED2))
                .child("✦ summarizing…")
                .into_any_element(),
            Some(SummaryState::Done(text)) => div()
                .w_full()
                .min_w_0() // wrap within the row, never expand it
                .overflow_hidden()
                .pl(pl)
                .pr(px(8.))
                .text_size(px(11.5))
                .text_color(rgb(MUTED))
                .italic()
                .child(SharedString::from(text.clone()))
                .into_any_element(),
            Some(SummaryState::Failed(err)) => {
                let r = rec.clone();
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .w_full()
                    .min_w_0()
                    .gap(px(8.))
                    .pl(pl)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_size(px(11.5))
                            .text_color(rgb(0xC98A8A))
                            .child(SharedString::from(format!(
                                "summary failed — {}",
                                err.chars().take(60).collect::<String>()
                            ))),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("retry-{eid}")))
                            .cursor_pointer()
                            .text_size(px(11.5))
                            .text_color(rgb(ACCENT))
                            .child("Retry")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.start_summary(&r, cx)
                            })),
                    )
                    .into_any_element()
            }
            None => {
                let r = rec.clone();
                div()
                    .id(SharedString::from(format!("sum-{eid}")))
                    .ml(pl)
                    .px(px(8.))
                    .py(px(2.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(rgb(HAIR))
                    .cursor_pointer()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .hover(|h| h.text_color(rgb(ACCENT)))
                    .child("✦ Summarize")
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| this.start_summary(&r, cx)),
                    )
                    .into_any_element()
            }
        }
    }

}
