use gpui::prelude::FluentBuilder;
use gpui::*;
use crate::*;


impl Orchestrator {
    /// Write a pasted image to $TMPDIR and return its path, or None if the
    /// paste was refused or the write failed.
    ///
    /// $TMPDIR, matching Ghostty: a pasted screenshot is scratch, and the OS
    /// already knows how to clean it up. The bytes go out in the format they
    /// arrived in rather than transcoded to PNG — lossless, and no image
    /// encoder in the dependency tree.
    ///
    /// NOTE this types a LOCAL path. Kod's sessions run locally, so that is the
    /// right answer here; it is the one thing Ghostty gets bitten by over SSH,
    /// where the path means nothing on the far side.
    fn write_pasted_image(&mut self, img: &gpui::Image) -> Option<String> {
        if !crate::pastedrop::image_paste_ok(img.bytes.len()) {
            return None;
        }
        self.paste_seq = self.paste_seq.wrapping_add(1);
        let name = crate::pastedrop::image_filename(
            crate::timefmt::now_ms(),
            self.paste_seq,
            &img.format,
        );
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, &img.bytes).ok()?;
        Some(path.to_string_lossy().into_owned())
    }


    /// The AGENT stage (#9 slice 2): the focused session's pane — a subhead with
    /// the Stream|Terminal toggle + the body (curated stream / raw terminal). The
    /// session ROSTER now lives in the sidebar (Agent Rail), so there's no in-stage
    /// list and no peek/expand drawer — the agent fills the stage at full size.
    pub(crate) fn render_agent_stage(&self, slug: &str, cx: &mut Context<Self>) -> AnyElement {
        let live = self.cached_infos(slug);
        let active = self.active_session_id();
        match active.and_then(|id| live.iter().find(|s| s.id == id).cloned()) {
            Some(info) => {
                let id = info.id;
                let subhead = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(9.))
                    .px(px(16.))
                    .py(px(9.))
                    .border_b_1()
                    .border_color(rgb(HAIR))
                    .child(
                        div()
                            .w(px(7.))
                            .h(px(7.))
                            .rounded(px(4.))
                            .bg(rgb(termview::phase_dot(info.phase))),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child(SharedString::from(termview::session_label(&info))),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(phase_color(info.phase)))
                            .child(phase_word(info.phase)),
                    )
                    .child(div().flex_1())
                    // ⇄ move this session to another project (dogfooding #10:
                    // work often outgrows the project it was spawned in).
                    .child({
                        let open = self.move_menu_open;
                        div()
                            .id("sess-move")
                            .px(px(8.))
                            .py(px(3.))
                            .rounded(px(7.))
                            .cursor_pointer()
                            .text_size(px(11.5))
                            .text_color(rgb(if open { ACCENT } else { MUTED2 }))
                            .when(open, |d| d.bg(rgb(CARD)))
                            .hover(|h| h.text_color(rgb(ACCENT)))
                            .child("⇄ move")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.move_menu_open = !this.move_menu_open;
                                cx.notify();
                            }))
                    }); // Stream tab killed (dogfooding: "no use at all") — terminal only.
                let body: AnyElement = {
                    {
                        // the grid + a read-only scrollbar thumb + a 'jump to live'
                        // pill when scrolled up (#9 slice 3). The PTY is sized to the
                        // viewport (reflow_terminal), so render every row it has.
                        let term: AnyElement = match self.host.snapshot(id) {
                            Some(snap) => {
                                let n = snap.rows.len();
                                let scrolled = snap.display_offset > 0;
                                let scrollbar = termview::render_scrollbar(&snap, n, cx);
                                // the selection only applies to the session it was made in.
                                let sel = if self.selection_session == Some(id) {
                                    self.selection
                                } else {
                                    None
                                };
                                // map matches → visible viewport rows (labove = off + n-1 - row).
                                let mut spaint: termview::SearchPaint = Vec::new();
                                if self.search.open && self.search.session == Some(id) {
                                    let off = snap.display_offset;
                                    for (mi, m) in self.search.matches.iter().enumerate() {
                                        if m.lines_above_bottom >= off
                                            && (m.lines_above_bottom - off) < n as u32
                                        {
                                            let row = n - 1 - (m.lines_above_bottom - off) as usize;
                                            spaint.push((
                                                row,
                                                m.start as usize,
                                                m.end as usize,
                                                mi == self.search.cur,
                                            ));
                                        }
                                    }
                                }
                                let squery = if self.search.open && self.search.session == Some(id)
                                {
                                    self.search.query.clone()
                                } else {
                                    String::new()
                                };
                                // IME (#9 3c): a non-interactive canvas overlay whose PAINT closure
                                // registers the input handler each frame (handle_input must run in
                                // paint). Self-guards on term_focus being focused.
                                let ime_entity = cx.entity();
                                let ime_focus = self.term_focus.clone();
                                div().flex_1().min_h_0().relative()
                                    // thumb-drag (#9 3b step 5): move/up live on the CONTAINER so
                                    // the drag survives the cursor leaving the 9px thumb.
                                    .on_mouse_move(cx.listener(move |this, e: &MouseMoveEvent, _w, cx| {
                                        // B4: only while the LEFT button is held. A hover with no button
                                        // means the release was missed (session exited mid-drag → this
                                        // handler wasn't painted to catch the up) — drop the stale drag so
                                        // it can't drive scroll on plain hover.
                                        if e.pressed_button != Some(MouseButton::Left) {
                                            if this.scrollbar_drag.take().is_some() {
                                                cx.notify();
                                            }
                                            return;
                                        }
                                        if let Some(a) = this.scrollbar_drag {
                                            if let Some(s) = this.host.snapshot(id) {
                                                // recompute container_h from the LIVE snapshot so a
                                                // mid-drag window resize doesn't skew the mapping.
                                                let container_h = (s.rows.len() as f32).max(1.0) * LINE_PX;
                                                let d = orchestrator_host::grid_view::scrollbar_delta(a.grab_y, f32::from(e.position.y), a.off0, s.display_offset, s.history_size, s.rows.len() as u32, container_h);
                                                if d != 0 { this.host.scroll(id, d); this.clear_term_selection(); cx.notify(); }
                                            }
                                        }
                                    }))
                                    .on_mouse_up(MouseButton::Left, cx.listener(move |this, _e: &MouseUpEvent, _w, cx| {
                                        if this.scrollbar_drag.take().is_some() { cx.notify(); }
                                    }))
                                    .child(termview::render_grid(&snap, n, sel, &spaint, &squery, cx))
                                    .when(self.search.open && self.search.session == Some(id), |c| {
                                        let total_label = if self.search.matches.is_empty() {
                                            if self.search.query.chars().count() >= 2 { "0".to_string() } else { String::new() }
                                        } else if (self.search.total as usize) > self.search.matches.len() || self.search.matches.len() >= 300 {
                                            format!("{}/{}+", self.search.cur + 1, self.search.matches.len())
                                        } else {
                                            format!("{}/{}", self.search.cur + 1, self.search.matches.len())
                                        };
                                        c.child(
                                            div()
                                                .absolute().top(px(8.)).right(px(18.))
                                                .flex().flex_row().items_center().gap(px(8.))
                                                .px(px(11.)).py(px(6.)).rounded(px(9.))
                                                .bg(rgb(0x1A2029)).border_1().border_color(rgb(0x3A4A57))
                                                .child(div().text_size(px(11.)).text_color(rgb(MUTED2)).child("⌘F"))
                                                .child(
                                                    div().flex().flex_row().items_center().min_w(px(120.)).max_w(px(260.)).overflow_hidden()
                                                        .font_family("Menlo").text_size(px(12.5)).text_color(rgb(TEXT_STRONG))
                                                        .child(SharedString::from(if self.search.query.is_empty() { "type to search…".to_string() } else { self.search.query.clone() }))
                                                        .when(!self.search.query.is_empty(), |c| c.child(div().text_color(rgb(ACCENT)).child("▏"))),
                                                )
                                                .when(!total_label.is_empty(), |c2| c2.child(div().flex_none().text_size(px(11.)).text_color(rgb(AMBER)).child(SharedString::from(total_label))))
                                                .child(div().flex_none().text_size(px(10.)).text_color(rgb(MUTED2)).child("↵ next · ⇧↵ prev · esc")),
                                        )
                                    })
                                    .child(
                                        canvas(
                                            |_b, _w, _a| {},
                                            move |bounds, _s, window, app| {
                                                window.handle_input(&ime_focus, ElementInputHandler::new(bounds, ime_entity.clone()), app);
                                                // pixel→cell drag-select (#9): 3 window-level handlers
                                                // replace ~3×rows per-row listeners. cell_at is pure (TDD'd).
                                                // rows_visible == total here, so the first rendered row is 0.
                                                let cell = move |pos: Point<Pixels>| orchestrator_host::grid_view::cell_at(
                                                    f32::from(pos.x), f32::from(pos.y), f32::from(bounds.origin.x), f32::from(bounds.origin.y),
                                                    12.0, 8.0, CHAR_PX, LINE_PX, 0, n,
                                                );
                                                let e1 = ime_entity.clone();
                                                window.on_mouse_event(move |e: &MouseDownEvent, phase, window, app| {
                                                    if phase != DispatchPhase::Bubble || e.button != MouseButton::Left { return; }
                                                    if !bounds.contains(&e.position) || e.modifiers.secondary() { return; }
                                                    let c = cell(e.position);
                                                    e1.update(app, |this, cx| {
                                                        // clicking the grid hands the keyboard back to the
                                                        // terminal — close the ⌘F bar (routing-modality critique).
                                                        this.search.close();
                                                        this.drag_anchor = Some(c);
                                                        this.drag_from_px = Some(e.position); // press origin for the click-vs-drag slop
                                                        this.drag_moved = false;
                                                        this.selection = None; // not a selection until it moves
                                                        this.selection_session = Some(id);
                                                        this.term_focus.focus(window);
                                                        cx.notify();
                                                    });
                                                });
                                                let e2 = ime_entity.clone();
                                                window.on_mouse_event(move |e: &MouseMoveEvent, phase, _w, app| {
                                                    if phase != DispatchPhase::Bubble || e.pressed_button != Some(MouseButton::Left) { return; }
                                                    e2.update(app, |this, cx| {
                                                        if let Some(a) = this.drag_anchor {
                                                            // LATCH a real drag the moment the pointer leaves the click
                                                            // slop, and keep it latched even if it returns to the press
                                                            // cell — so an out-and-back (or a straight-down drag whose
                                                            // cell clamps) never counts as a click-to-open on release.
                                                            if !this.drag_moved {
                                                                if let Some(p0) = this.drag_from_px {
                                                                    let dx = f32::from(e.position.x) - f32::from(p0.x);
                                                                    let dy = f32::from(e.position.y) - f32::from(p0.y);
                                                                    if dx * dx + dy * dy > CLICK_SLOP_PX * CLICK_SLOP_PX {
                                                                        this.drag_moved = true;
                                                                    }
                                                                }
                                                            }
                                                            let s = Some((a, cell(e.position)));
                                                            if this.selection != s { this.selection = s; cx.notify(); }
                                                        }
                                                    });
                                                });
                                                let e3 = ime_entity.clone();
                                                window.on_mouse_event(move |e: &MouseUpEvent, phase, _w, app| {
                                                    if phase != DispatchPhase::Bubble || e.button != MouseButton::Left { return; }
                                                    // a PLAIN click (no drag, no ⌘) on a link opens it — the browser-like
                                                    // gesture users expect. ⌘-click still opens via the link element's own
                                                    // handler, so gate on !secondary here to avoid a double-open. This
                                                    // path resolves the URL from the PRESS cell itself (grid_view::link_at),
                                                    // so opening never depends on the link element's hitbox winning over
                                                    // the canvas overlay.
                                                    let plain_click = !e.modifiers.secondary() && bounds.contains(&e.position);
                                                    e3.update(app, |this, cx| {
                                                        let anchor = this.drag_anchor.take();
                                                        let moved = this.drag_moved;
                                                        this.drag_from_px = None;
                                                        this.drag_moved = false;
                                                        // a click (no drag) leaves a zero-width selection — drop it.
                                                        if matches!(this.selection, Some((a, b)) if a == b) {
                                                            this.selection = None;
                                                            this.selection_session = None;
                                                        }
                                                        // open only if the pointer never dragged past the slop — on the
                                                        // cell we PRESSED (so a sub-slop jitter into a neighbour can't
                                                        // retarget or suppress it).
                                                        if plain_click && !moved {
                                                            if let Some((r, c)) = anchor {
                                                                if let Some(url) = this
                                                                    .host
                                                                    .snapshot(id)
                                                                    .and_then(|s| orchestrator_host::grid_view::link_at(&s, r, c))
                                                                {
                                                                    cx.open_url(&url);
                                                                }
                                                            }
                                                        }
                                                        cx.notify();
                                                    });
                                                });
                                            },
                                        )
                                        .absolute().top_0().left_0().size_full(),
                                    )
                                    .when_some(scrollbar, |c, sb| c.child(sb))
                                    .when(scrolled, |c| c.child(
                                        div().id("jump-live").absolute().right(px(18.)).bottom(px(10.))
                                            .px(px(10.)).py(px(4.)).rounded(px(14.)).cursor_pointer()
                                            .bg(rgb(0x23303a)).border_1().border_color(rgb(0x3a4a57))
                                            .text_size(px(10.5)).text_color(rgb(TEXT)).hover(|h| h.border_color(rgb(ACCENT)))
                                            .child("↧ jump to live")
                                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                                this.host.scroll(id, -1_000_000);
                                                this.clear_term_selection();
                                                this.term_focus.focus(window);
                                                cx.notify();
                                            }))))
                                    .into_any_element()
                            }
                            None => centered_notice("session ended", "").into_any_element(),
                        };
                        // a SIGNPOST above the grid — one line, never a mirror of the
                        // dialog. The terminal below is the source of truth for the
                        // diff/command and the only place it can be answered; the
                        // banner just says which session is blocked, on what, and
                        // points down. Its height is a known constant that
                        // `reflow_terminal` subtracts from the PTY's row budget, so
                        // the rows it costs are given up rather than masked off the
                        // bottom of the grid — where claude draws `1. Yes / 2. No`.
                        let banner = self
                            .host
                            .pending(id)
                            .map(|p| decision_banner(info.kind.label(), &p));
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .track_focus(&self.term_focus)
                            // Drop a file from Finder -> its escaped path is typed
                            // at the cursor, the way every other terminal behaves.
                            // ON THE TERMINAL SURFACE ONLY: a drop anywhere else in
                            // the window is ignored rather than guessed at.
                            .on_drop(cx.listener(
                                move |this, paths: &ExternalPaths, _, cx| {
                                    let strs: Vec<String> = paths
                                        .paths()
                                        .iter()
                                        .map(|p| p.to_string_lossy().into_owned())
                                        .collect();
                                    let text = crate::pastedrop::drop_text(&strs);
                                    if !text.is_empty() {
                                        this.host.send_key(id, &KeyInput::Paste(text));
                                    }
                                    cx.notify();
                                },
                            ))
                            // send to the session ACTUALLY ON SCREEN (captured `id`);
                            // PageUp/Down scroll the VIEW (scrollback), not the PTY.
                            .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _, cx| {
                                let k = ev.keystroke.key.as_str();
                                // During IME composition, Enter/Esc/space/arrows belong to the IME
                                // (commit / cancel / candidate-nav) — defer WITHOUT stop_propagation
                                // so the input handler owns them, or East Asian input breaks (#9 3c).
                                if !this.ime_preedit.is_empty()
                                    && matches!(
                                        k,
                                        "enter"
                                            | "escape"
                                            | "space"
                                            | "up"
                                            | "down"
                                            | "left"
                                            | "right"
                                    )
                                {
                                    return;
                                }
                                // ⌘F bar open: the bar owns the keyboard (critique: pinned
                                // ordering — after IME deferral, before every PTY route).
                                // Chars still flow via the IME handler into the QUERY; here we
                                // consume nav/edit keys and SWALLOW the rest so nothing leaks
                                // to the CLI (a leaked Esc would interrupt the agent's turn).
                                if this.search.open && this.search.session == Some(id) {
                                    if ev.keystroke.modifiers.secondary() && k == "v" {
                                        if let Some(text) =
                                            cx.read_from_clipboard().and_then(|c| c.text())
                                        {
                                            this.search.query.push_str(text.trim());
                                            this.kick_search(false);
                                        }
                                        cx.stop_propagation();
                                        cx.notify();
                                        return;
                                    }
                                    if !ev.keystroke.modifiers.secondary() {
                                        match k {
                                            "enter" => {
                                                if this.search.matches.is_empty() {
                                                    this.kick_search(true); // force even 1-char queries
                                                } else if ev.keystroke.modifiers.shift {
                                                    this.search.prev();
                                                    this.jump_to_current();
                                                } else {
                                                    this.search.next();
                                                    this.jump_to_current();
                                                }
                                                cx.stop_propagation();
                                                cx.notify();
                                                return;
                                            }
                                            "escape" => {
                                                // first Esc closes the bar and is CONSUMED — never
                                                // the same keystroke that clears a selection or
                                                // reaches the CLI (critique: pinned Esc ladder).
                                                this.search.close();
                                                cx.stop_propagation();
                                                cx.notify();
                                                return;
                                            }
                                            "backspace" => {
                                                this.search.query.pop();
                                                this.kick_search(false);
                                                cx.stop_propagation();
                                                cx.notify();
                                                return;
                                            }
                                            "pageup" | "pagedown" => {} // view keys fall through below
                                            _ => {
                                                // swallow non-char keys (arrows/tab/ctrl-…); chars
                                                // pass on to the IME handler → the query.
                                                if let Some(key) =
                                                    termview::to_key_input(&ev.keystroke)
                                                {
                                                    if !matches!(key, KeyInput::Char(_)) {
                                                        cx.stop_propagation();
                                                        return;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                // ⌘C copies the drag-selection (#9 3b), before to_key_input
                                // (which already drops ⌘-chords so they never reach the CLI).
                                if ev.keystroke.modifiers.secondary() && k == "c" {
                                    if let Some(sel) = this.selection {
                                        if let Some(snap) = this.host.snapshot(id) {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                orchestrator_host::grid_view::selection_text(
                                                    &snap, sel,
                                                ),
                                            ));
                                        }
                                        this.clear_term_selection();
                                        cx.stop_propagation();
                                        cx.notify();
                                        return;
                                    }
                                }
                                // ⌘V pastes the clipboard into the PTY — KeyInput::Paste gets
                                // bracketed-paste framing when the app enabled it (multi-line safe).
                                if ev.keystroke.modifiers.secondary() && k == "v" {
                                    // IMAGE FIRST, text second. A PTY carries
                                    // bytes, so no terminal can be handed an
                                    // image — what Ghostty and iTerm2 actually
                                    // do is write it to a temp file and type the
                                    // PATH. Kod only ever asked for .text(),
                                    // which is why a screenshot did nothing.
                                    let img = cx.read_from_clipboard().and_then(|c| {
                                        c.entries().iter().find_map(|e| match e {
                                            ClipboardEntry::Image(i) => Some(i.clone()),
                                            _ => None,
                                        })
                                    });
                                    if let Some(img) = img {
                                        match this.write_pasted_image(&img) {
                                            Some(path) => this.host.send_key(
                                                id,
                                                &KeyInput::Paste(crate::pastedrop::drop_text(&[
                                                    path,
                                                ])),
                                            ),
                                            // refused (empty or >10MB) or the
                                            // write failed — say so rather than
                                            // swallowing a paste that looked like
                                            // it worked.
                                            None => {
                                                this.term_error = Some(
                                                    "couldn't paste that image — it is empty, over 10MB, or the temp dir is not writable".into(),
                                                )
                                            }
                                        }
                                        cx.stop_propagation();
                                        cx.notify();
                                        return;
                                    }
                                    if let Some(text) =
                                        cx.read_from_clipboard().and_then(|c| c.text())
                                    {
                                        if !text.is_empty() {
                                            this.host.send_key(id, &KeyInput::Paste(text));
                                        }
                                    }
                                    cx.stop_propagation();
                                    cx.notify();
                                    return;
                                }
                                match k {
                                    "pageup" => {
                                        this.host.scroll(id, 18);
                                        this.clear_term_selection();
                                        cx.stop_propagation();
                                        cx.notify();
                                        return;
                                    }
                                    "pagedown" => {
                                        this.host.scroll(id, -18);
                                        this.clear_term_selection();
                                        cx.stop_propagation();
                                        cx.notify();
                                        return;
                                    }
                                    _ => {}
                                }
                                // Esc dismisses a live selection before reaching the CLI.
                                if k == "escape" && this.selection.is_some() {
                                    this.clear_term_selection();
                                    cx.stop_propagation();
                                    cx.notify();
                                    return;
                                }
                                if let Some(key) = termview::to_key_input(&ev.keystroke) {
                                    // Char keys are DEFERRED to the IME input handler (it owns plain
                                    // Latin AND CJK composition) — sending here would double-dispatch
                                    // (#9 3c). Non-text keys (Enter/Tab/arrows/Ctrl-…) we consume here.
                                    if !matches!(key, KeyInput::Char(_)) {
                                        this.clear_term_selection();
                                        this.host.send_key(id, &key);
                                        cx.stop_propagation();
                                        cx.notify();
                                    }
                                }
                            }))
                            .on_scroll_wheel(cx.listener(scroll_terminal))
                            .when_some(banner, |c, b| c.child(b))
                            .child(term)
                            .into_any_element()
                    }
                };
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(subhead)
                    .child(body)
                    .into_any_element()
            }
            None if live.is_empty() => {
                // no live agent → offer to resume a crashed/recoverable one, else
                // a hint to spawn fresh. The hint names the header's "+" and the
                // ⌘T family because those are the only spawn affordances left:
                // the rail went nav-only (render_sidebar), so the previous
                // "start one from the rail" line sent EVERY empty project at a
                // control that isn't on screen.
                let recs = self.recoverable_for_project();
                if recs.is_empty() {
                    centered_notice(
                        "No agents in this project yet",
                        "Start one from the + button above — ⌘T claude · ⇧⌘T codex · ⌥⌘T shell.",
                    )
                    .into_any_element()
                } else {
                    self.render_recoverable_list(recs, cx).into_any_element()
                }
            }
            None => centered_notice("Pick an agent", "Choose one from the rail on the left.")
                .into_any_element(),
        }
    }

    /// The dismissible spawn/dispatch-error banner (a failed spawn, or an mkdir /
    /// name-collision in spawn_cwd). Rendered ONCE at the workspace root so it is
    /// visible in EVERY mode — critically the Agent stage, where spawn errors land
    /// in the map-off build (default_workspace_mode -> Agent) and where a
    /// spawn_session failure could already strand silently. Click ✕ to clear.
    pub(crate) fn term_error_banner(&self, err: String, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("stage-err")
            .mx(px(14.))
            .mt(px(8.))
            .px(px(12.))
            .py(px(6.))
            .rounded(px(8.))
            .bg(rgb(0x2a1a1a))
            .border_1()
            .border_color(rgb(0x5a3535))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .cursor_pointer()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(0xE68A8A))
                    .child(SharedString::from(err)),
            )
            .child(div().text_size(px(11.)).text_color(rgb(MUTED2)).child("✕"))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.term_error = None;
                cx.notify();
            }))
            .into_any_element()
    }


    /// ⌘F availability matrix (critique): Workspace+Agent with a live session →
    /// open in place; the Standup with a live session in the current project →
    /// jump to its stage and open; no live session → no-op. (The old "not while
    /// on the Settings screen" guard died with that screen: ⌘F is simply not
    /// handled anywhere in the Settings window's tree, #54.)
    pub(crate) fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.active_session_id() else {
            return;
        };
        self.screen = Screen::Workspace;
        self.mode = Mode::Agent;
        self.search.open = true;
        self.search.session = Some(id);
        self.term_focus.focus(window); // the IME handler feeds the query
        if !self.search.query.is_empty() {
            self.kick_search(true);
        }
        cx.notify();
    }

    /// Dispatch the host search off the UI thread (remote = blocking RPC; a
    /// keystroke must never stall a frame — critique). One in flight; edits
    /// during flight coalesce into one requeue.
    pub(crate) fn kick_search(&mut self, force: bool) {
        use std::sync::atomic::Ordering;
        let Some(id) = self.search.session else {
            return;
        };
        let q = self.search.query.clone();
        if q.is_empty() || (q.chars().count() < 2 && !force) {
            self.search.matches.clear();
            self.search.total = 0;
            self.search.cur = 0;
            return;
        }
        if self.search_inflight.swap(true, Ordering::AcqRel) {
            self.search.requeue = true;
            return;
        }
        self.search.gen += 1;
        let gen = self.search.gen;
        if let Some(info) = self.infos_cache.values().flatten().find(|i| i.id == id) {
            self.search.last_dirty = info.dirty;
        }
        let host = self.host.clone();
        let rx = self.search_rx.clone();
        let inflight = self.search_inflight.clone();
        std::thread::spawn(move || {
            let m = host.search(id, &q);
            let total = m.len() as u32;
            *rx.lock().unwrap_or_else(|e| e.into_inner()) = Some((gen, m, total));
            inflight.store(false, Ordering::Release);
        });
    }

    /// Adopt finished search results (called at the top of render).
    pub(crate) fn drain_search(&mut self) {
        let got = self.search_rx.lock().ok().and_then(|mut g| g.take());
        if let Some((gen, matches, total)) = got {
            if gen == self.search.gen && self.search.open {
                self.search.adopt(matches, total);
            }
        }
        if self.search.requeue
            && !self
                .search_inflight
                .load(std::sync::atomic::Ordering::Acquire)
        {
            self.search.requeue = false;
            self.kick_search(false);
        }
    }

    /// Scroll the CURRENT match into view — PURE view scroll, and none at all
    /// on the alt screen (vim/htop: a scroll would become app input; there is
    /// no history there, every match is already on the grid — critique).
    pub(crate) fn jump_to_current(&mut self) {
        let Some(id) = self.search.session else {
            return;
        };
        let Some(m) = self.search.matches.get(self.search.cur).copied() else {
            return;
        };
        if let Some(snap) = self.host.snapshot(id) {
            if snap.alt_screen {
                return;
            }
            let target = m
                .lines_above_bottom
                .saturating_sub(u32::from(STAGE_ROWS) / 2)
                .min(snap.history_size);
            let delta = target as i32 - snap.display_offset as i32;
            if delta != 0 {
                self.host.scroll_view(id, delta);
            }
            self.clear_term_selection();
        }
    }

}

