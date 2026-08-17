use crate::*;


impl Orchestrator {

    /// #10 slice 2 — the LIVING loop: fold every NEW session summary for a
    /// project into ONE isolated claude call proposing evidence-bearing map
    /// updates. Batched (6h window; 10min when the map is open — you're
    /// looking at it), budget-shared with the summarizer, watermark advances
    /// even on empty proposals (the designed common output — otherwise the
    /// same summaries re-spend quota forever).
    /// Drift detector (docs/011 slice 3, sequenced LAST — it needs dispatch
    /// traffic as its freshness signal): a Building node with no linked
    /// activity in 14 days gets a SetStatus→Todo nudge through the SAME
    /// pending/✓✕ channel as everything else. Reject = 30d snooze. One sweep
    /// per project per 24h; pure detector, no LLM, no lock held across frames.
    pub(crate) fn maybe_sweep_drift(&mut self) {
        const SWEEP_GAP: u64 = 24 * 3600;
        const QUIET: u64 = 14 * 24 * 3600;
        const SNOOZE: u64 = 30 * 24 * 3600;
        let now_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let slugs: Vec<String> = self.projects.iter().map(|p| p.slug.clone()).collect();
        let Ok(store) = self.store.lock() else { return };
        for slug in slugs {
            let at = store
                .get_setting(&format!("map_drift_at:{slug}"))
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            if now_s.saturating_sub(at) < SWEEP_GAP {
                continue;
            }
            let Ok(parts) = store.load_tree(&slug) else {
                continue;
            };
            // gate stamps AFTER the load succeeds — a transient failure must
            // not burn the project's slot for a whole day (review).
            let _ = store.set_setting(&format!("map_drift_at:{slug}"), &now_s.to_string());
            if parts.is_empty() {
                continue;
            }
            let activity = store.part_activity(&slug);
            let pending = store.pending_diffs(&slug).unwrap_or_default();
            let already: std::collections::HashSet<PartId> = pending
                .iter()
                .filter(|pd| pd.kind == "drift")
                .flat_map(|pd| pd.ops.iter())
                .filter_map(|op| match op {
                    DiffOp::SetStatus { id, .. } => Some(*id),
                    _ => None,
                })
                .collect();
            for id in orchestrator_store::quiet_building(&parts, &activity, now_s, QUIET) {
                if already.contains(&id) {
                    continue;
                }
                let snoozed = store
                    .get_setting(&format!("drift_snooze:{id}"))
                    .and_then(|v| v.parse::<u64>().ok())
                    .is_some_and(|t| now_s.saturating_sub(t) < SNOOZE);
                if snoozed {
                    continue;
                }
                let days = activity
                    .get(&id)
                    .map(|t| now_s.saturating_sub(*t) / 86_400)
                    .unwrap_or(0);
                let ev = if days > 0 {
                    format!("marked building, but no linked activity for {days} days")
                } else {
                    "marked building, but nothing was ever linked to it".to_string()
                };
                let op = DiffOp::SetStatus {
                    id,
                    lifecycle: Lifecycle::Todo,
                    source: orchestrator_store::StatusSource::Agent,
                };
                let _ = store.add_pending_diff_with_evidence(&slug, "drift", &[op], &[Some(ev)]);
            }
        }
    }

    /// Re-file drift sweep (docs/019 chips): when a LIVE session's observed-touch
    /// mass on some OTHER node outweighs its dispatched home (≈3× for ≥15min), the
    /// map SURFACES a re-file suggestion on that node — observation PROPOSES, never
    /// rewrites. Deduped per (session, node); throttled ~2min/project.
    ///
    /// v1 SURFACING is an append-only note pointing at the outline's relink verb
    /// (the trust-safe "also touched"→intent lane). A one-click interactive re-file
    /// CARD is deferred: re-file is a session_part LINK op, and the changeset
    /// surface carries tree DiffOps only — a link-op card type is new machinery.
    pub(crate) fn maybe_sweep_refile(&mut self) {
        const GAP: u64 = 120;
        let now_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // only LIVE sessions can be drifting; a dead session's history is settled.
        let alive: std::collections::HashSet<String> = self
            .host
            .infos()
            .into_iter()
            .filter(|i| i.alive)
            .filter_map(|i| i.cli_session_id)
            .collect();
        if alive.is_empty() {
            return;
        }
        let slugs: Vec<String> = self.projects.iter().map(|p| p.slug.clone()).collect();
        let Ok(store) = self.store.lock() else { return };
        for slug in slugs {
            let at = store
                .get_setting(&format!("map_refile_at:{slug}"))
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            if now_s.saturating_sub(at) < GAP {
                continue;
            }
            let sparts = store.session_parts(&slug);
            if sparts.is_empty() {
                continue;
            }
            let _ = store.set_setting(&format!("map_refile_at:{slug}"), &now_s.to_string());
            let names: std::collections::HashMap<PartId, String> = store
                .load_tree(&slug)
                .unwrap_or_default()
                .into_iter()
                .map(|p| (p.id, p.name))
                .collect();
            // group rows by session.
            let mut by_sess: std::collections::HashMap<
                &str,
                Vec<&orchestrator_store::store::SessionPartRow>,
            > = std::collections::HashMap::new();
            for r in &sparts {
                if alive.contains(&r.cli_session_id) {
                    by_sess
                        .entry(r.cli_session_id.as_str())
                        .or_default()
                        .push(r);
                }
            }
            for (sess, rows) in by_sess {
                let Some(dispatch) = rows.iter().find(|r| r.role == "dispatch") else {
                    continue;
                };
                let home = names
                    .get(&dispatch.part_id)
                    .cloned()
                    .unwrap_or_else(|| format!("#{}", dispatch.part_id));
                for r in rows
                    .iter()
                    .filter(|r| r.role == "touch" || r.role == "trail")
                {
                    let sustained = r
                        .last_touch_secs
                        .unwrap_or(r.at_secs)
                        .saturating_sub(r.at_secs);
                    if !returnchannel::drift_should_refile(dispatch.weight, r.weight, sustained) {
                        continue;
                    }
                    // dedup: one re-file note per (session, node).
                    let src = format!("refile-{sess}");
                    let exists = store
                        .notes_for_part(r.part_id)
                        .unwrap_or_default()
                        .iter()
                        .any(|n| n.source == src);
                    if exists {
                        continue;
                    }
                    let msg = format!("↪ this session was dispatched from “{home}” but is working here now — re-file its dispatch? (relink from this node's outline)");
                    let _ = store.add_note(&slug, r.part_id, "note", &msg, &src);
                }
            }
        }
    }

