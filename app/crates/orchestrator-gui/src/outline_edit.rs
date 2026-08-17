use gpui::*;
use crate::*;


impl Orchestrator {

    /// The node the outline is REALLY showing: focused_part, else the same
    /// first-root fallback the render uses — commit/resolve must never bail on
    /// None while the pane looks interactive (review 1b: silent text loss).
    fn effective_focused(&self, store: &Store, slug: &str) -> Option<PartId> {
        self.focused_part.or_else(|| {
            let parts = store.load_tree(slug).unwrap_or_default();
            let tree = build_tree(&parts);
            tree.first().map(|n| n.part.id)
        })
    }

    /// Commit the one inline outline edit (#10 authoring). Every arm applies
    /// INSTANTLY on the human lane (accept_diff_from origin "user" — docs/019
    /// commitment 1: no cards, no dialogs; ⌘Z is the confirmation).
    fn commit_outline_edit(&mut self, cx: &mut Context<Self>) {
        let ed = std::mem::take(&mut self.outline_edit);
        let Some(slot) = ed.active else { return };
        // EMPTY PORTFOLIO (map builds only — ⌃` opens the sentinel's workspace when
        // the rail holds nothing): every arm below files under `project().slug`,
        // which is then the phantom key "welcome" — a key no rail row owns and
        // `store_projects` never injects. The nodes render back, so nothing looks
        // wrong, and the first real project makes them unreachable FOREVER. This is
        // the one place they can be created from nothing (every other writer here
        // needs an existing node), and the palette already refuses the same write
        // one keystroke away.
        if let Some(why) = self.no_project_reason() {
            self.canvas_create_pin = None;
            self.term_error = Some(why.to_string());
            cx.notify();
            return;
        }
        let slug = self.project().slug.clone();
        // Detail commits RAW (interior newlines are content) and empty is a
        // legal save — it deliberately clears the body; Esc is the cancel.
        if let outlinepane::EditSlot::Detail(id) = slot {
            let body = ed.buf.trim_end().to_string();
            let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let cur = store
                .load_tree(&slug)
                .unwrap_or_default()
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.detail_md.clone());
            // untouched body → no op: a blur-commit must not journal a no-op
            // SetDetail and burn a ⌘Z step (docs/019: undo is sacred). A
            // deleted node (cur=None) also lands here — nothing to describe.
            if cur.as_deref().is_some_and(|c| c.trim_end() != body) {
                let _ = store.accept_diff_from(
                    &slug,
                    &[DiffOp::SetDetail {
                        id,
                        detail_md: body,
                    }],
                    "user",
                    None,
                );
            }
            drop(store);
            cx.notify();
            return;
        }
        // edit-before-accept (docs/019 slice 1c): the edited Add name lands in
        // the review overlay, NOT the store — it flows into the applied op only
        // on accept. Empty = cancel (keep the machine's proposed name).
        if let outlinepane::EditSlot::ChangesetOpName(idx) = slot {
            let text = ed.buf.trim().to_string();
            if let Some(r) = self.review.as_mut() {
                if text.is_empty() {
                    // clearing the field REVERTS to the machine's proposed name
                    // (review finding 5: a no-op left a prior edit stuck, so the
                    // original suggestion was unrecoverable without rejecting).
                    r.names.remove(&idx);
                } else {
                    r.names.insert(idx, text);
                }
            }
            cx.notify();
            return;
        }
        // "Flag needs-me…" (docs/019 slice 4): the QUESTION is required — an
        // empty commit cancels (never flags without the payload). A non-empty
        // question writes the user needs-you flag (question + set-time) so
        // it renders verbatim and anti-decays. Handled before the focused-node
        // resolution — the target rides the slot, no selection needed.
        if let outlinepane::EditSlot::NeedsYou(id) = slot {
            let q = ed.buf.trim().to_string();
            if !q.is_empty() {
                let now_s = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if let Ok(store) = self.store.lock() {
                    let _ = store.set_needs_you(&slug, id, &q, now_s);
                }
            }
            cx.notify();
            return;
        }
        let text = ed.buf.trim().to_string();
        if text.is_empty() {
            // Esc-equivalent (docs/019 OUTLINE): committing an empty name
            // cancels the gesture — no ghost rows, no empty renames. An
            // abandoned dbl-click-create leaves no pin behind either.
            self.canvas_create_pin = None;
            cx.notify();
            return;
        }
        // dbl-click-create (docs/019 CANVAS): Add under the drill-frame root,
        // pinned where the click landed, focused so the next gesture chains.
        // Before the focused-node resolution — creation must work on a canvas
        // where nothing is (or can be) focused. map_root_of BEFORE the store
        // lock: it locks the store itself on a cache miss.
        if slot == outlinepane::EditSlot::CreateCanvas {
            let pin = self.canvas_create_pin.take();
            let drill = self.map_root_of(&slug);
            let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let parts = store.load_tree(&slug).unwrap_or_default();
            // a stale drill root (node deleted since) falls back to Root —
            // typed text is never silently dropped (review 1b's rule).
            let parent = drill
                .filter(|rid| parts.iter().any(|p| p.id == *rid))
                .map(PartRef::Id)
                .unwrap_or(PartRef::Root);
            let _ = store.accept_diff_from(
                &slug,
                &[DiffOp::Add {
                    temp: "t0".into(),
                    parent,
                    name: text,
                    detail: String::new(),
                    lifecycle: Lifecycle::Todo,
                    anchors: vec![],
                    kind: orchestrator_store::Kind::Task,
                    detail_md: None,
                    sort_order: None,
                    source_file: None,
                    source_quote: None,
                    rationale: None,
                }],
                "user",
                None,
            );
            let old: std::collections::HashSet<PartId> = parts.iter().map(|p| p.id).collect();
            if let Some(new_id) = store
                .load_tree(&slug)
                .unwrap_or_default()
                .iter()
                .find(|p| !old.contains(&p.id))
                .map(|p| p.id)
            {
                if let Some((nx, ny)) = pin {
                    // the pin is spatial memory, not structure — direct and
                    // unjournaled, exactly like a drag-to-pin (⌘Z removes
                    // the NODE; no gesture replays a spot).
                    let _ = store.set_part_pos(new_id, nx, ny);
                }
                self.focused_part = Some(new_id);
            }
            drop(store);
            cx.notify();
            return;
        }
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let Some(focused) = self.effective_focused(&store, &slug) else {
            return;
        };
        match slot {
            outlinepane::EditSlot::AddPart => {
                let _ = store.accept_diff_from(
                    &slug,
                    &[DiffOp::Add {
                        temp: "t0".into(),
                        parent: PartRef::Id(focused),
                        name: text,
                        detail: String::new(),
                        lifecycle: Lifecycle::Todo,
                        anchors: vec![],
                        kind: orchestrator_store::Kind::Task,
                        detail_md: None,
                        sort_order: None,
                        source_file: None,
                        source_quote: None,
                        rationale: None,
                    }],
                    "user",
                    None,
                );
            }
            outlinepane::EditSlot::AddSibling(after) => {
                // Enter's landing plan: renumber the siblings to integers and
                // slot the new row right below the anchor, all in ONE accept
                // (one ⌘Z) — collapse-proof against rapid repeated Enters
                // (review). If the anchor vanished mid-edit, fall back to a
                // root append; typed text is never silently dropped.
                let parts = store.load_tree(&slug).unwrap_or_default();
                let (renumber, parent, order) =
                    orchestrator_store::sibling_below_plan(&parts, after)
                        .map(|p| (p.renumber, p.parent, Some(p.order)))
                        .unwrap_or_else(|| (Vec::new(), PartRef::Root, None));
                let mut ops = renumber;
                ops.push(DiffOp::Add {
                    temp: "t0".into(),
                    parent,
                    name: text,
                    detail: String::new(),
                    lifecycle: Lifecycle::Todo,
                    anchors: vec![],
                    kind: orchestrator_store::Kind::Task,
                    detail_md: None,
                    sort_order: order,
                    source_file: None,
                    source_quote: None,
                    rationale: None,
                });
                let _ = store.accept_diff_from(&slug, &ops, "user", None);
                // focus follows the new row (Workflowy: Enter-Enter-Enter
                // walks downward, each sibling landing below the last).
                let old: std::collections::HashSet<PartId> = parts.iter().map(|p| p.id).collect();
                if let Some(new_id) = store
                    .load_tree(&slug)
                    .unwrap_or_default()
                    .iter()
                    .find(|p| !old.contains(&p.id))
                    .map(|p| p.id)
                {
                    self.focused_part = Some(new_id);
                }
            }
            outlinepane::EditSlot::AddDecision => {
                let _ = store.add_note(&slug, focused, "decision", &text, "user");
            }
            outlinepane::EditSlot::AddNote => {
                let _ = store.add_note(&slug, focused, "note", &text, "user");
            }
            outlinepane::EditSlot::RenameChild(id)
            | outlinepane::EditSlot::RenameFocused(id)
            | outlinepane::EditSlot::RenameCanvas(id) => {
                let detail = store
                    .load_tree(&slug)
                    .unwrap_or_default()
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.detail.clone())
                    .unwrap_or_default();
                let _ = store.accept_diff_from(
                    &slug,
                    &[DiffOp::Rename {
                        id,
                        name: text,
                        detail,
                    }],
                    "user",
                    None,
                );
            }
            outlinepane::EditSlot::Detail(_)
            | outlinepane::EditSlot::CreateCanvas
            | outlinepane::EditSlot::ChangesetOpName(_)
            | outlinepane::EditSlot::NeedsYou(_) => {
                unreachable!("handled above")
            }
        }
        drop(store);
        cx.notify();
    }

    /// Open the ONE outline inline editor on `slot` (docs/019 slice 1b).
    /// Rename/Detail slots PREFILL with the current text — F2 means "tweak
    /// this name", not "retype it" — Add* slots start blank. A key gesture
    /// can land while the pane is collapsed; the editor renders inside it,
    /// so opening the pane is part of the gesture.
    pub(crate) fn begin_outline_edit(
        &mut self,
        slot: outlinepane::EditSlot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use outlinepane::EditSlot::*;
        // opening a new editor blurs the current one FIRST — a live Detail
        // edit must save-on-blur, not vanish (review: begin overwrote the
        // buffer directly, silently dropping typed prose). blur commits Detail
        // and discards the single-line ghosts, exactly like any other blur.
        if self.outline_edit.active.is_some() && self.outline_edit.active != Some(slot) {
            self.blur_outline_edit(cx);
        }
        let slug = self.project().slug.clone();
        let buf = match slot {
            RenameChild(id) | RenameFocused(id) | RenameCanvas(id) => self
                .store
                .lock()
                .ok()
                .and_then(|s| s.load_tree(&slug).ok())
                .and_then(|ps| ps.iter().find(|p| p.id == id).map(|p| p.name.clone()))
                .unwrap_or_default(),
            Detail(id) => self
                .store
                .lock()
                .ok()
                .and_then(|s| s.load_tree(&slug).ok())
                .and_then(|ps| ps.iter().find(|p| p.id == id).map(|p| p.detail_md.clone()))
                .unwrap_or_default(),
            // edit-before-accept (docs/019 slice 1c): prefill with the user's
            // prior edit if any, else the machine's proposed Add name.
            ChangesetOpName(idx) => self.changeset_op_name(idx),
            // re-flagging edits the existing question in place (docs/019 slice 4).
            NeedsYou(id) => self
                .store
                .lock()
                .ok()
                .and_then(|s| s.needs_you_for(id))
                .map(|(q, _)| q)
                .unwrap_or_default(),
            _ => String::new(),
        };
        // canvas-side slots render ON the canvas (docs/019 CANVAS): opening
        // the pane here would reflow 980→560 and yank the card — editor and
        // all — out from under the gesture that started it. The ChangesetOpName
        // editor renders inside the review card (in the map root), not the
        // pane, so it must not force the pane open either. NeedsYou renders as a
        // top-of-map input bar, so it likewise never forces the pane open.
        if !matches!(
            slot,
            RenameCanvas(_) | CreateCanvas | ChangesetOpName(_) | NeedsYou(_)
        ) && !self.outline_open(&slug)
        {
            self.set_outline_open(&slug, true);
        }
        self.outline_edit = outlinepane::EditState {
            active: Some(slot),
            buf,
        };
        self.rail_new = None;
        self.root_focus.focus(window); // router owns keys (review 1b)
        cx.notify();
    }

    /// Blur the outline editor (focus moved elsewhere: another node, another
    /// project, the palette). The Detail slot COMMITS — a long body must
    /// survive a stray click (docs/019: save on blur) — every single-line
    /// slot discards, since its blur is an abandoned ghost, not lost prose.
    pub(crate) fn blur_outline_edit(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.outline_edit.active,
            Some(outlinepane::EditSlot::Detail(_))
        ) {
            self.commit_outline_edit(cx);
        } else {
            if self.outline_edit.active == Some(outlinepane::EditSlot::CreateCanvas) {
                // an abandoned dbl-click-create leaves no pin behind.
                self.canvas_create_pin = None;
            }
            self.outline_edit = outlinepane::EditState::default();
        }
    }

    /// The OUTLINE keyboard grammar (docs/019 slice 1b): Enter = sibling ·
    /// Tab/⇧Tab = indent/outdent · ⌥↑/⌥↓ = reorder · F2 = rename · E = detail
    /// · ⌘⌫ = delete subtree · N = QuickAdd · M = Move-to (the PALETTE verbs
    /// ride this same router slot — the guard above already proves no overlay
    /// or editor owns the keys, so bare letters are verbs, not content).
    /// Fires only on the Map+Outline stage with no overlay open and no inline
    /// editor live (the caller's else-if chain) — never in Agent mode, where
    /// term_focus bypasses this router entirely. Returns true when the key
    /// was consumed (even as a calm edge no-op).
    pub(crate) fn outline_grammar_key(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.screen != Screen::Workspace
            || self.mode != Mode::MapOutline
            || self.spawn_menu_open
            || self.move_menu_open
            || self.palette.open
        {
            return false;
        }
        let m = ev.keystroke.modifiers;
        let bare = !m.secondary() && !m.alt && !m.shift && !m.control;
        // N/M run BEFORE the focused-node resolution: they act on the REAL
        // selection (focused_part), never the effective first-root fallback —
        // N with nothing selected must capture to the idea tray (docs/019
        // commitment 5: zero filing decisions; it must fire even on an EMPTY
        // map), and M with nothing selected stays silent rather than moving
        // a root the user never picked.
        if bare && ev.keystroke.key.as_str() == "n" {
            self.open_quick_add(self.live_selection(), window, cx);
            cx.notify();
            return true;
        }
        if bare && ev.keystroke.key.as_str() == "m" {
            let Some(id) = self.live_selection() else {
                return false;
            };
            self.open_move_to(id, window, cx);
            cx.notify();
            return true;
        }
        let slug = self.project().slug.clone();
        let fid = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            self.effective_focused(&store, &slug)
        };
        let Some(fid) = fid else { return false };
        // one structural gesture → one accept (one ⌘Z) on the human lane. The
        // pure fns own the ordering arithmetic (tree.rs, unit-tested) and may
        // return several Moves — reorder swaps a pair, outdent renumbers a
        // sibling group (collapse-proof, review). Empty = calm edge no-op.
        let apply_ops = |this: &mut Self, ops: Vec<DiffOp>| {
            if !ops.is_empty() {
                let mut store = this.store.lock().unwrap_or_else(|e| e.into_inner());
                let _ = store.accept_diff_from(&slug, &ops, "user", None);
            }
        };
        match ev.keystroke.key.as_str() {
            // Enter = new sibling BELOW the focused node, straight into
            // inline edit (commit computes the slot; empty/Esc cancels).
            "enter" if !m.secondary() && !m.alt && !m.shift && !m.control => {
                self.begin_outline_edit(outlinepane::EditSlot::AddSibling(fid), window, cx);
            }
            "f2" => self.begin_outline_edit(outlinepane::EditSlot::RenameFocused(fid), window, cx),
            // bare-key accelerator (docs/019: menu accelerators work as bare
            // keys); safe here because no inline editor is live.
            "e" if !m.secondary() && !m.alt && !m.shift && !m.control => {
                self.begin_outline_edit(outlinepane::EditSlot::Detail(fid), window, cx);
            }
            "tab" if !m.shift => {
                let ops = self
                    .store
                    .lock()
                    .ok()
                    .and_then(|s| s.load_tree(&slug).ok())
                    .map(|parts| {
                        orchestrator_store::indent_op(&parts, fid)
                            .into_iter()
                            .collect()
                    })
                    .unwrap_or_default();
                apply_ops(self, ops);
            }
            "tab" => {
                let ops = self
                    .store
                    .lock()
                    .ok()
                    .and_then(|s| s.load_tree(&slug).ok())
                    .map(|parts| orchestrator_store::outdent_op(&parts, fid))
                    .unwrap_or_default();
                apply_ops(self, ops);
            }
            "up" | "down" if m.alt => {
                let up = ev.keystroke.key == "up";
                let ops = self
                    .store
                    .lock()
                    .ok()
                    .and_then(|s| s.load_tree(&slug).ok())
                    .map(|parts| orchestrator_store::reorder_op(&parts, fid, up))
                    .unwrap_or_default();
                apply_ops(self, ops);
            }
            // ⌘⌫ = delete the focused subtree INSTANTLY, leaf-first (docs/019:
            // a bare Remove strands children; undo is the confirmation). A
            // DESTRUCTIVE verb requires an EXPLICIT selection (review): the
            // effective-first-root fallback must never let a stray ⌘Delete
            // remove a root the user never picked.
            "backspace" if m.secondary() => {
                if self.focused_part.is_none() || !self.delete_part_subtree(fid, cx) {
                    return false;
                }
            }
            _ => return false,
        }
        cx.notify();
        true
    }

    /// Delete a subtree INSTANTLY, leaf-first (docs/019: a bare Remove
    /// strands children; ⌘Z is the confirmation — no dialogs). One body
    /// shared by the outline grammar's ⌘⌫ and the canvas context menu.
    /// Keeps the outline anchored on the hole's edge, and re-roots a canvas
    /// drilled into the dead node (its stale-root fallback would silently
    /// jump to the project root, losing drill context). False = no such node.
    pub(crate) fn delete_part_subtree(&mut self, id: PartId, cx: &mut Context<Self>) -> bool {
        let slug = self.project().slug.clone();
        let parent = {
            let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let parts = store.load_tree(&slug).unwrap_or_default();
            let ops = orchestrator_store::subtree_removal_ops(&parts, id);
            if ops.is_empty() {
                return false;
            }
            let _ = store.accept_diff_from(&slug, &ops, "user", None);
            parts.iter().find(|p| p.id == id).and_then(|p| p.parent_id)
        };
        self.focused_part = parent;
        if self.map_root_of(&slug) == Some(id) {
            self.set_map_root(&slug, parent);
        }
        cx.notify();
        true
    }

    /// Commit the rail's inline name field — "＋ new project" or "＋ idea".
    fn commit_rail_new(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.rail_new.take() else {
            return;
        };
        let name = name.trim().to_string();
        self.rail_new_err = None;
        if name.is_empty() {
            cx.notify();
            return;
        }
        match self.rail_new_kind {
            RailNewKind::Project => self.commit_new_project(&name, cx),
            RailNewKind::Idea => self.commit_new_idea(&name, cx),
            // OpenFolder never fills the inline field (it opens the picker
            // directly, #34), so `rail_new` is never Some for it and this branch
            // is unreachable — present only to keep the match exhaustive.
            RailNewKind::OpenFolder => {}
        }
    }

    /// ＋ NEW PROJECT (#29): the project gets its OWN DIRECTORY at
    /// `<projects_root>/<slug>/`, created on disk now, and is `path:`-keyed from
    /// birth — the same key the registry derives from a session run in that dir.
    /// That is what makes the folder impossible to see twice in the rail: the
    /// store source and the session sources land in ONE resolver group.
    fn commit_new_project(&mut self, name: &str, cx: &mut Context<Self>) {
        let dir = self.projects_root.join(projdir::dir_name_for(name));
        // the key the registry will derive for this dir — the same key its own
        // sessions produce (see scan::key_for_dir). Born canonical ⇒ no dup.
        let key = orchestrator_core::scan::key_for_dir(&dir);
        // who already owns that directory? (the rail AND the store — see
        // projdir::owner_of: a project the scan never saw still owns its key)
        let taken = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            store.project_exists(&key)
        };
        match projdir::owner_of(&self.projects, &dir, &key, taken, None) {
            // that folder IS a project already — land on it rather than fork a
            // second row over the same directory.
            projdir::Owner::Scanned(_) => {
                if let Some((slug, landed)) = self
                    .projects
                    .iter()
                    .find(|p| p.slug == key || p.path.as_deref() == Some(dir.as_path()))
                    .map(|p| (p.slug.clone(), p.name.clone()))
                {
                    self.select_project(&slug, cx);
                    // AFTER select_project, which clears rail_new_err. `dir_name_for`
                    // slugifies "Teams", "Teams!!" and "teams " to the same folder, so
                    // the typed name is DISCARDED here — silently, until #70: no
                    // message, and the rail just switched to a project he didn't ask
                    // for. Same note the Idea verb gives (idea_collision_note).
                    self.rail_new_err = idea_collision_note(name, &landed);
                    cx.notify();
                }
                return;
            }
            // …or it belongs to a project the last scan NEVER SAW. `ensure_project`
            // is an UPSERT — creating here would silently INHERIT that project's
            // map and memory, and RENAME it. Refuse; never merge blindly.
            projdir::Owner::Forgotten => {
                self.rail_new_err = Some(format!(
                    "{} already belongs to another project",
                    dir.display()
                ));
                cx.notify();
                return;
            }
            projdir::Owner::Free => {}
        }
        // a PATH-LESS project that would land on this very directory IS this
        // project — promote it, never fork a second row. Without this, naming a new
        // project after an existing idea minted a permanent duplicate: the idea
        // (slug `idea:omega`, path `None`) matched NEITHER guard above, so the rail
        // showed it twice forever with the notes stranded on the row he had to
        // abandon — and the idea then became permanently UN-SPAWNABLE, because
        // `ensure_project_dir` would find the sibling and refuse.
        let promote = projdir::pathless_owner(&self.projects, name);
        // adopting (never clobbering) a folder that already has content is
        // legitimate — but say so, so it's never silent.
        let adopting = dir.is_dir() && projdir::is_nonempty_dir(&dir);
        if let Err(e) = projdir::create_dir(&dir) {
            self.rail_new_err = Some(e);
            cx.notify();
            return;
        }
        match promote {
            Some(old) => {
                // moves the store rows, the RAM caches and any live session with it
                if let Err(e) = self.promote_project(&old, &key, &dir, name) {
                    self.rail_new_err = Some(e);
                    cx.notify();
                    return;
                }
            }
            None => {
                {
                    let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = store.ensure_project(&key, name);
                    // the recorded path is what makes the next scan inject an
                    // Explicit source at this dir — without it the row would
                    // vanish on rescan.
                    let _ = store.set_project_path(&key, &dir.to_string_lossy());
                }
                // optimistic insert; the next rescan re-derives an identical row
                // (pinned by core test at_dir_matches_resolver_path_row_shape).
                self.projects
                    .push(orchestrator_core::Project::at_dir(&key, dir.clone(), name));
            }
        }
        if adopting {
            self.term_error = Some(format!("using the existing folder {}", dir.display()));
        }
        self.select_project(&key, cx);
    }

    /// ＋ IDEA (#10): a project with no code yet — path-less on purpose. It gets
    /// a directory (and a `path:` key) the moment it earns one: its first spawn
    /// (`ensure_project_dir` → `promote_project`).
    ///
    /// The decision is `idea_action` (pure); this only performs it. It used to
    /// guard on the in-memory rail alone — the unguarded twin of
    /// `commit_new_project` — which is how a row could be a GHOST FROM BIRTH: see
    /// `idea_action` for the two ways.
    fn commit_new_idea(&mut self, name: &str, cx: &mut Context<Self>) {
        let key = orchestrator_core::registry::idea_key(name);
        // the folder this idea will claim at its FIRST SPAWN (ensure_project_dir
        // joins exactly this), and the key the registry mints for it.
        let dir = self.projects_root.join(projdir::dir_name_for(name));
        let dir_key = orchestrator_core::scan::key_for_dir(&dir);
        let (key_taken, dir_taken) = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            (store.project_exists(&key), store.project_exists(&dir_key))
        };
        let action = idea_action(&self.projects, name, &key, &dir, &dir_key, key_taken, dir_taken);
        match action {
            IdeaAction::Select { slug, note } => {
                self.select_project(&slug, cx);
                // AFTER select_project, which clears rail_new_err — the note is
                // about the name we just discarded, so it has to outlive the switch.
                self.rail_new_err = note;
                cx.notify();
            }
            IdeaAction::Refuse(msg) => {
                self.rail_new_err = Some(msg);
                cx.notify();
            }
            IdeaAction::Create => {
                if let Ok(s) = self.store.lock() {
                    let _ = s.ensure_project(&key, name);
                }
                // optimistic insert; the next rescan re-derives an identical row
                // (pinned by core test idea_constructor_matches_resolver_store_row_shape)
                self.projects
                    .push(orchestrator_core::Project::idea(&key, name));
                self.select_project(&key, cx);
            }
        }
    }

    /// Create a store-backed idea project (#10) — an idea is just a project.
    pub(crate) fn route_inline_key(
        &mut self,
        ev: &KeyDownEvent,
        target: InlineTarget,
        cx: &mut Context<Self>,
    ) {
        let k = ev.keystroke.key.as_str();
        let buf: &mut String = match target {
            InlineTarget::RailIdea => self.rail_new.as_mut().unwrap(),
            InlineTarget::Outline => &mut self.outline_edit.buf,
            // both unwraps are gated by the root router (draft.editing.is_some()
            // / setting_edit.is_some()) exactly as RailIdea is gated by is_some().
            InlineTarget::ProfileField => {
                let d = self.profile_draft.as_mut().unwrap();
                // the slot IS the buffer, so there is no third copy to keep in
                // sync; Label is the fallback so a slot-less draft can't panic.
                match d.editing {
                    Some(DraftSlot::Model) => &mut d.model,
                    Some(DraftSlot::ExtraArgs) => &mut d.extra_args,
                    _ => &mut d.label,
                }
            }
            InlineTarget::SettingText => &mut self.setting_edit.as_mut().unwrap().buf,
        };
        match k {
            "enter" => match target {
                InlineTarget::RailIdea => self.commit_rail_new(cx),
                // ⏎ ends field editing, keeping the typed buffer — the draft's
                // own Cancel is the discard-everything analog of RailIdea's Esc.
                InlineTarget::ProfileField => {
                    if let Some(d) = self.profile_draft.as_mut() {
                        d.editing = None;
                    }
                }
                // ⏎ commits the setting: writes the store key + re-applies config.
                InlineTarget::SettingText => self.commit_setting_text(cx),
                InlineTarget::Outline => {
                    // the Detail slot is a TEXTAREA (docs/019 slice 1b): plain
                    // Enter is a newline, ⌘⏎ commits. Every single-line slot
                    // keeps commit-on-⏎.
                    if matches!(
                        self.outline_edit.active,
                        Some(outlinepane::EditSlot::Detail(_))
                    ) && !ev.keystroke.modifiers.secondary()
                    {
                        self.outline_edit.buf.push('\n');
                    } else {
                        self.commit_outline_edit(cx);
                    }
                }
            },
            "escape" => match target {
                InlineTarget::RailIdea => {
                    self.rail_new = None;
                    self.rail_new_err = None;
                }
                InlineTarget::Outline => {
                    self.outline_edit = outlinepane::EditState::default();
                    // an Esc'd dbl-click-create leaves no pin behind.
                    self.canvas_create_pin = None;
                }
                // Esc leaves the field (the draft stays open — cancelling the
                // whole draft is the Cancel button); the buffer is kept. Kept
                // even for a half-typed Custom… model, unlike SettingText below:
                // nothing in the draft is persisted until Save, so there is no
                // half-written store value to protect the user from.
                InlineTarget::ProfileField => {
                    if let Some(d) = self.profile_draft.as_mut() {
                        d.editing = None;
                    }
                }
                // Esc discards the in-progress setting edit (nothing persisted),
                // exactly like RailIdea drops rail_new.
                InlineTarget::SettingText => {
                    self.setting_edit = None;
                }
            },
            "backspace" => {
                buf.pop();
            }
            "space" => buf.push(' '),
            _ => {
                if !ev.keystroke.modifiers.secondary() && !ev.keystroke.modifiers.control {
                    if let Some(ch) = ev
                        .keystroke
                        .key_char
                        .clone()
                        .filter(|c| !c.is_empty())
                        .or_else(|| (k.chars().count() == 1).then(|| k.to_string()))
                    {
                        buf.push_str(&ch);
                    }
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

}

/// What "＋ Idea" should DO with a typed name — decided PURELY from (the rail,
/// the idea key, the folder the idea would claim at its first spawn, and whether
/// the store already holds either key). No gpui, no store, no fs — the shape of
/// `projdir::open_folder_action` (#34) — so the anti-ghost guarantee is testable
/// without an App.
#[derive(Debug, PartialEq, Eq)]
enum IdeaAction {
    /// The name landed on a project that ALREADY EXISTS — select it instead of
    /// forking a second row over it. `note` is the line telling the user the
    /// name they typed was discarded.
    Select { slug: String, note: Option<String> },
    /// A project the last scan never saw owns that key or that folder. Refuse:
    /// `ensure_project` is an UPSERT and would rename them out from under
    /// themselves.
    Refuse(String),
    /// Nobody owns either — mint the path-less row.
    Create,
}

/// The line the rail shows when a typed name LANDS ON AN EXISTING PROJECT rather
/// than creating one. `idea_key` strips every non-alphanumeric, so "🚀🔥", "!!!"
/// and "---" all key to `idea:untitled`, and "Weird -- Name" keys the same as
/// "weird name" — the typed name is dropped and the user arrives on someone
/// else's project. That was SILENT (#70): no message, no toast, and
/// `rail_new_err` cleared on the way in and never set again.
///
/// `None` when the typed name IS the landed-on name: opening the project you
/// just named is not a surprise, and a red line under it is noise.
fn idea_collision_note(typed: &str, landed: &str) -> Option<String> {
    (typed != landed).then(|| format!("“{typed}” lands on the existing “{landed}”"))
}

/// The guards `commit_new_idea` never had (#70). It checked ONE thing — "is this
/// idea key in the rail?" — where its twin `commit_new_project` checks the
/// directory's owner AND the store. Each miss is its own disaster, and neither is
/// undoable: the app has no delete and no rename.
///
///   * the FOLDER. `dir_name_for` slugifies exactly like `idea_key`, so typing an
///     existing project's name minted a second same-named row that is a GHOST
///     FROM BIRTH — its every spawn hits `ensure_project_dir`'s anti-fusion
///     refusal, forever, because the folder belongs to the project sitting right
///     above it in the rail. `idea:zomb` in the tracker (#33) is the fossil of
///     this class.
///   * the STORE. `ensure_project` is an UPSERT (`ON CONFLICT(key) DO UPDATE SET
///     name=?2`) and the rail is only what the LAST SCAN SAW — so in the
///     pre-first-scan window (or against a project whose transcripts aged out)
///     writing the key RENAMED that project and handed the new row its map,
///     journal, notes, memory and sessions.
///
/// `dir_key` is the key the REGISTRY will mint for `dir` (not one of our
/// invention), and the two `*_in_store` flags are the store's answer for the idea
/// key and for `dir_key` — passed in so this stays pure.
fn idea_action(
    projects: &[orchestrator_core::Project],
    name: &str,
    key: &str,
    dir: &std::path::Path,
    dir_key: &str,
    key_in_store: bool,
    dir_key_in_store: bool,
) -> IdeaAction {
    // the same name slugifies to an idea already in the rail — land there, never
    // silently rename/merge the existing project (review 1b).
    if let Some(p) = projects.iter().find(|p| p.slug == key) {
        return IdeaAction::Select {
            slug: p.slug.clone(),
            note: idea_collision_note(name, &p.name),
        };
    }
    // …and if the STORE holds that key while the rail doesn't, we cannot land on
    // it (there is no row to select) and must not write it. Refuse.
    if key_in_store {
        return IdeaAction::Refuse(format!("“{name}” already belongs to another project"));
    }
    match projdir::owner_of(projects, dir, dir_key, dir_key_in_store, None) {
        projdir::Owner::Scanned(landed) => {
            match projects
                .iter()
                .find(|p| p.slug == dir_key || p.path.as_deref() == Some(dir))
            {
                // the very row `owner_of` matched, found by its own predicate
                Some(p) => IdeaAction::Select {
                    slug: p.slug.clone(),
                    note: idea_collision_note(name, &p.name),
                },
                // unreachable by construction; refuse rather than mint a row over
                // a folder somebody owns.
                None => IdeaAction::Refuse(format!(
                    "{} is already the project “{landed}”",
                    dir.display()
                )),
            }
        }
        projdir::Owner::Forgotten => IdeaAction::Refuse(format!(
            "{} already belongs to another project",
            dir.display()
        )),
        // a PATH-LESS project that would claim this very folder IS this idea.
        // `dir_name_for` caps the slug at 64 chars, so two long names can share a
        // directory without sharing an idea key — and two path-less rows racing
        // for one folder means whichever spawns SECOND is refused forever.
        projdir::Owner::Free => match projdir::pathless_owner(projects, name)
            .and_then(|slug| projects.iter().find(|p| p.slug == slug))
        {
            Some(p) => IdeaAction::Select {
                slug: p.slug.clone(),
                note: idea_collision_note(name, &p.name),
            },
            None => IdeaAction::Create,
        },
    }
}

#[cfg(test)]
mod tests {
    // gpui is glob-imported above; import selectively (house rule).
    use super::{idea_action, idea_collision_note, IdeaAction};
    use orchestrator_core::Project;

    const ROOT: &str = "/Users/dev/local";

    fn idea(name: &str) -> Project {
        Project::idea(&orchestrator_core::registry::idea_key(name), name)
    }
    fn at(dir: &str, name: &str) -> Project {
        Project::at_dir(&format!("path:{dir}"), std::path::PathBuf::from(dir), name)
    }

    /// The arguments `commit_new_idea` computes, so a test names only what it is
    /// actually varying. `dir_key` is spelled out rather than derived through
    /// `scan::key_for_dir`, which would touch the filesystem (RULE ZERO).
    fn act(projects: &[Project], name: &str, key_in_store: bool, dir_in_store: bool) -> IdeaAction {
        let key = orchestrator_core::registry::idea_key(name);
        let dir = std::path::Path::new(ROOT).join(crate::projdir::dir_name_for(name));
        let dir_key = format!("path:{}", dir.display());
        idea_action(projects, name, &key, &dir, &dir_key, key_in_store, dir_in_store)
    }

    /// The two guards that live inside `&mut Context<Self>` methods and so can't be
    /// driven from a unit test (gpui has no test Context). Source canary, the
    /// features.rs idiom: every needle is split across a `concat!`, because the file
    /// being grepped is this one and a whole literal would match ITSELF — a canary
    /// that can only ever pass.
    #[test]
    fn the_two_creation_verbs_both_say_where_the_typed_name_went() {
        let src = include_str!("outline_edit.rs");
        let body = src.split("mod tests").next().unwrap_or(src);
        // `commit_new_project` is the verb the DEFAULT build exposes (Idea is
        // behind MAP_ENABLED), and its Scanned arm used to switch projects with no
        // message at all: "Teams", "Teams!!" and "teams " all land on `teams`.
        let proj = body.split("fn commit_new_project").nth(1).unwrap_or("");
        let proj = proj.split("fn commit_new_idea").next().unwrap_or(proj);
        assert!(
            proj.contains(concat!("idea_collision_", "note(name, &landed)")),
            "a name discarded by slugification must be named out loud"
        );
        // …and the map/outline authoring surface must not file nodes under the
        // empty-portfolio sentinel's phantom key, which nothing can ever reopen.
        let outline = body.split("fn commit_outline_edit").nth(1).unwrap_or("");
        assert!(
            outline
                .split("let slug")
                .next()
                .unwrap_or("")
                .contains(concat!("no_project_", "reason()")),
            "the sentinel guard must run BEFORE the slug the ops are filed under"
        );
    }

    /// The bug: `idea_key("Teams")` and `dir_name_for("Teams")` slugify the same
    /// way, so an idea named after a project that already exists claims a folder
    /// that project already owns. The row was minted anyway — and then EVERY
    /// spawn on it hit `ensure_project_dir`'s anti-fusion refusal, with no delete
    /// anywhere in the app to clear it. Land on the project instead.
    #[test]
    fn an_idea_named_after_an_existing_project_can_never_mint_a_second_row() {
        let rail = vec![at("/Users/dev/local/teams", "teams")];
        assert_eq!(
            act(&rail, "Teams", false, false),
            IdeaAction::Select {
                slug: "path:/Users/dev/local/teams".into(),
                note: Some("“Teams” lands on the existing “teams”".into()),
            }
        );
        // a GIT-keyed row owns its directory too — different key, same folder.
        let repo = Project::at_dir(
            "github:acme/teams",
            std::path::PathBuf::from("/Users/dev/local/teams"),
            "teams",
        );
        assert_eq!(
            act(&[repo], "Teams", false, false),
            IdeaAction::Select {
                slug: "github:acme/teams".into(),
                note: Some("“Teams” lands on the existing “teams”".into()),
            }
        );
    }

    /// `ensure_project` is an UPSERT, and the rail is only what the LAST SCAN
    /// SAW: at boot (before the first scan lands) it is EMPTY while the store
    /// still holds every project. Writing the key there renamed a real project
    /// and handed the new idea its map, journal, notes, memory and sessions.
    #[test]
    fn an_idea_can_never_upsert_over_a_project_the_scan_has_not_seen() {
        assert_eq!(
            act(&[], "Zomb", true, false),
            IdeaAction::Refuse("“Zomb” already belongs to another project".into())
        );
        // …and the same for the FOLDER's key: `path:<dir>` held by a project whose
        // transcripts aged out (claude prunes at ~30d) is just as absent.
        assert_eq!(
            act(&[], "Zomb", false, true),
            IdeaAction::Refuse("/Users/dev/local/zomb already belongs to another project".into())
        );
    }

    /// The silent landing (#70 part C): `idea_key` strips every non-alphanumeric,
    /// so a name made of emoji or punctuation keys to `idea:untitled` — the typed
    /// name is discarded and the user arrives on a project that isn't theirs, with
    /// no message at all. Say where they landed.
    #[test]
    fn a_slug_collision_says_where_the_user_landed() {
        let rail = vec![idea("!!!")]; // slug `idea:untitled`
        assert_eq!(
            act(&rail, "🚀🔥", false, false),
            IdeaAction::Select {
                slug: "idea:untitled".into(),
                note: Some("“🚀🔥” lands on the existing “!!!”".into()),
            }
        );
        // dashes and case collapse too: "Weird -- Name" and "weird name" are one key.
        let rail = vec![idea("Weird -- Name")];
        assert_eq!(
            act(&rail, "weird name", false, false),
            IdeaAction::Select {
                slug: "idea:weird-name".into(),
                note: Some("“weird name” lands on the existing “Weird -- Name”".into()),
            }
        );
    }

    /// Re-typing a name you already used is not a collision worth a red line —
    /// the project you asked for is the project you got.
    #[test]
    fn landing_on_your_own_name_is_not_worth_a_message() {
        assert_eq!(idea_collision_note("Omega", "Omega"), None);
        assert_eq!(
            act(&[idea("Omega")], "Omega", false, false),
            IdeaAction::Select { slug: "idea:omega".into(), note: None }
        );
    }

    /// Two names longer than the 64-char folder cap share a DIRECTORY without
    /// sharing an idea key, so neither `owner_of` (both rows are path-less) nor
    /// the key check sees the clash — and whichever spawns second is refused
    /// forever. They are one idea.
    #[test]
    fn two_long_names_that_share_a_folder_are_one_idea() {
        let long = "a".repeat(64);
        let first = format!("{long} one");
        let second = format!("{long} two");
        assert_eq!(
            crate::projdir::dir_name_for(&first),
            crate::projdir::dir_name_for(&second),
            "the fixture must actually collide on the folder"
        );
        let rail = vec![idea(&first)];
        assert_eq!(
            act(&rail, &second, false, false),
            IdeaAction::Select {
                slug: orchestrator_core::registry::idea_key(&first),
                note: Some(format!("“{second}” lands on the existing “{first}”")),
            }
        );
    }

    /// The happy path stays happy: an unclaimed name mints the path-less row, and
    /// unrelated neighbours in the rail don't block it.
    #[test]
    fn a_free_name_still_creates_the_idea() {
        let rail = vec![at("/Users/dev/local/teams", "teams"), idea("Omega")];
        assert_eq!(act(&rail, "Cinematic Check-in", false, false), IdeaAction::Create);
    }
}
