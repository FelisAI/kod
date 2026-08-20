use gpui::prelude::FluentBuilder;
use gpui::*;
use crate::*;


impl Orchestrator {

    /// An ACTIVE project row: badge + name + live pill, with its live sessions
    /// nested below — de-dotted (the phase-colored CLI glyph is the only mark).
    fn rail_active_project(&self, i: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let slug = self.projects[i].slug.clone();
        let name = self.projects[i].name.clone();
        let unread = self.project_unread(&slug); // #50: has updates you haven't opened
        let is_sel = self.screen == Screen::Workspace && i == self.selected;
        let (awaiting, busy) = live_rank(self.cached_infos(&slug));
        let hosted = self.cached_infos(&slug).iter().filter(|s| s.alive).count();
        // pill = the project's live mix, one segment per non-zero state in
        // priority order (needs > idle > working). Collapsing to only the
        // strongest signal read as a lie — "● 1" green while two sessions
        // worked invisibly (dogfooding). Bg still tints by the strongest.
        let idle = hosted.saturating_sub(awaiting + busy);
        let mut segs: Vec<(String, u32)> = Vec::new();
        if awaiting > 0 {
            segs.push((format!("⚠ {awaiting}"), AMBER));
        }
        if idle > 0 {
            segs.push((format!("● {idle}"), GREEN));
        }
        if busy > 0 {
            segs.push((format!("● {busy}"), ORANGE));
        }
        let pbg = if awaiting > 0 {
            0x2A2214u32
        } else if idle > 0 {
            0x16241Du32
        } else {
            0x2A1F14u32
        };
        let head_slug = slug.clone();
        // the row id keys off the SLUG, not the index: element state (hover, the
        // pending mouse-down a drag consumes) must follow the row, not the slot.
        let (drag_slug, drag_name) = (slug.clone(), name.clone());
        let head = div()
            .id(SharedString::from(format!("proj-{slug}")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(9.))
            .px(px(8.))
            .py(px(6.))
            .rounded(px(8.))
            .cursor_pointer()
            .when(is_sel, |r| r.bg(rgb(CARD2)))
            .when(!is_sel, |r| r.hover(|h| h.bg(rgba(0xFFFFFF0A))))
            // select on mouse-DOWN, not click: gpui starts a drag past 2px of
            // drift and then TAKES the pending mouse-down, so the click never
            // fires — a trackpad click that drifts 2-4px selected nothing at all.
            // Mouse-down runs before any drag can start, so selection is never
            // swallowed (and picking a row up to drag it selects it too).
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.select_project(&head_slug, cx)
                }),
            )
            .child(project_badge(&name, &slug, 21.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_size(px(13.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    // #50: an unread project's title brightens to TEXT_STRONG; once
                    // opened (seen) it settles to the normal TEXT.
                    .text_color(rgb(if is_sel || unread { TEXT_STRONG } else { TEXT }))
                    .child(SharedString::from(name)),
            )
            // the pill is a BUTTON: it jumps straight to the hottest session
            // (the ask/idle agent that lit it up), not the project Map (#12).
            .child({
                let pill_slug = slug.clone();
                let mut pill = div()
                    .id(SharedString::from(format!("pill-{slug}")))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(10.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .bg(rgb(pbg))
                    .rounded(px(20.))
                    .px(px(8.))
                    .py(px(1.))
                    .cursor_pointer()
                    .hover(|h| h.opacity(0.85))
                    // the row now selects on mouse-DOWN, which bubbles from here —
                    // so the pill has to stop it, or a pill click would first run
                    // the whole project-switch (blur the outline edit, drop the
                    // focused part) before focus_hottest lands. The pill's OWN
                    // click still fires: gpui registers this listener before the
                    // pending-mouse-down capture, and bubble runs them in reverse.
                    .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, app| {
                        app.stop_propagation()
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        this.focus_hottest(&pill_slug, window, cx);
                    }));
                for (txt, col) in segs {
                    pill = pill.child(div().text_color(rgb(col)).child(SharedString::from(txt)));
                }
                pill
            });
        // PICKUP is the header; the DROP TARGET is the whole card (below). Both are
        // attached only once the real scan has landed — before that the rail shows
        // seed_projects() and a drag is a no-op (see `reorder_project`), so it must
        // not offer the affordance either.
        let head = if self.scanned {
            rail_order::rail_draggable(head, &drag_slug, &drag_name, true)
        } else {
            head
        };
        let card = div().flex().flex_col().mb(px(3.));
        // the WHOLE card is the landing zone, not just the ~33px header: the
        // session rows below it are ~110px of card that used to swallow a drop
        // silently (gpui just clears the drag on mouse-up — no snap-back).
        let card = if self.scanned {
            rail_order::rail_drop_target(card, &slug, true, cx)
        } else {
            card
        };
        let mut card = card.child(head);
        // pin ONLY awaiting-decision rows to the top; everything else keeps its
        // stable spawn order. Ranking idle vs busy would reshuffle every ~2s as
        // output-activity flaps Busy↔Idle — the exact "why is it reordering?"
        // flap the project sort was rewritten to avoid (adversarial review).
        let mut agents: Vec<&SessionInfo> = self
            .cached_infos(&slug)
            .iter()
            .filter(|s| s.alive)
            .collect();
        agents.sort_by_key(|s| s.phase != orchestrator_host::Phase::AwaitingDecision);
        if !agents.is_empty() {
            let mut group = div().flex().flex_col().gap(px(1.)).mt(px(1.)).mb(px(4.));
            for info in &agents {
                let id = info.id;
                let ph = info.phase;
                let needs_action = ph == orchestrator_host::Phase::AwaitingDecision;
                let focused = is_sel && self.active_session.get(&slug).copied() == Some(id);
                // finished a turn you have not opened (#13). Never on the row
                // you are already on, and never competing with needs-you amber.
                let unreviewed = !focused && !needs_action && self.session_unreviewed(id);
                let raw = if info.title.is_empty() {
                    info.kind.label().to_string()
                } else {
                    info.title.clone()
                };
                // soft cap only — the title's truncate() does the visual clipping,
                // so names aren't chopped to 20 chars regardless of rail width (#12).
                let label: String = raw.chars().take(40).collect();
                let pslug = slug.clone();
                let usage = info.usage_limit.clone();
                group = group.child(
                    div()
                        .id(SharedString::from(format!("ag-{}", id.0)))
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .pl(px(15.))
                        .pr(px(8.))
                        .py(px(4.))
                        .rounded(px(6.))
                        .cursor_pointer()
                        .when(focused, |c| c.bg(rgb(CARD2)))
                        .when(needs_action && !focused, |c| c.bg(rgb(0x231D11)))
                        .when(!focused && !needs_action, |c| {
                            c.hover(|h| h.bg(rgba(0xFFFFFF0A)))
                        })
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(9.))
                                .child(
                                    div()
                                        .w(px(7.))
                                        .h(px(7.))
                                        .rounded(px(4.))
                                        .flex_none()
                                        .bg(rgb(
                                            if info.usage_limit.as_ref().is_some_and(|u| u.hit) {
                                                0xE68A8A
                                            } else if info.trouble.is_some() {
                                                0xE68A8A
                                            } else {
                                                termview::phase_dot(ph)
                                            },
                                        )),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(12.5))
                                        // MUTED -> TEXT -> TEXT_STRONG is the
                                        // brightness ladder; unreviewed sits one
                                        // step up from resting, so "something
                                        // finished here" reads without shouting.
                                        .text_color(rgb(row_tone(
                                            needs_action,
                                            focused,
                                            unreviewed,
                                        )
                                        .color()))
                                        .when(unreviewed, |d| {
                                            d.font_weight(FontWeight::MEDIUM)
                                        })
                                        .child(SharedString::from(label)),
                                )
                                .when(needs_action, |c| {
                                    c.child(
                                        div()
                                            .flex_none()
                                            .whitespace_nowrap()
                                            .text_size(px(10.))
                                            .text_color(rgb(AMBER))
                                            .child("needs you"),
                                    )
                                }),
                        )
                        // the limit pill gets its OWN line: the rail has vertical
                        // room, not horizontal (194px inner). Inline, the ~150px
                        // pill squeezes the title to ~0 and gpui re-wraps the text
                        // per character — a 370px row. Only limited sessions pay
                        // the extra line, and the title keeps its full width.
                        // …and it's the COMPACT chip: the second line's paintable
                        // width is 162px (214 rail − 20 padding − 1 border − 15 pl
                        // − 8 pr − 16 indent), and the FULL wording blows past it
                        // the moment a reset crosses local midnight and
                        // `fmt_reset_clock` qualifies it with a weekday —
                        // "⛔ limit hit · resets Wed 11:30pm" measures 179px and is
                        // hard-clipped by the rail's overflow mask (the pill is
                        // flex_none + nowrap, with no max_w). "⛔ Wed 11:30pm" is
                        // 98px. Truncating instead would eat the reset TIME, which
                        // is the one thing the user actually needs.
                        .when_some(usage, |c, u| {
                            c.child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .pl(px(16.))
                                    .child(usage_chip_compact(&u)),
                            )
                        })
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.focus_session(&pslug, id, window, cx)
                        })),
                );
            }
            card = card.child(group);
        }
        card
    }

    /// A dormant project row: badge + name + amber needs-you dot or a "last active" stamp.
    fn rail_rest_project(&self, i: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let slug = self.projects[i].slug.clone();
        let name = self.projects[i].name.clone();
        let unread = self.project_unread(&slug); // #50: has updates you haven't opened
        let is_sel = self.screen == Screen::Workspace && i == self.selected;
        let proj_needs = self
            .cached_infos(&slug)
            .iter()
            .any(|s| s.alive && s.phase == orchestrator_host::Phase::AwaitingDecision);
        let now = orchestrator_core::registry::now_secs();
        let when = if self.projects[i].last_activity_secs > 0 {
            orchestrator_core::recap::rel_time(
                now.saturating_sub(self.projects[i].last_activity_secs),
            )
        } else {
            String::new()
        };
        let rslug = slug.clone();
        let (drag_slug, drag_name) = (slug.clone(), name.clone());
        // ideas are just projects (#10) — same row, dimmed, ◌ instead of badge.
        let is_idea = slug.starts_with("idea:");
        let row = div()
            .id(SharedString::from(format!("proj-{slug}")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(9.))
            .px(px(8.))
            .py(px(5.))
            .rounded(px(8.))
            .cursor_pointer()
            .when(is_sel, |r| r.bg(rgb(CARD2)))
            .when(!is_sel, |r| r.hover(|h| h.bg(rgba(0xFFFFFF0A))))
            .when(is_idea, |r| r.opacity(0.6))
            // mouse-DOWN, not click: a click that drifts past gpui's 2px drag
            // threshold has its pending mouse-down taken by the drag, so the click
            // never fires (see `rail_active_project`).
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.select_project(&rslug, cx)
                }),
            )
            .child(if is_idea {
                div()
                    .w(px(20.))
                    .h(px(20.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.))
                    .text_color(rgb(MUTED2))
                    .child("◌")
                    .into_any_element()
            } else {
                project_badge(&name, &slug, 20.).into_any_element()
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_size(px(13.))
                    // #50: an UNREAD dormant project bolds + brightens (bold/bright
                    // = "look here"); once opened it settles back to normal weight
                    // and the dimmer TEXT (ideas stay MUTED).
                    .font_weight(if unread {
                        FontWeight::SEMIBOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(rgb(if is_sel || unread {
                        TEXT_STRONG
                    } else if is_idea {
                        MUTED
                    } else {
                        TEXT
                    }))
                    .child(SharedString::from(name)),
            )
            .when(proj_needs, |r| {
                r.child(
                    div()
                        .w(px(7.))
                        .h(px(7.))
                        .rounded(px(4.))
                        .flex_none()
                        .bg(rgb(AMBER)),
                )
            })
            .when(!proj_needs && !when.is_empty() && !is_idea, |r| {
                r.child(
                    div()
                        .flex_none()
                        .text_size(px(10.5))
                        .text_color(rgb(MUTED2))
                        .child(SharedString::from(when)),
                )
            });
        // no drag affordance until the real scan has landed — the pre-scan rail is
        // seed_projects() and `reorder_project` refuses to persist that order.
        if self.scanned {
            rail_order::rail_reorderable(row, &drag_slug, &drag_name, false, cx)
        } else {
            row
        }
    }

    pub(crate) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // one source of truth (real M3 needs-you), matching the Standup header.
        let needs = self.needs_you_count();
        let on_standup = self.screen == Screen::Standup;
        let s = div()
            // user-resizable (#52); the drag gutter itself is an absolutely
            // positioned sibling on the root, so it can't be clipped by the rail.
            .w(px(self.sidebar_w))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(2.))
            .bg(rgb(PANEL))
            .border_r_1()
            .border_color(rgb(HAIR))
            .p(px(10.))
            // the brand row: the ensō (ink-tinted at this size) + wordmark.
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .px(px(10.))
                    .pt(px(2.))
                    .pb(px(8.))
                    .child(
                        svg()
                            .path("logo/kod.svg")
                            .w(px(20.))
                            .h(px(20.))
                            .flex_none()
                            .text_color(rgb(TEXT_STRONG)),
                    )
                    .child(
                        div()
                            .text_size(px(14.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT_STRONG))
                            .child("kod"),
                    ),
            )
            .child(
                div()
                    .id("standup")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(9.))
                    .px(px(10.))
                    .py(px(8.))
                    .mb(px(8.))
                    .rounded(px(9.))
                    .cursor_pointer()
                    .bg(rgb(if on_standup { CARD2 } else { CARD }))
                    .border_1()
                    .border_color(rgb(HAIR))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.screen = Screen::Standup;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(rgb(if on_standup { ACCENT } else { MUTED }))
                            .child("◳"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.))
                            .text_color(rgb(if on_standup { TEXT_STRONG } else { TEXT }))
                            .child("Standup"),
                    )
                    // prominent amber pill, not a near-invisible number (#4).
                    .when(needs > 0, |r| {
                        r.child(
                            div()
                                .bg(rgb(AMBER))
                                .text_color(rgb(0x1A1408))
                                .rounded(px(20.))
                                .px(px(8.))
                                .py(px(1.))
                                .text_size(px(10.5))
                                .font_weight(FontWeight::BOLD)
                                .child(SharedString::from(format!("⚠ {needs}"))),
                        )
                    }),
            );
        // Recover is a per-project control-bar action, not global nav. Projects:
        // the ones the app owns LIVE sessions in float to an ACTIVE group on top
        // (needs-you pinned first); the rest sit below (#9).
        //
        // `rail_view` is the ONE definition of the rendered order — `reorder_project`
        // reads the same one, because a drag has to move rows within the sequence
        // the user SEES. `rest` is NOT re-sorted: ACTIVE/PROJECTS is a VIEW of
        // the one global `self.projects` order (registry order, overlaid with the
        // user's drag order — #28), not an order of its own.
        let (active, rest) = self.rail_view();
        // The project list scrolls INSIDE its own zone so the fixed NEW SESSION
        // controls below never scroll off-screen when there are many projects.
        let mut list = div()
            .id("rail-scroll")
            .flex()
            .flex_col()
            .gap(px(2.))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();
        if !active.is_empty() {
            list = list.child(section_header("● ACTIVE", Some(active.len()), GREEN));
            for &i in &active {
                list = list.child(self.rail_active_project(i, cx));
            }
        }
        list = list.child(section_header("PROJECTS", None, MUTED2));
        for &i in &rest {
            list = list.child(self.rail_rest_project(i, cx));
        }
        // ＋ new project — gets its OWN DIRECTORY under the projects folder (#29)
        // and is path-keyed from birth. ＋ idea — still just a project without
        // code yet (#10), path-less until its first spawn promotes it.
        match &self.rail_new {
            Some(buf) => {
                let (glyph, hint) = match self.rail_new_kind {
                    RailNewKind::Project => ("＋", self.projects_root.join(projdir::dir_name_for(buf)).display().to_string()),
                    RailNewKind::Idea => ("◌", "idea — no code yet".to_string()),
                    // OpenFolder never populates `rail_new`, so the inline field is
                    // never drawn for it (it opens the picker directly) — this arm
                    // exists only to keep the match exhaustive.
                    RailNewKind::OpenFolder => ("▤", String::new()),
                };
                list = list.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(7.))
                        .px(px(8.))
                        .py(px(5.))
                        .rounded(px(8.))
                        .bg(rgb(CARD2))
                        .border_1()
                        .border_color(rgb(0x346B54))
                        .font_family("Menlo")
                        .text_size(px(12.5))
                        .text_color(rgb(TEXT_STRONG))
                        .child(div().flex_none().text_color(rgb(MUTED2)).child(glyph))
                        // The caret has to sit AT the insertion point, which means
                        // hugging the text — it used to follow a `flex_1` text div,
                        // so it was pinned to the far right edge of the rail with a
                        // gap between it and what you were typing, and the row read
                        // as a label rather than a field. The group is flex_1 (it
                        // owns the free space); the text inside it is not.
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_row()
                                .items_center()
                                .when(buf.is_empty(), |r| {
                                    // empty field: caret first, then a MUTED prompt.
                                    // The placeholder used to inherit TEXT_STRONG and
                                    // was indistinguishable from a real typed name.
                                    r.child(div().flex_none().text_color(rgb(ACCENT)).child("▏"))
                                        .child(
                                            div()
                                                .min_w_0()
                                                .truncate()
                                                .text_color(rgb(MUTED2))
                                                .child("name…"),
                                        )
                                })
                                .when(!buf.is_empty(), |r| {
                                    r.child(
                                        div().min_w_0().truncate().child(SharedString::from(buf.clone())),
                                    )
                                    .child(div().flex_none().text_color(rgb(ACCENT)).child("▏"))
                                }),
                        ),
                );
                // where the folder will land — the user sees the directory
                // BEFORE he commits to it (the whole point of #29).
                if !buf.trim().is_empty() {
                    list = list.child(
                        div()
                            .px(px(9.))
                            .pb(px(3.))
                            .text_size(px(10.))
                            .text_color(rgb(MUTED2))
                            .overflow_hidden()
                            .child(SharedString::from(hint)),
                    );
                }
            }
            None => {
                // ONE entry point, not three (#67). The three creations are
                // genuinely different and none can be dropped — but "new project /
                // idea / open folder" named them without saying what any of them
                // DOES, and the rail (168–214px) has no room for the sentence that
                // would. So the choice moves into a menu wide enough to explain it.
                let open = self.rail_new_menu_open;
                list = list.child(
                    div()
                        .relative()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .id("rail-new")
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(9.))
                                .px(px(8.))
                                .py(px(5.))
                                .rounded(px(8.))
                                .cursor_pointer()
                                .when(open, |r| r.bg(rgb(CARD2)))
                                .when(!open, |r| r.opacity(0.5).hover(|h| h.opacity(0.9)))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.rail_new_menu_open = !this.rail_new_menu_open;
                                    // two menus open at once would fight over Esc.
                                    this.spawn_menu_open = false;
                                    cx.stop_propagation();
                                    cx.notify();
                                }))
                                .child(
                                    div()
                                        .w(px(20.))
                                        .flex_none()
                                        .text_size(px(13.))
                                        .text_color(rgb(MUTED2))
                                        .child("＋"),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(12.5))
                                        .text_color(rgb(MUTED2))
                                        .child("New…"),
                                ),
                        )
                        // The panel is wider than the rail AND hangs off a row that
                        // scrolls with the project list, so it can be neither a fixed
                        // window coordinate nor an ordinary child: `deferred` lifts it
                        // above the stage (and out of the list's scroll clip), while
                        // `anchored` keeps it on the row and slides it back inside the
                        // window when the row sits too near the bottom for it to fit.
                        // Deferred is also what keeps it over its click-catcher
                        // (`rail_new_backdrop`), which is a plain root child.
                        .when(open, |c| {
                            c.child(
                                deferred(
                                    anchored()
                                        .offset(point(px(0.), px(28.)))
                                        .snap_to_window_with_margin(px(8.))
                                        .child(self.rail_new_menu(cx)),
                                )
                                .with_priority(1),
                            )
                        }),
                );
            }
        }
        // a creation failure (mkdir denied, folder taken) belongs where the
        // user is looking — never swallowed (docs/013 §1).
        if let Some(err) = self.rail_new_err.clone() {
            list = list.child(
                div()
                    .px(px(9.))
                    .py(px(4.))
                    .text_size(px(10.5))
                    .text_color(rgb(0xE68A8A))
                    .child(SharedString::from(err)),
            );
        }
        // spawning lives in the workspace header's "+" — the rail stays nav-only
        // (dogfooding: the bottom chips were redundant and ate rail space).
        s.child(list)
    }

    /// The ＋New menu's click-catcher, mounted on the ROOT rather than inside the
    /// rail: the menu overhangs the stage, so only a window-wide catcher can
    /// dismiss it from anywhere — including a click on the terminal.
    ///
    /// A plain root child, NOT a `deferred` layer. Deferred draws register their
    /// mouse listeners last, and the bubble phase runs them first, so a deferred
    /// catcher swallowed the press of every in-tree overlay above it — the
    /// needs-you toast's "Open in terminal ▸" went dead for one click while this
    /// menu was open. Mounted before the toast, it still eats the rail, the stage
    /// and the terminal, while the panel itself (deferred) stays above it.
    pub(crate) fn rail_new_backdrop(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        if !self.rail_new_menu_open {
            return None;
        }
        Some(
            div()
                .id("rail-new-backdrop")
                .absolute()
                .top_0()
                .left_0()
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
                        this.rail_new_menu_open = false;
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.rail_new_menu_open = false;
                    cx.stop_propagation();
                    cx.notify();
                })),
        )
    }

    /// The rail's ＋New menu (#67): one row per creation, each carrying the ONE
    /// line that says what it actually does. They are different things — a
    /// Project gets its own directory NOW, an Idea has none until its first
    /// spawn, Open folder adopts a directory that already exists — and the rail
    /// was never wide enough to say so. Idea is map-only (see its row below), so
    /// the default build offers two verbs and `--features map` three.
    fn rail_new_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // the REAL root, not a description of one: seeing where the folder lands
        // before committing is the whole point of #29.
        let where_line = new_project_where(
            &self.projects_root,
            &orchestrator_core::scan::home(),
            ROOT_BUDGET,
        );
        div()
            .id("rail-new-menu")
            .w(px(320.))
            .p(px(5.))
            .rounded(px(10.))
            .bg(rgb(CARD))
            .border_1()
            .border_color(rgb(HAIR))
            .shadow_lg()
            .flex()
            .flex_col()
            .gap(px(1.))
            // the panel eats its own presses so the catcher below can't race a
            // row's on_click.
            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, app| {
                app.stop_propagation()
            })
            .on_mouse_up(MouseButton::Left, |_: &MouseUpEvent, _, app| {
                app.stop_propagation()
            })
            .on_mouse_down(MouseButton::Right, |_: &MouseDownEvent, _, app| {
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
                    .child("ADD A PROJECT"),
            )
            .child(rail_new_item(
                "rn-project",
                "＋",
                "New project",
                where_line.into(),
                RailNewKind::Project,
                cx,
            ))
            // OSS gate (features.rs): with the map compiled out, Idea does exactly
            // ONE thing — defer the mkdir until the first spawn, which then creates
            // `<projects_root>/<slug>` and promotes the row to an ordinary `path:`
            // project. Everything it was FOR is map-side: the blank map to author
            // and the kickoff NO_REPO_LINE "thinking/spec session" prompt. So in a
            // default build this row is a third creation verb whose tagline
            // ("no code yet") advertises a surface that isn't compiled in — worse
            // than no verb, because the user picks it expecting the thing it
            // promises and gets a folder. The plumbing stays (commit_new_idea,
            // promote_project); only the entry point is gated, and it returns
            // intact under `--features map`.
            .when(crate::features::MAP_ENABLED, |m| {
                m.child(rail_new_item(
                    "rn-idea",
                    "◌",
                    "Idea",
                    "capture it now, no code yet".into(),
                    RailNewKind::Idea,
                    cx,
                ))
            })
            .child(rail_new_item(
                "rn-folder",
                "▤",
                "Open folder…",
                // "folder", not "repo": open_folder (#34) adopts ANY directory in
                // place — git is not required — so "repo" would send someone with a
                // plain project folder looking for a command that doesn't exist.
                "point Kod at a folder you already have".into(),
                RailNewKind::OpenFolder,
                cx,
            ))
    }

}

