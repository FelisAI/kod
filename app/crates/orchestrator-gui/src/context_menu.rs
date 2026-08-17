use gpui::*;
use crate::*;


impl Orchestrator {

    /// Rename in situ: the canvas-side editor at the node's rect. Focus
    /// follows (one selection shared between canvas and outline).
    pub(crate) fn menu_rename(&mut self, id: PartId, window: &mut Window, cx: &mut Context<Self>) {
        self.focused_part = Some(id);
        self.begin_outline_edit(outlinepane::EditSlot::RenameCanvas(id), window, cx);
    }

    /// Edit detail: the outline pane's multi-line detail_md editor, focused
    /// on this node (the pane opens as part of the gesture).
    fn menu_detail(&mut self, id: PartId, window: &mut Window, cx: &mut Context<Self>) {
        self.focused_part = Some(id);
        self.begin_outline_edit(outlinepane::EditSlot::Detail(id), window, cx);
    }

    /// Add child: the outline's ＋part editor under this node (AddPart
    /// commits under the FOCUSED node, so focus moves first).
    fn menu_add_child(&mut self, id: PartId, window: &mut Window, cx: &mut Context<Self>) {
        self.focused_part = Some(id);
        self.begin_outline_edit(outlinepane::EditSlot::AddPart, window, cx);
    }

    /// Move to… (docs/019 PALETTE): the long-distance reparent — ⌥-drag
    /// covers what's on screen, this covers everything else (and satellites,
    /// which can't arm a drag). Opens the palette in MoveTo mode listing
    /// every legal destination as a fuzzy-filtered breadcrumb; ↵ applies ONE
    /// reparent_op Move on the human lane. One body for the menu row, its M
    /// accelerator, and the grammar's bare M.
    pub(crate) fn open_move_to(&mut self, id: PartId, window: &mut Window, cx: &mut Context<Self>) {
        self.palette.open_move_to(id);
        self.stage_palette(window, cx);
        self.rekick_palette(); // seed the full destination list (empty query)
    }