/// The terminal-stage DECISION BANNER — ONE line, fixed height, above the grid.
///
/// It is a SIGNPOST, not a mirror: which session is blocked, on what, and an
/// arrow down. The fat card this replaced re-rendered the diff with an expand
/// drawer and was self-defeating — it grew taller than the rows it displaced,
/// and since the grid is `overflow_hidden` + top-aligned, the surplus PTY rows
/// were MASKED off the bottom, which is precisely where claude draws its
/// `❯ 1. Yes / 2. No` dialog. The card hid the dialog it told you to answer.
///
/// Its height is `DECISION_BANNER_PX` — a CONSTANT, subtracted by `pty_rows` so
/// the terminal gives those rows up instead of having them masked. That constant
/// is only true if the banner NEVER WRAPS: gpui wraps text by default, and a
/// wrapped line silently doubles the element's height, which would put the
/// dialog right back under the mask. Two defences, both load-bearing:
///   * the variable part is `flex_1 min_w_0 truncate` (ellipsis, never a 2nd line)
///     and its text is squashed to one line by `one_line` (payloads really do
///     carry `\n` — ExitPlanMode's plan, multi-line Bash);
///   * the fixed tail is `flex_none whitespace_nowrap` so it can't be re-flowed.
/// Not clickable, no buttons: the answer lives in the dialog below.
fn decision_banner(cli: &str, p: &PendingDecision) -> Div {
    let line = |c: u32| div().flex_none().whitespace_nowrap().text_size(px(12.)).text_color(rgb(c));
    div()
        .flex_none()
        .h(px(DECISION_BANNER_PX))
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .px(px(14.))
        .bg(rgb(AMBER_INK))
        .child(line(AMBER).child("⚠"))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(12.))
                .text_color(rgb(AMBER))
                .child(SharedString::from(banner_text(cli, p))),
        )
        .child(line(AMBER).font_weight(FontWeight::SEMIBOLD).child("— answer below ↓"))
}