/// How many characters of the projects root the "New project" line can spend
/// before it middle-elides. It bounds the row's HEIGHT, not its width: the desc
/// wraps (see `rail_new_item`), and the text column is 267px wide (320 panel −
/// 2×5 panel padding − 2×10 row padding − 14 glyph − 9 gap). Measured at 11px
/// system UI: the frame "creates a folder in … and starts there" alone is 184px,
/// the default `~/local` line 219px (one line), and the widest 22 chars a path
/// can be (all W) 420px — still two lines, never three.
const ROOT_BUDGET: usize = 22;

/// One ＋New menu row: glyph, title, and the explanatory line beneath it. The
/// click behaviour is the three rail rows' behaviour, unchanged — only where the
/// user reads about it moved.
fn rail_new_item(
    id: &'static str,
    glyph: &'static str,
    title: &'static str,
    desc: SharedString,
    kind: RailNewKind,
    cx: &mut Context<Orchestrator>,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_start()
        .gap(px(9.))
        .px(px(10.))
        .py(px(6.))
        .rounded(px(7.))
        .cursor_pointer()
        .hover(|h| h.bg(rgb(CARD2)))
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.rail_new_menu_open = false;
            // blur, not discard — a live Detail edit commits (docs/019).
            this.blur_outline_edit(cx);
            cx.stop_propagation();
            // "open folder…" takes no typed name — it opens the native picker
            // directly and registers the CHOSEN folder in place (#34). It never
            // touches the inline field the other two open.
            if kind == RailNewKind::OpenFolder {
                this.open_folder(cx);
                cx.notify();
                return;
            }
            this.rail_new_kind = kind;
            this.rail_new = Some(String::new());
            this.seed_inline_caret(InlineTarget::RailIdea);
            this.rail_new_err = None;
            // the root router owns the keystream — never the PTY (review 1b)
            this.root_focus.focus(window);
            cx.notify();
        }))
        .child(
            div()
                .w(px(14.))
                .flex_none()
                .text_size(px(13.))
                .text_color(rgb(ACCENT))
                .child(glyph),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(1.))
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(rgb(TEXT))
                        .truncate()
                        .child(title),
                )
                // the desc WRAPS where the title truncates: the title is a name you
                // scan down a list, but this line is the whole reason the menu
                // exists, and at 11px the 267px column holds ~40 chars — every
                // projects root past "~/local" ellipsised the sentence away
                // ("creates a folder in ~/Documents/code and starts th…").
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(MUTED2))
                        .child(desc),
                ),
        )
}