    /// Bare-key accelerators while the context menu is open (docs/019 CANVAS:
    /// "menu accelerators work as bare keys"). Every key is swallowed — an
    /// open menu owns the keyboard, mirroring the palette overlay.
    pub(crate) fn map_menu_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.map_menu else { return };
        let id = menu.id;
        match ev.keystroke.key.as_str() {
            "escape" => self.map_menu = None,
            "r" => {
                self.map_menu = None;
                self.menu_rename(id, window, cx);
            }
            "e" => {
                self.map_menu = None;
                self.menu_detail(id, window, cx);
            }
            "n" => {
                self.map_menu = None;
                self.menu_add_child(id, window, cx);
            }
            "m" => {
                self.map_menu = None;
                self.open_move_to(id, window, cx);
            }
            "s" => {
                // ★ pin the selection (docs/019 slice 4 · commitment 5: "set
                // via context menu / palette / `s` on selection").
                self.map_menu = None;
                self.toggle_star(id, cx);
            }
            "backspace" if ev.keystroke.modifiers.secondary() => {
                self.map_menu = None;
                self.delete_part_subtree(id, cx);
            }
            _ => {}
        }
        cx.notify();
    }

    /// The context menu overlay (docs/019 CANVAS): a window-space scrim +
    /// panel at the right-click point. Root pane lists every verb with its
    /// accelerator as a small right-aligned label (ruling 13); Change kind /
    /// Status / Why-is-this-here swap the panel body in place. None when no
    /// menu is open or its node vanished (render() heals the state).
    pub(crate) fn map_menu_layer(&self, viewport: Size<Pixels>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.map_menu?;
        let id = menu.id;
        let slug = self.project().slug.clone();
        let (part, prov, has_children, needs_flagged) = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let tree = store.load_tree(&slug).ok()?;
            let has_children = tree.iter().any(|p| p.parent_id == Some(id));
            let part = tree.into_iter().find(|p| p.id == id)?;
            let prov = store.part_provenance(id);
            let needs_flagged = store.needs_you_for(id).is_some();
            (part, prov, has_children, needs_flagged)
        };
        let is_starred = self.project_stars(&slug).contains(&id);
        // one row: label + right-aligned accelerator hint (keys are secondary
        // labels, never the only path — docs/019 ruling 13).
        fn item(
            eid: &'static str,
            label: SharedString,
            accel: Option<SharedString>,
            ink: u32,
        ) -> Stateful<Div> {
            let mut row = div()
                .id(eid)
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .px(px(11.))
                .py(px(4.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(rgb(ink))
                .hover(|s| s.bg(rgb(CARD2)))
                .child(div().flex_1().min_w_0().child(label));
            if let Some(a) = accel {
                row = row.child(
                    div()
                        .flex_none()
                        .text_size(px(10.))
                        .font_family("Menlo")
                        .text_color(rgb(MUTED2))
                        .child(a),
                );
            }
            row
        }
        fn divider() -> Div {
            div().my(px(3.)).h(px(1.)).bg(rgb(HAIR_SOFT))
        }
        let mut body = div().flex().flex_col().py(px(4.));
        match menu.pane {
            MenuPane::Root => {
                body = body
                    .child(
                        item("cm-ren", "Rename".into(), Some("R".into()), TEXT).on_click(
                            cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.map_menu = None;
                                this.menu_rename(id, window, cx);
                            }),
                        ),
                    )
                    .child(
                        item("cm-det", "Edit detail".into(), Some("E".into()), TEXT).on_click(
                            cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.map_menu = None;
                                this.menu_detail(id, window, cx);
                            }),
                        ),
                    )
                    .child(
                        item("cm-add", "Add child".into(), Some("N".into()), TEXT).on_click(
                            cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.map_menu = None;
                                this.menu_add_child(id, window, cx);
                            }),
                        ),
                    )
                    .child(divider())
                    .child(
                        item(
                            "cm-kind",
                            "Change kind".into(),
                            Some(format!("{} ▸", part.kind.as_str()).into()),
                            TEXT,
                        )
                        .on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                if let Some(m) = &mut this.map_menu {
                                    m.pane = MenuPane::Kind;
                                }
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        item(
                            "cm-status",
                            "Status".into(),
                            Some(format!("{} ▸", part.lifecycle.as_str()).into()),
                            TEXT,
                        )
                        .on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                if let Some(m) = &mut this.map_menu {
                                    m.pane = MenuPane::Status;
                                }
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        item("cm-move", "Move to…".into(), Some("M".into()), TEXT).on_click(
                            cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.map_menu = None;
                                this.open_move_to(id, window, cx);
                                cx.notify();
                            }),
                        ),
                    )
                    .child(divider())
                    // ★ pin (docs/019 slice 4): a user "up next" pointer,
                    // ≤3, a 4th unpins the oldest — never a priority system.
                    .child(
                        item(
                            "cm-star",
                            if is_starred {
                                "★ Unpin".into()
                            } else {
                                "☆ Pin ★".into()
                            },
                            Some("S".into()),
                            if is_starred { ACCENT } else { TEXT },
                        )
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, _, cx| {
                                this.map_menu = None;
                                this.toggle_star(id, cx);
                            },
                        )),
                    );
                // "Flag needs-me…" (docs/019 slice 4): typing the one-line
                // blocking question is REQUIRED (the question is the payload);
                // once flagged, the same row CLEARS it.
                if needs_flagged {
                    body = body.child(
                        item("cm-needs-clear", "Clear needs-me".into(), None, AMBER).on_click(
                            cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.map_menu = None;
                                this.clear_needs_you(id, cx);
                            }),
                        ),
                    );
                } else {
                    body = body.child(
                        item("cm-needs", "Flag needs-me…".into(), None, TEXT).on_click(
                            cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.map_menu = None;
                                this.open_needs_you_editor(id, window, cx);
                            }),
                        ),
                    );
                }
                if part.map_x.is_some() {
                    // pinning happens by dragging; the menu's half is UNPIN
                    // (docs/019 CANVAS: clear map_x/map_y → auto-layout).
                    body = body.child(
                        item("cm-unpin", "Unpin (auto-place)".into(), None, TEXT).on_click(
                            cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.map_menu = None;
                                if let Ok(store) = this.store.lock() {
                                    let _ = store.clear_part_pos(id);
                                }
                                cx.notify();
                            }),
                        ),
                    );
                }
                body =
                    body.child(item("cm-disp", "▶ Dispatch".into(), None, TEXT).on_click(
                        cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.map_menu = None;
                            this.dispatch_to_part(id, false, window, cx);
                        }),
                    ))
                    // docs/019 C4: fenced machine-lane runs. Both open the "talk
                    // to evolve" bar pre-fenced to this node for a one-line
                    // intent; ↵ dispatches an agentic run that lands a cited,
                    // fenced changeset (expand = add children; rework = restructure).
                    .child(
                        item(
                            "cm-expand",
                            "Expand with Claude…".into(),
                            Some("⟳".into()),
                            TEXT,
                        )
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, window, cx| {
                                this.map_menu = None;
                                this.open_intent_bar(id, false, window, cx);
                            },
                        )),
                    )
                    .child(
                        item(
                            "cm-rework",
                            "Rework this subtree…".into(),
                            Some("⟳".into()),
                            TEXT,
                        )
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, window, cx| {
                                this.map_menu = None;
                                this.open_intent_bar(id, true, window, cx);
                            },
                        )),
                    )
                    .child(
                        item("cm-why", "Why is this here?".into(), Some("▸".into()), TEXT)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                if let Some(m) = &mut this.map_menu {
                                    m.pane = MenuPane::Why;
                                }
                                cx.notify();
                            })),
                    )
                    .child(divider());
                // Dissolve = the MACHINE-lane restructure (docs/019 ruling 2):
                // unwrap this container's children up to its parent + remove the
                // husk, reviewed as one changeset. Only meaningful with children.
                if has_children {
                    body = body.child(
                        item(
                            "cm-dissolve",
                            "Dissolve (unwrap children)".into(),
                            Some("⟳".into()),
                            TEXT,
                        )
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, _, cx| {
                                this.map_menu = None;
                                this.dissolve_node(id, cx);
                            },
                        )),
                    );
                }
                body = body.child(
                    item("cm-del", "Delete".into(), Some("⌘⌫".into()), 0xE68A8A).on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| {
                            // instant, leaf-first, ⌘Z undoes (docs/019: no dialogs).
                            this.map_menu = None;
                            this.delete_part_subtree(id, cx);
                        }),
                    ),
                );
            }
            MenuPane::Kind => {
                body = body.child(item("cm-back", "‹ back".into(), None, MUTED2).on_click(
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        if let Some(m) = &mut this.map_menu {
                            m.pane = MenuPane::Root;
                        }
                        cx.notify();
                    }),
                ));
                for (eid, k) in [
                    ("cm-k-area", orchestrator_store::Kind::Area),
                    ("cm-k-task", orchestrator_store::Kind::Task),
                    ("cm-k-idea", orchestrator_store::Kind::Idea),
                ] {
                    let cur = part.kind == k;
                    body = body.child(
                        item(
                            eid,
                            k.as_str().into(),
                            cur.then(|| "✓".into()),
                            if cur { ACCENT } else { TEXT },
                        )
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, _, cx| {
                                this.map_menu = None;
                                if !cur {
                                    // one-gesture kind repair (docs/019 commitment 2),
                                    // instant on the human lane like the pane's chip.
                                    let slug = this.project().slug.clone();
                                    let mut store =
                                        this.store.lock().unwrap_or_else(|e| e.into_inner());
                                    let _ = store.accept_diff_from(
                                        &slug,
                                        &[DiffOp::SetKind { id, kind: k }],
                                        "user",
                                        None,
                                    );
                                }
                                cx.notify();
                            },
                        )),
                    );
                }
            }
            MenuPane::Status => {
                body = body.child(item("cm-back", "‹ back".into(), None, MUTED2).on_click(
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        if let Some(m) = &mut this.map_menu {
                            m.pane = MenuPane::Root;
                        }
                        cx.notify();
                    }),
                ));
                // the assertable set only — `building` is derived, never a
                // menu choice (docs/019 commitment 2).
                for (eid, lc) in [
                    ("cm-s-idea", Lifecycle::Idea),
                    ("cm-s-todo", Lifecycle::Todo),
                    ("cm-s-done", Lifecycle::Done),
                ] {
                    let cur = part.lifecycle == lc;
                    body = body.child(
                        item(
                            eid,
                            lc.as_str().into(),
                            cur.then(|| "✓".into()),
                            if cur { ACCENT } else { TEXT },
                        )
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, _, cx| {
                                this.map_menu = None;
                                if !cur {
                                    // set_part_status journals via accept_diff → the
                                    // human lane, undoable (same path as glyph clicks).
                                    this.set_part_status(id, lc, cx);
                                }
                                cx.notify();
                            },
                        )),
                    );
                }
            }
            MenuPane::Why => {
                body = body.child(item("cm-back", "‹ back".into(), None, MUTED2).on_click(
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        if let Some(m) = &mut this.map_menu {
                            m.pane = MenuPane::Root;
                        }
                        cx.notify();
                    }),
                ));
                let (who, sf, sq, ra) = prov.unwrap_or_else(|| ("legacy".into(), None, None, None));
                let line = |s: SharedString, ink: u32, mono: bool| {
                    let d = div()
                        .px(px(11.))
                        .py(px(2.))
                        .text_size(px(11.))
                        .text_color(rgb(ink))
                        .child(s);
                    if mono {
                        d.font_family("Menlo")
                    } else {
                        d
                    }
                };
                if who == "legacy" && sf.is_none() && sq.is_none() && ra.is_none() {
                    // the permanent C2 answer for pre-provenance rows
                    // (docs/019 schema): an honest shrug + the repair verb.
                    body = body.child(
                        div()
                            .px(px(11.))
                            .py(px(4.))
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .child(
                                "seeded before provenance existed — re-ground to explain this map",
                            ),
                    );
                } else {
                    body = body.child(line(format!("created by {who}").into(), MUTED, false));
                    if let Some(f) = sf {
                        body = body.child(line(f.into(), MUTED, true));
                    }
                    if let Some(q) = sq {
                        body = body.child(line(format!("“{q}”").into(), TEXT, false));
                    }
                    if let Some(r) = ra {
                        body = body.child(line(r.into(), MUTED, false));
                    }
                }
            }
        }
        // clamp so the panel never opens clipped off the window edge.
        let x = f32::from(menu.at.x)
            .min(f32::from(viewport.width) - 252.0)
            .max(8.0);
        let y = f32::from(menu.at.y)
            .min(f32::from(viewport.height) - 340.0)
            .max(8.0);
        Some(
            div()
                .id("map-menu-scrim")
                .absolute()
                .size_full()
                // any press outside the panel closes — including a fresh
                // right-click (close first; the next one reopens).
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.map_menu = None;
                        cx.notify();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.map_menu = None;
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .absolute()
                        .left(px(x))
                        .top(px(y))
                        .w(px(244.))
                        .flex()
                        .flex_col()
                        .rounded(px(10.))
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(HAIR))
                        .shadow_lg()
                        // the panel eats its own presses so the scrim's
                        // close-on-click can't race a row's on_click.
                        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, app| {
                            app.stop_propagation()
                        })
                        .on_mouse_down(MouseButton::Right, |_: &MouseDownEvent, _, app| {
                            app.stop_propagation()
                        })
                        .child(
                            div()
                                .px(px(11.))
                                .pt(px(7.))
                                .pb(px(3.))
                                .text_size(px(10.5))
                                .text_color(rgb(MUTED2))
                                .child(SharedString::from(termview::trim(&part.name, 28))),
                        )
                        .child(body),
                )
                .into_any_element(),
        )
    }

}