/// The banner's variable half (the ⚠ and the "— answer below ↓" tail are drawn
/// as separate, un-truncatable elements). Terse by design: the TERMINAL is the
/// source of truth for the diff/command, so this only has to name the CLI, the
/// tool and the target well enough that the user knows which window to walk
/// to. Pure, so the no-newline invariant is unit-testable.
fn banner_text(cli: &str, p: &PendingDecision) -> String {
    let or = |s: String, fallback: &str| if s.is_empty() { fallback.to_string() } else { s };
    match &p.view {
        DecisionView::Bash { command, .. } => {
            format!("{cli} wants to run: {}", or(one_line(command), "a command"))
        }
        DecisionView::Edit { path, .. } => {
            format!("{cli} wants to edit {}", or(one_line(path), "a file"))
        }
        DecisionView::Write { path, .. } => {
            format!("{cli} wants to write {}", or(one_line(path), "a file"))
        }
        DecisionView::Question { .. } => format!("{cli} is asking you a question"),
        DecisionView::Other { tool, summary } => {
            let tool = or(one_line(tool), &p.tool_name);
            match one_line(summary).as_str() {
                "" => format!("{cli} wants to use {tool}"),
                s => format!("{cli} wants to use {tool}: {s}"),
            }
        }
    }
}

/// How much of a payload the banner will carry before it elides.
const BANNER_CHARS: usize = 120;