/// The "where does it land?" line under ＋New's "New project" (#29): the real
/// projects root, home-collapsed and MIDDLE-elided past `budget`. Both ends of a
/// path carry meaning — where it lives, and which folder it is — so a plain
/// prefix trim would keep the least useful half. `home` is injected so the line
/// stays pure (and testable) instead of reading the environment.
fn new_project_where(root: &std::path::Path, home: &std::path::Path, budget: usize) -> String {
    // an unset HOME must not turn "/srv/x" into "~//srv/x": an empty prefix
    // strips off EVERY path, and core::scan::home yields one rather than
    // inventing a user-specific fallback.
    let shown = if home.as_os_str().is_empty() {
        root.display().to_string()
    } else {
        match root.strip_prefix(home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => root.display().to_string(),
        }
    };
    format!(
        "creates a folder in {} and starts there",
        elide_middle(&shown, budget)
    )
}

fn elide_middle(s: &str, budget: usize) -> String {
    let n = s.chars().count();
    if n <= budget {
        return s.to_string();
    }
    let keep = budget.saturating_sub(1); // the … costs one
    let tail = keep * 2 / 3; // the folder itself is the half worth keeping
    let head: String = s.chars().take(keep - tail).collect();
    let tail: String = s.chars().skip(n - tail).collect();
    format!("{head}…{tail}")
}

