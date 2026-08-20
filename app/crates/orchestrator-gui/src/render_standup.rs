use gpui::prelude::FluentBuilder;
use gpui::*;
use crate::*;


impl Orchestrator {
    /// A section rule inside a tier: LABEL, then a hairline to the right edge.
    /// Used for the new/earlier split in ▲ WHAT HAPPENED.
    fn section_bar(label: SharedString, colour: u32) -> Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(9.))
            .pt(px(6.))
            .text_size(px(10.))
            .text_color(rgb(colour))
            .child(label)
            .child(div().flex_1().h(px(1.)).bg(rgb(HAIR_SOFT)))
    }

    /// One project's report in ▲ WHAT HAPPENED.
    ///
    /// ONE builder for both densities on purpose: they share the freshness dot,
    /// the name, the click target and the meta, and differ only in whether the
    /// event lines render BENEATH the header or the newest one rides inline.
    /// Two builders would drift the moment either is touched.
    fn update_block(
        &self,
        p: &crate::standup_plan::PlannedProject,
        name: String,
        density: crate::standup_plan::Density,
        now_ms: u64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let digest = matches!(density, crate::standup_plan::Density::Digest);
        let age = crate::timefmt::age_ms_since(p.newest_ms, now_ms);
        let (fresh, count, hidden) = (p.fresh, p.total, p.hidden_lines);
        let jslug = p.key.clone();
        let lead = p
            .lines
            .first()
            .map(|l| termview::trim(&l.text, 90))
            .unwrap_or_default();
        let lines: Vec<(&'static str, u32, String)> = if digest {
            Vec::new()
        } else {
            p.lines
                .iter()
                .map(|l| {
                    let (g, gc) = crate::standup_plan::kind_glyph(&l.kind);
                    (g, gc, termview::trim(&l.text, 120))
                })
                .collect()
        };

        let head = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(9.))
            // a RESERVED blank when read, so names stay column-aligned —
            .child(
                div()
                    .w(px(6.))
                    .h(px(6.))
                    .rounded(px(3.))
                    .flex_none()
                    .when(fresh, |d| d.bg(rgb(ACCENT))),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(if digest { 104. } else { 150. }))
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.5))
                    .text_color(rgb(if fresh { TEXT_STRONG } else { MUTED }))
                    .when(fresh, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .child(SharedString::from(termview::trim(&name, 24))),
            )
            .when(digest, |r| {
                r.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.))
                        .text_color(rgb(if fresh { TEXT } else { MUTED2 }))
                        .child(SharedString::from(lead)),
                )
            })
            .when(!digest, |r| r.child(div().flex_1()))
            .child(
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .text_size(px(10.5))
                    .text_color(rgb(MUTED2))
                    .child(SharedString::from(if digest {
                        format!("{count} · {age}")
                    } else {
                        format!(
                            "{count} update{} · {age}",
                            if count == 1 { "" } else { "s" }
                        )
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .text_size(px(10.5))
                    .text_color(rgb(MUTED2))
                    .child("open ▸"),
            );

        let mut card = div()
            .id(SharedString::from(format!("upd-{}", p.key)))
            .flex()
            .flex_col()
            .gap(px(5.))
            .px(px(12.))
            .py(px(if digest { 5. } else { 9. }))
            .rounded(px(if digest { 8. } else { 10. }))
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(if fresh { 0x346B54 } else { HAIR }))
            .cursor_pointer()
            .hover(|h| h.border_color(rgb(0x36404A)))
            .child(head);
        for (glyph, gcol, text) in lines {
            card = card.child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.))
                    .pl(px(15.))
                    .child(
                        div()
                            .flex_none()
                            .w(px(13.))
                            .text_size(px(10.5))
                            .text_color(rgb(gcol))
                            .child(glyph),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(rgb(TEXT))
                            .child(SharedString::from(text)),
                    ),
            );
        }
        if !digest && hidden > 0 {
            card = card.child(
                div()
                    .pl(px(15.))
                    .text_size(px(11.))
                    .text_color(rgb(MUTED2))
                    .child(SharedString::from(format!("+{hidden} more"))),
            );
        }
        card.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.select_project(&jslug, cx)
        }))
    }


    /// A pill telling the user whether sessions survive a restart — green when
    /// the daemon is attached, a LOUD amber warning on a silent in-process
    /// fallback (dogfooding: invisible daemon = the feature "doesn't exist").
    fn render_host_mode(&self) -> impl IntoElement {
        use orchestrator_daemon::HostMode;
        let pill = |dot: u32, fg: u32, bg: u32, text: String| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .px(px(9.))
                .py(px(3.))
                .rounded(px(7.))
                .bg(rgb(bg))
                .child(div().w(px(6.)).h(px(6.)).rounded(px(3.)).bg(rgb(dot)))
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(rgb(fg))
                        .child(SharedString::from(text)),
                )
        };
        match self.host_mode {
            HostMode::Daemon => {
                let n = self.host.infos().len();
                pill(GREEN, MUTED, CARD, format!("daemon · {n} live"))
            }
            HostMode::InProcessByChoice => pill(MUTED, MUTED2, CARD, "in-process".into()),
            HostMode::InProcessFallback => pill(
                0xE6A23C,
                0xE6C07A,
                0x2A2418,
                "in-process — won't survive restart".into(),
            ),
        }
    }

    pub(crate) fn render_standup(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Gather every LIVE agent across all projects — the Standup is now an
        // AGENT-centric live dashboard (who needs me / what's working / today),
        // not a project list (#4).
        let mut needs: Vec<(usize, String, String, SessionInfo)> = Vec::new();
        let mut working: Vec<(String, SessionInfo)> = Vec::new();
        let mut idle: Vec<(String, SessionInfo)> = Vec::new();
        let mut blocked: Vec<(String, SessionInfo)> = Vec::new();
        let mut live_n = 0usize;
        if self.scanned {
            for (i, p) in self.projects.iter().enumerate() {
                for info in self.cached_infos(&p.slug) {
                    if !info.alive {
                        continue;
                    }
                    live_n += 1;
                    // A hard limit HIT pins to ⛔ BLOCKED — UNLESS the session is
                    // ALSO sitting on a real permission prompt. Running out of quota
                    // does not make an ask unanswerable: approve it now and the work
                    // resumes when the limit resets, so filing it under "blocked,
                    // wait it out" buried something the user could clear in seconds.
                    //
                    // This `continue` was also the ONLY surface that disagreed about
                    // what "needs you" means: the Dock badge, the toast, the macOS
                    // notification and the sidebar dot all count AwaitingDecision
                    // alone, which made Dock-badge 1 / Standup "nothing needs you"
                    // reachable at the same instant.
                    //
                    // Still exactly ONE tier per session — an asking session lands in
                    // ⚠ NEEDS YOU only — and that row renders the usage chip, so the
                    // limit is surfaced rather than traded away for the ask.
                    if blocked_tier_claims(
                        info.usage_limit.as_ref().is_some_and(|u| u.hit),
                        info.phase == orchestrator_host::Phase::AwaitingDecision,
                    ) {
                        blocked.push((p.name.clone(), info.clone()));
                        // don't ALSO fall through into working/idle. Still counted in
                        // `live_n` above, so the "N agents live" headline is unchanged.
                        continue;
                    }
                    match info.phase {
                        orchestrator_host::Phase::AwaitingDecision => {
                            needs.push((i, p.name.clone(), p.slug.clone(), info.clone()))
                        }
                        orchestrator_host::Phase::Busy => {
                            working.push((p.name.clone(), info.clone()))
                        }
                        _ => idle.push((p.name.clone(), info.clone())),
                    }
                }
            }
        }
        // oldest ask first — the one you've been ignoring longest leads (#4).
        needs.sort_by_key(|(_, _, _, info)| info.phase_since_ms);
        let need_n = needs.len();
        let work_n = working.len();
        let idle_n = idle.len();
        // session-centric headline (the Deck reframe: sessions are the home).
        let greeting = if !self.scanned {
            "Reading your sessions…".to_string()
        } else if live_n > 0 {
            format!("{live_n} agent{} live.", if live_n == 1 { "" } else { "s" })
        } else {
            "All quiet.".to_string()
        };
        let subline = if !self.scanned {
            "Reading your Claude + Codex sessions…".to_string()
        } else {
            let mut parts = Vec::new();
            if need_n > 0 {
                parts.push(format!("⚠ {need_n} need you"));
            }
            if work_n > 0 {
                parts.push(format!("● {work_n} working"));
            }
            if idle_n > 0 {
                parts.push(format!("◌ {idle_n} idle"));
            }
            if parts.is_empty() {
                "nothing needs you right now".to_string()
            } else {
                parts.join(" · ")
            }
        };

        let mut feed = div().flex().flex_col().gap(px(18.));
        if !self.scanned {
            feed = feed.child(
                div()
                    .p(px(16.))
                    .text_size(px(13.5))
                    .text_color(rgb(MUTED2))
                    .child("Reading your Claude + Codex sessions…"),
            );
        }
        // ── ⛔ BLOCKED — subscription-limit HITS, pinned above even a waiting
        // prompt (docs/019): a capped session otherwise reads as plain Idle. Only
        // a hard `hit` earns this loud red pin; the amber warning stays a chip.
        if !blocked.is_empty() {
            let now_ms = crate::render_sidebar::wall_now_ms();
            let mut tier = div()
                .flex().flex_col().gap(px(6.))
                .child(
                    div().flex().flex_row().items_center().gap(px(7.))
                        .text_size(px(11.5)).font_weight(FontWeight::BOLD).text_color(rgb(0xE68A8A))
                        .child("⛔ BLOCKED")
                        .child(div().text_color(rgb(MUTED2)).child(SharedString::from(blocked.len().to_string()))),
                );
            for (name, info) in blocked {
                // the tier is built from `is_some_and(|u| u.hit)`, so this is Some —
                // but a render path must not carry a panic that a later refactor of
                // that gate could arm.
                let Some(u) = info.usage_limit.as_ref() else {
                    continue;
                };
                let cd = u.reset_countdown(now_ms);
                let when = u.reset_label();
                let tz = if u.reset_tz.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", u.reset_tz.rsplit('/').next().unwrap_or("").replace('_', " "))
                };
                let detail: String = if !cd.is_empty() {
                    if when.is_empty() {
                        format!("resets in {cd}{tz}")
                    } else {
                        format!("resets in {cd} · {when}{tz}")
                    }
                } else if !when.is_empty() {
                    format!("resets {when}{tz}")
                } else {
                    // no reset at all — a credit cap ("add credits") or a zone-less
                    // banner. NOT "waiting on limit reset": a credit cap never resets,
                    // so telling the user to wait would send them to sit on their hands.
                    "limit reached".into()
                };
                let (jslug, jid) = (info.project_slug.clone(), info.id);
                tier = tier.child(
                    div()
                        .id(SharedString::from(format!("blocked-{}", info.id.0)))
                        .flex().flex_row().items_center().gap(px(10.))
                        .px(px(12.)).py(px(7.)).rounded(px(9.))
                        .bg(rgb(0x201414)).border_1().border_color(rgb(0x5a2c2c))
                        .cursor_pointer().hover(|h| h.border_color(rgb(0x7a3c3c)))
                        .child(div().flex_none().whitespace_nowrap().w(px(14.)).text_size(px(11.)).text_color(rgb(0xE68A8A)).child("⛔"))
                        .child(div().flex_none().w(px(150.)).min_w_0().truncate().text_size(px(12.5)).text_color(rgb(TEXT_STRONG))
                            .child(SharedString::from(termview::session_label(&info))))
                        .child(div().flex_none().whitespace_nowrap().text_size(px(11.)).text_color(rgb(MUTED2)).bg(rgb(CARD)).rounded(px(5.)).px(px(6.)).py(px(1.))
                            .child(SharedString::from(termview::trim(&name, 20))))
                        .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(rgb(0xE0A0A0)).child(SharedString::from(detail)))
                        .child(div().flex_none().whitespace_nowrap().text_size(px(10.5)).text_color(rgb(MUTED2)).child("open ▸"))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.focus_session(&jslug, jid, window, cx)
                        })),
                );
            }
            feed = feed.child(tier);
        }
        // ── ⚠ NEEDS YOU — pinned top, loud, ONE CTA: open it in the terminal ──
        if need_n > 0 {
            let mut tier = div().flex().flex_col().gap(px(9.)).child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(7.))
                    .text_size(px(11.5))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(AMBER))
                    .child("⚠ NEEDS YOU")
                    .child(
                        div()
                            .text_color(rgb(MUTED2))
                            .child(SharedString::from(need_n.to_string())),
                    ),
            );
            for (_i, name, slug, info) in needs {
                tier = tier.child(self.needs_card(name, slug, info, cx));
            }
            feed = feed.child(tier);
        }
        // ── ▲ WHAT HAPPENED — the standup proper.
        //
        // This was TWO tiers (▲ UPDATED + ▦ PORTFOLIO), both keyed on rollup
        // lines, and both sat BELOW the live-session list. That ordering was the
        // bug: a running session is REASSURANCE — it tells you nothing you can
        // act on — while an update is the thing you opened the app for. So
        // reassurance now sits underneath information, as a single line.
        //
        // Grouping is by PROJECT, split once at your last check, because the
        // unit you carry in your head is the project. A flat chronological list
        // mixing five event kinds across nine projects is what made this
        // unreadable. Every cap and threshold lives in standup_plan, tested.
        //
        // ONE timeline read for the whole screen: this tier and the thread far
        // below are two views of the SAME events, so a second read would be
        // another lock AND a chance for the two to disagree.
        let timeline: Vec<orchestrator_store::TimelineEvent> = if self.scanned {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            store.timeline(120)
        } else {
            Vec::new()
        };
        let pname = |key: &str| {
            self.projects
                .iter()
                .find(|p| p.slug == key)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| key.rsplit(['/', ':']).next().unwrap_or(key).to_string())
        };
        let now_ms = crate::timefmt::now_ms();
        if self.scanned {
            let plan = crate::standup_plan::plan_updates(
                &timeline,
                self.standup_divider_ms,
                self.standup_updates_all,
            );
            if !plan.is_empty() {
                // The bars exist to SEPARATE two groups. With only one group
                // they label nothing, so they are suppressed — which is also
                // the first-ever visit, where everything is new by definition.
                let bars = !plan.fresh.is_empty() && !plan.earlier.is_empty();
                let mut tier = div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .pt(px(14.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(7.))
                            .text_size(px(11.5))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(ACCENT))
                            .child("▲ WHAT HAPPENED")
                            .child(
                                div()
                                    .text_color(rgb(MUTED2))
                                    .child(SharedString::from(plan.reporting.to_string())),
                            ),
                    );
                if bars {
                    let since = crate::timefmt::age_ms_since(self.standup_divider_ms, now_ms);
                    tier = tier.child(Self::section_bar(
                        SharedString::from(format!("NEW SINCE YOU LAST LOOKED · {since}")),
                        ACCENT,
                    ));
                }
                for pp in &plan.fresh {
                    tier = tier.child(self.update_block(pp, pname(&pp.key), plan.density, now_ms, cx));
                }
                if bars {
                    tier = tier.child(Self::section_bar("EARLIER".into(), MUTED2));
                }
                for pp in &plan.earlier {
                    tier = tier.child(self.update_block(pp, pname(&pp.key), plan.density, now_ms, cx));
                }
                if plan.hidden_projects > 0 {
                    let n = plan.hidden_projects;
                    tier = tier.child(
                        div()
                            .id("upd-show-all")
                            .px(px(12.))
                            .py(px(3.))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED2))
                            .hover(|h| h.text_color(rgb(ACCENT)))
                            .child(SharedString::from(format!(
                                "{n} more project{} — show all ▸",
                                if n == 1 { "" } else { "s" }
                            )))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.standup_updates_all = true;
                                cx.notify();
                            })),
                    );
                }
                feed = feed.child(tier);
            }
        }

        // ── ● LIVE — ambient, and deliberately BELOW the updates.
        //
        // "Eleven things are running and fine" is a sentence, not eleven rows.
        // The per-session list is still here, one click away — collapsed, not
        // deleted. Uncollapsed it grew without limit, and thirty sessions
        // pushed everything worth reading off the screen.
        let summary = {
            let projects: std::collections::HashSet<&str> = working
                .iter()
                .chain(idle.iter())
                .map(|(_, i)| i.project_slug.as_str())
                .collect();
            crate::standup_plan::LiveSummary {
                working: work_n,
                idle: idle_n,
                projects: projects.len(),
            }
        };
        // the summary already knows the count and whether there is anything to
        // show — asking `work_n + idle_n` again here is the same sum written
        // twice, and two places to get it wrong.
        if !summary.is_empty() {
            // materialised BEFORE the row loop consumes `working` / `idle`
            let dots: Vec<bool> = working
                .iter()
                .map(|_| true)
                .chain(idle.iter().map(|_| false))
                .collect();
            let open = self.standup_live_open;
            let mut tier = div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .pt(px(14.))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(7.))
                        .text_size(px(11.5))
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(MUTED2))
                        .child("● LIVE")
                        .child(
                            div()
                                .text_color(rgb(MUTED2))
                                .child(SharedString::from(summary.total().to_string())),
                        ),
                )
                .child(
                    div()
                        .id("live-strip")
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(10.))
                        .px(px(12.))
                        .py(px(7.))
                        .rounded(px(9.))
                        .bg(rgb(0x12161D))
                        .border_1()
                        .border_color(rgb(HAIR_SOFT))
                        .cursor_pointer()
                        .hover(|h| h.border_color(rgb(HAIR)))
                        .child({
                            let mut row = div().flex().flex_row().flex_none().gap(px(3.));
                            for busy in &dots {
                                row = row.child(
                                    div()
                                        .w(px(6.))
                                        .h(px(6.))
                                        .rounded(px(3.))
                                        .bg(rgb(if *busy { GREEN } else { 0x3A424E })),
                                );
                            }
                            row
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.))
                                .text_color(rgb(MUTED))
                                .child(SharedString::from(summary.line())),
                        )
                        .child(
                            div()
                                .flex_none()
                                .whitespace_nowrap()
                                .text_size(px(10.5))
                                .text_color(rgb(MUTED2))
                                .child(if open { "hide ▾" } else { "show ▸" }),
                        )
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.standup_live_open = !this.standup_live_open;
                            cx.notify();
                        })),
                );
            if open {
                for (busy, (name, info)) in working
                    .into_iter()
                    .map(|r| (true, r))
                    .chain(idle.into_iter().map(|r| (false, r)))
                {
                    let (glyph, gcol): (&str, u32) = if busy { ("●", GREEN) } else { ("◌", MUTED2) };
                    let doing = {
                        let m = info.last_message.trim();
                        if m.is_empty() {
                            (if busy { "working…" } else { "idle" }).to_string()
                        } else {
                            termview::trim(m, 80)
                        }
                    };
                    let (jslug, jid) = (info.project_slug.clone(), info.id);
                    tier = tier.child(
                        div()
                            .id(SharedString::from(format!("live-{}", jid.0)))
                            .flex().flex_row().items_center().gap(px(10.))
                            .px(px(12.)).py(px(7.)).rounded(px(9.))
                            .bg(rgb(PANEL)).border_1().border_color(rgb(HAIR))
                            .cursor_pointer().hover(|h| h.border_color(rgb(0x36404A)))
                            .child(div().flex_none().whitespace_nowrap().w(px(14.)).text_size(px(11.)).text_color(rgb(gcol)).child(glyph))
                            .child(div().flex_none().w(px(150.)).min_w_0().truncate().text_size(px(12.5)).text_color(rgb(TEXT_STRONG))
                                .child(SharedString::from(termview::session_label(&info))))
                            .child(div().flex_none().whitespace_nowrap().text_size(px(11.)).text_color(rgb(MUTED2)).bg(rgb(CARD)).rounded(px(5.)).px(px(6.)).py(px(1.))
                                .child(SharedString::from(termview::trim(&name, 20))))
                            .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(rgb(MUTED)).child(SharedString::from(doing)))
                            .when_some(info.usage_limit.clone(), |c, u| c.child(crate::render_sidebar::usage_chip(&u)))
                            .child(div().flex_none().whitespace_nowrap().text_size(px(10.5)).text_color(rgb(MUTED2)).child("open ▸"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.focus_session(&jslug, jid, window, cx)
                            })),
                    );
                }
            }
            feed = feed.child(tier);
        }
        // ── ▲ MAP UPDATES — the morning checkmarks (docs/011 slice 3): every
        // pending proposal op across projects (sessions, break-downs, drift),
        // resolvable inline; 'map ▸' jumps to the node for context.
        {
            let rows: Vec<(
                String,
                String,
                DiffOp,
                Option<String>,
                Option<PartId>,
                String,
                String,
            )> = {
                let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
                let gen = store.write_gen();
                let mut memo = self.standup_updates.borrow_mut();
                if memo.0 != gen {
                    let mut rows = Vec::new();
                    for p in &self.projects {
                        // singleton proposals only; changeset rows are reviewed
                        // as a group on the map, not itemized in the standup.
                        let diffs: Vec<_> = store
                            .pending_diffs(&p.slug)
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|pd| pd.kind != "seed" && pd.changeset_id.is_none())
                            .collect();
                        if diffs.is_empty() {
                            continue;
                        }
                        // ONE tree read per project per write-gen, not per row.
                        let names: std::collections::HashMap<PartId, String> = store
                            .load_tree(&p.slug)
                            .unwrap_or_default()
                            .iter()
                            .map(|pt| (pt.id, pt.name.clone()))
                            .collect();
                        let name_of = |id: PartId| {
                            names.get(&id).cloned().unwrap_or_else(|| format!("#{id}"))
                        };
                        for pd in diffs {
                            for (op, ev) in pd.ops.iter().zip(pd.evidence.iter()) {
                                let target = match op {
                                    DiffOp::SetStatus { id, .. }
                                    | DiffOp::Rename { id, .. }
                                    | DiffOp::Remove { id }
                                    | DiffOp::Move { id, .. } => Some(*id),
                                    DiffOp::Add {
                                        parent: PartRef::Id(id),
                                        ..
                                    } => Some(*id),
                                    _ => None,
                                };
                                let desc = describe_op(op, &name_of);
                                rows.push((
                                    p.slug.clone(),
                                    p.name.clone(),
                                    op.clone(),
                                    ev.clone(),
                                    target,
                                    desc,
                                    pd.kind.clone(),
                                ));
                            }
                        }
                    }
                    *memo = (gen, rows);
                }
                memo.1.clone()
            };
            if !rows.is_empty() {
                // NO .px() here: the scroll container already applies px(26).
                // Every tier used to add its own 18 on top, so four of six sat
                // inset from ⛔ BLOCKED and ⚠ NEEDS YOU, which add none.
                let mut tier = div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .pt(px(14.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(7.))
                            .text_size(px(11.5))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(AMBER))
                            .child("▲ MAP UPDATES")
                            .child(
                                div()
                                    .text_color(rgb(MUTED2))
                                    .child(SharedString::from(rows.len().to_string())),
                            ),
                    );
                let total = rows.len();
                for (ix, (rslug, rname, op, ev, target, desc, kind)) in
                    rows.into_iter().enumerate().take(12)
                {
                    let (aslug, dslug, jslug) = (rslug.clone(), rslug.clone(), rslug);
                    let (aop, dop) = (op.clone(), op);
                    let (akind, dkind) = (kind.clone(), kind);
                    let mut row = div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .px(px(12.))
                        .py(px(6.))
                        .rounded(px(9.))
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(HAIR))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(MUTED2))
                                .child(SharedString::from(rname)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(12.))
                                .text_color(rgb(TEXT))
                                .child(SharedString::from(desc)),
                        );
                    if let Some(q) = ev.filter(|q| !q.is_empty()) {
                        row = row.child(
                            div()
                                .max_w(px(260.))
                                .text_size(px(10.5))
                                .text_color(rgb(MUTED2))
                                .child(SharedString::from(format!("“{}”", termview::trim(&q, 60)))),
                        );
                    }
                    row = row
                        .child(
                            div()
                                .id(SharedString::from(format!("mu-ok-{ix}")))
                                .px(px(7.))
                                .py(px(2.))
                                .rounded(px(6.))
                                .cursor_pointer()
                                .text_size(px(12.))
                                .text_color(rgb(ACCENT))
                                .hover(|h| h.bg(rgb(CARD)))
                                .child("✓")
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.resolve_pending_op(&aslug, Some(&akind), &aop, true, cx)
                                })),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("mu-no-{ix}")))
                                .px(px(7.))
                                .py(px(2.))
                                .rounded(px(6.))
                                .cursor_pointer()
                                .text_size(px(12.))
                                .text_color(rgb(MUTED2))
                                .hover(|h| h.bg(rgb(CARD)))
                                .child("✕")
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.resolve_pending_op(&dslug, Some(&dkind), &dop, false, cx)
                                })),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("mu-map-{ix}")))
                                .px(px(7.))
                                .py(px(2.))
                                .rounded(px(6.))
                                .cursor_pointer()
                                .text_size(px(11.))
                                .text_color(rgb(MUTED2))
                                .hover(|h| h.text_color(rgb(ACCENT)))
                                .child("map ▸")
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    match target {
                                        Some(t) => this.focus_node_on_map(&jslug, t, cx),
                                        None => this.select_project(&jslug, cx),
                                    }
                                })),
                        );
                    tier = tier.child(row);
                }
                if total > 12 {
                    tier = tier.child(
                        div()
                            .px(px(12.))
                            .text_size(px(11.))
                            .text_color(rgb(MUTED2))
                            .child(SharedString::from(format!(
                                "+{} more — review on the maps",
                                total - 12
                            ))),
                    );
                }
                feed = feed.child(tier);
            }
        }

        // ── THE TIMELINE — the thread of the company (docs/012 §1-2) ──
        // NEWEST ON TOP (notification feed, not chat): the eye lands on what's
        // new. Day headers descend today → yesterday → …; the seen divider
        // sits BELOW the new entries, above the dimmed already-seen history.
        // The ● LIVE working/idle tier is restored ABOVE (between NEEDS YOU and
        // PORTFOLIO, #21); the rail pill still shows the same mix. ENDED strip
        // died (Recover + the "■ finished" trail absorb it); the TurnEnd digest
        // died (noise).
        if self.scanned {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            // summaries that DIED in the last day. The generator is self-healing
            // now (a death cools the session off, it never blacklists it), but a
            // silent generator is exactly what cost the user two days of
            // standup — so failure gets a surface, not just a count buried on
            // the Map screen with its error text thrown away.
            let day_ago = now_ms.saturating_sub(returnchannel::FAILURE_WINDOW_SECS * 1000);
            // The ▲ WHAT HAPPENED tier above already read the timeline; reuse
            // that exact Vec. Two reads would be two locks, and worse, two views
            // of one screen that could disagree about what happened.
            let events = timeline.clone();
            let failed: Vec<(String, String)> = {
                let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
                store
                    .dead_summary_jobs()
                    .into_iter()
                    .filter(|(_, _, _, _, died_ms)| *died_ms >= day_ago)
                    .map(|(_, cid, _, err, _)| (cid, err))
                    .collect()
            };
            let divider_ms = self.standup_divider_ms;
            let mut thread = div().flex().flex_col().gap(px(1.)).pt(px(4.));
            // ONE line, never two, and only when it tells the user something they
            // can act on. Both used to render at once, stacked, and the second read
            // "nothing on the thread yet — dispatch a session from a map node…":
            // "the thread" is a word this screen never defines, and the map is
            // COMPILED OUT of a default build, so it pointed a new user at a
            // feature their binary does not contain. With summaries on and no
            // events, the headline above ("All quiet." / "nothing needs you right
            // now") has already said it — a second sentence saying the same thing
            // is noise, so there is deliberately no empty-thread line at all.
            //
            // px(26) matches the headline block's own padding (see the header at
            // the bottom of this file); at px(8) these sat 18px left of everything
            // above them, which reads as a layout bug because it is one.
            if let Some(hint) = standup_thread_hint(self.summaries_on, events.is_empty()) {
                thread = thread.child(
                    div()
                        // NO horizontal padding. The scroll container this lives in
                        // already applies px(26) — the same value the headline block
                        // uses — so anything here is ADDED to it, not aligned with
                        // it. Measured: with px(26) the line started at x=267 while
                        // the headline started at x=240. The original px(8) was
                        // wrong the same way, just less visibly (x=247).
                        .pb(px(6.))
                        .text_size(px(11.5))
                        .text_color(rgb(MUTED2))
                        .child(hint),
                );
            }
            // A summarizer that has failed recently gets said OUT LOUD, above
            // the feed. The old surface was a grey count on the Map screen that
            // discarded the error text, so 10 permanently-blacklisted sessions
            // rendered exactly like healthy ones and the user had no way to
            // know the pipeline was dead.
            if !failed.is_empty() {
                let n = failed.len();
                let (_, err) = &failed[n - 1];
                let reason: String = err.chars().take(140).collect();
                thread = thread.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .px(px(8.))
                        .py(px(6.))
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(rgb(AMBER))
                                .child(SharedString::from(format!(
                                    "⚠ {n} session summar{} failed in the last day — retrying on a backoff.",
                                    if n == 1 { "y" } else { "ies" }
                                ))),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(MUTED2))
                                .truncate()
                                .child(SharedString::from(reason)),
                        ),
                );
            }
            // caught-up sits at the TOP now (newest-on-top): if the freshest
            // entry is already seen, say so before the (all-dimmed) history.
            //
            // But "caught up" is a claim about the THREAD, and the seen-ledger
            // is stamped every time he leaves this screen — so once the
            // summarizer died, the divider ratcheted past every event forever
            // and this line reported the starvation as SUCCESS: a friendly
            // "you're caught up" over a wall of dimmed 2-day-old text. A feed
            // whose newest entry isn't from today cannot say "caught up"; it
            // says how old it is, which is the thing that would have exposed
            // the dead pipeline on day one.
            let has_new = divider_ms > 0 && events.first().is_some_and(|e| e.ts_ms > divider_ms);
            if !events.is_empty() && divider_ms > 0 && !has_new {
                let newest = events.first().map(|e| e.ts_ms).unwrap_or(0);
                let stale_days = local_day(now_ms).saturating_sub(local_day(newest));
                let msg = match stale_days {
                    0 => "you\u{2019}re caught up — nothing new since your last check.".to_string(),
                    1 => "nothing has landed today — the newest entry on the thread is from yesterday.".to_string(),
                    n => format!(
                        "nothing has landed today — the newest entry on the thread is {n} days old."
                    ),
                };
                thread = thread.child(
                    div()
                        .px(px(8.))
                        .py(px(8.))
                        .text_size(px(11.5))
                        .text_color(if stale_days > 0 {
                            rgb(AMBER)
                        } else {
                            rgb(MUTED2)
                        })
                        .child(SharedString::from(msg)),
                );
            }
            let mut last_day: i64 = i64::MIN;
            let mut divider_done = divider_ms == 0;
            for ev in events.iter() {
                use orchestrator_store::TimelineKind as K;
                // day header FIRST so a day boundary that coincides with the
                // divider reads "YESTERDAY" then the divider, not the reverse.
                let day = local_day(ev.ts_ms);
                if day != last_day {
                    last_day = day;
                    let today = local_day(now_ms);
                    let label = match today.saturating_sub(day) {
                        0 => "today".to_string(),
                        1 => "yesterday".to_string(),
                        n => format!("{n} days ago"),
                    };
                    thread = thread.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.))
                            .mt(px(10.))
                            .mb(px(2.))
                            .child(div().flex_1().h(px(1.)).bg(rgb(HAIR_SOFT)))
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED2))
                                    .child(SharedString::from(label.to_uppercase())),
                            )
                            .child(div().flex_1().h(px(1.)).bg(rgb(HAIR_SOFT))),
                    );
                }
                // descending: the FIRST already-seen entry gets the divider
                // above it; everything from here down is dimmed history.
                if !divider_done && ev.ts_ms <= divider_ms {
                    divider_done = true;
                    thread = thread.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.))
                            .my(px(8.))
                            .child(div().flex_1().h(px(1.)).bg(rgb(0x3a3320)))
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(AMBER))
                                    .child(SharedString::from(format!(
                                        "last checked · {} ago",
                                        orchestrator_core::recap::rel_time(
                                            now_ms.saturating_sub(divider_ms) / 1000
                                        )
                                    ))),
                            )
                            .child(div().flex_1().h(px(1.)).bg(rgb(0x3a3320))),
                    );
                }
                // dim only what's below a REAL last-checked boundary. On the
                // first-ever visit (divider_ms==0) divider_done starts true to
                // suppress the divider line, but nothing is "already seen" —
                // everything is fresh (review: the flag flip dimmed the whole
                // first-look timeline as read history).
                let old = divider_done && divider_ms > 0;
                let key = ev.ts_ms ^ ((ev.kind.clone() as u64) << 1) ^ (ev.count as u64);
                let (glyph, gcol): (&str, u32) = match ev.kind {
                    K::Summary => ("☁", ACCENT),
                    K::Trail => {
                        if ev.text.starts_with('■') {
                            ("■", MUTED2)
                        } else {
                            ("▶", GREEN)
                        }
                    }
                    K::Decision => ("◆", AMBER),
                    K::Map => ("🗺", 0x9A7FD1),
                };
                let line: String = match ev.kind {
                    K::Summary => ev.text.clone(),
                    K::Trail => {
                        let node = ev.node.as_ref().map(|(_, n)| n.as_str()).unwrap_or("?");
                        match ev.text.split_once('—') {
                            Some((_, tail)) => format!("finished {node} — {}", tail.trim()),
                            None if ev.text.starts_with('■') => format!("finished {node}"),
                            None => format!("dispatched claude onto {node}"),
                        }
                    }
                    K::Decision => {
                        let node = ev.node.as_ref().map(|(_, n)| n.as_str()).unwrap_or("?");
                        format!("you decided on {node}: \u{201c}{}\u{201d}", ev.text)
                    }
                    K::Map => format!("map updated — {} accepted", ev.count),
                };
                let secs = (ev.ts_ms / 1000) as i64 + orchestrator_host::host::local_off_secs();
                let hhmm = format!(
                    "{:02}:{:02}",
                    (secs.rem_euclid(86400)) / 3600,
                    (secs.rem_euclid(86400) % 3600) / 60
                );
                let detail: Vec<String> = serde_json::from_str(&ev.detail_json).unwrap_or_default();
                let expandable = !detail.is_empty();
                let expanded = self.standup_expanded.contains(&key);
                let mut body = div().flex_1().min_w_0().child({
                    let mut l = div()
                        .flex()
                        .flex_row()
                        .items_baseline()
                        .gap(px(8.))
                        .flex_wrap()
                        .child(div().text_size(px(12.5)).text_color(rgb(TEXT)).child(
                            SharedString::from(format!("{} · {}", pname(&ev.project_key), line)),
                        ));
                    if !ev.next.is_empty() {
                        l = l.child(
                            div()
                                .text_size(px(10.5))
                                .text_color(rgb(MUTED))
                                .bg(rgb(CARD))
                                .rounded(px(5.))
                                .px(px(7.))
                                .child(SharedString::from(format!("next: {}", ev.next))),
                        );
                    }
                    l
                });
                if expanded {
                    let mut det = div().flex().flex_col().gap(px(2.)).mt(px(3.));
                    for b in &detail {
                        det = det.child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(MUTED))
                                .child(SharedString::from(format!("· {b}"))),
                        );
                    }
                    body = body.child(det);
                }
                let jump: Option<AnyElement> = match ev.kind {
                    K::Summary => self.find_live_by_cli_id(&ev.sess).map(|(jslug, jid)| {
                        div()
                            .id(SharedString::from(format!("tlj-{key}")))
                            .flex_none()
                            .text_size(px(10.5))
                            .text_color(rgb(MUTED2))
                            .cursor_pointer()
                            .hover(|h| h.text_color(rgb(ACCENT)))
                            .child("session ▸")
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.focus_session(&jslug.clone(), jid, window, cx);
                            }))
                            .into_any_element()
                    }),
                    K::Trail | K::Decision => ev.node.as_ref().map(|(nid, _)| {
                        let (jslug, nid) = (ev.project_key.clone(), *nid);
                        div()
                            .id(SharedString::from(format!("tlj-{key}")))
                            .flex_none()
                            .text_size(px(10.5))
                            .text_color(rgb(MUTED2))
                            .cursor_pointer()
                            .hover(|h| h.text_color(rgb(ACCENT)))
                            .child("map ▸")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                this.focus_node_on_map(&jslug, nid, cx);
                            }))
                            .into_any_element()
                    }),
                    K::Map => {
                        let jslug = ev.project_key.clone();
                        Some(
                            div()
                                .id(SharedString::from(format!("tlj-{key}")))
                                .flex_none()
                                .text_size(px(10.5))
                                .text_color(rgb(MUTED2))
                                .cursor_pointer()
                                .hover(|h| h.text_color(rgb(ACCENT)))
                                .child("map ▸")
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.select_project(&jslug, cx);
                                }))
                                .into_any_element(),
                        )
                    }
                };
                let mut row = div()
                    .id(SharedString::from(format!("tl-{key}")))
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(9.))
                    .px(px(8.))
                    .py(px(4.))
                    .rounded(px(8.))
                    .when(old, |r| r.opacity(0.45))
                    .when(expandable, |r| {
                        r.cursor_pointer()
                            .hover(|h| h.bg(rgba(0xFFFFFF06)))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                if !this.standup_expanded.remove(&key) {
                                    this.standup_expanded.insert(key);
                                }
                                cx.notify();
                            }))
                    })
                    .child(
                        div()
                            .w(px(36.))
                            .flex_none()
                            .text_size(px(10.5))
                            .text_color(rgb(MUTED2))
                            .child(SharedString::from(hhmm)),
                    )
                    .child(
                        div()
                            .w(px(16.))
                            .flex_none()
                            .text_size(px(11.))
                            .text_color(rgb(gcol))
                            .child(glyph),
                    )
                    .child(body);
                if let Some(j) = jump {
                    row = row.child(j);
                }
                thread = thread.child(row);
            }
            feed = feed.child(thread);
        }

        div()
            .flex_1()
            .flex()
            .flex_col()
            .min_w_0()
            .when_some(self.render_restore_banner(cx), |c, b| c.child(b))
            .child(
                div()
                    .px(px(26.))
                    .pt(px(22.))
                    .pb(px(10.))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.))
                            .child(
                                div()
                                    .text_size(px(23.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(TEXT_STRONG))
                                    .child(SharedString::from(greeting)),
                            )
                            .child(self.render_host_mode()),
                    )
                    .child(
                        div()
                            .text_size(px(14.))
                            .text_color(rgb(TEXT))
                            .child(SharedString::from(subline)),
                    ),
            )
            .child(
                div()
                    .id("standup-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(26.))
                    .py(px(10.))
                    .child(feed),
            )
    }

    /// One ⚠ Needs-you card (Standup): agent · project · the ask · one action.
    ///
    /// The ask TEXT stays — knowing WHAT is waiting is the whole value of the
    /// card. Answering it does not: from here the user can see one summarized
    /// line, and no one should consent to something they can't see. The single
    /// CTA takes them to the terminal, where the real dialog is.
    fn needs_card(
        &self,
        name: String,
        slug: String,
        info: SessionInfo,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = info.id;
        let eid = id.0;
        let ask = info
            .pending
            .as_ref()
            .map(|p| p.view.summary())
            .unwrap_or_else(|| "a decision is waiting".to_string());
        // the rot signal: how long this ask has been ignored — always shown (#4).
        let age_secs =
            orchestrator_core::registry::now_secs().saturating_sub(info.phase_since_ms / 1000);
        let waiting = if age_secs < 60 {
            "waiting".to_string()
        } else {
            format!("waiting {}", orchestrator_core::recap::rel_time(age_secs))
        };
        let actions = div().flex().flex_row().items_center().gap(px(8.)).child(
            div()
                .id(SharedString::from(format!("term-{eid}")))
                .px(px(15.))
                .py(px(6.))
                .rounded(px(9.))
                .cursor_pointer()
                .bg(rgb(0x23413a))
                .border_1()
                .border_color(rgb(0x346b54))
                .text_size(px(12.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(ACCENT))
                .child("Open in terminal ▸")
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.focus_session(&slug, id, window, cx)
                })),
        );
        div()
            .id(SharedString::from(format!("need-{eid}")))
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(14.))
            .rounded(px(12.))
            .bg(rgb(0x201a10))
            .border_1()
            .border_color(rgb(0x5a4a2c))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(9.))
                    .child(
                        div()
                            .text_size(px(14.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT_STRONG))
                            .child(SharedString::from(termview::session_label(&info))),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED2))
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(HAIR))
                            .rounded(px(6.))
                            .px(px(7.))
                            .py(px(1.))
                            .child(SharedString::from(name)),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(rgb(AMBER))
                            .child(SharedString::from(waiting)),
                    ),
            )
            .child(
                div()
                    .text_size(px(13.5))
                    .text_color(rgb(TEXT))
                    .child(SharedString::from(
                        ask.chars().take(170).collect::<String>(),
                    )),
            )
            .child(actions)
            .into_any_element()
    }

}


