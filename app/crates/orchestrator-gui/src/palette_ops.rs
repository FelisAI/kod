use gpui::*;
use crate::*;


impl Orchestrator {

    /// Close the palette AND hand focus back — gpui never clears focus off an
    /// unmounted element, which strands the keyboard (review 1b).
    pub(crate) fn close_palette(&mut self, window: &mut Window) {
        self.palette.close();
        if self.screen == Screen::Workspace && self.mode == Mode::Agent {
            self.term_focus.focus(window);
        } else {
            self.root_focus.focus(window);
        }
    }

    /// Re-run the palette's row set on each edit. Synchronous on purpose:
    /// search_all rebuilds its FTS per call — sub-ms at this scale (store.rs),
    /// no worker — and the move-to/verb filters are pure over one load_tree.
    pub(crate) fn rekick_palette(&mut self) {
        let slug = self.project().slug.clone();
        match self.palette.mode {
            // quick-add: the query is a NAME, not a search.
            palette::PaletteMode::QuickAdd { .. } => {}
            palette::PaletteMode::MoveTo { moving } => {
                // re-filter the destination list (pure, palette.rs): every
                // node minus the moving subtree (cycle) and its current
                // parent, plus the "▸ root" escape hatch.
                let parts = {
                    let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
                    store.load_tree(&slug).unwrap_or_default()
                };
                let all = palette::move_targets(&parts, moving);
                self.palette
                    .adopt_targets(palette::filter_targets(&all, &self.palette.query));
            }
            palette::PaletteMode::Recall => {
                // the selection's verb rows lead (docs/019 PALETTE), then the
                // search hits. Empty query = no search: the body would now
                // RENDER whatever came back (verbs force it open), and an
                // unqueried FTS dump is noise, not recall.
                let (verbs, hits) = {
                    let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
                    let verbs = self
                        .focused_part
                        .and_then(|id| {
                            store
                                .load_tree(&slug)
                                .unwrap_or_default()
                                .into_iter()
                                .find(|p| p.id == id)
                        })
                        .map(|p| palette::verb_rows(&p, &self.palette.query))
                        .unwrap_or_default();
                    let hits = if self.palette.query.trim().is_empty() {
                        Vec::new()
                    } else {
                        store
                            .search_all(&self.palette.query, 20)
                            .unwrap_or_default()
                    };
                    (verbs, hits)
                };
                self.palette.adopt_verbs(verbs);
                self.palette.adopt_hits(hits);
            }
        }
    }