/// Wall-clock now in ms — the live clock the usage countdown reads. Kept at the
/// render boundary so `usage_words` stays pure and unit-testable with an injected
/// `now_ms`; `render_standup`'s BLOCKED tier borrows it too.
pub(crate) fn wall_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The words + colors behind both usage pills: `(full, compact, fg, bg)`. One
/// source, so the two widths can never disagree about state (docs/019). `now_ms`
/// drives the live countdown — the reset instant (`reset_at_unix`) was parsed but
/// never shown, so the app only ever printed a bare wall-clock (claude shows a
/// countdown, distinguishes weekly, and a midnight-crossing reset read as today).
fn usage_words(
    u: &orchestrator_host::session::UsageLimit,
    now_ms: u64,
) -> (String, String, u32, u32) {
    let cd = u.reset_countdown(now_ms); // "2h 15m" / "5d 3h" / "" (unknown or past)
    let when = u.reset_label(); // "Jun 5, 7am" (weekly) / "9:50pm" / ""
    if u.hit {
        // headline the live COUNTDOWN when we have it (what claude shows); else the
        // wall-time label; else the bare state.
        let full = if !cd.is_empty() {
            if when.is_empty() {
                format!("⛔ resets in {cd}")
            } else {
                format!("⛔ resets in {cd} · {when}")
            }
        } else if !when.is_empty() {
            format!("⛔ limit hit · resets {when}")
        } else {
            "⛔ limit hit".to_string()
        };
        // the rail's compact line: a countdown is both SHORTER and more actionable
        // than a clock, so prefer it; else the clock (weekday and all); else bare.
        let compact = if !cd.is_empty() {
            format!("⛔ {cd}")
        } else if !u.reset_clock.is_empty() {
            format!("⛔ {}", u.reset_clock)
        } else {
            "⛔ limit".to_string()
        };
        (full, compact, 0xE68A8Au32, 0x2A1616u32)
    } else {
        // "used N%" when the percent is known; the "Approaching usage limit" banner
        // carries NO percent and used to render a misleading "used 0%".
        let head = match u.percent {
            Some(p) => format!("used {p}%"),
            None => "approaching limit".to_string(),
        };
        let full = if !cd.is_empty() {
            format!("◔ {head} · resets in {cd}")
        } else if !when.is_empty() {
            format!("◔ {head} · resets {when}")
        } else {
            format!("◔ {head}")
        };
        let compact = match u.percent {
            Some(p) => format!("◔ {p}%"),
            None => "◔ limit".to_string(),
        };
        (full, compact, AMBER, AMBER_INK)
    }
}