/// Does ⛔ BLOCKED claim this session, or does it fall through to the phase tiers?
///
/// Pure because this single rule is the one that used to make Standup disagree
/// with every other needs-you surface in the app — a comment could not prove it
/// no longer does, and a test can.
pub(crate) fn blocked_tier_claims(limit_hit: bool, awaiting_decision: bool) -> bool {
    limit_hit && !awaiting_decision
}

/// The ONE grey line under the standup thread, or none at all.
///
/// Pure so the "never two at once" rule is a test rather than a reading of two
/// separate `if`s. Only the summaries-off case earns a line: it names a setting
/// the user can go turn on. An empty thread with summaries ON says nothing the
/// headline ("All quiet." / "nothing needs you right now") has not already said,
/// so it gets silence.
pub(crate) fn standup_thread_hint(summaries_on: bool, _thread_empty: bool) -> Option<&'static str> {
    if summaries_on {
        return None;
    }
    Some("Session summaries are off — turn them on in Settings to see what each session got done.")
}

/// A project's plain-words rollup, computed DETERMINISTICALLY from the store
/// (docs/019 slice 4 Standup portfolio) — no live-session state, so the same DB
/// always yields the same line. `working` is derived-building over stored link
/// recency (the alive-stamp is a live-view concern); `drifted` is a live-only
/// signal, so it stays 0 here (the map view carries it). `None` = no parts yet.