    fn palette_confirm(&mut self, cmd: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ev) = self.palette.confirm(cmd) else {
            return;
        };
        // A capture with nowhere to land is refused OUT LOUD (capture_refusal) —
        // never written, and never a dead key.
        if matches!(
            ev,
            palette::PaletteEvent::AddNode { .. } | palette::PaletteEvent::AddHere { .. }
        ) {
            if let Some(why) = self.capture_refusal() {
                // land where term_error is drawn — the workspace root, which paints
                // it in every mode (the spawn_cwd rule: a refusal the user can't
                // see is exactly as useless as no refusal at all).
                //
                // BEFORE close_palette, which hands the keyboard back by reading
                // exactly these two fields: closing first made it see the OLD screen
                // (Standup) and park focus on `root_focus`, and then the switch put a
                // LIVE TERMINAL on screen that the keystream never reached — main.rs's
                // reclaim leaves a focused root alone by design.
                self.screen = Screen::Workspace;
                self.close_palette(window);
                self.term_error = Some(why.to_string());
                cx.notify();
                return;
            }
        }
        // ⌘↵ in QuickAdd = rapid capture (docs/019 commitment 5): the palette
        // stays open with the query cleared — N thoughts, N enters, zero
        // filing decisions. Everything else closes first, as before.
        if cmd && matches!(ev, palette::PaletteEvent::AddNode { .. }) {
            self.palette.query.clear();
        } else {
            self.close_palette(window);
        }
        match ev {
            palette::PaletteEvent::FocusNode { project_key, part }
            | palette::PaletteEvent::OpenLog { project_key, part } => {
                // only focus the node if the project actually exists — a stale
                // hit must not plant a cross-project focused_part (review 1b).
                if self.projects.iter().any(|p| p.slug == project_key) {
                    self.focus_node_on_map(&project_key, part, cx);
                }
            }
            palette::PaletteEvent::OpenSession {
                project_key,
                cli_session_id,
            } => {
                let live = self.host.infos_for(&project_key).into_iter().find(|i| {
                    i.alive && i.cli_session_id.as_deref() == Some(cli_session_id.as_str())
                });
                match live {
                    Some(i) => self.focus_session(&project_key, i.id, window, cx),
                    None => {
                        self.select_project(&project_key, cx);
                        self.mode = Mode::Recover;
                    }
                }
            }
            palette::PaletteEvent::AddNode { parent, name } => {
                let new_id = match parent {
                    Some(p) => self.palette_add(PartRef::Id(p), name),
                    None => self.capture_to_idea_tray(name),
                };
                // plain ↵ selects the new node so the next gesture chains
                // (docs/019 PALETTE); rapid ⌘↵ capture leaves the selection
                // alone — you're still capturing, not filing.
                if !cmd {
                    if let Some(id) = new_id {
                        self.focused_part = Some(id);
                    }
                }
            }
            palette::PaletteEvent::AddHere { name } => {
                // "here" = the live selection; none → the idea tray (docs/019
                // PALETTE: no meaningful selection files the capture, it
                // never plants a silent root-level orphan).
                match self.live_selection() {
                    Some(p) => {
                        self.palette_add(PartRef::Id(p), name);
                    }
                    None => {
                        self.capture_to_idea_tray(name);
                    }
                }
            }
            palette::PaletteEvent::MoveNode { id, dest } => {
                // ONE Move appended last under dest — the same tree.rs op an
                // ⌥-drag drop applies; calm None on cycles/ghosts/non-moves.
                let slug = self.project().slug.clone();
                let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
                let parts = store.load_tree(&slug).unwrap_or_default();
                if let Some(op) = orchestrator_store::reparent_op(&parts, id, dest) {
                    let _ = store.accept_diff_from(&slug, &[op], "user", None);
                    // a reparent is structure, not placement — the pin
                    // follows auto-layout in its new frame (the ⌥-drag rule).
                    let _ = store.clear_part_pos(id);
                    drop(store);
                    self.focused_part = Some(id);
                }
            }
            palette::PaletteEvent::Verb(v) => {
                // the four context-menu verbs, palette-routed (docs/019
                // PALETTE): same handler bodies, so the two entrances can
                // never drift. live_selection re-validates — the verbs were
                // built for a node that must still exist.
                let Some(id) = self.live_selection() else {
                    cx.notify();
                    return;
                };
                let slug = self.project().slug.clone();
                match v {
                    // RenameFocused, not RenameCanvas: the selection may be
                    // off-canvas (drilled elsewhere), and a canvas editor
                    // with no card renders nowhere while owning the keyboard.
                    palette::NodeVerb::Rename => self.begin_outline_edit(
                        outlinepane::EditSlot::RenameFocused(id),
                        window,
                        cx,
                    ),
                    palette::NodeVerb::Delete => {
                        self.delete_part_subtree(id, cx);
                    }
                    palette::NodeVerb::CycleStatus => {
                        let lc = {
                            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
                            store
                                .load_tree(&slug)
                                .unwrap_or_default()
                                .iter()
                                .find(|p| p.id == id)
                                .map(|p| p.lifecycle)
                        };
                        if let Some(lc) = lc {
                            // journals via accept_diff → human lane, undoable
                            // (the same path as a glyph click).
                            self.set_part_status(id, next_lifecycle(lc), cx);
                        }
                    }
                    palette::NodeVerb::CycleKind => {
                        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(k) = store
                            .load_tree(&slug)
                            .unwrap_or_default()
                            .iter()
                            .find(|p| p.id == id)
                            .map(|p| p.kind)
                        {
                            let _ = store.accept_diff_from(
                                &slug,
                                &[DiffOp::SetKind {
                                    id,
                                    kind: k.cycle(),
                                }],
                                "user",
                                None,
                            );
                        }
                    }
                }
            }
        }
        cx.notify();
    }

    /// ⌘K/N add — one Add op, instant on the human lane (docs/019 commitment
    /// 1). Returns the created node's id (diffed against the pre-add row set,
    /// the CreateCanvas pattern) so callers can chain selection onto it.
    fn palette_add(&mut self, parent: PartRef, name: String) -> Option<PartId> {
        // The same backstop `capture_to_idea_tray` carries, so the rule holds for
        // BOTH palette writers: no `part` row while a capture stands refused.
        if self.capture_refusal().is_some() {
            return None;
        }
        let slug = self.project().slug.clone();
        let op = DiffOp::Add {
            temp: "k1".into(),
            parent,
            name,
            detail: String::new(),
            lifecycle: Lifecycle::Todo,
            anchors: vec![],
            kind: orchestrator_store::Kind::Task,
            detail_md: None,
            sort_order: None,
            source_file: None,
            source_quote: None,
            rationale: None,
        };
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let old: std::collections::HashSet<PartId> = store
            .load_tree(&slug)
            .unwrap_or_default()
            .iter()
            .map(|p| p.id)
            .collect();
        let _ = store.accept_diff_from(&slug, &[op], "user", None);
        store
            .load_tree(&slug)
            .unwrap_or_default()
            .iter()
            .find(|p| !old.contains(&p.id))
            .map(|p| p.id)
    }

    /// WHY a ⌘K capture cannot land — `None` = it can. Both cases are the same
    /// bug in different clothes: a `part` row written where nothing will ever
    /// render it. Commitment 5 (docs/019) assumes both a map to hold the thought
    /// and a project to own it; when either is missing, the capture must be
    /// refused rather than accepted into the void.
    pub(crate) fn capture_refusal(&self) -> Option<&'static str> {
        // OSS gate (features.rs): a capture is a MAP node — a `kind=idea` row in
        // the project tree, which this build never renders. It would be invisible
        // the instant it landed and dead on recall afterwards (focus_node_on_map is
        // gated too, so its own ⌘K hit's ↵ would do nothing).
        //
        // ⌘K ITSELF stays: its other tier is alive here. Session summaries come from
        // the UNGATED standup summarizer, and a SESSION hit opens the live session
        // (or its Recover card) — cross-project session recall exists nowhere else
        // in the app. These two Add arms were also the last ungated writers of
        // `part` rows: with them shut, a store that never saw a `--features map`
        // build can hold no node/decision rows at all, so the palette's other two
        // tiers can't even populate — nothing to offer, nothing dead to click.
        if !crate::features::MAP_ENABLED {
            return Some("this build has nowhere to keep ideas — the map is not compiled in");
        }
        // EMPTY portfolio: `project()` is the sentinel, so the capture would be
        // filed under the phantom key "welcome" — a key no rail row owns and
        // `store_projects` never injects (neither `idea:` nor path-backed). Written,
        // then unreachable forever. `no_project_reason` (not the sentinel test) so
        // the first seconds of every launch — same sentinel, projects merely not
        // scanned yet — say "one moment" instead of "you have no projects".
        self.no_project_reason()
    }

    /// Commitment 5 capture (docs/019, user amendment): a thought with no
    /// home lands in the per-project idea tray — a lazily-created root node
    /// named "ideas" (kind=idea, looked up by name+kind via tree.rs) — as a
    /// kind=idea, lifecycle=idea child. Tray + first capture are ONE journal
    /// transaction (temp-ref parent), so ⌘Z removes both, never half.
    fn capture_to_idea_tray(&mut self, name: String) -> Option<PartId> {
        // The backstop half of the gate: `palette_confirm` refuses out loud before
        // anything gets here, so this only catches a NEW capture entrance added
        // later — it must not be the thing that TELLS the user, or the refusal
        // becomes silent again.
        if self.capture_refusal().is_some() {
            return None;
        }
        let slug = self.project().slug.clone();
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let parts = store.load_tree(&slug).unwrap_or_default();
        let capture = |parent: PartRef| DiffOp::Add {
            temp: "cap".into(),
            parent,
            name: name.clone(),
            detail: String::new(),
            lifecycle: Lifecycle::Idea,
            anchors: vec![],
            kind: orchestrator_store::Kind::Idea,
            detail_md: None,
            sort_order: None,
            source_file: None,
            source_quote: None,
            rationale: None,
        };
        let ops = match orchestrator_store::idea_tray_id(&parts) {
            Some(tray) => vec![capture(PartRef::Id(tray))],
            None => vec![
                DiffOp::Add {
                    temp: "tray".into(),
                    parent: PartRef::Root,
                    name: orchestrator_store::IDEA_TRAY_NAME.into(),
                    detail: String::new(),
                    lifecycle: Lifecycle::Idea,
                    anchors: vec![],
                    kind: orchestrator_store::Kind::Idea,
                    detail_md: None,
                    sort_order: None,
                    source_file: None,
                    source_quote: None,
                    rationale: None,
                },
                capture(PartRef::Temp("tray".into())),
            ],
        };
        let _ = store.accept_diff_from(&slug, &ops, "user", None);
        // the first capture creates TWO rows — the capture is the non-root
        // one (the tray is root-born; an existing tray parents the capture).
        let old: std::collections::HashSet<PartId> = parts.iter().map(|p| p.id).collect();
        store
            .load_tree(&slug)
            .unwrap_or_default()
            .iter()
            .filter(|p| !old.contains(&p.id))
            .find(|p| p.parent_id.is_some())
            .map(|p| p.id)
    }

    /// Hand the stage to the palette (any mode): the palette owns keys while
    /// open, so every other key sink closes/blurs FIRST (the 1b key-sink rule
    /// — a context menu or live editor left underneath would reclaim the
    /// keyboard the moment the palette closes). Detail edits COMMIT on the
    /// way out (save-on-blur); single-line ghosts just close.
    pub(crate) fn stage_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rail_new = None;
        self.map_menu = None;
        self.cmd = CmdBar::default(); // never two typed-input sinks at once
        self.blur_outline_edit(cx);
        self.palette_focus.focus(window);
    }

    /// N (docs/019 PALETTE): QuickAdd — the typed query IS the new node's
    /// name. `parent: None` = no meaningful selection → the idea tray
    /// (commitment 5): no selection means no filing decision to make.
    pub(crate) fn open_quick_add(
        &mut self,
        parent: Option<PartId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.palette.open_quick_add(parent);
        self.stage_palette(window, cx);
    }

    /// The ⌘K overlay + its keyboard (chars via keystroke — no IME v1).
    pub(crate) fn palette_layer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let e1 = cx.entity();
        let e2 = cx.entity();
        let handlers = palette::PaletteHandlers {
            activate_hit: Rc::new(move |ix, window, app| {
                e1.update(app, |this, cx| {
                    this.palette.sel = ix;
                    this.palette_confirm(false, window, cx);
                })
            }),
            close: Rc::new(move |window, app| {
                e2.update(app, |this, cx| {
                    this.close_palette(window);
                    cx.notify();
                })
            }),
        };
        // the mode's context node name for the chip (QuickAdd's parent /
        // MoveTo's moving node) — resolved here, the state only holds ids.
        let ctx_id = match self.palette.mode {
            palette::PaletteMode::QuickAdd { parent } => parent,
            palette::PaletteMode::MoveTo { moving } => Some(moving),
            palette::PaletteMode::Recall => None,
        };
        let ctx_name = ctx_id.and_then(|id| {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            store
                .load_tree(&self.project().slug)
                .unwrap_or_default()
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.name.clone())
        });
        div()
            .absolute()
            .size_full()
            .track_focus(&self.palette_focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                let k = ev.keystroke.key.as_str();
                match k {
                    "enter" => this.palette_confirm(ev.keystroke.modifiers.secondary(), window, cx),
                    "escape" => this.close_palette(window),
                    "backspace" => {
                        this.palette.backspace();
                        this.rekick_palette();
                    }
                    "up" => this.palette.prev(),
                    "down" => this.palette.next(),
                    _ => {
                        if !ev.keystroke.modifiers.secondary() && !ev.keystroke.modifiers.control {
                            if let Some(ch) = ev
                                .keystroke
                                .key_char
                                .clone()
                                .filter(|c| !c.is_empty())
                                .or_else(|| (k.chars().count() == 1).then(|| k.to_string()))
                            {
                                this.palette.push_str(&ch);
                                this.rekick_palette();
                            } else if k == "space" {
                                this.palette.push_str(" ");
                                this.rekick_palette();
                            }
                        }
                    }
                }
                cx.stop_propagation();
                cx.notify();
            }))
            .child(palette::palette_overlay(
                &self.palette,
                ctx_name.as_deref(),
                self.capture_refusal().is_none(),
                &handlers,
            ))
    }

}