/// The pill chrome. `flex_none` + `whitespace_nowrap` are both load-bearing: a
/// flex_none item can still be handed a narrow known width by taffy, and gpui
/// text WRAPS at whatever width it lands on — which grows the row instead of
/// overflowing it (the ~370px rail row that started all this).
fn usage_pill(txt: String, fg: u32, bg: u32) -> impl IntoElement {
    div().flex_none().flex().flex_row().items_center().whitespace_nowrap()
        .rounded(px(20.)).px(px(7.)).py(px(1.))
        .text_size(px(10.)).font_weight(FontWeight::SEMIBOLD).bg(rgb(bg)).text_color(rgb(fg))
        .child(SharedString::from(txt))
}

/// The FULL pill surfacing a session's subscription-limit state (docs/019): a red
/// "⛔ limit hit · resets 9:50pm" for a hard block, amber "◔ used N% · resets …"
/// for the benign warning — so a capped session no longer reads as a plain Idle.
///
/// 151–183px depending on the words (a reset that crosses local midnight gets a
/// WEEKDAY: "resets Wed 11:30pm"), so it belongs on the WIDE surfaces ONLY — the
/// Standup rows. The rail's 162px second line fits only the lucky strings; use
/// `usage_chip_compact` there.
pub(crate) fn usage_chip(u: &orchestrator_host::session::UsageLimit) -> impl IntoElement {
    let (full, _, fg, bg) = usage_words(u, wall_now_ms());
    usage_pill(full, fg, bg)
}

