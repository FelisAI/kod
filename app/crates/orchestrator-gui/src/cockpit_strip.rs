use gpui::prelude::FluentBuilder;
use gpui::*;
use crate::*;


impl Orchestrator {

    pub(crate) fn set_part_status(&mut self, id: PartId, lifecycle: Lifecycle, cx: &mut Context<Self>) {
        let slug = self.project().slug.clone();
        if let Ok(mut store) = self.store.lock() {
            let _ = store.set_status(&slug, id, lifecycle);
        }
        cx.notify();
    }

    /// The user's ★ pins for a project — a ≤3 POINTER, not a priority queue
    /// (ruling 8). Persisted as a comma-separated id list in the setting
    /// `map_stars:<slug>`; oldest-first (front = oldest, dropped first).
    pub(crate) fn project_stars(&self, slug: &str) -> Vec<PartId> {
        self.store
            .lock()
            .ok()
            .and_then(|s| s.get_setting(&format!("map_stars:{slug}")))
            .map(|v| {
                v.split(',')
                    .filter_map(|t| t.trim().parse::<PartId>().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Toggle a ★ pin (docs/019 slice 4): idempotent unpin, a 4th drops the
    /// oldest — the cap is enforced by `cockpit::star_toggle`. Journaled as a
    /// spatial-memory-grade setting (unjournaled, like a canvas pin); it's a
    /// pointer, never structure.
    pub(crate) fn toggle_star(&mut self, id: PartId, cx: &mut Context<Self>) {
        let slug = self.project().slug.clone();
        // prune GHOST stars (ids deleted via any path — a dissolve, a subtree
        // delete) before toggling, so the ≤3 eviction can never drop a LIVE
        // star to keep a dead one (review finding 5).
        let live: std::collections::HashSet<PartId> = self
            .store
            .lock()
            .ok()
            .and_then(|s| s.load_tree(&slug).ok())
            .map(|ps| ps.iter().map(|p| p.id).collect())
            .unwrap_or_default();
        let current: Vec<PartId> = self
            .project_stars(&slug)
            .into_iter()
            .filter(|i| live.contains(i))
            .collect();
        let next = cockpit::star_toggle(&current, id, cockpit::STAR_CAP);
        if let Ok(store) = self.store.lock() {
            let line = next
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let _ = store.set_setting(&format!("map_stars:{slug}"), &line);
        }
        cx.notify();
    }

    /// Open the "Flag needs-me…" input (docs/019 slice 4): typing the one-line
    /// blocking question is REQUIRED — the question is the payload.
    pub(crate) fn open_needs_you_editor(&mut self, id: PartId, window: &mut Window, cx: &mut Context<Self>) {
        self.begin_outline_edit(outlinepane::EditSlot::NeedsYou(id), window, cx);
    }

    /// Clear a user-set needs-you flag (the user answered it inline).
    pub(crate) fn clear_needs_you(&mut self, id: PartId, cx: &mut Context<Self>) {
        if let Ok(store) = self.store.lock() {
            let _ = store.clear_needs_you(id);
        }
        cx.notify();
    }

    /// The "Flag needs-me…" input bar (docs/019 slice 4): a top-of-map row that
    /// REQUIRES typing the one-line blocking question (the question is the
    /// payload). Display-only — keys ride the root router; ⏎ commits the flag.
    pub(crate) fn render_needs_you_bar(&self, _cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = match self.outline_edit.active {
            Some(outlinepane::EditSlot::NeedsYou(id)) => id,
            _ => return None,
        };
        let slug = self.project().slug.clone();
        let name = self
            .store
            .lock()
            .ok()
            .and_then(|s| s.load_tree(&slug).ok())
            .and_then(|ps| ps.iter().find(|p| p.id == id).map(|p| p.name.clone()))
            .unwrap_or_default();
        let buf = self.outline_edit.buf.clone();
        let shown = if buf.is_empty() {
            "what decision is this blocking?".to_string()
        } else {
            buf
        };
        Some(
            div()
                .id("needs-you-bar")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(9.))
                .mx(px(14.))
                .mt(px(8.))
                .px(px(12.))
                .py(px(8.))
                .rounded(px(10.))
                .bg(rgb(AMBER_INK))
                .border_1()
                .border_color(rgb(AMBER_HAIR))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.5))
                        .text_color(rgb(AMBER))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(SharedString::from(format!(
                            "⚑ needs me — {}:",
                            termview::trim(&name, 28)
                        ))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.5))
                        .text_color(rgb(if self.outline_edit.buf.is_empty() {
                            MUTED2
                        } else {
                            TEXT
                        }))
                        .child(SharedString::from(shown)),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(10.))
                        .font_family("Menlo")
                        .text_color(rgb(MUTED2))
                        .child("⏎ flag · esc"),
                )
                .into_any_element(),
        )
    }

    /// THE COCKPIT STRIP (docs/019 slice 4, C6): the ONE-SUMMONS pulse, the
    /// plain-words rollup line, the Next-up tray, and the suggestions drawer —
    /// the whole glance, computed from asserted state (arithmetic, never
    /// inferred) and rendered in PLAIN WORDS (ruling 13).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_cockpit_strip(
        &self,
        slug: &str,
        parts: &[DesignPart],
        tree: &[TreeNode],
        building: &std::collections::HashMap<PartId, u64>,
        drifted_ids: &[PartId],
        activity: &std::collections::HashMap<PartId, u64>,
        needs: &std::collections::HashSet<PartId>,
        needs_ages: &std::collections::HashMap<PartId, u64>,
        now_secs: u64,
        review_summons: Option<(i64, String)>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use std::collections::HashSet;
        let names: std::collections::HashMap<PartId, String> =
            parts.iter().map(|p| (p.id, p.name.clone())).collect();
        let name_of = |id: PartId| names.get(&id).cloned().unwrap_or_else(|| format!("#{id}"));

        // ---- inputs: stars, user needs-you flags, stale set ----
        let stars = self.project_stars(slug);
        let user_flags: Vec<(PartId, String, u64)> = self
            .store
            .lock()
            .ok()
            .map(|s| s.needs_you_flags(slug))
            .unwrap_or_default();
        let stale: HashSet<PartId> = parts.iter().filter(|p| p.stale).map(|p| p.id).collect();
        let anchor_counts: std::collections::HashMap<PartId, usize> =
            std::collections::HashMap::new(); // undermapped file-count source: deferred

        // attention BOOSTS Next-up rank (awaiting sessions + user flags).
        let mut attention: HashSet<PartId> = needs.clone();
        for (id, _, _) in &user_flags {
            attention.insert(*id);
        }
        let blocked: HashSet<PartId> = HashSet::new(); // no depends-on edges yet (deferred)

        // ---- the four glance computations (all pure, all over asserted state) ----
        let ready_full =
            cockpit::next_up(parts, activity, &stars, &attention, &blocked, usize::MAX);
        let gaps = cockpit::gap_findings(parts, activity, now_secs, &stale, &anchor_counts);
        let building_ids: Vec<PartId> = building.keys().copied().collect();
        let rollup = cockpit::rollup(tree, &building_ids, drifted_ids, &ready_full, gaps.len());

        // ---- ONE SUMMONS: the computed singleton (needs-you > review > start) ----
        let mut ny_input: Vec<(PartId, String, u64)> = Vec::new();
        for (id, q, set_secs) in &user_flags {
            ny_input.push((*id, q.clone(), now_secs.saturating_sub(*set_secs)));
        }
        for id in needs {
            if ny_input.iter().any(|(nid, _, _)| nid == id) {
                continue; // a user question already speaks for this node
            }
            let age = needs_ages.get(id).copied().unwrap_or(0);
            ny_input.push((
                *id,
                format!("{} — an agent needs your decision", name_of(*id)),
                age,
            ));
        }
        let summons = cockpit::summons(&ny_input, review_summons, ready_full.first().copied());

        let mut col = div().flex().flex_col().gap(px(8.)).mx(px(14.)).mt(px(8.));

        // ---- the ONE pulse (a calm map pulses nothing — None renders nothing) ----
        if let Some(pulse) = self.render_summons(&summons, now_secs, &name_of, cx) {
            col = col.child(pulse);
        }

        // ---- the plain-words header rollup + the triage-sweep entrance ----
        let sweeping = self.triage_active;
        col = col.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.5))
                        .text_color(rgb(TEXT))
                        .child(SharedString::from(rollup.line())),
                )
                .child(
                    div()
                        .id("map-sweep")
                        .flex_none()
                        .px(px(9.))
                        .py(px(3.))
                        .rounded(px(8.))
                        .cursor_pointer()
                        .border_1()
                        .border_color(rgb(if sweeping { ACCENT } else { HAIR }))
                        .hover(|h| h.border_color(rgb(ACCENT)))
                        .text_size(px(11.))
                        .text_color(rgb(if sweeping { ACCENT } else { MUTED }))
                        .child("⌦ Sweep")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            if this.triage_active {
                                this.exit_triage(cx)
                            } else {
                                this.enter_triage(cx)
                            }
                        })),
                ),
        );

        // ---- NEXT-UP tray: dispatch-ready task nodes, ▶ + ★ per row (CAN) ----
        if !ready_full.is_empty() {
            let mut tray = div().flex().flex_col().gap(px(4.)).child(
                div()
                    .text_size(px(10.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(MUTED2))
                    .child("NEXT UP"),
            );
            for id in ready_full.iter().copied().take(cockpit::NEXT_UP_CAP) {
                let starred = stars.contains(&id);
                let row = div()
                    .id(SharedString::from(format!("nu-{id}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .px(px(10.))
                    .py(px(5.))
                    .rounded(px(8.))
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(HAIR))
                    .child(
                        div()
                            .id(SharedString::from(format!("nu-star-{id}")))
                            .flex_none()
                            .text_size(px(12.))
                            .cursor_pointer()
                            .text_color(rgb(if starred { ACCENT } else { MUTED2 }))
                            .hover(|h| h.text_color(rgb(ACCENT)))
                            .child(if starred { "★" } else { "☆" })
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.toggle_star(id, cx)
                            })),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("nu-name-{id}")))
                            .flex_1()
                            .min_w_0()
                            .cursor_pointer()
                            .text_size(px(12.5))
                            .text_color(rgb(TEXT))
                            .hover(|h| h.text_color(rgb(TEXT_STRONG)))
                            .child(SharedString::from(termview::trim(&name_of(id), 40)))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                let s = this.project().slug.clone();
                                this.focus_node_on_map(&s, id, cx);
                            })),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("nu-go-{id}")))
                            .flex_none()
                            .px(px(8.))
                            .py(px(2.))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_size(px(11.5))
                            .text_color(rgb(ACCENT))
                            .hover(|h| h.bg(rgb(CARD)))
                            .child("▶ start")
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.dispatch_to_part(id, false, window, cx);
                            })),
                    );
                tray = tray.child(row);
            }
            col = col.child(tray);
        }

        // ---- SHOULD: the collapsed suggestions drawer (never colors nodes) ----
        if !gaps.is_empty() {
            let open = self.suggest_open;
            let mut drawer = div().flex().flex_col().gap(px(3.));
            drawer = drawer.child(
                div()
                    .id("gap-toggle")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .text_size(px(11.5))
                    .text_color(rgb(MUTED))
                    .hover(|h| h.text_color(rgb(TEXT)))
                    .child(SharedString::from(format!(
                        "{} {} suggestion{}",
                        if open { "▾" } else { "▸" },
                        gaps.len(),
                        if gaps.len() == 1 { "" } else { "s" }
                    )))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.suggest_open = !this.suggest_open;
                        cx.notify();
                    })),
            );
            if open {
                for (gi, g) in gaps.iter().enumerate() {
                    let node = g.node;
                    let row = div()
                        .id(SharedString::from(format!("gap-{gi}")))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .px(px(10.))
                        .py(px(4.))
                        .rounded(px(8.))
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(HAIR))
                        .cursor_pointer()
                        .hover(|h| h.border_color(rgb(0x36404A)))
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(rgb(MUTED2))
                                .child(SharedString::from(termview::trim(&name_of(node), 22))),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(11.5))
                                .text_color(rgb(MUTED))
                                .child(SharedString::from(g.detail.clone())),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(10.5))
                                .text_color(rgb(MUTED2))
                                .child("map ▸"),
                        )
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            let s = this.project().slug.clone();
                            this.focus_node_on_map(&s, node, cx);
                        }));
                    drawer = drawer.child(row);
                }
            }
            col = col.child(drawer);
        }

        col.into_any_element()
    }

    /// Render the ONE SUMMONS pulse (docs/019 slice 4): at most one amber
    /// "Decide" affordance in the whole UI — the map asking for the user,
    /// visually distinct from the chip-pulse (a session burning tokens). Because
    /// `summons` returns exactly one variant, two pulses are impossible by
    /// construction. `None`/calm renders nothing.
    fn render_summons(
        &self,
        summons: &cockpit::Summons,
        now_secs: u64,
        name_of: &impl Fn(PartId) -> String,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        // (node to jump/dispatch, changeset to review, is this the Start tier?,
        //  has this needs-you ESCALATED (unanswered 7d, anti-decay), verb, line)
        let (node, review_cs, is_start, escalated, verb, line): (
            Option<PartId>,
            Option<i64>,
            bool,
            bool,
            &str,
            String,
        ) = match summons {
            cockpit::Summons::None => return None,
            cockpit::Summons::NeedsYou {
                node,
                question,
                age_secs,
            } => {
                // anti-decay: an unanswered question does not fade — at 7d it
                // ESCALATES (docs/019 commitment 5). age = now − set, so the
                // set-time is now − age.
                let esc =
                    cockpit::needs_you_escalated(now_secs.saturating_sub(*age_secs), now_secs);
                (Some(*node), None, false, esc, "Decide", question.clone())
            }
            cockpit::Summons::Review { changeset, title } => (
                None,
                Some(*changeset),
                false,
                false,
                "Review",
                title.clone(),
            ),
            cockpit::Summons::Start { node } => (
                Some(*node),
                None,
                true,
                false,
                "Start",
                format!("ready to start: {}", name_of(*node)),
            ),
        };
        // the pulsing amber dot — the singular "map wants you" signal.
        let dot = div()
            .flex_none()
            .w(px(8.))
            .h(px(8.))
            .rounded(px(4.))
            .bg(rgb(AMBER))
            .with_animation(
                "summons-pulse",
                Animation::new(std::time::Duration::from_millis(1400))
                    .repeat()
                    .with_easing(pulsating_between(0.35, 1.0)),
                |d, delta| d.opacity(delta),
            );
        Some(
            div()
                .id("map-summons")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .px(px(13.))
                .py(px(9.))
                .rounded(px(10.))
                .bg(rgb(AMBER_INK))
                .border_1()
                .border_color(rgb(AMBER_HAIR))
                .cursor_pointer()
                .hover(|h| h.border_color(rgb(AMBER)))
                .child(dot)
                .child(
                    div()
                        .flex_none()
                        .text_size(px(12.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(AMBER))
                        .child(SharedString::from(verb.to_string())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.5))
                        .text_color(rgb(TEXT))
                        .child(SharedString::from(line)),
                )
                // anti-decay escalation (docs/019 C5): a 7d-unanswered needs-you
                // does not fade — it says so, louder.
                .when(escalated, |c| {
                    c.child(
                        div()
                            .flex_none()
                            .px(px(6.))
                            .py(px(1.))
                            .rounded(px(10.))
                            .bg(rgb(AMBER))
                            .text_color(rgb(0x1A1408))
                            .text_size(px(9.5))
                            .font_weight(FontWeight::BOLD)
                            .child("7d+ waiting"),
                    )
                })
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(rgb(MUTED2))
                        .child("▸"),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    let slug = this.project().slug.clone();
                    if let Some(cs) = review_cs {
                        // surface the changeset the user should review.
                        this.review = Some(ChangesetReview {
                            id: cs,
                            ..Default::default()
                        });
                    } else if let Some(n) = node {
                        // Start dispatches the top ready item; needs-you jumps to
                        // the node so the user can answer it in context.
                        if is_start {
                            this.dispatch_to_part(n, false, window, cx);
                        } else {
                            this.focus_node_on_map(&slug, n, cx);
                        }
                    }
                    cx.notify();
                }))
                .into_any_element(),
        )
    }

}