#[cfg(test)]
mod tests {
    //! Guardrail canary (the features.rs idiom, applied to a WRITE rather than an
    //! LLM trigger). The palette's two Add arms were the last ungated writers of
    //! `part` rows: with the map compiled out they filed `kind=idea` nodes into a
    //! tree the build never renders — invisible on arrival, and unreachable
    //! afterwards, since focus_node_on_map is gated too. A gpui `Context` can't be
    //! built in a unit test, so this greps the source: it fires if the gate is
    //! deleted or renamed, turning a silent data leak back into a red test.

    #[test]
    fn palette_capture_stays_behind_the_map_gate() {
        // Every needle is SPLIT across a concat!: the file being grepped is THIS
        // file, so a whole literal would match itself and the test would stay green
        // with the gate deleted — a canary that can only ever pass.
        let src = include_str!("palette_ops.rs");
        assert!(
            src.contains(concat!("if !crate::features::MAP", "_ENABLED")),
            "capture_refusal must keep the map gate that makes the capture impossible"
        );
        assert!(
            src.contains(concat!("if let Some(why) = self.capture_", "refusal()")),
            "palette_confirm must be the site that SPEAKS the refusal, before any write"
        );
        // …and both writers keep their own backstop under it.
        assert!(
            src.matches(concat!("self.capture_", "refusal().is_some()"))
                .count()
                >= 2,
            "capture_to_idea_tray AND palette_add must each refuse to touch the store"
        );
        assert!(
            src.contains(concat!(
                "PaletteEvent::AddNode { .. } | palette::",
                "PaletteEvent::AddHere { .. }"
            )),
            "the refusal must cover BOTH capture events, not just ⌘↵ add-here"
        );
        assert!(
            src.contains(concat!("self.term", "_error =")),
            "a refused capture must SAY so — a silent no-op is the same bug, quieter"
        );
    }
}