/// The COMPACT pill — "⛔ Wed 11:30pm" (98px) / "⛔ 9:50pm" (70px) / "◔ 97%" (46px).
/// Keeps the reset CLOCK, drops the wording, and so clears the rail's 162px
/// paintable line with room to spare where the full chip is hard-clipped by the
/// overflow mask (the pill is `flex_none` + `whitespace_nowrap` with no `max_w`).
pub(crate) fn usage_chip_compact(u: &orchestrator_host::session::UsageLimit) -> impl IntoElement {
    let (_, compact, fg, bg) = usage_words(u, wall_now_ms());
    usage_pill(compact, fg, bg)
}

#[cfg(test)]
mod tests {
    use super::{usage_words, AMBER};
    use orchestrator_host::session::UsageLimit;

    fn limit(hit: bool, percent: Option<u8>, clock: &str) -> UsageLimit {
        UsageLimit {
            hit,
            percent,
            reset_clock: clock.to_string(),
            reset_tz: String::new(),
            reset_date: String::new(),
            reset_at_unix: None,
            since_ms: 0,
        }
    }

    // no `reset_at_unix` in the fixtures below → no countdown → the display falls
    // back to the wall-clock, exactly the pre-countdown wording (now_ms irrelevant).
    #[test]
    fn a_hit_reads_red_and_carries_the_reset_clock() {
        let (full, compact, fg, bg) = usage_words(&limit(true, None, "9:50pm"), 0);
        assert_eq!(full, "⛔ limit hit · resets 9:50pm");
        assert_eq!(compact, "⛔ 9:50pm");
        assert_eq!((fg, bg), (0xE68A8A, 0x2A1616));
    }

    #[test]
    fn a_warning_reads_amber_and_carries_the_percent() {
        let (full, compact, fg, _) = usage_words(&limit(false, Some(92), "9:50pm"), 0);
        assert_eq!(full, "◔ used 92% · resets 9:50pm");
        assert_eq!(compact, "◔ 92%");
        assert_eq!(fg, AMBER);
    }

    #[test]
    fn a_missing_clock_drops_the_reset_clause_instead_of_printing_an_empty_one() {
        let (full, compact, ..) = usage_words(&limit(true, None, ""), 0);
        assert_eq!(full, "⛔ limit hit");
        assert_eq!(compact, "⛔ limit");
        let (full, compact, ..) = usage_words(&limit(false, Some(80), ""), 0);
        assert_eq!(full, "◔ used 80%");
        assert_eq!(compact, "◔ 80%");
    }

