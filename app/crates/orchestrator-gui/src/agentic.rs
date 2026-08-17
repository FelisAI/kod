use gpui::*;
use crate::*;


impl Orchestrator {

    /// Kick the one-shot LLM structure extraction for the open project on a
    /// background thread (it shells `claude -p` — quota + ~30s). The poll task
    /// lands the result as a pending accept-diff.
    pub(crate) fn start_extract(&mut self, cx: &mut Context<Self>) {
        // OSS gate (features.rs): the map seed-extract LLM call is compiled out of
        // the default build. The UI entry points are already hidden; this is the
        // load-bearing backstop that guarantees no future caller can spend on it.
        if !crate::features::MAP_ENABLED {
            return;
        }
        let proj = self.project();
        let Some(path) = proj.path.clone() else {
            return;
        };
        let slug = proj.slug.clone();
        if self.extracting.as_deref() == Some(&slug) {
            return;
        }
        self.extracting = Some(slug.clone());
        let slot = self.extract_slot.clone();
        std::thread::spawn(move || {
            let res = extract::extract_tree(&path);
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some((slug, res));
        });
        let slot = self.extract_slot.clone();
        cx.spawn(async move |this, cx| loop {
            Timer::after(std::time::Duration::from_millis(200)).await;
            let ready = slot.lock().ok().and_then(|mut g| g.take());
            let done = this
                .update(cx, |this, cx| {
                    if let Some((slug, res)) = ready {
                        this.extracting = None;
                        match res {
                            Ok(ops) if !ops.is_empty() => {
                                if let Ok(store) = this.store.lock() {
                                    let _ = store.add_pending_diff(&slug, "seed", &ops);
                                }
                            }
                            Ok(_) => this.term_error = Some("extraction found no areas".into()),
                            Err(e) => this.term_error = Some(e),
                        }
                        cx.notify();
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(true);
            if done {
                break;
            }
        })
        .detach();
    }

    /// docs/019 slice 2 — the AGENTIC CARTOGRAPHER dispatch (seed / re-ground /
    /// expand / rework / cmd-bar intent). User-invoked, high-capability, on
    /// its OWN small hourly ledger (never the cheaper plumbing lane). The result
    /// ALWAYS lands as a carded changeset (never applied directly). Blocking
    /// claude runs off the UI thread; a poll turns the result into the
    /// changeset + arms the review overlay. Progress is visible on a card (a
    /// silent multi-minute call reads as a hang).
    pub(crate) fn start_agentic_run(&mut self, kind: AgenticKind, cx: &mut Context<Self>) {
        // OSS gate (features.rs): the agentic cartographer (seed / expand / rework)
        // is compiled out of the default build. Backstop behind the hidden UI.
        if !crate::features::MAP_ENABLED {
            return;
        }
        let proj = self.project();
        let Some(root) = proj.path.clone() else {
            self.term_error = Some("this project has no local path to read".into());
            cx.notify();
            return;
        };
        let slug = proj.slug.clone();
        if self.agentic.is_some() {
            return; // one user-invoked structural run at a time
        }
        // the structural lane's OWN ledger (docs/019 T3): prune to the trailing
        // hour, then gate — a burst of summaries can never starve this.
        let now = orchestrator_core::registry::now_secs();
        self.map_intel_times
            .retain(|t| now.saturating_sub(*t) < 3600);
        if !extract::cartographer::map_intel_rate_allows(
            &self.map_intel_times,
            now,
            extract::cartographer::MAP_INTEL_HOURLY_CAP,
        ) {
            self.term_error = Some(format!(
                "re-ground is rate-limited to {}/hr — try again shortly",
                extract::cartographer::MAP_INTEL_HOURLY_CAP
            ));
            cx.notify();
            return;
        }

        // ---- gather grounding under one store lock (off-thread thereafter) ----
        let (
            label,
            scope,
            title,
            instruction,
            origin_run,
            existing_tree,
            node_path,
            node_detail,
            taxonomy_note,
            intent,
            rework_data,
        ) = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let parts = store.load_tree(&slug).unwrap_or_default();
            let ts = now;
            match kind {
                AgenticKind::Seed { intent } => {
                    let fresh = parts.is_empty();
                    let existing = (!fresh).then(|| extract::serialize_tree_for_llm(&parts));
                    let (label, origin) = if fresh {
                        (
                            "Reading your docs & code to draft the map…".to_string(),
                            format!("seed:{ts}"),
                        )
                    } else {
                        (
                            "Re-grounding from your docs & code…".to_string(),
                            format!("reground:{ts}"),
                        )
                    };
                    let title = match &intent {
                        Some(t) if !t.trim().is_empty() => format!("Re-ground: {}", t.trim()),
                        _ if fresh => "Draft map from docs & code".to_string(),
                        _ => "Re-ground map from docs & code".to_string(),
                    };
                    let instruction = "Every proposed node cites a real repo file; unverified ones are flagged and excluded from Accept all.".to_string();
                    (
                        label,
                        None,
                        title,
                        instruction,
                        origin,
                        existing,
                        String::new(),
                        String::new(),
                        String::new(),
                        intent,
                        None,
                    )
                }
                AgenticKind::Fenced {
                    node,
                    rework,
                    intent,
                } => {
                    let path = Self::node_breadcrumb(&parts, node);
                    let detail = parts
                        .iter()
                        .find(|p| p.id == node)
                        .map(|p| {
                            let d = p.detail_md.clone();
                            if d.trim().is_empty() {
                                p.detail.clone()
                            } else {
                                d
                            }
                        })
                        .unwrap_or_default();
                    let tax = store.taxonomy_note(&slug).unwrap_or_default();
                    let name = parts
                        .iter()
                        .find(|p| p.id == node)
                        .map(|p| p.name.clone())
                        .unwrap_or_else(|| format!("#{node}"));
                    let verb = if rework { "Rework" } else { "Expand" };
                    let label = format!(
                        "{}ing “{name}” — reading its docs & anchored code…",
                        if rework { "Rework" } else { "Expand" }
                    );
                    let title = match &intent {
                        Some(t) if !t.trim().is_empty() => format!("{verb} {name}: {}", t.trim()),
                        _ => format!("{verb} {name}"),
                    };
                    let instruction = format!("Fenced to “{name}” — proposals land under it; every node cites a real repo file.");
                    // rework needs the CURRENT subtree serialized with real ids
                    // (so the agent can Move/Rename/Remove them); expand (add-only)
                    // does not. Build it here under the lock; the thread owns it.
                    let rework_data = rework
                        .then(|| extract::cartographer::serialize_subtree_for_rework(&parts, node));
                    (
                        label,
                        Some(node),
                        title,
                        instruction,
                        format!("{}:{ts}", verb.to_lowercase()),
                        None,
                        path,
                        detail,
                        tax,
                        intent,
                        rework_data,
                    )
                }
            }
        };

        // the intent (cmd-bar text / one-line box) grounds a fenced run by
        // riding in the node's detail; a whole-map run carries it as a param.
        let node_detail_full = match (&intent, scope) {
            (Some(t), Some(_)) if !t.trim().is_empty() => {
                if node_detail.trim().is_empty() {
                    format!("User's intent: {}", t.trim())
                } else {
                    format!("{node_detail}\nUser's intent: {}", t.trim())
                }
            }
            _ => node_detail,
        };
        let whole_map_intent = if scope.is_none() {
            intent.clone()
        } else {
            None
        };

        self.map_intel_times.push(now);
        self.agentic = Some(AgenticRun {
            slug: slug.clone(),
            label,
            scope,
            started: std::time::Instant::now(),
        });

        let slot = self.agentic_slot.clone();
        std::thread::spawn(move || {
            let mut transcript = String::new();
            let parsed = match scope {
                None => extract::cartographer::seed_map_agentic(
                    &root,
                    existing_tree.as_deref(),
                    whole_map_intent.as_deref(),
                    &mut transcript,
                ),
                // rework (FULL op whitelist, restructure) vs expand (add-only):
                // the presence of the serialized subtree picks the lane.
                Some(node) => match rework_data {
                    Some((subtree_json, subtree_ids, subtree_nodes)) => {
                        extract::cartographer::rework_subtree_agentic(
                            &root,
                            node,
                            &node_path,
                            &node_detail_full,
                            &taxonomy_note,
                            &subtree_json,
                            &subtree_ids,
                            &subtree_nodes,
                            &mut transcript,
                        )
                    }
                    None => extract::cartographer::expand_subtree_agentic(
                        &root,
                        node,
                        &node_path,
                        &node_detail_full,
                        &taxonomy_note,
                        &mut transcript,
                    ),
                },
            };
            let landed = parsed.map(|r| AgenticLanded {
                title,
                instruction,
                scope,
                origin_run,
                ops: r.ops,
                evidence: r.evidence,
                flagged: r.flagged,
            });
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some((slug, landed));
        });

        let slot = self.agentic_slot.clone();
        cx.spawn(async move |this, cx| loop {
            Timer::after(std::time::Duration::from_millis(300)).await;
            let ready = slot.lock().ok().and_then(|mut g| g.take());
            let done = this
                .update(cx, |this, cx| {
                    match ready {
                        Some((slug, res)) => {
                            this.agentic = None;
                            match res {
                                Ok(landed) if !landed.ops.is_empty() => {
                                    this.land_agentic_changeset(&slug, landed)
                                }
                                Ok(_) => {
                                    this.term_error = Some(
                                        "Claude proposed no cited changes — nothing to review."
                                            .into(),
                                    )
                                }
                                Err(e) => this.term_error = Some(format!("re-ground failed: {e}")),
                            }
                            cx.notify();
                            true
                        }
                        None => {
                            // repaint the elapsed timer on the progress card so a
                            // multi-minute call never looks hung.
                            if this.agentic.is_some() {
                                cx.notify();
                                false
                            } else {
                                true // cancelled elsewhere
                            }
                        }
                    }
                })
                .unwrap_or(true);
            if done {
                break;
            }
        })
        .detach();
    }

    /// Land a cartographer proposal as a carded changeset (HOUSE RULE: machine
    /// edits are ALWAYS carded — create_changeset + add_pending_diff_full +
    /// link_pending_to_changeset, the slice-1c surface) and arm the review
    /// overlay so it surfaces immediately.
    fn land_agentic_changeset(&mut self, slug: &str, landed: AgenticLanded) {
        let cs = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let Ok(cs) = store.create_changeset(
                slug,
                &landed.title,
                &landed.instruction,
                landed.scope,
                &landed.origin_run,
            ) else {
                self.term_error = Some("couldn't open a changeset for the proposal".into());
                return;
            };
            match store.add_pending_diff_full(
                slug,
                "changeset",
                &landed.ops,
                &landed.evidence,
                &landed.flagged,
            ) {
                // link failure would leave the row with changeset_id=NULL, so it
                // would leak into the per-op proposal lane (review finding 6):
                // drop the orphaned row + reject the empty changeset.
                Ok(pid) => match store.link_pending_to_changeset(pid, cs) {
                    Ok(()) => Some(cs),
                    Err(_) => {
                        let _ = store.drop_pending_diff(pid);
                        let _ = store.set_changeset_status(cs, "rejected");
                        None
                    }
                },
                Err(_) => {
                    let _ = store.set_changeset_status(cs, "rejected");
                    None
                }
            }
        };
        if let Some(cs) = cs {
            self.review = Some(ChangesetReview {
                id: cs,
                ..Default::default()
            });
        }
    }

    /// The breadcrumb path to a node ("Engine ▸ Store ▸ Parser") for grounding
    /// a fenced run — the model's "home base" line.
    fn node_breadcrumb(parts: &[DesignPart], id: PartId) -> String {
        use std::collections::HashMap;
        let by_id: HashMap<PartId, &DesignPart> = parts.iter().map(|p| (p.id, p)).collect();
        let mut names = Vec::new();
        let mut cur = Some(id);
        while let Some(c) = cur {
            match by_id.get(&c) {
                Some(p) => {
                    names.push(p.name.clone());
                    cur = p.parent_id;
                }
                None => break,
            }
        }
        names.reverse();
        names.join(" ▸ ")
    }

}
