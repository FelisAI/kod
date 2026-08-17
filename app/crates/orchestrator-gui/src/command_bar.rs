use gpui::prelude::FluentBuilder;
use gpui::*;
use crate::*;


impl Orchestrator {

    /// Open the command bar. `node: Some` forces the machine lane fenced to
    /// that node (context-menu Expand/Rework); `None` is the free classifier
    /// (⌘-typed / clicking the strip). Blurs every other key sink first (the
    /// 1b key-sink rule — a menu/editor underneath would reclaim the keyboard).
    fn open_cmd_bar(
        &mut self,
        node: Option<PartId>,
        rework: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.map_menu = None;
        self.palette.close();
        self.rail_new = None;
        self.blur_outline_edit(cx);
        self.cmd = CmdBar {
            open: true,
            node,
            rework,
            ..Default::default()
        };
        self.cmd_focus.focus(window);
        cx.notify();
    }

    /// Context-menu Expand/Rework entry: open the bar fenced to a node.
    pub(crate) fn open_intent_bar(
        &mut self,
        id: PartId,
        rework: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focused_part = Some(id);
        self.open_cmd_bar(Some(id), rework, window, cx);
    }

    fn close_cmd_bar(&mut self, window: &mut Window) {
        self.cmd = CmdBar::default();
        if self.screen == Screen::Workspace && self.mode == Mode::Agent {
            self.term_focus.focus(window);
        } else {
            self.root_focus.focus(window);
        }
    }