    /// The live countdown claude parity turns on: a hit with a resolved reset
    /// instant HEADLINES "resets in 2h 15m" (and the rail shows the countdown, not
    /// the clock), with the wall-time kept as context.
    #[test]
    fn a_hit_with_a_reset_instant_headlines_the_live_countdown() {
        let mut u = limit(true, None, "4:30pm"); // now = 1000s
        u.reset_at_unix = Some(1000 + 2 * 3600 + 15 * 60);
        let (full, compact, ..) = usage_words(&u, 1_000_000);
        assert_eq!(full, "⛔ resets in 2h 15m · 4:30pm");
        assert_eq!(compact, "⛔ 2h 15m");
    }

    /// A WEEKLY hit surfaces its printed DATE (previously parsed into `reset_date`
    /// but never shown — a weekly reset read identically to a same-day one).
    #[test]
    fn a_weekly_hit_shows_its_date() {
        let mut u = limit(true, None, "7am");
        u.reset_date = "Jun 5".into();
        u.reset_at_unix = Some(1000 + 5 * 86400);
        let (full, compact, ..) = usage_words(&u, 1_000_000);
        assert_eq!(full, "⛔ resets in 5d · Jun 5, 7am");
        assert_eq!(compact, "⛔ 5d");
    }

    /// "Approaching usage limit" carries NO percent — it must say so, not render a
    /// misleading "used 0%".
    #[test]
    fn an_approaching_warning_without_a_percent_says_approaching_not_used_0() {
        let (full, compact, ..) = usage_words(&limit(false, None, ""), 0);
        assert_eq!(full, "◔ approaching limit");
        assert_eq!(compact, "◔ limit");
    }

    /// The rail's second line is 162px of paintable width. `fmt_reset_clock`
    /// prefixes a WEEKDAY whenever the reset crosses local midnight (a 5h window
    /// hit at 9pm → "Thu 2:00am"), and the FULL wording then measures ~167–183px
    /// and is hard-clipped — the pill is flex_none + nowrap with no max_w. The
    /// compact form keeps the whole clock, weekday included, inside the line.
    #[test]
    fn the_compact_chip_keeps_a_weekday_qualified_clock_whole() {
        let (full, compact, ..) = usage_words(&limit(true, None, "Wed 11:30pm"), 0);
        assert_eq!(full, "⛔ limit hit · resets Wed 11:30pm"); // 179px — CLIPS
        assert_eq!(compact, "⛔ Wed 11:30pm"); // 98px — fits
        let (_, compact, ..) = usage_words(&limit(false, Some(97), "Wed 11:30pm"), 0);
        assert_eq!(compact, "◔ 97%");
        // the compact chip is what the RAIL renders, so nothing it can produce may
        // outrun that line: the widest string is the weekday clock. Cover both the
        // clock fallback AND the countdown (the widest countdown, "6d 23h", is far
        // shorter than the weekday clock).
        let widest = "⛔ Wed 11:30pm".chars().count();
        for clock in ["Wed 11:30pm", "Fri 4:30pm", "Thu 1:30am", "9:50pm", ""] {
            let (_, compact, ..) = usage_words(&limit(true, None, clock), 0);
            assert!(
                compact.chars().count() <= widest,
                "compact chip grew past the rail's line: {compact:?}"
            );
        }
        for secs in [30, 12 * 60, 2 * 3600 + 15 * 60, 6 * 86400 + 23 * 3600] {
            let mut u = limit(true, None, "");
            u.reset_at_unix = Some(1000 + secs);
            let (_, compact, ..) = usage_words(&u, 1_000_000);
            assert!(
                compact.chars().count() <= widest,
                "countdown compact chip grew past the rail's line: {compact:?}"
            );
        }
    }

    // ── the ＋New menu's "New project" line (#67 / #29) ──────────────────────
    use super::{elide_middle, new_project_where, ROOT_BUDGET};
    use std::path::Path;

    /// A tripwire, not a proof — the menu can't be rendered without a gpui App,
    /// so what's pinned is that the Idea row is still INSIDE the gate (the same
    /// `include_str!` canary features.rs uses for the LLM entry points). Deleting
    /// the `.when(…)` would otherwise silently restore a third creation verb whose
    /// tagline advertises a surface the default build doesn't compile:
    /// default = 2 rows (New project, Open folder…), `--features map` = 3.
    #[test]
    fn the_idea_verb_stays_behind_the_map_gate() {
        // the CODE only, never this module: the assertions quote the very strings
        // they hunt for, and a self-match would keep the test green after somebody
        // deleted the real gate.
        let src = include_str!("render_sidebar.rs");
        let code = &src[..src.find("\nmod tests {").expect("the test module moved")];
        let gate = code
            .find(".when(crate::features::MAP_ENABLED,")
            .expect("the ＋New menu's Idea row must stay behind MAP_ENABLED");
        let idea = code.find("\"rn-idea\"").expect("the Idea row");
        let folder = code.find("\"rn-folder\"").expect("the Open folder row");
        assert!(
            gate < idea && idea < folder,
            "the Idea row must be the gate's child, still between New project and Open folder"
        );
    }

    /// The point of the line: it names the REAL root, in the shorthand the user
    /// thinks in.
    #[test]
    fn the_new_project_line_names_the_real_root_and_collapses_home() {
        assert_eq!(
            new_project_where(Path::new("/Users/dev/local"), Path::new("/Users/dev"), ROOT_BUDGET),
            "creates a folder in ~/local and starts there"
        );
    }

    /// A root OUTSIDE home has no shorthand — it must print whole, not sprout a
    /// tilde it doesn't own.
    #[test]
    fn a_root_outside_home_is_printed_whole() {
        assert_eq!(
            new_project_where(Path::new("/work/code"), Path::new("/Users/dev"), ROOT_BUDGET),
            "creates a folder in /work/code and starts there"
        );
    }

