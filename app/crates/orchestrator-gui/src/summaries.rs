use gpui::*;
use crate::*;


impl Orchestrator {

    /// Per-~500ms: expire a stale toast, then surface any NEW needs-you — fire a
    /// macOS notification per new request (when unfocused) and raise the toast for
    /// the newest. seen_needs is pruned to the still-waiting set so a re-ask
    /// re-fires; HashSet::insert returns true exactly for a genuinely-new request.
    pub(crate) fn tick_needs(&mut self, active: bool, cx: &mut Context<Self>) {
        let now = orchestrator_core::registry::now_secs();
        if self
            .active_toast
            .as_ref()
            .is_some_and(|t| t.expire_at != 0 && now >= t.expire_at)
        {
            self.active_toast = None;
            cx.notify();
        }
        let all_infos = self.host.infos();
        // heartbeat stamp (docs/019 commitment 4): the observe tick just reached
        // the daemon; if this loop later STALLS (daemon RPC hangs, process gone),
        // last_beat_ms goes stale and the ARE layer washes grey instead of lying.
        self.last_beat_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        // clean-exit observation rides the tick, which fires even when the
        // window is occluded and render() isn't running (review #12).
        self.observe_clean_exits(&all_infos);
        let awaiting: Vec<SessionInfo> = all_infos
            .iter()
            .filter(|s| s.alive && s.phase == orchestrator_host::Phase::AwaitingDecision)
            .cloned()
            .collect();
        // The Dock badge rides the same tick and the EXACT `awaiting` set above
        // — phase only — so the bubble always agrees with the toast, the macOS
        // notification and the sidebar's needs-action dot, which are all driven
        // from this same set. It does NOT subtract usage-limit sessions, and
        // must not be "fixed" to: a limit-hit session can also be
        // AwaitingDecision (phase and usage_limit are independent — see
        // session.rs's auto-continue gate, which handles exactly that pair), and
        // its permission prompt is still a real ask the user can answer now.
        // Standup is the one surface that reads differently: render_standup pins
        // any `usage_limit.hit` session into ⛔ BLOCKED and `continue`s, so it
        // never lands in ⚠ NEEDS YOU there. Badge 1 / Standup "nothing needs
        // you" is therefore reachable — a Standup tiering question, not a badge
        // one; do not paper over it here by making the badge disagree with the
        // other three surfaces.
        // Ambient by design: it interrupts nothing, so it is safe to fire it on
        // every tick — including while the window is occluded and render() is
        // not running, which is precisely when a badge is the only signal left.
        // MAIN THREAD: this whole tick runs in the GPUI update loop (boot.rs's
        // cx.spawn loop), which is what set_dock_badge requires.
        // ORCH_BADGE_TEST=<n> pins the count, so the badge can be SEEN without
        // driving a real agent into a real permission prompt. Read every tick on
        // purpose: the tick would otherwise overwrite a one-shot demo value within
        // 500ms, which is what makes "just set it once at boot" not work here.
        #[cfg(target_os = "macos")]
        {
            let n = std::env::var("ORCH_BADGE_TEST")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(awaiting.len());
            crate::macnotify::set_dock_badge(n);
        }
        let current: std::collections::HashSet<SessionId> = awaiting.iter().map(|s| s.id).collect();
        self.seen_needs.retain(|id| current.contains(id));
        // #6: a toast whose session has LEFT AwaitingDecision (answered
        // in-terminal, or otherwise resolved) must clear. With #2's permanent
        // default it no longer hard-expires, so reconcile it against the live
        // awaiting set or a card-less "⚠ NEEDS YOU" lingers forever (review F6).
        if self
            .active_toast
            .as_ref()
            .is_some_and(|t| !current.contains(&t.id))
        {
            self.active_toast = None;
            cx.notify();
        }
        // #2: a fresh toast's deadline — `0` (permanent) stays `0`, otherwise
        // now + the configured lifetime. The expire check above treats `0` as
        // never-expiring, so a permanent toast waits for an explicit dismiss.
        let toast_expire = if self.toast_secs == 0 {
            0
        } else {
            now + self.toast_secs
        };
        let mut newest: Option<ToastData> = None;
        for info in &awaiting {
            if self.seen_needs.insert(info.id) {
                let proj_idx = self
                    .projects
                    .iter()
                    .position(|p| p.slug == info.project_slug)
                    .unwrap_or(0);
                let pname = self
                    .projects
                    .get(proj_idx)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| info.project_slug.clone());
                let title = format!("{pname} · {}", termview::session_label(info));
                let ask = info
                    .pending
                    .as_ref()
                    .map(|p| p.view.summary())
                    .unwrap_or_else(|| "a decision is waiting".to_string());
                if !active {
                    notify::needs_you(&title, &ask);
                }
                newest = Some(ToastData {
                    id: info.id,
                    slug: info.project_slug.clone(),
                    title,
                    ask: ask.chars().take(120).collect(),
                    expire_at: toast_expire,
                });
            }
        }
        if let Some(t) = newest {
            self.active_toast = Some(t);
            cx.notify();
        }
        self.persist_events();
        if let Ok(store) = self.store.lock() {
            // #16: latest summaries + the durable freshness anchor.
            if let Ok(rows) = store.latest_summaries() {
                self.sess_summaries = rows.into_iter().map(|r| (r.sess.clone(), r)).collect();
            }
            if let Ok(ev) = store.latest_event_by_sess() {
                self.latest_turn_at = ev.into_iter().collect();
            }
        }
        // freshness gate: a summary is shown ONLY if it covers the session's
        // newest content. claude = durable TurnEnd clock; codex = rollout size
        // (its live turns don't emit events — file growth IS the signal).
        self.summary_fresh.clear();
        for (cid, s) in &self.sess_summaries {
            let is_codex = s.src_path.contains("/.codex/");
            let fresh = if is_codex {
                std::fs::metadata(&s.src_path)
                    .map(|m| m.len() == s.src_bytes)
                    .unwrap_or(false)
            } else {
                s.thru_at_ms >= self.latest_turn_at.get(cid).copied().unwrap_or(0)
            };
            if fresh {
                self.summary_fresh.insert(cid.clone());
            }
        }
        // ⌘F drift: matches are bottom-anchored; new output shifts them. While
        // the bar is open, re-search when the session's dirty counter moved
        // (adopt() re-anchors cur by line — critique).
        if self.search.open {
            if let Some(id) = self.search.session {
                if let Some(info) = self.infos_cache.values().flatten().find(|i| i.id == id) {
                    if info.dirty != self.search.last_dirty && !self.search.query.is_empty() {
                        self.kick_search(false);
                    }
                }
            }
        }
        if let Some(e) = self.breakdown_err.lock().ok().and_then(|mut g| g.take()) {
            self.term_error = Some(e);
        }
        // the Standup seen-ledger (docs/012 §3): stamp on LEAVE so the divider
        // holds still while he reads; load the divider on ENTER. (Settings used
        // to need explicit exclusions here — a peek into it must NOT count as
        // leaving/re-entering Standup, or the divider jumps forward and marks
        // unread items seen, review F4/F8. It is its own WINDOW now, so it never
        // changes `screen` at all and the guard is satisfied structurally, #54.)
        if self.prev_screen == Screen::Standup && self.screen != Screen::Standup {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if let Ok(store) = self.store.lock() {
                let _ = store.set_setting("standup_seen_ms", &now_ms.to_string());
            }
        } else if self.prev_screen != Screen::Standup && self.screen == Screen::Standup {
            self.standup_divider_ms = self
                .store
                .lock()
                .ok()
                .and_then(|s| s.get_setting("standup_seen_ms"))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }
        self.prev_screen = self.screen;
        if let Some((slug, at)) = self.outline_open_pending.clone() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if now.saturating_sub(at) >= 450 {
                self.outline_open_pending = None;
                self.set_outline_open(&slug, true);
            }
        }
        self.enqueue_summary_jobs(&all_infos);
        self.maybe_spawn_summary();
        // OSS gates (features.rs): the map proposer + memory extractor are the
        // only recurring LLM spend on the tick. Both fold to a no-op in the
        // default build; Standup (maybe_spawn_summary, above) is never gated.
        if crate::features::MAP_ENABLED {
            self.maybe_spawn_map_proposal();
        }
        if crate::features::MEMORY_ENABLED {
            self.maybe_spawn_memory_proposal();
        }
        self.maybe_sweep_drift();
        self.maybe_sweep_refile();
        // codex limit poll — the HOST self-polls now (docs/019). Via the backend
        // trait this is a NO-OP in daemon (default) mode (the daemon's own
        // SessionHost polls in its 1s sweep) and the REAL poll in local/in-process
        // mode. Either way codex limits surface with no GUI-side driver.
        self.host.poll_codex_limits();
    }

    /// Resolve a session's transcript by DISCOVERY (docs/019 slice 3 worker):
    /// claude first, else codex. Returns (path, is_codex). None = not yet
    /// discovered — the worker re-queues rather than inventing a summary.
    fn resolve_transcript(cid: &str) -> Option<(std::path::PathBuf, bool)> {
        if let Some(p) = orchestrator_core::scan::claude_transcript_path(cid) {
            return Some((p, false));
        }
        orchestrator_core::scan::codex_rollout_path(cid).map(|p| (p, true))
    }

    /// docs/019 slice 3 (C5/T12): enqueue summarize jobs on the DURABLE queue
    /// from the End / Idle / Delta triggers. Cheap + idempotent
    /// (`enqueue_summary_job` dedups a queued job per session), so it rides every
    /// tick — the queue, not RAM, is the source of truth (crash-surviving; the
    /// map may be behind, never silently stale). The expensive claim+LLM stays
    /// behind the rate ledger in `maybe_spawn_summary`. Gated on the same opt-in
    /// as the worker so an explicit summaries-off never piles queued jobs.
    fn enqueue_summary_jobs(&mut self, infos: &[SessionInfo]) {
        if !self.summaries_on {
            return;
        }
        let now_ms = orchestrator_core::registry::now_secs().saturating_mul(1000);
        // snapshot the (cid, project, events_since, idle_secs) inputs, then
        // enqueue under one lock — no LLM, no thread, just durable rows.
        let mut plan: Vec<(String, String, String)> = Vec::new(); // (cid, project, trigger)
        {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            // `infos` is the tick's OWN fresh read (host.infos()), never
            // `infos_cache` — that map is written in exactly one place,
            // Render::render, so it freezes the moment the window is occluded.
            // The tick keeps firing when minimized (that is the whole point of
            // the clean-exit comment above), but it was reading a last-rendered
            // snapshot: stale `phase`, stale `phase_since_ms`. A session last
            // rendered as Working had idle_secs pinned to 0 forever, so a codex
            // session — which can never fire Delta (its events_since is capped
            // at 1) — could not enqueue AT ALL while the window was minimized.
            for i in infos {
                if !i.alive || matches!(i.kind, CliKind::Shell) {
                    continue;
                }
                let Some(cid) = i.cli_session_id.as_deref() else {
                    continue;
                };
                let last_thru = self
                    .sess_summaries
                    .get(cid)
                    .map(|r| r.thru_at_ms)
                    .unwrap_or(0);
                // codex emits no TurnEnd rows, so its turn count is unknowable —
                // approximate "has new work" from rollout growth and feed the
                // trigger events_since=1 (fires Idle/End, never a phantom Delta).
                let events_since = if i.kind == CliKind::Codex {
                    let grew = match self.sess_summaries.get(cid) {
                        None => true,
                        Some(s) => std::fs::metadata(&s.src_path)
                            .map(|m| m.len() != s.src_bytes)
                            .unwrap_or(false),
                    };
                    u64::from(grew)
                } else {
                    store.count_events_since_sess(cid, last_thru)
                };
                let idle_secs = if i.phase == Phase::Idle {
                    now_ms.saturating_sub(i.phase_since_ms) / 1000
                } else {
                    0
                };
                if let Some(t) = returnchannel::summarize_trigger(events_since, idle_secs, false) {
                    plan.push((
                        cid.to_string(),
                        i.project_slug.clone(),
                        returnchannel::trigger_str(t).to_string(),
                    ));
                }
            }
            for (cid, proj, trig) in &plan {
                // a session whose summaries recently DIED backs off — for a
                // WHILE, escalating with the number of recent deaths, never
                // forever. This is the budget guarantee the old permanent
                // blacklist was reaching for (a hot-looping failure must not
                // re-enqueue every tick and drain the shared hourly budget,
                // blinding every other session) without its fatal flaw: one bad
                // night used to kill a session's standup for good.
                let deaths = store.session_death_times(cid);
                let (n, last) = returnchannel::recent_deaths(&deaths, now_ms);
                if returnchannel::cooling_off(n, last, now_ms) {
                    continue;
                }
                let _ = store.enqueue_summary_job(cid, proj, trig);
            }
        }
    }

    /// docs/019 slice 3: the durable-queue WORKER. At most ONE summarizer at a
    /// time, opt-in, budgeted (≤20/hr), rate-limit cooldown. Claims the oldest
    /// queued job (`claim_summary_job`), reads the transcript, calls the isolated
    /// Sonnet shim, and on success writes session_summary + `finish(id,None)`. A
    /// failure that is the JOB's (bad JSON, a CLI error) → `finish(id,Some(err))`
    /// (retry→dead→the session cools off); a failure that is the WORLD's (rate
    /// limit, transcript not ready yet) → `defer` (requeue, attempt refunded, so
    /// it can never march a healthy session to death). The
    /// trigger's `end` variant rides the ■ end note with the fresh headline.
    fn maybe_spawn_summary(&mut self) {
        use std::sync::atomic::Ordering;
        if !self.summaries_on || self.sum_running.load(Ordering::Relaxed) {
            return;
        }
        let now_s = orchestrator_core::registry::now_secs();
        if now_s < self.sum_cooldown_until.load(Ordering::Relaxed) {
            return;
        }
        self.sum_job_times
            .retain(|t| now_s.saturating_sub(*t) < 3600);
        if self.sum_job_times.len() >= 20 {
            return;
        }
        // claim the oldest durable job (id, cid, project_key, trigger).
        let Some((job_id, cid, project_key, trigger)) = ({
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            store.claim_summary_job()
        }) else {
            return;
        };
        // resolve the transcript by discovery: claude first, else codex. A
        // not-yet-found transcript is NOT a failure — DEFER it (requeue + give
        // back the attempt) so a transient miss can't march a healthy job to
        // 'dead' in seconds (review finding 1). It spends no hourly budget (the
        // push happens below).
        //
        // The defer sets the job's OWN not-before, so it yields the queue head to
        // every other session — which is why NO global worker cooldown is set
        // here any more. The old 20s cooldown stalled the whole queue on one
        // session's missing file; the per-job backoff is the targeted version of
        // it, and it is what makes an unresolvable session unable to starve
        // anyone.
        let Some((path, is_codex)) = Self::resolve_transcript(&cid) else {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let _ = store.defer_summary_job(
                job_id,
                returnchannel::NOT_READY_DEFER_SECS,
                "transcript: no transcript file found for this session yet",
            );
            return;
        };
        let is_end = trigger == "end";
        let prev_headline = self.sess_summaries.get(&cid).map(|r| r.headline.clone());
        self.sum_job_times.push(now_s);
        self.sum_running.store(true, Ordering::Relaxed);
        let store = self.store.clone();
        let running = self.sum_running.clone();
        let cooldown = self.sum_cooldown_until.clone();
        let end_note = is_end;
        std::thread::spawn(move || {
            // anchors read at RUN time, before the transcript (so the summary
            // can only UNDER-claim coverage, never over-claim).
            let src_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let thru = store
                .lock()
                .ok()
                .and_then(|s| s.latest_event_by_sess().ok())
                .and_then(|v| v.into_iter().find(|(k, _)| *k == cid).map(|(_, t)| t))
                .unwrap_or(0);
            match extract::standup_summarize(&path, is_codex, prev_headline.as_deref()) {
                Ok(sum) => {
                    let detail = serde_json::to_string(&sum.detail).unwrap_or_else(|_| "[]".into());
                    let at = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    // full-transcript read for touch-linking — MUST happen
                    // before the lock (the render path contends on this mutex).
                    let files = extract::files_touched(&path, is_codex, 20);
                    // lock LAST, only for the inserts; poison-tolerant so a
                    // panicked writer elsewhere can't disable persistence.
                    let s = store.lock().unwrap_or_else(|e| e.into_inner());
                    // the durable job is DONE only if the summary row actually
                    // LANDED — a swallowed record_summary error would retire the
                    // job with no row, blinding the meter forever with no card
                    // and no retry (review finding 3). On a write error, fail the
                    // job (retry→dead card) instead of marking it done.
                    match s.record_summary(
                        &cid,
                        &project_key,
                        at,
                        thru,
                        src_bytes,
                        &path.to_string_lossy(),
                        &sum.goal,
                        &sum.headline,
                        &sum.next_action,
                        &detail,
                    ) {
                        Ok(()) => {
                            let _ = s.finish_summary_job(job_id, None);
                        }
                        Err(e) => {
                            let _ = s.finish_summary_job(
                                job_id,
                                Some(&format!("summary write failed: {e}")),
                            );
                        }
                    }
                    // queued final for a DISPATCHED session: the end note rides
                    // the fresh achievement headline (docs/012 §4). Dedup: skip
                    // if the trail already ends with a '■'.
                    if end_note {
                        if let Some((pid, pkey)) = s.dispatch_of(&cid) {
                            let src = format!("sess-{cid}");
                            let already = s
                                .notes_for_part(pid)
                                .unwrap_or_default()
                                .iter()
                                .find(|n| n.kind == "session" && n.source == src)
                                .is_some_and(|n| n.text.starts_with('■'));
                            if !already {
                                let _ = s.add_note(
                                    &pkey,
                                    pid,
                                    "session",
                                    &format!("■ session ended — {}", sum.headline),
                                    &src,
                                );
                            }
                        }
                    }
                    // touch-linking (docs/011 slice 3): files this session
                    // touched ∩ node anchors → role=touch rows, cap 5.
                    // Observed linkage NEVER paints chips — "also touched" in
                    // the outline only; a dispatch row wins the PK. Matching
                    // is PATH-BOUNDARY anchored: bare contains() made anchor
                    // "src/**" match every file in the repo (review).
                    if !files.is_empty() {
                        if let Ok(parts) = s.load_tree(&project_key) {
                            let mut linked = 0usize;
                            'parts: for part in parts.iter().filter(|p| !p.anchors.is_empty()) {
                                for a in &part.anchors {
                                    let prefix: String = a
                                        .chars()
                                        .take_while(|c| !matches!(c, '*' | '?' | '['))
                                        .collect();
                                    let prefix = prefix.trim();
                                    if prefix.len() < 3 {
                                        continue;
                                    }
                                    let dir = format!("/{}", prefix.trim_end_matches('/'));
                                    if files.iter().any(|f| {
                                        f.ends_with(&dir)
                                            || f.contains(&format!("{dir}/"))
                                            || f.starts_with(prefix)
                                    }) {
                                        let _ = s.link_session_part(
                                            &cid,
                                            part.id,
                                            &project_key,
                                            "touch",
                                        );
                                        linked += 1;
                                        if linked >= 5 {
                                            break 'parts;
                                        }
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    // Not every failure is the JOB's fault, and only the job's
                    // own failures may march it toward death:
                    //   · a RATE LIMIT is the account's state, not a defect —
                    //     this is the one that killed the standup. `is_rate_limited`
                    //     was fed the first 500 chars of codex's stderr, which is
                    //     pure banner, so it never matched: the 900s backoff never
                    //     fired, all 3 attempts burned inside the SAME ~3 minutes of
                    //     one transient window, and the session was blacklisted
                    //     forever. Now the stderr TAIL reaches it — and a rate limit
                    //     costs the job no attempt at all.
                    //   · `transcript:` — a rollout with no messages yet. Same class
                    //     as the not-yet-found transcript already deferred before the
                    //     claim; the world isn't ready, the job is fine.
                    // Both DEFER: requeue, refund the claim's speculative attempt,
                    // and go to the BACK of the queue (a not-before, escalating) so
                    // this session can never hold the head and starve the rest.
                    //
                    // `is_rate_limited` is STAGE-GATED (`cli:` only): the model's
                    // own prose rides in the `parse:` error now, and an ungated
                    // substring grep read "…happy to gene-RATE a summary…" as a
                    // rate limit — which, since a rate limit defers and refunds,
                    // made a permanent parse failure an IMMORTAL job. Belt and
                    // braces: even if the classifier is wrong, the store bounds
                    // the defers (MAX_SUMMARY_DEFERS) and the job dies anyway.
                    let rate_limited = is_rate_limited(&e);
                    let retryable = rate_limited || e.starts_with("transcript:");
                    {
                        let s = store.lock().unwrap_or_else(|p| p.into_inner());
                        if retryable {
                            let base = if rate_limited {
                                returnchannel::RATE_LIMIT_DEFER_SECS
                            } else {
                                returnchannel::NOT_READY_DEFER_SECS
                            };
                            let _ = s.defer_summary_job(job_id, base, &e);
                        } else {
                            // a real generation failure: retries up to 3, then
                            // → dead, which cools the session off (never silent,
                            // never permanent).
                            let _ = s.finish_summary_job(job_id, Some(&e));
                        }
                    }
                    if end_note {
                        // the summary failed but the trail must not dangle.
                        let s = store.lock().unwrap_or_else(|p| p.into_inner());
                        if let Some((pid, pkey)) = s.dispatch_of(&cid) {
                            let src = format!("sess-{cid}");
                            let already = s
                                .notes_for_part(pid)
                                .unwrap_or_default()
                                .iter()
                                .find(|n| n.kind == "session" && n.source == src)
                                .is_some_and(|n| n.text.starts_with('■'));
                            if !already {
                                let _ = s.add_note(&pkey, pid, "session", "■ session ended", &src);
                            }
                        }
                    }
                    let now = orchestrator_core::registry::now_secs();
                    let backoff = if rate_limited {
                        // back off HARD: never compete with his interactive
                        // sessions. This one IS global — the provider is refusing
                        // the ACCOUNT, so no other session would fare better.
                        returnchannel::RATE_LIMIT_DEFER_SECS
                    } else if retryable {
                        // not-ready is about THIS job's world, not the worker's:
                        // the job's own not-before holds it back, and the worker
                        // is free to summarize somebody else on the next tick.
                        0
                    } else {
                        // a real failure still spaces retries so a reliably-
                        // failing session can't spin the worker and drain the
                        // shared budget every tick (review finding 2).
                        60
                    };
                    cooldown.store(now + backoff, Ordering::Relaxed);
                    eprintln!("[kod] summary failed for {cid}: {e}");
                }
            }
            running.store(false, Ordering::Relaxed);
        });
    }

    /// Persist NEW TurnEnd summaries to the store so the Today digest survives
    /// restarts (the live event ring is RAM-only, 256-capped). Only sessions with
    /// a stable cli_session_id (claude/codex — the ones that emit TurnEnd) are
    /// recorded; `last_persisted_seq` skips already-written events, and the store
    /// dedupes on (sess, at_ms) so a resume/backfill can't double-count a turn.
    ///
    /// docs/019 slice 3 ALSO rides this observation loop (the tap is LIVE, not at
    /// a summarize that never fires):
    ///   - TOOL events → the weighted touch tap: `target` ∩ node anchors →
    ///     `record_touch` (arithmetic, auto-applied without a card).
    ///   - TURNEND messages → the session's own map verbs (`map here|done|note`).
    fn persist_events(&mut self) {
        use orchestrator_host::{SessionEventKind, ToolVerb};
        let infos = self.host.infos();
        let Ok(store) = self.store.lock() else { return };
        // anchors/names per project, loaded once per touched project (not per
        // tick — most ticks have no new events at all).
        let mut parts_cache: std::collections::HashMap<String, Vec<DesignPart>> =
            std::collections::HashMap::new();
        for info in &infos {
            let Some(sess) = info.cli_session_id.as_deref() else {
                continue;
            };
            let last = self.last_persisted_seq.get(&info.id).copied().unwrap_or(0);
            let events: Vec<_> = self
                .host
                .events_for(info.id)
                .into_iter()
                .filter(|e| e.seq > last)
                .collect();
            if events.is_empty() {
                continue;
            }
            let max_seq = events.iter().map(|e| e.seq).max().unwrap_or(last);
            let slug = info.project_slug.clone();
            let parts = parts_cache
                .entry(slug.clone())
                .or_insert_with(|| store.load_tree(&slug).unwrap_or_default());
            for e in &events {
                match &e.kind {
                    SessionEventKind::TurnEnd { summary } => {
                        let s = summary.trim();
                        if !s.is_empty() {
                            let _ = store.record_event(
                                sess,
                                &slug,
                                e.at_ms,
                                &s.chars().take(200).collect::<String>(),
                            );
                        }
                        // MAP VERBS (docs/019 T11): the session steers its own
                        // chip. Fenced + minimal — see apply_map_verb.
                        for v in returnchannel::scan_map_verbs(summary, 4) {
                            Self::apply_map_verb(&store, sess, &slug, parts, v);
                        }
                    }
                    SessionEventKind::Tool { verb, target } => {
                        // LIVE TOUCH TAP (docs/019 slice 3): the reduced event
                        // already carries the file `target` (a mutation/read
                        // path); Bash/search targets are the tool NAME, not a
                        // path, so they intersect no anchor and write nothing.
                        self.tap_events_seen = self.tap_events_seen.saturating_add(1);
                        let weight = match verb {
                            ToolVerb::Edited | ToolVerb::Created => {
                                returnchannel::touch_weight(returnchannel::TouchVerb::Mutate)
                            }
                            ToolVerb::Read => {
                                returnchannel::touch_weight(returnchannel::TouchVerb::Read)
                            }
                            ToolVerb::Ran | ToolVerb::Used => 0.0,
                        };
                        if weight > 0.0 && !target.trim().is_empty() {
                            let mut n = 0usize;
                            for p in parts.iter().filter(|p| !p.anchors.is_empty()) {
                                if returnchannel::touch_intersects_anchor(target, &p.anchors) {
                                    if store.record_touch(sess, p.id, &slug, weight).is_ok() {
                                        self.tap_rows_written =
                                            self.tap_rows_written.saturating_add(1);
                                    }
                                    n += 1;
                                    if n >= 3 {
                                        break; // one turn rarely spans >3 anchored nodes
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            if max_seq > last {
                self.last_persisted_seq.insert(info.id, max_seq);
            }
        }
    }

    /// Apply ONE session-emitted map verb (docs/019 T11), fenced + minimal:
    ///   - `here`  → a `declared` link (the session declaring new territory;
    ///     observation-grade, safe, upgrades a prior touch row).
    ///   - `note`  → an append-only part_note in the session's own words (one of
    ///     the exactly-two auto-applied-without-a-card writes) — also FENCED to
    ///     the session's dispatch/declared subtree (commitment 1), rejected loud
    ///     if the target is outside it.
    ///   - `done`  → a CARDED SetStatus→done proposal (every part-row status flip
    ///     is carded per the trust rules), FENCED to the session's own
    ///     dispatch/declared subtree, carrying the one-line shipped-claim as
    ///     evidence. A target outside the fence is rejected loudly, never applied.
    ///
    /// A target that matches no node is skipped (logged) — never a guess.
    fn apply_map_verb(
        store: &orchestrator_store::Store,
        sess: &str,
        slug: &str,
        parts: &[DesignPart],
        verb: returnchannel::MapVerb,
    ) {
        use returnchannel::MapVerb;
        // case-insensitive exact name match; ties prefer an in-fence node.
        let fence = Self::session_fence_ids(store, sess, slug, parts);
        let find = |name: &str| -> Option<PartId> {
            let name_l = name.trim().to_lowercase();
            let mut hit: Option<PartId> = None;
            for p in parts {
                if p.name.trim().to_lowercase() == name_l {
                    if fence.contains(&p.id) {
                        return Some(p.id); // in-fence match wins outright
                    }
                    hit.get_or_insert(p.id);
                }
            }
            hit
        };
        match verb {
            MapVerb::Here { target } => {
                if let Some(pid) = find(&target) {
                    let _ = store.link_session_part(sess, pid, slug, "declared");
                } else {
                    eprintln!("[kod] map here: no node named {target:?} in {slug}");
                }
            }
            MapVerb::Note { target, text } => {
                let Some(pid) = find(&target) else {
                    eprintln!("[kod] map note: no node named {target:?} in {slug}");
                    return;
                };
                if !fence.contains(&pid) {
                    // commitment 1: `map note` is fenced to the session's own
                    // dispatch/declared subtree, mirroring the changeset fence —
                    // a session annotates only where it works, never an arbitrary
                    // node (even an append-only note is a write to a part row).
                    eprintln!("[kod] map note: {target:?} is outside session {sess}'s dispatch/declared scope — rejected");
                    return;
                }
                let _ = store.add_note(slug, pid, "note", &text, &format!("sess-{sess}"));
            }
            MapVerb::Done { target, claim } => {
                let Some(pid) = find(&target) else {
                    eprintln!("[kod] map done: no node named {target:?} in {slug}");
                    return;
                };
                if !fence.contains(&pid) {
                    // the fence mirror of the changeset fence — a session may
                    // only claim its OWN dispatch/declared subtree done.
                    eprintln!("[kod] map done: {target:?} is outside session {sess}'s dispatch/declared scope — rejected");
                    return;
                }
                // CARDED (trust rules: every part-row status flip is carded).
                // The shipped-claim rides as the op's evidence — the user
                // sees the session's own words before the provisional done sticks.
                // status_source=Agent (the closest existing variant for a
                // session-proposed-then-accepted status; docs/019's dedicated
                // sess:<id> source is a reconciler/enum change deferred past this
                // wiring slice — the `sess:` card kind still attributes it).
                let op = DiffOp::SetStatus {
                    id: pid,
                    lifecycle: Lifecycle::Done,
                    source: orchestrator_store::StatusSource::Agent,
                };
                let kind = format!("sess:{sess}");
                // COALESCE (review finding 9): a session restating `map done X`
                // across turns must not stack duplicate cards. Drop any prior
                // open sess:<sess> card that already proposes done for THIS node.
                for pd in store.pending_diffs(slug).unwrap_or_default() {
                    if pd.kind == kind
                        && pd
                            .ops
                            .iter()
                            .any(|o| matches!(o, DiffOp::SetStatus { id, .. } if *id == pid))
                    {
                        let _ = store.drop_pending_diff(pd.id);
                    }
                }
                let _ = store.add_pending_diff_with_evidence(
                    slug,
                    &kind,
                    &[op],
                    &[Some(format!("map done — {claim}"))],
                );
            }
        }
    }

    /// The part ids inside a session's dispatch/declared FENCE (docs/019: the
    /// session's own subtree) — every dispatch/declared link target plus all its
    /// descendants. `map done`/name-match resolution honor this fence.
    fn session_fence_ids(
        store: &orchestrator_store::Store,
        sess: &str,
        slug: &str,
        parts: &[DesignPart],
    ) -> std::collections::HashSet<PartId> {
        let roots: Vec<PartId> = store
            .session_parts(slug)
            .into_iter()
            .filter(|r| r.cli_session_id == sess && (r.role == "dispatch" || r.role == "declared"))
            .map(|r| r.part_id)
            .collect();
        let mut fence: std::collections::HashSet<PartId> = roots.iter().copied().collect();
        // walk every node; include it if an ancestor is a fence root.
        let by_id: std::collections::HashMap<PartId, Option<PartId>> =
            parts.iter().map(|p| (p.id, p.parent_id)).collect();
        for p in parts {
            let mut cur = Some(p.id);
            let mut hops = 0;
            while let Some(id) = cur {
                hops += 1;
                if hops > parts.len() {
                    break;
                }
                if fence.contains(&id) {
                    fence.insert(p.id);
                    break;
                }
                cur = by_id.get(&id).copied().flatten();
            }
        }
        fence
    }

    /// Backfill a session's timeline from its on-disk transcript, once (#9 §4).
    /// Resolves the path by CLI (core knows the disk layout) on a background
    /// thread, then asks the backend to parse + push — the events stream back via
    /// the usual dirty/repaint path (local: direct; daemon: an Events delta). For
    /// claude this is a no-op on a fresh session (the host's pre-attach cutoff
    /// filters everything); it only fills the history of a resumed/imported one.
    pub(crate) fn start_transcript_backfill(
        &mut self,
        id: SessionId,
        kind: CliKind,
        cli_id: String,
        _cx: &mut Context<Self>,
    ) {
        if !self.backfilled.insert(id) {
            return; // already done this session
        }
        let host = self.host.clone();
        std::thread::spawn(move || {
            let path = match kind {
                CliKind::Codex => orchestrator_core::scan::codex_rollout_path(&cli_id),
                CliKind::Claude => orchestrator_core::scan::claude_transcript_path(&cli_id),
                CliKind::Shell => None,
            };
            if let Some(path) = path {
                host.backfill_transcript(id, &path);
            }
        });
    }
}