    /// The command bar's keystream (chars via keystroke, like the palette).
    fn cmd_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = ev.keystroke.key.as_str();
        // while awaiting the scope keystroke, t/w answer it (Esc backs out).
        if let Some((text, sel)) = self.cmd.scope_ask.clone() {
            match k {
                "escape" => {
                    self.cmd.scope_ask = None;
                }
                _ => {
                    let ch = ev
                        .keystroke
                        .key_char
                        .clone()
                        .unwrap_or_default()
                        .to_lowercase();
                    if k == "t" || ch == "t" {
                        // this subtree — fence to the selection.
                        self.close_cmd_bar(window);
                        self.start_agentic_run(
                            AgenticKind::Fenced {
                                node: sel,
                                rework: true,
                                intent: Some(text),
                            },
                            cx,
                        );
                    } else if k == "w" || k == "m" || ch == "w" || ch == "m" {
                        // whole map — scope NULL, the intent rides the re-ground.
                        self.close_cmd_bar(window);
                        self.start_agentic_run(AgenticKind::Seed { intent: Some(text) }, cx);
                    }
                }
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        match k {
            "enter" => self.cmd_confirm(window, cx),
            "escape" => self.close_cmd_bar(window),
            "backspace" => {
                self.cmd.query.pop();
            }
            _ => {
                if !ev.keystroke.modifiers.secondary() && !ev.keystroke.modifiers.control {
                    if let Some(ch) = ev
                        .keystroke
                        .key_char
                        .clone()
                        .filter(|c| !c.is_empty())
                        .or_else(|| (k.chars().count() == 1).then(|| k.to_string()))
                    {
                        self.cmd.query.push_str(&ch);
                    } else if k == "space" {
                        self.cmd.query.push(' ');
                    }
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// ↵ in the command bar — route the typed line to its lane.
    fn cmd_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let q = self.cmd.query.trim().to_string();
        // a node-forced bar (context-menu Expand/Rework) is always machine-lane
        // fenced to that node — the query is the one-line intent (may be empty).
        if let Some(node) = self.cmd.node {
            let rework = self.cmd.rework;
            let intent = (!q.is_empty()).then_some(q);
            self.close_cmd_bar(window);
            self.start_agentic_run(
                AgenticKind::Fenced {
                    node,
                    rework,
                    intent,
                },
                cx,
            );
            return;
        }
        if q.is_empty() {
            return;
        }
        match cmdbar::classify(&q) {
            cmdbar::Lane::Imperative(imp) => {
                // human lane, instant + undoable. On a resolution miss, keep the
                // bar open (the preview already shows why) so it can be fixed —
                // an Err leaves the bar as-is.
                if self.cmd_apply_imperative(&imp, cx).is_ok() {
                    self.close_cmd_bar(window);
                }
            }
            cmdbar::Lane::Intent => {
                let sel = self.live_selection();
                match cmdbar::infer_scope(&q, sel.is_some()) {
                    cmdbar::ScopeDecision::WholeMap => {
                        self.close_cmd_bar(window);
                        self.start_agentic_run(AgenticKind::Seed { intent: Some(q) }, cx);
                    }
                    cmdbar::ScopeDecision::Subtree => {
                        let node = sel.expect("Subtree implies a selection");
                        self.close_cmd_bar(window);
                        self.start_agentic_run(
                            AgenticKind::Fenced {
                                node,
                                rework: true,
                                intent: Some(q),
                            },
                            cx,
                        );
                    }
                    cmdbar::ScopeDecision::Ask => {
                        // the marquee keystroke: hold the intent, wait for t/w.
                        if let Some(node) = sel {
                            self.cmd.scope_ask = Some((q, node));
                        }
                    }
                }
            }
        }
        cx.notify();
    }

    /// Apply a parsed imperative on the HUMAN lane (docs/019: dictation is human
    /// intent through a machine hand — instant, journaled origin `human:cmdbar`,
    /// ⌘Z undoes). Returns Err on a resolution miss (no/ambiguous node) so the
    /// caller keeps the bar open with the preview explaining why.
    fn cmd_apply_imperative(
        &mut self,
        imp: &cmdbar::Imperative,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let slug = self.project().slug.clone();
        let parts = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            store.load_tree(&slug).unwrap_or_default()
        };
        let index: Vec<(PartId, String)> = parts.iter().map(|p| (p.id, p.name.clone())).collect();
        let one = |name: &str| match cmdbar::resolve(name, &index) {
            cmdbar::Resolve::One(id) => Ok(id),
            cmdbar::Resolve::None => Err(format!("no node named “{name}”")),
            cmdbar::Resolve::Ambiguous => Err(format!("“{name}” matches more than one node")),
        };
        match imp {
            cmdbar::Imperative::Rename { target, to } => {
                let id = one(target)?;
                let detail = parts
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.detail.clone())
                    .unwrap_or_default();
                let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
                let _ = store.accept_diff_from(
                    &slug,
                    &[DiffOp::Rename {
                        id,
                        name: to.clone(),
                        detail,
                    }],
                    "human:cmdbar",
                    None,
                );
                self.focused_part = Some(id);
                Ok(())
            }
            cmdbar::Imperative::Move { target, dest } => {
                let id = one(target)?;
                let dest_id = match dest {
                    cmdbar::MoveDest::Root => None,
                    cmdbar::MoveDest::Named(n) => Some(one(n)?),
                };
                match orchestrator_store::reparent_op(&parts, id, dest_id) {
                    Some(op) => {
                        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
                        let _ = store.accept_diff_from(&slug, &[op], "human:cmdbar", None);
                        let _ = store.clear_part_pos(id);
                        self.focused_part = Some(id);
                        Ok(())
                    }
                    None => Err("that move would create a cycle".into()),
                }
            }
            cmdbar::Imperative::Delete { target } => {
                let id = one(target)?;
                self.delete_part_subtree(id, cx);
                Ok(())
            }
        }
    }

    pub(crate) fn render_cmd_bar(&self, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let (n_live, needs) = {
            let live = self.cached_infos(&self.project().slug);
            (
                live.len(),
                live.iter()
                    .filter(|s| s.phase == orchestrator_host::Phase::AwaitingDecision)
                    .count(),
            )
        };
        let term_toggle = div()
            .id("term-toggle")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(7.))
            .px(px(10.))
            .py(px(5.))
            .rounded(px(8.))
            .cursor_pointer()
            .border_1()
            .border_color(rgb(if self.mode == Mode::Agent {
                0x36404A
            } else {
                HAIR
            }))
            .hover(|h| h.border_color(rgb(0x36404A)))
            .when(n_live > 0, |c| {
                c.child(
                    div()
                        .w(px(6.))
                        .h(px(6.))
                        .rounded(px(3.))
                        .bg(rgb(if needs > 0 { AMBER } else { 0x5BB99B })),
                )
            })
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(rgb(if self.mode == Mode::Agent {
                        TEXT_STRONG
                    } else {
                        MUTED
                    }))
                    .child("Terminal"),
            )
            .child(div().text_size(px(11.)).text_color(rgb(MUTED2)).child("⌃`"))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                // mouse twin of ⌃`; default_workspace_mode() is Agent in the OSS
                // build, so this keeps you on the Agent stage (no map to toggle to).
                if this.mode == Mode::Agent {
                    this.mode = crate::default_workspace_mode();
                } else {
                    this.mode = Mode::Agent;
                    this.term_focus.focus(window);
                }
                cx.notify();
            }));

        // CLOSED: the teaser strip — clicking the prompt opens the command bar.
        if !self.cmd.open {
            return div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .px(px(16.))
                .py(px(10.))
                .border_t_1()
                .border_color(rgb(HAIR))
                .bg(rgb(PANEL))
                .child(div().text_size(px(12.)).text_color(rgb(MUTED2)).child("⌘K"))
                .child(
                    div()
                        .id("cmd-open")
                        .flex_1()
                        .cursor_text()
                        .text_size(px(13.))
                        .text_color(rgb(MUTED))
                        .child(SharedString::from(format!("talk to evolve {name}…")))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.open_cmd_bar(None, false, window, cx)
                        })),
                )
                .child(term_toggle)
                .into_any_element();
        }

        // OPEN: resolve the live preview against the current tree (cheap — one
        // load_tree while the bar is focused).
        let slug = self.project().slug.clone();
        let parts = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            store.load_tree(&slug).unwrap_or_default()
        };
        let index: Vec<(PartId, String)> = parts.iter().map(|p| (p.id, p.name.clone())).collect();
        let name_by = |id: PartId| {
            index
                .iter()
                .find(|(i, _)| *i == id)
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| format!("#{id}"))
        };
        let has_sel = self
            .focused_part
            .is_some_and(|id| index.iter().any(|(i, _)| *i == id));
        let q = self.cmd.query.trim().to_string();

        // -- the preview / scope-question line above the input --
        let mut preview_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .px(px(16.))
            .pt(px(8.));
        let mut is_scope_ask = false;
        if let Some((_text, sel)) = self.cmd.scope_ask.clone() {
            is_scope_ask = true;
            let sel_name = name_by(sel);
            preview_row =
                preview_row
                    .child(div().text_size(px(12.)).text_color(rgb(AMBER)).child(
                        SharedString::from(format!(
                            "Scope this restructure — “{sel_name}” only, or the whole map?"
                        )),
                    ))
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("cmd-scope-sub")
                            .px(px(9.))
                            .py(px(3.))
                            .rounded(px(7.))
                            .cursor_pointer()
                            .border_1()
                            .border_color(rgb(HAIR))
                            .hover(|h| h.border_color(rgb(ACCENT)))
                            .text_size(px(11.5))
                            .text_color(rgb(TEXT))
                            .child("T · this subtree")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                if let Some((text, sel)) = this.cmd.scope_ask.clone() {
                                    this.close_cmd_bar(window);
                                    this.start_agentic_run(
                                        AgenticKind::Fenced {
                                            node: sel,
                                            rework: true,
                                            intent: Some(text),
                                        },
                                        cx,
                                    );
                                }
                            })),
                    )
                    .child(
                        div()
                            .id("cmd-scope-whole")
                            .px(px(9.))
                            .py(px(3.))
                            .rounded(px(7.))
                            .cursor_pointer()
                            .border_1()
                            .border_color(rgb(HAIR))
                            .hover(|h| h.border_color(rgb(ACCENT)))
                            .text_size(px(11.5))
                            .text_color(rgb(TEXT))
                            .child("W · whole map")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                if let Some((text, _)) = this.cmd.scope_ask.clone() {
                                    this.close_cmd_bar(window);
                                    this.start_agentic_run(
                                        AgenticKind::Seed { intent: Some(text) },
                                        cx,
                                    );
                                }
                            })),
                    );
        } else if let Some(node) = self.cmd.node {
            let verb = if self.cmd.rework {
                "reworks"
            } else {
                "expands"
            };
            preview_row =
                preview_row.child(div().text_size(px(12.)).text_color(rgb(ACCENT)).child(
                    SharedString::from(format!(
                        "↵ Claude {verb} “{}”, fenced — proposes a cited changeset you review",
                        name_by(node)
                    )),
                ));
        } else if q.is_empty() {
            preview_row = preview_row.child(
                div().text_size(px(12.)).text_color(rgb(MUTED2))
                    .child("“rename auth to Identity” applies instantly · “reorganize by product area” asks Claude for a reviewed proposal"),
            );
        } else {
            let (txt, col): (String, u32) = match cmdbar::classify(&q) {
                cmdbar::Lane::Imperative(imp) => {
                    let resolve1 = |nm: &str| cmdbar::resolve(nm, &index);
                    match &imp {
                        cmdbar::Imperative::Rename { target, to } => match resolve1(target) {
                            cmdbar::Resolve::One(id) => (
                                format!(
                                    "↵ Rename “{}” → “{}” — instant, ⌘Z undoes",
                                    name_by(id),
                                    to
                                ),
                                GREEN,
                            ),
                            cmdbar::Resolve::None => {
                                (format!("no node named “{target}”"), 0xE6A08A)
                            }
                            cmdbar::Resolve::Ambiguous => (
                                format!("“{target}” matches more than one node — be specific"),
                                0xE6A08A,
                            ),
                        },
                        cmdbar::Imperative::Move { target, dest } => {
                            match resolve1(target) {
                                cmdbar::Resolve::None => {
                                    (format!("no node named “{target}”"), 0xE6A08A)
                                }
                                cmdbar::Resolve::Ambiguous => {
                                    (format!("“{target}” matches more than one node"), 0xE6A08A)
                                }
                                cmdbar::Resolve::One(tid) => {
                                    let t = name_by(tid);
                                    match dest {
                                    cmdbar::MoveDest::Root => (format!("↵ Move “{t}” to the top level — instant, ⌘Z undoes"), GREEN),
                                    cmdbar::MoveDest::Named(d) => match resolve1(d) {
                                        cmdbar::Resolve::One(id) => (format!("↵ Move “{t}” under “{}” — instant, ⌘Z undoes", name_by(id)), GREEN),
                                        cmdbar::Resolve::None => (format!("no destination named “{d}”"), 0xE6A08A),
                                        cmdbar::Resolve::Ambiguous => (format!("“{d}” matches more than one node"), 0xE6A08A),
                                    },
                                }
                                }
                            }
                        }
                        cmdbar::Imperative::Delete { target } => match resolve1(target) {
                            cmdbar::Resolve::One(id) => (
                                format!("↵ Delete “{}” + its subtree — ⌘Z undoes", name_by(id)),
                                0xE6A08A,
                            ),
                            cmdbar::Resolve::None => {
                                (format!("no node named “{target}”"), 0xE6A08A)
                            }
                            cmdbar::Resolve::Ambiguous => {
                                (format!("“{target}” matches more than one node"), 0xE6A08A)
                            }
                        },
                    }
                }
                cmdbar::Lane::Intent => match cmdbar::infer_scope(&q, has_sel) {
                    cmdbar::ScopeDecision::WholeMap => (
                        "↵ Ask Claude — proposes a cited whole-map changeset you review"
                            .to_string(),
                        ACCENT,
                    ),
                    cmdbar::ScopeDecision::Subtree => (
                        format!(
                            "↵ Ask Claude — proposes a cited changeset fenced to “{}”",
                            self.focused_part.map(&name_by).unwrap_or_default()
                        ),
                        ACCENT,
                    ),
                    cmdbar::ScopeDecision::Ask => (
                        "↵ Ask Claude — it'll ask: this subtree or the whole map?".to_string(),
                        AMBER,
                    ),
                },
            };
            preview_row = preview_row.child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(col))
                    .child(SharedString::from(txt)),
            );
        }

        // -- the input strip --
        let chip: SharedString = match self.cmd.node {
            Some(node) => format!(
                "{} {} ▸",
                if self.cmd.rework { "rework" } else { "expand" },
                palette::snippet(&name_by(node), 18)
            )
            .into(),
            None => "evolve ▸".into(),
        };
        let footer = if is_scope_ask {
            "T this subtree · W whole map · esc"
        } else {
            "↵ run · esc"
        };
        let input = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(9.))
            .px(px(16.))
            .py(px(9.))
            .child(
                div()
                    .flex_none()
                    .text_size(px(10.5))
                    .font_family("Menlo")
                    .text_color(rgb(ACCENT))
                    .child(chip),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .flex()
                    .flex_row()
                    .items_center()
                    .font_family("Menlo")
                    .text_size(px(13.5))
                    .when(self.cmd.query.is_empty(), |c| {
                        c.child(div().text_color(rgb(ACCENT)).child("▏"))
                            .child(div().text_color(rgb(MUTED2)).child("type a command…"))
                    })
                    .when(!self.cmd.query.is_empty(), |c| {
                        c.child(
                            div()
                                .text_color(rgb(TEXT_STRONG))
                                .child(SharedString::from(self.cmd.query.clone())),
                        )
                        .child(div().text_color(rgb(ACCENT)).child("▏"))
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED2))
                    .child(footer),
            );

        div()
            .id("cmd-bar-open")
            .track_focus(&self.cmd_focus)
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(rgb(0x3a4a55))
            .bg(rgb(PANEL))
            .on_key_down(
                cx.listener(|this, ev: &KeyDownEvent, window, cx| this.cmd_key(ev, window, cx)),
            )
            .child(preview_row)
            .child(input)
            .into_any_element()
    }

}
