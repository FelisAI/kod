use gpui::prelude::FluentBuilder;
use gpui::*;
use crate::*;


impl Orchestrator {

    /// Dissolve a container node into its parent (docs/019 ruling 2): build the
    /// canned deterministic changeset — Move its children out (order preserved),
    /// then Remove the husk — and surface it through the review UI. The machine
    /// cleans up its own mess (the Tech husk); one accept, one ⌘Z. The exact
    /// inverse of the old aspect-migration that created the husk.
    /// Create + surface a canned dissolve changeset for `id` (moves children
    /// out, then removes the husk). ONE seeding sequence shared by the explicit
    /// context-menu Dissolve and the auto-seeded Dissolve-Tech (review: was
    /// copy-pasted). If the pending rows fail to attach, the empty changeset is
    /// rejected so no empty card ever lingers (review finding 6). Returns the
    /// new changeset id, or None for a leaf / store error.
    fn open_dissolve_changeset(
        store: &Store,
        slug: &str,
        id: PartId,
        title: &str,
        instruction: &str,
    ) -> Option<i64> {
        let parts = store.load_tree(slug).unwrap_or_default();
        let ops = orchestrator_store::dissolve_node_ops(&parts, id);
        if ops.len() < 2 {
            return None; // a leaf has nothing to unwrap
        }
        // the fence root itself is in scope (docs/019: a rework pointed at the
        // node may remove its husk) — the Remove targets `id`.
        let cs = store
            .create_changeset(slug, title, instruction, Some(id), "canned")
            .ok()?;
        match store.add_pending_diff(slug, "changeset", &ops) {
            Ok(pid) => {
                let _ = store.link_pending_to_changeset(pid, cs);
                Some(cs)
            }
            Err(_) => {
                let _ = store.set_changeset_status(cs, "rejected");
                None
            }
        }
    }