    /// `core::scan::home()` yields an EMPTY path when HOME is unset rather than
    /// inventing one — and an empty prefix strips off every path, so the naive
    /// collapse would print "~//srv/projects".
    #[test]
    fn an_unset_home_never_fakes_a_tilde() {
        assert_eq!(
            new_project_where(Path::new("/srv/projects"), Path::new(""), ROOT_BUDGET),
            "creates a folder in /srv/projects and starts there"
        );
        // home itself as the root is the one case the collapse yields a bare ~.
        assert_eq!(
            new_project_where(Path::new("/Users/dev"), Path::new("/Users/dev"), ROOT_BUDGET),
            "creates a folder in ~ and starts there"
        );
    }

    /// A deep root elides in the MIDDLE: the folder it lands in survives, which a
    /// prefix trim ("~/Documents/work/cli…") would have eaten.
    #[test]
    fn a_long_root_keeps_the_folder_it_lands_in() {
        let line = new_project_where(
            Path::new("/Users/dev/Documents/work/clients/acme-2026"),
            Path::new("/Users/dev"),
            ROOT_BUDGET,
        );
        assert_eq!(
            line,
            "creates a folder in ~/Docum…ents/acme-2026 and starts there"
        );
    }

    /// The desc WRAPS (a truncating one lost the sentence: "…~/Documents/code and
    /// starts th…"), so what the budget buys is the row's HEIGHT. Measured at
    /// 11px system UI in the panel's 267px text column: the widest line this can
    /// produce — 59 chars, every one of them a `W` — is 420px, i.e. two lines.
    /// Lengthening the frame or raising the budget has to face the third.
    #[test]
    fn the_new_project_line_cannot_outgrow_two_wrapped_lines() {
        for root in [
            "/Users/dev/local",
            "/Users/dev/Documents/work/clients/acme-2026",
            "/Volumes/External Drive/projects/2026/clients",
        ] {
            let line = new_project_where(Path::new(root), Path::new("/Users/dev"), ROOT_BUDGET);
            assert!(
                line.chars().count() <= 59,
                "the ＋New desc grew past its two-line ceiling: {line:?}"
            );
        }
    }

    /// The elision must respect its budget exactly (it is what keeps the wrapped
    /// desc to at most two lines in a 320px panel), and must not touch a path
    /// that already fits.
    #[test]
    fn the_elision_spends_exactly_its_budget() {
        assert_eq!(elide_middle("~/local", ROOT_BUDGET), "~/local");
        let exact = "~/0123456789012345678"; // 21 chars
        assert_eq!(elide_middle(exact, 21), exact);
        for p in [
            "~/Documents/work/clients/acme-2026",
            "/Volumes/External Drive/projects/2026",
            "~/ünïcödé/påth/that/is/quite/long/indeed",
        ] {
            let out = elide_middle(p, ROOT_BUDGET);
            assert_eq!(
                out.chars().count(),
                ROOT_BUDGET,
                "elided path missed its budget: {out:?}"
            );
        }
    }
}

/// How a session row's title reads. Split out as a pure function because the
/// PRECEDENCE is the whole rule and it is easy to break by reordering four
/// branches: needs-you outranks everything, the row you are on is never also
/// "unreviewed", and resting is the floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowTone {
    /// The agent is blocked on you. Amber, and it owns that colour alone.
    NeedsYou,
    /// The session you have open.
    Focused,
    /// Finished a turn you have not opened since (#13).
    Unreviewed,
    Resting,
}

impl RowTone {
    pub(crate) fn color(self) -> u32 {
        match self {
            RowTone::NeedsYou => AMBER,
            RowTone::Focused => TEXT_STRONG,
            // one step up the MUTED -> TEXT -> TEXT_STRONG ladder: "something
            // finished here" should read without shouting over needs-you.
            RowTone::Unreviewed => TEXT,
            RowTone::Resting => MUTED,
        }
    }
}

pub(crate) fn row_tone(needs_action: bool, focused: bool, unreviewed: bool) -> RowTone {
    if needs_action {
        RowTone::NeedsYou
    } else if focused {
        RowTone::Focused
    } else if unreviewed {
        RowTone::Unreviewed
    } else {
        RowTone::Resting
    }
}

#[cfg(test)]
mod row_tone_tests {
    // Selective imports, not `use super::*`: gpui is glob-imported at the top of
    // this file and its `test` attribute macro would shadow the real one — which
    // fails as "recursion limit reached while expanding #[test]". Same reason
    // rail_order.rs imports selectively.
    use super::{row_tone, RowTone};

    #[test]
    fn needs_you_outranks_everything() {
        // A blocked agent is the only thing amber means; a finished turn must
        // never dilute it.
        assert_eq!(row_tone(true, true, true), RowTone::NeedsYou);
        assert_eq!(row_tone(true, false, true), RowTone::NeedsYou);
    }

    #[test]
    fn the_row_you_are_on_is_never_flagged() {
        // The whole point of the cue is "you have not looked at this". The
        // session you are watching cannot qualify, even if the flag leaked in.
        assert_eq!(row_tone(false, true, true), RowTone::Focused);
    }

    #[test]
    fn unreviewed_sits_between_resting_and_focused() {
        assert_eq!(row_tone(false, false, true), RowTone::Unreviewed);
        assert_eq!(row_tone(false, false, false), RowTone::Resting);
        // brightness ladder, so the cue reads as "brighter" rather than "other"
        assert!(RowTone::Resting.color() < RowTone::Unreviewed.color());
        assert!(RowTone::Unreviewed.color() < RowTone::Focused.color());
    }

    #[test]
    fn every_tone_has_a_distinct_colour() {
        let all = [
            RowTone::NeedsYou.color(),
            RowTone::Focused.color(),
            RowTone::Unreviewed.color(),
            RowTone::Resting.color(),
        ];
        let mut u = all.to_vec();
        u.sort_unstable();
        u.dedup();
        assert_eq!(u.len(), all.len(), "two tones render identically");
    }
}