/// The standup's one grey line (pure — no store, no window).
#[cfg(test)]
mod tests {
    use super::standup_thread_hint;

    #[test]
    fn only_the_actionable_line_is_ever_shown() {
        // summaries OFF: say so, and say where to change it — the one case where
        // the line tells the user something they can act on.
        let off = standup_thread_hint(false, true).expect("summaries-off earns a line");
        assert!(off.contains("Settings"), "the line must name where to fix it");
        assert_eq!(standup_thread_hint(false, false), Some(off));
    }

    #[test]
    fn an_empty_thread_with_summaries_on_says_nothing() {
        // the headline above already reads "All quiet." / "nothing needs you right
        // now"; a second sentence repeating it was noise, and the old one pointed
        // at the map — which a default build does not compile in.
        assert_eq!(standup_thread_hint(true, true), None);
        assert_eq!(standup_thread_hint(true, false), None);
    }

    #[test]
    fn no_hint_mentions_the_map_or_the_thread() {
        // regression: the retired copy read "nothing on the thread yet — dispatch a
        // session from a map node…", naming two things a default-build user has no
        // access to (the map is feature-gated) and one this screen never defines.
        for on in [true, false] {
            for empty in [true, false] {
                if let Some(h) = standup_thread_hint(on, empty) {
                    let l = h.to_lowercase();
                    assert!(!l.contains("map node"), "hint names the map: {h}");
                    assert!(!l.contains("thread"), "hint says 'thread': {h}");
                }
            }
        }
    }
}

