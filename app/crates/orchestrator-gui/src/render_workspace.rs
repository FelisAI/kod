use gpui::prelude::FluentBuilder;
use gpui::*;
use crate::spawn::{resolve_spawn_profile, ProfilePick};
use crate::*;


impl Orchestrator {

    /// The REAL Flow map (docs/016) — renders the persisted DESIGN tree from
    /// the store. Three states: a pending seed proposal (accept-diff), the seed
    /// CTA (no tree yet), or the live map + outline.
    pub(crate) fn render_workspace(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let proj = self.project();
        let slug = proj.slug.clone();
        let name = proj.name.clone();
        // In AGENT mode the stage shows the focused terminal, so the context
        // views (map/outline/brain) are hidden — DON'T hit the store or build the
        // tree EVERY frame. That per-frame SQLite x3 + tree build was the
        // per-keystroke latency while typing in the fullscreen terminal (#9).
        // Agent + Recover show no product tree → skip the per-frame SQLite x3 +
        // tree build (the typing-path latency fix, extended to Recover).
        let skip_tree = matches!(self.mode, Mode::Agent | Mode::Recover);
        let (parts, seed_state, pending) = if skip_tree {
            (Vec::new(), orchestrator_store::SeedState::None, Vec::new())
        } else {
            // poison-tolerant: a panic in another thread holding this lock must
            // not take down every workspace render.
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            (
                store.load_tree(&slug).unwrap_or_default(),
                store.seed_state(&slug),
                store.pending_diffs(&slug).unwrap_or_default(),
            )
        };
        let tree = build_tree(&parts);
        let total = parts.len();
        let (hosted, busy, awaiting, _) = self.live_overlay(&slug);
        let mode = self.mode;
        let rec_n = self.recoverable_count_for_project();

        // a centered Mac-style tab (Map/Flow/Brain switch the view mode).
        let tab = |id: &'static str,
                   label: &'static str,
                   this_mode: Mode,
                   active: bool,
                   cx: &mut Context<Self>| {
            div()
                .id(id)
                .px(px(16.))
                .py(px(5.))
                .rounded(px(7.))
                .cursor_pointer()
                .when(active, |s| s.bg(rgb(CARD2)))
                .text_size(px(13.))
                .font_weight(if active {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::NORMAL
                })
                .text_color(rgb(if active { TEXT_STRONG } else { MUTED }))
                .child(label)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.mode = this_mode;
                    cx.notify();
                }))
        };
        // a row in the "+" new-session dropdown — spawns the CLI + closes the
        // menu. `account` is the dim trailing name of the profile the row will
        // actually land on: a plain row LOOKS ambient but takes this CLI's
        // default profile if one is set (#56), and a row that silently switches
        // account with nothing on screen to say so is exactly how the old
        // "plain rows stay ambient" contract went stale (#62.3).
        let spawn_item = |id: &'static str,
                          glyph: &'static str,
                          label: &'static str,
                          color: u32,
                          kind: CliKind,
                          pick: ProfilePick,
                          account: Option<SharedString>,
                          cx: &mut Context<Self>| {
            div()
                .id(id)
                .flex()
                .flex_row()
                .items_center()
                .gap(px(9.))
                .px(px(10.))
                .py(px(6.))
                .rounded(px(7.))
                .cursor_pointer()
                .hover(|h| h.bg(rgb(CARD2)))
                .child(div().w(px(14.)).text_color(rgb(color)).child(glyph))
                .child(
                    div()
                        .min_w_0()
                        .text_size(px(13.))
                        .text_color(rgb(TEXT))
                        .truncate()
                        .child(label),
                )
                .when_some(account, |r, a| {
                    r.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED2))
                            .truncate()
                            .child(a),
                    )
                })
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.spawn_menu_open = false;
                    this.spawn_session(kind, pick, window, cx);
                    cx.stop_propagation();
                }))
        };

        // ── Mac-style toolbar: 3 zones — name·status (left) · centered segmented
        // tabs (the hero) · "+"/"⟲" icon actions (right). Spawn moved into the "+"
        // dropdown; the crowded chip row + the inline subtitle are gone.
        let header = div()
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(rgb(HAIR_SOFT))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .px(px(14.))
                    .py(px(10.))
                    // LEFT zone: project name + live status (flex_1 so the center is true-centered).
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(TEXT_STRONG))
                                    .overflow_hidden()
                                    .child(SharedString::from(name.clone())),
                            )
                            .when(hosted > 0, |c| {
                                let (col, lbl) = if awaiting > 0 {
                                    (AMBER, "needs you")
                                } else if busy > 0 {
                                    (ORANGE, "working")
                                } else {
                                    (GREEN, "idle")
                                };
                                c.child(div().w(px(6.)).h(px(6.)).rounded(px(3.)).bg(rgb(col)))
                                    .child(div().text_size(px(11.)).text_color(rgb(col)).child(lbl))
                            }),
                    )
                    // CENTER zone: the segmented tab control (centered, elevated active tab).
                    //
                    // OSS gate (features.rs): with the map compiled out, Agent is the only
                    // tab — and a segmented control holding a single, permanently-selected
                    // entry reads as broken chrome rather than a switch. So the whole
                    // control is hidden and the Agent stage is simply always shown; it
                    // returns intact under `--features map`. LEFT/RIGHT are both flex_1,
                    // so the row still lays out correctly with no center child.
                    .when(crate::features::MAP_ENABLED, |row| {
                        row.child(
                            div()
                                .flex()
                                .flex_row()
                                .gap(px(2.))
                                .p(px(3.))
                                .rounded(px(9.))
                                .bg(rgb(0x0D1218))
                                .border_1()
                                .border_color(rgb(HAIR))
                                .child(
                                    div()
                                        .id("seg-agent")
                                        .px(px(16.))
                                        .py(px(5.))
                                        .rounded(px(7.))
                                        .cursor_pointer()
                                        .when(mode == Mode::Agent, |s| s.bg(rgb(CARD2)))
                                        .text_size(px(13.))
                                        .font_weight(if mode == Mode::Agent {
                                            FontWeight::SEMIBOLD
                                        } else {
                                            FontWeight::NORMAL
                                        })
                                        .text_color(rgb(if mode == Mode::Agent {
                                            TEXT_STRONG
                                        } else {
                                            MUTED
                                        }))
                                        .child("Agent")
                                        .on_click(cx.listener(
                                            |this, _: &ClickEvent, window, cx| {
                                                this.mode = Mode::Agent;
                                                this.term_focus.focus(window);
                                                cx.notify();
                                            },
                                        )),
                                )
                                .child(tab(
                                    "seg-map",
                                    "Map",
                                    Mode::MapOutline,
                                    mode == Mode::MapOutline,
                                    cx,
                                )),
                        )
                    })
                    // RIGHT zone: demoted icon actions — "+" spawn dropdown, "⟲" recover.
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_end()
                            .gap(px(4.))
                            .flex_1()
                            .child(
                                div()
                                    .id("ws-new")
                                    .w(px(28.))
                                    .h(px(28.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(8.))
                                    .cursor_pointer()
                                    .text_size(px(17.))
                                    .text_color(rgb(if self.spawn_menu_open {
                                        TEXT_STRONG
                                    } else {
                                        MUTED
                                    }))
                                    .when(self.spawn_menu_open, |s| s.bg(rgb(CARD)))
                                    .hover(|h| h.bg(rgb(CARD)).text_color(rgb(TEXT)))
                                    .child("+")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.spawn_menu_open = !this.spawn_menu_open;
                                        cx.notify();
                                    })),
                            )
                            .child({
                                let rec_active = mode == Mode::Recover;
                                div()
                                    .id("ws-recover")
                                    .relative()
                                    .w(px(28.))
                                    .h(px(28.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(8.))
                                    .cursor_pointer()
                                    .text_size(px(15.))
                                    .text_color(rgb(if rec_active { ACCENT } else { MUTED }))
                                    .when(rec_active, |s| s.bg(rgb(CARD)))
                                    .hover(|h| h.bg(rgb(CARD)).text_color(rgb(TEXT)))
                                    .child("⟲")
                                    .when(rec_n > 0, |r| {
                                        r.child(
                                            div()
                                                .absolute()
                                                .top(px(-1.))
                                                .right(px(-1.))
                                                .min_w(px(14.))
                                                .h(px(14.))
                                                .px(px(3.))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .rounded(px(7.))
                                                .bg(rgb(AMBER))
                                                .text_size(px(9.))
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(rgb(0x0B0F14))
                                                .child(SharedString::from(rec_n.to_string())),
                                        )
                                    })
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.mode = Mode::Recover;
                                        this.attach_picker = None;
                                        this.recover_all = false;
                                        this.spawn_menu_open = false;
                                        cx.notify();
                                    }))
                            }),
                    ),
            );

        let cmd_bar = self.render_cmd_bar(&name, cx);

        // The stage shows EXACTLY one mode: the focused AGENT (its terminal/stream)
        // or a context view (Map/Flow/Brain). No peek/expand drawer (#9 slice 2).
        let mut root = div()
            .flex_1()
            .flex()
            .flex_col()
            .min_w_0()
            .relative()
            .child(header);
        // A spawn/dispatch error surfaces here at the workspace root so it is
        // visible in EVERY mode (Agent / Brain / Recover / Map) — not just the map
        // stage, which is compiled out in the OSS build. Without this a failed
        // ⌘T / "+" new-session (mkdir or name-collision in spawn_cwd) that lands on
        // the Agent stage would be a silent dead key (review).
        if let Some(err) = self.term_error.clone() {
            root = root.child(self.term_error_banner(err, cx));
        }
        root = match mode {
            Mode::Agent => root.child(self.render_agent_stage(&slug, cx)),
            Mode::Recover => root.child(self.render_project_recover(&name, cx)),
            _ => {
                let content: AnyElement = if !crate::features::MAP_ENABLED {
                    // OSS gate (features.rs): the map is compiled out. `mode` is
                    // never MapOutline in this build (every landing routes to the
                    // Agent stage), so render nothing for any residual non-map
                    // state rather than a hidden map stage. The map renderers
                    // below stay compiled + tested.
                    div().into_any_element()
                } else if let Some(pd) = pending.iter().find(|pd| pd.kind == "seed") {
                    // ONLY the initial seed takes over the stage; other pending
                    // diffs render per-op inside the outline (critique #10).
                    self.render_seed_proposal(pd.id, &pd.ops, &name, cx)
                        .into_any_element()
                } else if total == 0 && !pending.iter().any(|pd| pd.changeset_id.is_some()) {
                    // still empty AND no changeset to review — the seed CTA (it
                    // shows the agentic progress itself while a seed is reading).
                    self.render_seed_cta(&slug, &name, seed_state, cx)
                        .into_any_element()
                } else {
                    // a non-empty tree, OR an empty tree with a seed changeset /
                    // in-flight run to surface (render_map_outline draws both
                    // the progress card and the changeset review over the canvas).
                    self.render_map_outline(&parts, &tree, &pending, cx)
                        .into_any_element()
                };
                root.child(content)
            }
        };
        // OSS gate (features.rs): the talk-to-evolve command bar exists only to
        // edit the map tree (imperative parse → DiffOps, or design-intent →
        // changeset), so it's hidden when the map is compiled out.
        if crate::features::MAP_ENABLED {
            root = root.child(cmd_bar);
        }
        // the ⇄ move-session dropdown (overlay — an inline absolute menu would
        // paint UNDER the terminal body; dogfooding: "the popup is cut off").
        if self.move_menu_open {
            if let Some(id) = self.active_session_id() {
                let cli = self
                    .cached_infos(&slug)
                    .iter()
                    .find(|i| i.id == id)
                    .and_then(|i| i.cli_session_id.clone());
                let cur = slug.clone();
                let mut menu = div()
                    .absolute()
                    .top(px(78.))
                    .right(px(14.))
                    .id("move-menu")
                    .w(px(200.))
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .p(px(5.))
                    .rounded(px(10.))
                    .bg(rgb(CARD))
                    .border_1()
                    .border_color(rgb(HAIR))
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, app| {
                        app.stop_propagation()
                    })
                    .on_mouse_up(MouseButton::Left, |_: &MouseUpEvent, _, app| {
                        app.stop_propagation()
                    })
                    .on_mouse_down(MouseButton::Right, |_: &MouseDownEvent, _, app| {
                        app.stop_propagation()
                    })
                    .on_mouse_up(MouseButton::Right, |_: &MouseUpEvent, _, app| {
                        app.stop_propagation()
                    })
                    .on_click(|_: &ClickEvent, _, app| app.stop_propagation())
                    .child(
                        div()
                            .px(px(9.))
                            .pb(px(3.))
                            .text_size(px(10.))
                            .text_color(rgb(MUTED2))
                            .child("MOVE SESSION TO"),
                    );
                for p in self.projects.iter().filter(|p| p.slug != cur) {
                    let dest = p.slug.clone();
                    let cli = cli.clone();
                    menu = menu.child(
                        div()
                            .id(SharedString::from(format!("mv-{}", p.slug)))
                            .px(px(9.))
                            .py(px(5.))
                            .rounded(px(7.))
                            .cursor_pointer()
                            .text_size(px(12.5))
                            .text_color(rgb(TEXT))
                            .hover(|h| h.bg(rgb(CARD2)))
                            .child(SharedString::from(p.name.clone()))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.move_menu_open = false;
                                this.move_session_to(id, cli.clone(), &dest, window, cx);
                                cx.stop_propagation();
                            })),
                    );
                }
                root = root
                    .child(
                        div()
                            .id("move-backdrop")
                            .absolute()
                            .size_full()
                            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, app| {
                                app.stop_propagation()
                            })
                            .on_mouse_up(MouseButton::Left, |_: &MouseUpEvent, _, app| {
                                app.stop_propagation()
                            })
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.move_menu_open = false;
                                cx.stop_propagation();
                                cx.notify();
                            })),
                    )
                    .child(menu);
            }
        }
        // the "+" new-session dropdown (overlay): a click-anywhere backdrop + the
        // menu, top-right under the "+". Closes on pick or outside-click.
        if self.spawn_menu_open {
            // What the plain claude/codex rows at the top of this menu ACTUALLY
            // do (#56): they make no pick, so each takes its CLI's DEFAULT
            // profile — ambient only until the user names a default in Settings.
            // Resolved here, off the lock the menu already takes, so a plain row
            // can name the account it will land on and the ACCOUNTS section knows
            // whether the ambient escape is even needed. Inside this `if` on
            // purpose: the menu is closed on the typing path, and a per-frame
            // SQLite read there is the latency the tree skip above exists to
            // avoid.
            let (profiles, def_claude, def_codex) = self
                .store
                .lock()
                .ok()
                .map(|s| {
                    (
                        s.profiles(),
                        resolve_spawn_profile(&s, CliKind::Claude, ProfilePick::Default),
                        resolve_spawn_profile(&s, CliKind::Codex, ProfilePick::Default),
                    )
                })
                .unwrap_or_default();
            // one row per saved profile: spawns its CLI under that account's
            // config-dir (CLAUDE_CONFIG_DIR / CODEX_HOME). An explicit pick, so
            // it lands there whether or not it is this CLI's default.
            let mut account_rows: Vec<AnyElement> = profiles
                .into_iter()
                .map(|p| {
                    let kind = if p.cli_kind == "codex" {
                        CliKind::Codex
                    } else {
                        CliKind::Claude
                    };
                    let pid = p.id;
                    let glyph = if p.cli_kind == "codex" { "◆" } else { "✦" };
                    let label = format!("{} · {}", p.cli_kind, p.label);
                    div()
                        .id(SharedString::from(format!("nm-prof-{pid}")))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(9.))
                        .px(px(10.))
                        .py(px(6.))
                        .rounded(px(7.))
                        .cursor_pointer()
                        .hover(|h| h.bg(rgb(CARD2)))
                        .child(div().w(px(14.)).text_color(rgb(ACCENT)).child(glyph))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(13.))
                                .text_color(rgb(TEXT))
                                .truncate()
                                .child(SharedString::from(label)),
                        )
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.spawn_menu_open = false;
                            this.spawn_session(kind, ProfilePick::Profile(pid), window, cx);
                            cx.stop_propagation();
                        }))
                        .into_any_element()
                })
                .collect();
            // ...and, for each CLI that HAS a default, one row that deliberately
            // spawns on NO profile: the account the CLI is already logged into.
            // Without it a default is a one-way door — the only way back to your
            // own login is to go clear the setting in Settings (#62.3). It costs
            // the common case nothing: with no default set the plain row above is
            // already ambient, so no row is added and the menu is untouched.
            for (kind, glyph, row_id, label, defaulted) in [
                (
                    CliKind::Claude,
                    "✦",
                    "nm-ambient-claude",
                    "claude · ambient",
                    def_claude.is_some(),
                ),
                (
                    CliKind::Codex,
                    "◆",
                    "nm-ambient-codex",
                    "codex · ambient",
                    def_codex.is_some(),
                ),
            ] {
                if !defaulted {
                    continue;
                }
                account_rows.push(
                    spawn_item(
                        row_id,
                        glyph,
                        label,
                        MUTED,
                        kind,
                        ProfilePick::Ambient,
                        None,
                        cx,
                    )
                    .into_any_element(),
                );
            }
            root = root
                .child(
                    div()
                        .id("spawn-backdrop")
                        .absolute()
                        .size_full()
                        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, app| {
                            app.stop_propagation()
                        })
                        .on_mouse_up(MouseButton::Left, |_: &MouseUpEvent, _, app| {
                            app.stop_propagation()
                        })
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                this.spawn_menu_open = false;
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        )
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.spawn_menu_open = false;
                            cx.stop_propagation();
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .id("spawn-menu")
                        .absolute()
                        .top(px(48.))
                        .right(px(14.))
                        .w(px(168.))
                        .p(px(5.))
                        .rounded(px(10.))
                        .bg(rgb(CARD))
                        .border_1()
                        .border_color(rgb(HAIR))
                        .flex()
                        .flex_col()
                        .gap(px(1.))
                        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, app| {
                            app.stop_propagation()
                        })
                        .on_mouse_up(MouseButton::Left, |_: &MouseUpEvent, _, app| {
                            app.stop_propagation()
                        })
                        .on_mouse_down(MouseButton::Right, |_: &MouseDownEvent, _, app| {
                            app.stop_propagation()
                        })
                        .on_mouse_up(MouseButton::Right, |_: &MouseUpEvent, _, app| {
                            app.stop_propagation()
                        })
                        .on_click(|_: &ClickEvent, _, app| app.stop_propagation())
                        .child(
                            div()
                                .px(px(10.))
                                .pt(px(5.))
                                .pb(px(3.))
                                .text_size(px(10.))
                                .text_color(rgb(MUTED2))
                                .child("NEW SESSION"),
                        )
                        .child(spawn_item(
                            "nm-claude",
                            "✦",
                            "claude",
                            ACCENT,
                            CliKind::Claude,
                            ProfilePick::Default,
                            def_claude.map(|p| SharedString::from(p.label)),
                            cx,
                        ))
                        .child(spawn_item(
                            "nm-codex",
                            "◆",
                            "codex",
                            MUTED,
                            CliKind::Codex,
                            ProfilePick::Default,
                            def_codex.map(|p| SharedString::from(p.label)),
                            cx,
                        ))
                        // a shell has no account concept, so it can't adopt a
                        // default and never needs the ambient row either.
                        .child(spawn_item(
                            "nm-shell",
                            "›_",
                            "shell",
                            MUTED,
                            CliKind::Shell,
                            ProfilePick::Default,
                            None,
                            cx,
                        ))
                        .when(!account_rows.is_empty(), |m| {
                            m.child(
                                div()
                                    .px(px(10.))
                                    .pt(px(6.))
                                    .pb(px(3.))
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED2))
                                    .child("ACCOUNTS"),
                            )
                        })
                        .children(account_rows),
                );
        }
        root
    }

    /// The seed CTA — shown when a project has no tree yet. Its primary action
    /// is the agentic RE-GROUND (docs/019 seed/re-ground): Claude reads all of
    /// docs/, README, and entry-point code and proposes a cited whole-map
    /// changeset the user reviews. A faster shallow one-shot and "start
    /// blank" sit beside it.
    fn render_seed_cta(
        &self,
        slug: &str,
        name: &str,
        _seed: orchestrator_store::SeedState,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let extracting = self.extracting.as_deref() == Some(&self.project().slug);
        let seeding = self
            .agentic
            .as_ref()
            .is_some_and(|r| r.slug == slug && r.scope.is_none());
        let busy = extracting || seeding;
        let err = self.term_error.clone();
        div().flex_1().flex().flex_col().items_center().justify_center().gap(px(10.)).p(px(40.))
            .child(div().text_size(px(17.)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(TEXT_STRONG)).child(SharedString::from(format!("{name} has no product map yet"))))
            .child(div().max_w(px(440.)).text_size(px(13.)).text_color(rgb(MUTED)).child("Let Claude read your docs & code and draft a cited map (you review every node before anything sticks), take a faster shallow pass, or start blank."))
            // the live "reading docs…" state while the agentic seed runs.
            .when_some(self.render_agentic_progress(slug).filter(|_| seeding), |c, card| c.child(div().w_full().max_w(px(560.)).child(card)))
            .child(
                div().mt(px(8.)).flex().flex_row().gap(px(10.))
                    .child(
                        div().id("seed-reground").px(px(14.)).py(px(7.)).rounded(px(9.)).cursor_pointer()
                            .bg(rgb(if busy { CARD2 } else { ACCENT }))
                            .text_size(px(13.)).text_color(rgb(if busy { MUTED } else { 0x0C140F }))
                            .child(if seeding { "Reading your docs & code…".to_string() } else { "⟳ Draft map from docs & code".to_string() })
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| { this.term_error = None; this.start_agentic_run(AgenticKind::Seed { intent: None }, cx); })),
                    )
                    .child(
                        div().id("seed-extract").px(px(14.)).py(px(7.)).rounded(px(9.)).cursor_pointer()
                            .border_1().border_color(rgb(HAIR))
                            .text_size(px(13.)).text_color(rgb(MUTED))
                            .child(if extracting { "Quick extract… reading code".to_string() } else { "✦ Quick extract".to_string() })
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| { this.term_error = None; this.start_extract(cx); })),
                    )
                    .child(
                        div().id("seed-blank").px(px(14.)).py(px(7.)).rounded(px(9.)).cursor_pointer()
                            .border_1().border_color(rgb(HAIR)).text_size(px(13.)).text_color(rgb(MUTED))
                            .child("Start blank")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.start_blank(cx))),
                    ),
            )
            .when_some(err, |c, e| c.child(div().mt(px(6.)).max_w(px(440.)).text_size(px(12.)).text_color(rgb(0xE69595)).child(SharedString::from(e))))
    }

}