    pub(crate) fn dissolve_node(&mut self, id: PartId, cx: &mut Context<Self>) {
        let slug = self.project().slug.clone();
        let cs = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let parts = store.load_tree(&slug).unwrap_or_default();
            let name = parts
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            let moves = parts.iter().filter(|p| p.parent_id == Some(id)).count();
            let title = format!("Restructure: dissolve {name}");
            let instruction = format!(
                "Move {moves} child area{} out of “{name}”, then remove the empty husk.",
                if moves == 1 { "" } else { "s" }
            );
            Self::open_dissolve_changeset(&store, &slug, id, &title, &instruction)
        };
        // an explicit gesture surfaces immediately: render prefers self.review
        // over the oldest-open changeset (review finding 4).
        if let Some(cs) = cs {
            self.review = Some(ChangesetReview {
                id: cs,
                ..Default::default()
            });
        }
        cx.notify();
    }

    /// Auto-seed the canned "Dissolve Tech" changeset (docs/019 slice 1c, Panel
    /// ruling 2 — the machine cleans its own Tech mess as the review UI's first
    /// customer). On OPENING a project whose tree still carries the legacy
    /// `build_aspect_migration` husk (a "Tech" ROOT wrapping the real areas —
    /// detected by `dissolve_tech_target`), offer ONE changeset that promotes
    /// the children back to root and removes the husk (`dissolve_node_ops`),
    /// surfaced through the same review card the user reviews by hand.
    ///
    /// Fires AT MOST ONCE per project: an `app_settings("dissolve_tech_offered:
    /// <slug>")` flag is stamped when the offer is materialized, so a REJECTED
    /// (or ignored) dissolve never respawns on the next launch. It is a
    /// PROPOSAL — the user may reject it and keep Tech. No `self.review` is
    /// armed: the map render picks the open changeset up from `open_changesets`
    /// and the overlay lazily attaches on the first toggle/edit (`review_mut`).
    pub(crate) fn maybe_seed_dissolve_tech(&self, slug: &str) {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let flag = format!("dissolve_tech_offered:{slug}");
        if store.get_setting(&flag).is_some() {
            return; // already offered once (accepted, rejected, or still open)
        }
        let parts = store.load_tree(slug).unwrap_or_default();
        let Some(tech) = orchestrator_store::dissolve_tech_target(&parts) else {
            return; // no husk — nothing to offer; the flag stays unset
        };
        let moves = parts.iter().filter(|p| p.parent_id == Some(tech)).count();
        let title = "Dissolve Tech — group by product area";
        let instruction = format!(
            "Promote {moves} area{} out of the legacy “Tech” wrapper back to the top level, then remove the empty husk.",
            if moves == 1 { "" } else { "s" }
        );
        // stamp the flag whether or not the rows materialize — the offer is
        // consumed once the husk is detected; a transient store error must not
        // re-propose a destructive restructure every launch (review finding 6).
        Self::open_dissolve_changeset(&store, slug, tech, title, &instruction);
        let _ = store.set_setting(&flag, "1");
    }

    /// Ensure `self.review` targets `cs_id` (a different open changeset, or a
    /// first interaction with a store-persisted one, resets the overlay), then
    /// hand back a mutable reference to it.
    fn review_mut(&mut self, cs_id: i64) -> &mut ChangesetReview {
        if self.review.as_ref().map(|r| r.id) != Some(cs_id) {
            self.review = Some(ChangesetReview {
                id: cs_id,
                ..Default::default()
            });
        }
        self.review.as_mut().expect("just set")
    }

    /// Per-op toggle-off (docs/019 T9): the ✕ on a diff row excludes JUST that
    /// op from accept-all; toggling again restores it. Indices are the stable
    /// global index (flatten_changeset_ops order).
    fn toggle_changeset_op(&mut self, cs_id: i64, idx: usize, cx: &mut Context<Self>) {
        let r = self.review_mut(cs_id);
        if !r.off.remove(&idx) {
            r.off.insert(idx);
        }
        cx.notify();
    }

    /// Open the inline editor on a proposed Add's name (edit-before-accept).
    fn begin_changeset_name_edit(
        &mut self,
        cs_id: i64,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.review_mut(cs_id); // sync the overlay before the editor reads it
        self.begin_outline_edit(outlinepane::EditSlot::ChangesetOpName(idx), window, cx);
    }

    /// The name shown in a changeset Add row / its inline editor (docs/019
    /// slice 1c): the user's prior edit if any, else the machine's proposal.
    /// Empty for a non-Add op or a stale index.
    pub(crate) fn changeset_op_name(&self, idx: usize) -> String {
        let Some(r) = &self.review else {
            return String::new();
        };
        if let Some(n) = r.names.get(&idx) {
            return n.clone();
        }
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let rows = store.changeset_pending(r.id).unwrap_or_default();
        match orchestrator_store::flatten_changeset_ops(&rows)
            .into_iter()
            .nth(idx)
        {
            Some((DiffOp::Add { name, .. }, _)) => name,
            _ => String::new(),
        }
    }

    /// Accept the changeset (docs/019 ruling 6, HOUSE RULE): rebuild the KEPT
    /// ops — drop toggled-off, apply Add-name edits — and apply them as ONE
    /// accept_diff_from on the compound-review lane (`human:review`), so one
    /// ⌘Z reverts the whole restructure. Then drop the pending rows and mark
    /// the changeset accepted (partial if any op was excluded).
    fn accept_changeset(&mut self, cs_id: i64, cx: &mut Context<Self>) {
        let slug = self.project().slug.clone();
        let (off, names) = self
            .review
            .as_ref()
            .filter(|r| r.id == cs_id)
            .map(|r| (r.off.clone(), r.names.clone()))
            .unwrap_or_default();
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let rows = store.changeset_pending(cs_id).unwrap_or_default();
        let flat = orchestrator_store::flatten_changeset_ops(&rows);
        let flags = orchestrator_store::flatten_changeset_flags(&rows);
        // plan the accept (docs/019 slice 2): APPLY the dependency-closed kept
        // set (verified/non-done minus toggled-off, plus flagged/done toggled
        // in); UNRESOLVED held/deferred ops must PERSIST, never be silently
        // dropped (review findings 1+2). A kept child of a held-back parent is
        // deferred, not orphaned to root.
        let (applied_idx, leftover_idx) = plan_changeset_accept(&flat, &flags, &off);
        let kept: Vec<DiffOp> = applied_idx
            .iter()
            // edit-before-accept: the tweaked name flows into the applied Add
            // (same helper the review card renders with — never diverges).
            .map(|&i| op_with_name_edit(&flat[i].0, i, Some(&names)))
            .collect();
        // ONE journal event for the whole restructure (never one-per-op — that
        // would let ⌘Z peel them off individually / half-apply the diff).
        if !kept.is_empty() {
            let _ = store.accept_diff_from(&slug, &kept, "human:review", None);
        }
        // drop the OLD rows; the leftover ops are re-persisted below.
        for pd in &rows {
            let _ = store.drop_pending_diff(pd.id);
        }
        // the ratified roots + organizing principle (docs/019 seed/re-ground):
        // once a whole-map seed/re-ground is accepted, write project.taxonomy_note
        // from the surviving root AREAS so every later expand/rework prompt is
        // fenced to the map's own grammar.
        let cs_meta = store
            .open_changesets(&slug)
            .into_iter()
            .find(|c| c.0 == cs_id);
        if let Some((_, _, _, None, origin, _)) = &cs_meta {
            if origin.starts_with("seed") || origin.starts_with("reground") {
                let roots: Vec<String> = store
                    .load_tree(&slug)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|p| {
                        p.parent_id.is_none() && matches!(p.kind, orchestrator_store::Kind::Area)
                    })
                    .map(|p| p.name)
                    .collect();
                if !roots.is_empty() {
                    let _ = store.set_taxonomy_note(
                        &slug,
                        &format!("organized by these top-level areas: {}", roots.join(" · ")),
                    );
                }
            }
        }
        // re-persist the UNRESOLVED ops so the user can still review them:
        // the changeset STAYS OPEN (never silently discarded — review 1). Only
        // when nothing is left does it close as accepted.
        if leftover_idx.is_empty() {
            let _ = store.set_changeset_status(cs_id, "accepted");
        } else {
            let ops: Vec<DiffOp> = leftover_idx.iter().map(|&i| flat[i].0.clone()).collect();
            let ev: Vec<Option<String>> = leftover_idx.iter().map(|&i| flat[i].1.clone()).collect();
            let fl: Vec<bool> = leftover_idx
                .iter()
                .map(|&i| flags.get(i).copied().unwrap_or(false))
                .collect();
            if let Ok(pid) = store.add_pending_diff_full(&slug, "changeset", &ops, &ev, &fl) {
                let _ = store.link_pending_to_changeset(pid, cs_id);
            }
            // status stays 'open' (created that way) so it keeps surfacing.
        }
        drop(store);
        // the inline name editor closes WITH the card — a live ChangesetOpName
        // slot left open would keep trapping the keystream after its changeset
        // is gone (review 1b: no invisible key sinks across a close).
        if matches!(
            self.outline_edit.active,
            Some(outlinepane::EditSlot::ChangesetOpName(_))
        ) {
            self.outline_edit = outlinepane::EditState::default();
        }
        self.review = None;
        cx.notify();
    }

    /// Reject the whole changeset (docs/019 T9): drop its pending rows, mark it
    /// rejected. Nothing was applied, so there is nothing to undo.
    fn reject_changeset(&mut self, cs_id: i64, cx: &mut Context<Self>) {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        for pd in store.changeset_pending(cs_id).unwrap_or_default() {
            let _ = store.drop_pending_diff(pd.id);
        }
        let _ = store.set_changeset_status(cs_id, "rejected");
        drop(store);
        // close a live name editor with the card (see accept_changeset).
        if matches!(
            self.outline_edit.active,
            Some(outlinepane::EditSlot::ChangesetOpName(_))
        ) {
            self.outline_edit = outlinepane::EditState::default();
        }
        self.review = None;
        cx.notify();
    }

    /// Accept (or dismiss) ONE op of the focused node's pending proposals —
    /// per-op review, never all-or-nothing (critique #10). Resolved by VALUE:
    /// the first currently-pending op equal to the one that was clicked, so a
    /// stale frame or double-click can never hit a neighbour (review 1b).
    pub(crate) fn resolve_outline_op(&mut self, target: &DiffOp, accept: bool, cx: &mut Context<Self>) {
        let slug = self.project().slug.clone();
        self.resolve_pending_op(&slug, None, target, accept, cx);
    }

    /// `kind_hint`: identical ops can live in DIFFERENT pending rows (a summary
    /// SetStatus and a drift SetStatus are byte-equal) — the caller that knows
    /// which row it rendered passes the kind so the ✕ side-effects (drift
    /// snooze) land on the right one (review).
    pub(crate) fn resolve_pending_op(
        &mut self,
        slug: &str,
        kind_hint: Option<&str>,
        target: &DiffOp,
        accept: bool,
        cx: &mut Context<Self>,
    ) {
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let pending = store.pending_diffs(slug).unwrap_or_default();
        for pd in pending.iter().filter(|pd| {
            pd.kind != "seed" && pd.changeset_id.is_none() && kind_hint.is_none_or(|k| pd.kind == k)
        }) {
            if let Some(opix) = pd.ops.iter().position(|op| op == target) {
                if accept {
                    // summary proposals journal their SOURCE SESSION (#10):
                    // kind = "summary:<cli id>" → origin summary + source_sess.
                    let (origin, sess) = match pd.kind.strip_prefix("summary:") {
                        Some(sess) => ("summary", Some(sess.to_string())),
                        None => ("agent", None),
                    };
                    let _ =
                        store.accept_diff_from(slug, &[target.clone()], origin, sess.as_deref());
                } else if pd.kind == "drift" {
                    // rejecting a drift nudge = "still on it" — snooze 30d so
                    // the detector doesn't re-nag (docs/011 slice 3).
                    if let DiffOp::SetStatus { id, .. } = target {
                        let now_s = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        let _ =
                            store.set_setting(&format!("drift_snooze:{id}"), &now_s.to_string());
                    }
                }
                // shrink the source row, KEEPING evidence aligned (a plain
                // add_pending_diff re-add would drop the survivors' quotes).
                let remaining: Vec<DiffOp> = pd
                    .ops
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != opix)
                    .map(|(_, o)| o.clone())
                    .collect();
                let remaining_ev: Vec<Option<String>> = pd
                    .evidence
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != opix)
                    .map(|(_, e)| e.clone())
                    .collect();
                let _ = store.drop_pending_diff(pd.id);
                if !remaining.is_empty() {
                    let _ = store.add_pending_diff_with_evidence(
                        slug,
                        &pd.kind,
                        &remaining,
                        &remaining_ev,
                    );
                }
                drop(store);
                cx.notify();
                return;
            }
        }
    }

    fn accept_pending(&mut self, pending_id: i64, ops: Vec<DiffOp>, cx: &mut Context<Self>) {
        let slug = self.project().slug.clone();
        if let Ok(mut store) = self.store.lock() {
            let _ = store.accept_diff(&slug, &ops);
            let _ = store.drop_pending_diff(pending_id);
        }
        cx.notify();
    }

    fn discard_pending(&mut self, pending_id: i64, cx: &mut Context<Self>) {
        if let Ok(store) = self.store.lock() {
            let _ = store.drop_pending_diff(pending_id);
        }
        cx.notify();
    }

    /// The seed proposal accept-diff card — the extracted tree, reviewed before
    /// it sticks (never silently authoritative, docs/016).
    pub(crate) fn render_seed_proposal(
        &self,
        pending_id: i64,
        ops: &[DiffOp],
        name: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut list = div()
            .id("seed-list")
            .flex()
            .flex_col()
            .gap(px(2.))
            .py(px(8.))
            .max_h(px(420.))
            .overflow_y_scroll();
        for op in ops {
            if let DiffOp::Add {
                name: n,
                detail,
                parent,
                anchors,
                ..
            } = op
            {
                let child = !matches!(parent, orchestrator_store::PartRef::Root);
                list = list.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .px(px(12.))
                        .py(px(3.))
                        .when(child, |d| d.pl(px(30.)))
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(0x8FE3B6))
                                .child("+"),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(rgb(TEXT_STRONG))
                                .child(SharedString::from(n.clone())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(11.5))
                                .text_color(rgb(MUTED))
                                .child(SharedString::from(detail.clone())),
                        )
                        .when(!anchors.is_empty(), |d| {
                            d.child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(rgb(MUTED2))
                                    .font_family("Menlo")
                                    .child(SharedString::from(anchors.join(" "))),
                            )
                        }),
                );
            }
        }
        let ops_owned = ops.to_vec();
        div().flex_1().flex().flex_col().min_h_0().p(px(20.))
            .child(div().flex().flex_row().items_center().gap(px(9.)).mb(px(4.))
                .child(div().text_size(px(12.)).text_color(rgb(AMBER)).child("proposed structure"))
                .child(div().text_size(px(13.5)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(TEXT_STRONG)).child(SharedString::from(name.to_string())))
                .child(div().flex_1().text_size(px(11.5)).text_color(rgb(MUTED2)).child("extracted from your code — edit after accepting; nothing is marked done")))
            .child(div().flex_1().min_h_0().rounded(px(11.)).bg(rgb(CARD)).border_1().border_color(rgb(0x4A4636)).child(list))
            .child(
                div().mt(px(12.)).flex().flex_row().gap(px(8.))
                    .child(div().id("seed-accept").px(px(12.)).py(px(6.)).rounded(px(8.)).bg(rgb(ACCENT)).cursor_pointer().text_size(px(12.)).text_color(rgb(0x0C140F)).child("⏎ Accept structure")
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| { this.accept_pending(pending_id, ops_owned.clone(), cx); })))
                    .child(div().id("seed-discard").px(px(12.)).py(px(6.)).rounded(px(8.)).border_1().border_color(rgb(HAIR)).cursor_pointer().text_size(px(12.)).text_color(rgb(MUTED)).child("esc Discard")
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| { this.discard_pending(pending_id, cx); }))),
            )
    }

    /// The CHANGESET REVIEW surface (docs/019 slice 1c T9): an open machine
    /// proposal rendered as a diff-of-the-document — a titled card listing each
    /// op as a diff row (Add = green ghost, Remove = struck, Move = old→new
    /// breadcrumb, the rest = a described change). Verbs: per-op toggle-off,
    /// reject-all, accept-all. A proposed Add's name is inline-editable before
    /// accept. Full drag-a-ghost repositioning is DEFERRED (docs/019: canvas
    /// ghost overlay deferred). Rendered as one card in the map stage column
    /// (the `root` banner slot, not a per-node outline card) because a
    /// changeset is a document-level diff, and that column already stacks such
    /// banners — the most existing render code reused.
    /// docs/019 slice 2: the "reading docs…" progress card for an in-flight
    /// cartographer run on THIS project — visible so a multi-minute claude call
    /// never reads as a hang. The transcript is captured whole at exit
    /// (foundations), so this shows the run label + a live elapsed timer,
    /// replaced by the changeset card the moment the proposal lands.
    pub(crate) fn render_agentic_progress(&self, slug: &str) -> Option<AnyElement> {
        let run = self.agentic.as_ref().filter(|r| r.slug == slug)?;
        let secs = run.started.elapsed().as_secs();
        Some(
            div()
                .id("agentic-progress")
                .mx(px(14.))
                .mt(px(10.))
                .px(px(14.))
                .py(px(11.))
                .rounded(px(11.))
                .bg(rgb(0x141a20))
                .border_1()
                .border_color(rgb(0x2f4a55))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .child(div().w(px(7.)).h(px(7.)).rounded(px(4.)).bg(rgb(ACCENT)))
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_STRONG))
                        .child(SharedString::from(run.label.clone())),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(rgb(MUTED2))
                        .child(SharedString::from(format!(
                            "{secs}s · every node it proposes will cite a real file"
                        ))),
                )
                .into_any_element(),
        )
    }

    pub(crate) fn render_changeset_review(
        &self,
        cs: &(i64, String, String, Option<i64>, String, u64),
        flat: &[(DiffOp, Option<String>)],
        flags: &[bool],
        parts: &[DesignPart],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use std::collections::HashMap;
        let cs_id = cs.0;
        let title = cs.1.clone();
        let instruction = cs.2.clone();
        let names: HashMap<PartId, String> = parts.iter().map(|p| (p.id, p.name.clone())).collect();
        let parents: HashMap<PartId, Option<PartId>> =
            parts.iter().map(|p| (p.id, p.parent_id)).collect();
        let name_of = |pid: PartId| {
            names
                .get(&pid)
                .cloned()
                .unwrap_or_else(|| format!("#{pid}"))
        };
        let cur_parent = |pid: PartId| {
            parents
                .get(&pid)
                .and_then(|pp| *pp)
                .and_then(|pp| names.get(&pp).cloned())
                .unwrap_or_default()
        };
        let describe = |op: &DiffOp| describe_op(op, &name_of);

        let review = self.review.as_ref().filter(|r| r.id == cs_id);
        let flag_of = |i: usize| flags.get(i).copied().unwrap_or(false);
        let flipped = |i: usize| review.is_some_and(|r| r.off.contains(&i));
        // effective keep (docs/019 slice 2): flagged/done default OUT, plain
        // default IN; the toggle flips either. Accept all applies exactly this.
        let kept_of = |i: usize, op: &DiffOp| changeset_kept(op, flag_of(i), flipped(i));
        let total = flat.len();
        let kept = flat
            .iter()
            .enumerate()
            .filter(|(i, (op, _))| kept_of(*i, op))
            .count();
        let flagged_n = (0..total).filter(|i| flag_of(*i)).count();

        // ---- header: title + op count + accept/reject ----
        let count_label = if kept == total {
            format!("— {total} op{}", if total == 1 { "" } else { "s" })
        } else if flagged_n > 0 {
            format!("— {kept} of {total} kept · {flagged_n} unverified held back")
        } else {
            format!("— {kept} of {total} ops kept")
        };
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .child(
                div()
                    .text_size(px(13.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(AMBER))
                    .child(SharedString::from(format!("⟳ {title}"))),
            )
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(rgb(MUTED2))
                    .child(SharedString::from(count_label)),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("cs-accept")
                    .px(px(11.))
                    .py(px(4.))
                    .rounded(px(8.))
                    .bg(rgb(ACCENT))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(rgb(0x0C140F))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Accept all")
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.accept_changeset(cs_id, cx)
                    })),
            )
            .child(
                div()
                    .id("cs-reject")
                    .px(px(11.))
                    .py(px(4.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(rgb(HAIR))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .child("Reject all")
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.reject_changeset(cs_id, cx)
                    })),
            );

        let mut rows = div().flex().flex_col().gap(px(3.)).mt(px(8.));
        for (i, (op, evidence)) in flat.iter().enumerate() {
            let flagged = flag_of(i);
            let done = op_asserts_done(op);
            // "hold" = excluded from Accept all by default (unverified or a
            // done-assertion) — individually accepted only (docs/019 slice 2).
            let hold = flagged || done;
            let keep = kept_of(i, op);
            let off = !keep; // "off" = not currently kept (dropped or held back)
                             // reflect an Add name edit into the displayed row (same helper
                             // accept applies with — preview never drifts from effect).
            let display_op = op_with_name_edit(op, i, review.map(|r| &r.names));
            let (kind, text) =
                outlinepane::changeset_row(&display_op, &name_of, &cur_parent, &describe);
            let ink = if off {
                MUTED2
            } else {
                match kind {
                    outlinepane::DiffRowKind::Add => 0x8FE3B6,
                    outlinepane::DiffRowKind::Remove => 0xE6A08A,
                    outlinepane::DiffRowKind::Move => TEXT,
                    outlinepane::DiffRowKind::Change => TEXT,
                }
            };
            let editable_add = matches!(op, DiffOp::Add { .. });
            let editing =
                self.outline_edit.active == Some(outlinepane::EditSlot::ChangesetOpName(i));

            // the row's text (or the inline name editor for an Add being edited).
            let text_cell: AnyElement = if editing {
                div()
                    .flex_1()
                    .min_w_0()
                    .child(outlinepane::inline_input("name", &self.outline_edit.buf, self.inline_caret))
                    .into_any_element()
            } else {
                let mut t = div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.5))
                    .text_color(rgb(ink))
                    .child(SharedString::from(text));
                if off || matches!(kind, outlinepane::DiffRowKind::Remove) {
                    t = t.line_through();
                }
                let mut cell = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(7.))
                    .flex_1()
                    .min_w_0()
                    .child(t);
                // ✎ edit-before-accept on Add rows (docs/019 slice 1c).
                if editable_add && keep {
                    cell = cell.child(
                        div()
                            .id(SharedString::from(format!("cs-edit-{i}")))
                            .flex_none()
                            .px(px(5.))
                            .rounded(px(5.))
                            .cursor_pointer()
                            .text_size(px(10.5))
                            .text_color(rgb(MUTED2))
                            .hover(|h| h.text_color(rgb(ACCENT)))
                            .child("✎")
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.begin_changeset_name_edit(cs_id, i, window, cx)
                            })),
                    );
                }
                cell.into_any_element()
            };

            // a distinct badge for held-back ops (docs/019 ruling 4 + 6): the
            // unverified evidence flag, or a done-assertion needing confirmation.
            let hold_badge = if flagged {
                Some(("⚠ no verified quote — accept it individually", 0xE6A08A))
            } else if done {
                Some((
                    "done — string match isn't proof; confirm individually",
                    AMBER,
                ))
            } else {
                None
            };

            let mut row = div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(9.))
                .px(px(10.))
                .py(px(6.))
                .rounded(px(8.))
                .bg(rgb(CARD))
                .border_1()
                .border_color(rgb(if hold {
                    0x5A4A30
                } else if off {
                    HAIR_SOFT
                } else {
                    HAIR
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .child(text_cell)
                        .when_some(evidence.clone().filter(|_| !editing), |c, q| {
                            c.child(
                                div()
                                    .text_size(px(11.))
                                    .italic()
                                    .text_color(rgb(MUTED2))
                                    .child(SharedString::from(format!("“{q}”"))),
                            )
                        })
                        .when_some(hold_badge.filter(|_| !editing), |c, (txt, col)| {
                            c.child(div().text_size(px(10.5)).text_color(rgb(col)).child(txt))
                        }),
                );
            // per-op toggle. A KEPT op shows ✕ (drop); a plain dropped op shows
            // ↩ (restore); a held-back op shows "✓ keep" (accept this one anyway
            // — the individual-accept path flagged/done ops require).
            let (glyph, hint_ink): (&str, u32) = if keep {
                ("✕", MUTED)
            } else if hold {
                ("✓ keep", GREEN)
            } else {
                ("↩", MUTED)
            };
            row = row.child(
                div()
                    .id(SharedString::from(format!("cs-tog-{i}")))
                    .flex_none()
                    .px(px(8.))
                    .py(px(3.))
                    .rounded(px(7.))
                    .border_1()
                    .border_color(rgb(if !keep && hold { GREEN } else { HAIR }))
                    .cursor_pointer()
                    .text_size(px(11.5))
                    .text_color(rgb(hint_ink))
                    .hover(|h| h.border_color(rgb(ACCENT)))
                    .child(glyph)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.toggle_changeset_op(cs_id, i, cx)
                    })),
            );
            rows = rows.child(row);
        }

        div()
            .id("changeset-review")
            .mx(px(14.))
            .mt(px(10.))
            .px(px(14.))
            .py(px(11.))
            .rounded(px(11.))
            .bg(rgb(0x1c1a12))
            .border_1()
            .border_color(rgb(0x4a4530))
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .mt(px(3.))
                    .text_size(px(11.5))
                    .text_color(rgb(MUTED))
                    .child(SharedString::from(instruction)),
            )
            .child(rows)
            .child(
                div()
                    .mt(px(9.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(11.))
                    .child(
                        div()
                            .text_color(rgb(MUTED2))
                            .child("✕ drops one · ✎ edits a name · ✓ keep an unverified one"),
                    )
                    .child(div().flex_1())
                    .child(div().text_color(rgb(MUTED2)).child(
                        "accept applies the kept ops as one change — ⌘Z reverts the whole thing",
                    )),
            )
            .into_any_element()
    }

}