    pub(crate) fn maybe_spawn_map_proposal(&mut self) {
        use std::sync::atomic::Ordering;
        if !self.summaries_on || self.sum_running.load(Ordering::Relaxed) {
            return; // same lane + budget family as the summarizer
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
        let map_open = self.screen == Screen::Workspace && self.mode == Mode::MapOutline;
        let cur_slug = self.project().slug.clone();
        // find one eligible project (map-open project gets the fast window)
        let mut job: Option<(String, Vec<SummaryRow>, u64, Vec<DesignPart>)> = None;
        {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            for p in &self.projects {
                let window = if map_open && p.slug == cur_slug {
                    600
                } else {
                    6 * 3600
                };
                if now_s.saturating_sub(store.last_map_proposal_secs(&p.slug)) < window {
                    continue;
                }
                let thru: u64 = store
                    .get_setting(&format!("map_prop_thru:{}", p.slug))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let new = store.summaries_since(&p.slug, thru).unwrap_or_default();
                if new.is_empty() {
                    continue;
                }
                let parts = store.load_tree(&p.slug).unwrap_or_default();
                if parts.is_empty() {
                    continue; // the seed flow owns the first draft
                }
                job = Some((p.slug.clone(), new, thru, parts));
                break;
            }
        }
        let Some((slug, rows, _thru, parts)) = job else {
            return;
        };
        // dispatch hints: bindings for the sessions IN THIS BATCH only — every
        // dispatch row ever would ride every future prompt as noise (review).
        let dispatched: Vec<(String, i64)> = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let dmap = store.session_dispatch_map(&slug);
            rows.iter()
                .filter_map(|r| {
                    dmap.get(&r.sess)
                        .map(|pid| (outlinepane::id8(&r.sess), *pid))
                })
                .collect()
        };
        self.sum_job_times.push(now_s);
        self.sum_running.store(true, Ordering::Relaxed);
        let store = self.store.clone();
        let running = self.sum_running.clone();
        let cooldown = self.sum_cooldown_until.clone();
        std::thread::spawn(move || {
            // gather (pure, no lock needed — inputs already snapshotted)
            let tree_txt = extract::serialize_tree_for_llm(&parts);
            let valid_ids: std::collections::HashSet<i64> = parts.iter().map(|p| p.id).collect();
            let current: std::collections::HashMap<i64, (String, String)> = parts
                .iter()
                .map(|p| (p.id, (p.name.clone(), p.detail.clone())))
                .collect();
            let mut summaries_txt = String::new();
            for r in &rows {
                summaries_txt.push_str(&format!(
                    "SESSION {}\nGOAL: {}\nHEADLINE: {}\nNEXT: {}\n",
                    r.sess, r.goal, r.headline, r.next_action
                ));
                if let Ok(bullets) = serde_json::from_str::<Vec<String>>(&r.detail_json) {
                    for b in bullets {
                        summaries_txt.push_str(&format!("- {b}\n"));
                    }
                }
                summaries_txt.push('\n');
                if summaries_txt.len() > 6000 {
                    break;
                }
            }
            let mut files: Vec<String> = Vec::new();
            for r in &rows {
                for f in extract::files_touched(
                    std::path::Path::new(&r.src_path),
                    r.src_path.contains("/.codex/"),
                    20,
                ) {
                    if !files.contains(&f) {
                        files.push(f);
                    }
                }
            }
            let files_txt = if files.is_empty() {
                "(none)".to_string()
            } else {
                files.join("\n")
            };
            let newest_sess = rows.last().map(|r| r.sess.clone()).unwrap_or_default();
            let thru_new = rows.iter().map(|r| r.at_ms).max().unwrap_or(0);
            match extract::propose_map_updates(
                &tree_txt,
                &summaries_txt,
                &files_txt,
                &dispatched,
                &valid_ids,
                &current,
            ) {
                Ok(prop) => {
                    let s = store.lock().unwrap_or_else(|e| e.into_inner());
                    // ALWAYS advance the watermark — empty ops is success.
                    let _ = s.set_setting(&format!("map_prop_thru:{slug}"), &thru_new.to_string());
                    let _ = s
                        .set_last_map_proposal_secs(&slug, orchestrator_core::registry::now_secs());
                    if !prop.ops.is_empty() {
                        // COALESCE (docs/019 ruling 6 / card-volume amendment): a
                        // newer summary SUPERSEDES this session's un-reviewed open
                        // card in place — drop the prior `summary:<sess>` singleton,
                        // then land the fresh full proposal. Auto summary cards
                        // never STACK per session (the ≤10-open-cards bound). Only
                        // this session's own card is superseded; other sessions'
                        // cards persist until reviewed.
                        let kind = format!("summary:{newest_sess}");
                        let prior: Vec<i64> = s
                            .pending_diffs(&slug)
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|pd| pd.kind == kind && pd.changeset_id.is_none())
                            .map(|pd| pd.id)
                            .collect();
                        for id in prior {
                            let _ = s.drop_pending_diff(id);
                        }
                        let (ops, ev): (Vec<DiffOp>, Vec<Option<String>>) =
                            prop.ops.into_iter().map(|(op, e)| (op, Some(e))).unzip();
                        if !ops.is_empty() {
                            let _ = s.add_pending_diff_with_evidence(&slug, &kind, &ops, &ev);
                        }
                    }
                }
                Err(e) => {
                    if is_rate_limited(&e) {
                        cooldown.store(
                            orchestrator_core::registry::now_secs() + 900,
                            Ordering::Relaxed,
                        );
                    }
                    eprintln!("[kod] map proposal failed for {slug}: {e}");
                }
            }
            running.store(false, Ordering::Relaxed);
        });
    }

    /// Fold new real-session summaries into the durable memory graph. The LLM
    /// only proposes typed candidates; the store's native engine verifies
    /// evidence, skips duplicates/no-ops, supersedes stale memories, and owns the
    /// persisted graph.
    pub(crate) fn maybe_spawn_memory_proposal(&mut self) {
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
        let map_open = self.screen == Screen::Workspace && self.mode == Mode::MapOutline;
        let cur_slug = self.project().slug.clone();
        let mut job: Option<(String, std::path::PathBuf, Vec<MemoryDocument>, u64)> = None;
        {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            for p in &self.projects {
                let Some(root) = p.path.clone() else {
                    continue;
                };
                let window = if map_open && p.slug == cur_slug {
                    600
                } else {
                    6 * 3600
                };
                if now_s.saturating_sub(store.last_memory_proposal_secs(&p.slug)) < window {
                    continue;
                }
                let thru: u64 = store
                    .get_setting(&format!("memory_prop_thru:{}", p.slug))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let (documents, thru_new) = store
                    .summary_memory_documents_since_with_watermark(&p.slug, thru)
                    .unwrap_or_else(|_| (Vec::new(), thru));
                if documents.is_empty() {
                    continue;
                }
                job = Some((p.slug.clone(), root, documents, thru_new));
                break;
            }
        }
        let Some((slug, root, documents, thru_new)) = job else {
            return;
        };
        self.sum_job_times.push(now_s);
        self.sum_running.store(true, Ordering::Relaxed);
        let store = self.store.clone();
        let running = self.sum_running.clone();
        let cooldown = self.sum_cooldown_until.clone();
        std::thread::spawn(move || {
            let mut transcript = String::new();
            match extract::memory_agent::propose_memory_candidates(
                &slug,
                &documents,
                &root,
                &mut transcript,
            ) {
                Ok(candidates) => {
                    let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
                    match s.apply_memory_candidates(
                        &slug,
                        &documents,
                        &candidates,
                        "memory_agent:background",
                        orchestrator_core::registry::now_secs(),
                    ) {
                        Ok(report) => {
                            let _ = s.set_setting(
                                &format!("memory_prop_thru:{slug}"),
                                &thru_new.to_string(),
                            );
                            let _ = s.set_last_memory_proposal_secs(
                                &slug,
                                orchestrator_core::registry::now_secs(),
                            );
                            if report.inserted > 0 {
                                eprintln!(
                                    "[kod] memory extraction inserted {} objects for {slug}",
                                    report.inserted
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!("[kod] memory apply failed for {slug}: {e}");
                        }
                    }
                }
                Err(e) => {
                    if is_rate_limited(&e) {
                        cooldown.store(
                            orchestrator_core::registry::now_secs() + 900,
                            Ordering::Relaxed,
                        );
                    }
                    eprintln!("[kod] memory extraction failed for {slug}: {e}");
                }
            }
            running.store(false, Ordering::Relaxed);
        });
    }

}