/// The ⛔ BLOCKED tier's claim rule (pure — no store, no window).
#[cfg(test)]
mod blocked_tier_tests {
    use super::blocked_tier_claims;

    #[test]
    fn quota_alone_is_blocked() {
        // out of quota with nothing to answer: nothing the user can do but wait,
        // which is exactly what the BLOCKED tier is for.
        assert!(blocked_tier_claims(true, false));
    }

    #[test]
    fn an_ask_outranks_the_limit() {
        // THE FIX: a limit-hit session that is ALSO sitting on a permission prompt
        // falls through to ⚠ NEEDS YOU. Being out of quota does not make the ask
        // unanswerable — approving it now lets the work resume when the limit
        // resets, so burying it under "wait it out" hid a two-second action.
        assert!(!blocked_tier_claims(true, true));
    }

    #[test]
    fn a_healthy_session_is_never_blocked() {
        assert!(!blocked_tier_claims(false, false));
        assert!(!blocked_tier_claims(false, true));
    }

    #[test]
    fn standup_now_agrees_with_every_other_needs_you_surface() {
        // The badge, toast, macOS notification and sidebar dot all count
        // AwaitingDecision ALONE. Standup used to drop the limit-hit ones, which
        // made "Dock badge 1 / Standup: nothing needs you" reachable at the same
        // instant. For every session that is awaiting a decision, BLOCKED must now
        // decline it — whatever its quota state.
        for limit_hit in [true, false] {
            assert!(
                !blocked_tier_claims(limit_hit, true),
                "an awaiting session must reach NEEDS YOU (limit_hit={limit_hit})"
            );
        }
    }
}