/// Squash a payload to exactly ONE line, elided. Load-bearing for the reflow
/// math: a `\n` reaching the banner would lay out a second line and DOUBLE its
/// height, so `DECISION_BANNER_PX` would understate what the banner steals and
/// the masked band at the bottom of the grid would come back. Real payloads have
/// newlines (ExitPlanMode's plan, `Write` content, multi-line Bash). Char-wise,
/// so a multi-byte payload can never split mid-UTF-8.
fn one_line(s: &str) -> String {
    let mut lines = s.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or("");
    let more = lines.next().is_some();
    let long = first.chars().count() > BANNER_CHARS;
    let mut out: String = first.chars().take(BANNER_CHARS).collect();
    if long || more {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    // NOTE: no `use super::*` — gpui is glob-imported at the top of this file.
    use super::{banner_text, one_line, BANNER_CHARS};
    use orchestrator_host::{DecisionView, PendingDecision};

    fn pending(tool: &str, view: DecisionView) -> PendingDecision {
        PendingDecision { tool_use_id: None, tool_name: tool.into(), view }
    }

    #[test]
    fn the_banner_names_the_cli_the_tool_and_the_target() {
        let cases = [
            (
                pending("Edit", DecisionView::Edit {
                    path: "src/main.rs".into(),
                    old: "a".into(),
                    new: "b".into(),
                }),
                "claude wants to edit src/main.rs",
            ),
            (
                pending("Write", DecisionView::Write {
                    path: "src/new.rs".into(),
                    content: "x".into(),
                }),
                "claude wants to write src/new.rs",
            ),
            (
                pending("Bash", DecisionView::Bash {
                    command: "cargo test --workspace".into(),
                    description: "run the tests".into(),
                }),
                "claude wants to run: cargo test --workspace",
            ),
            (
                pending("AskUserQuestion", DecisionView::Question {
                    header: "Choice".into(),
                    prompt: "A or B?".into(),
                    options: vec!["A".into(), "B".into()],
                }),
                // the OPTIONS are never mirrored — the TUI list below is the only
                // complete, correct one, and it is the only thing that can answer.
                "claude is asking you a question",
            ),
            (
                pending("WebFetch", DecisionView::Other {
                    tool: "WebFetch".into(),
                    summary: "https://example.com/x".into(),
                }),
                "claude wants to use WebFetch: https://example.com/x",
            ),
        ];
        for (p, want) in cases {
            assert_eq!(banner_text("claude", &p), want);
        }
        // the CLI name comes from the session, not a hardcoded "claude".
        let p = pending("Bash", DecisionView::Bash { command: "ls".into(), description: String::new() });
        assert_eq!(banner_text("codex", &p), "codex wants to run: ls");
    }

    #[test]
    fn an_unmodelled_tool_with_no_payload_still_names_itself() {
        // regression: a payload-less tool (mcp__*, ExitPlanMode with nothing in it)
        // used to render a BLANK card body. The banner must never degrade to
        // "claude wants to use : " — a signpost pointing at nothing.
        let p = pending("mcp__qc-mcp__read_project", DecisionView::Other {
            tool: "mcp__qc-mcp__read_project".into(),
            summary: "   \n ".into(),
        });
        assert_eq!(banner_text("claude", &p), "claude wants to use mcp__qc-mcp__read_project");
    }

    #[test]
    fn the_banner_is_always_exactly_one_line() {
        // THE WRAP HAZARD. gpui wraps text by default; a wrapped banner is TWICE as
        // tall as DECISION_BANNER_PX, which is the number `pty_rows` subtracts —
        // and the height it understates comes straight back as a masked band at the
        // bottom of the grid, over claude's `1. Yes / 2. No`. Real payloads carry
        // newlines, so `.truncate()` alone is not enough: the STRING must be flat.
        let multi = [
            pending("ExitPlanMode", DecisionView::Other {
                tool: "ExitPlanMode".into(),
                summary: "Plan:\n1. do it\n2. then this".into(),
            }),
            pending("Bash", DecisionView::Bash {
                command: "set -e\ncargo build\ncargo test".into(),
                description: "build\nand test".into(),
            }),
            pending("Write", DecisionView::Write {
                path: "/p/a.rs".into(),
                content: "line1\nline2".into(),
            }),
            pending("Edit", DecisionView::Edit {
                path: "/p/a.rs".into(),
                old: "a\nb".into(),
                new: "c\nd".into(),
            }),
        ];
        for p in &multi {
            let t = banner_text("claude", p);
            assert!(!t.contains('\n'), "banner would WRAP (2× its fixed height): {t:?}");
            assert_eq!(t.lines().count(), 1, "banner must be exactly one line: {t:?}");
        }
        // a hidden tail is signalled, never silently dropped.
        assert_eq!(banner_text("claude", &multi[1]), "claude wants to run: set -e…");
        // and one absurd line can't become a 4000-char string either.
        let long = "x".repeat(4000);
        let p = pending("Bash", DecisionView::Bash { command: long, description: String::new() });
        assert_eq!(banner_text("claude", &p).chars().count(), "claude wants to run: ".chars().count() + BANNER_CHARS + 1);
    }

    #[test]
    fn one_line_elides_on_char_boundaries() {
        // a byte-index truncate would PANIC mid-UTF-8 on a multi-byte payload.
        let e = "é".repeat(500);
        assert_eq!(one_line(&e).chars().count(), BANNER_CHARS + 1);
        assert!(one_line(&e).ends_with('…'));
        // an all-blank payload is EMPTY (never a lone "…"), so the caller can fall
        // back to naming the tool.
        assert_eq!(one_line("  \n\n \t "), "");
        assert_eq!(one_line(""), "");
        // a short single line passes through untouched — no gratuitous ellipsis.
        assert_eq!(one_line("cargo test"), "cargo test");
    }
}
