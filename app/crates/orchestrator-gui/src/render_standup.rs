use gpui::prelude::FluentBuilder;
use gpui::*;
use crate::*;


impl Orchestrator {
    /// One project row in the standup's project tiers (#49).
    ///
    /// Shared by ▲ UPDATED and ▦ PORTFOLIO so the two tiers can never drift
    /// apart visually. `age` is `Some` only for unread rows, which get the
    /// accent dot, a brighter name, and a "5m ago" stamp; read rows pass `None`
    /// and stay quiet.
    fn portfolio_row(
        &self,
        slug: String,
        name: String,
        line: String,
        age: Option<u64>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let unread = age.is_some();
        let jslug = slug.clone();
        div()
            .id(SharedString::from(format!("pf-{slug}")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .px(px(12.))
            .py(px(7.))
            .rounded(px(9.))
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(if unread { 0x346B54 } else { HAIR }))
            .cursor_pointer()
            .hover(|h| h.border_color(rgb(0x36404A)))
            // unread marker: an accent dot, and a reserved gap when read so the
            // names in both tiers still line up in the same column.
            .child(
                div()
                    .w(px(6.))
                    .h(px(6.))
                    .flex_none()
                    .rounded(px(3.))
                    .when(unread, |d| d.bg(rgb(ACCENT))),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(150.))
                    .min_w_0()
                    .text_size(px(12.5))
                    .font_weight(if unread {
                        FontWeight::SEMIBOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(rgb(if unread { TEXT_STRONG } else { TEXT }))
                    .child(SharedString::from(termview::trim(&name, 24))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.))
                    .font_family("Menlo")
                    .text_color(rgb(MUTED))
                    .child(SharedString::from(line)),
            )
            .when_some(age, |c, a| {
                c.child(
                    div()
                        .flex_none()
                        .text_size(px(10.5))
                        .text_color(rgb(MUTED))
                        .child(SharedString::from(crate::timefmt::ago_label(a))),
                )
            })
            .child(
                div()
                    .flex_none()
                    .text_size(px(10.5))
                    .text_color(rgb(MUTED2))
                    // with the map compiled out this opens the workspace, not a
                    // map — so don't promise a map that isn't in the build.
                    .child(if crate::features::MAP_ENABLED {
                        "map ▸"
                    } else {
                        "open ▸"
                    }),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
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
        // ── ● LIVE — the working/idle agents that DON'T need you (#21):
        // one row per live session, ● working / ◌ idle, session label +
        // project + what it's doing; click opens the session. Restores the
        // tier the timeline tombstone noted as dead.
        if work_n + idle_n > 0 {
            let mut tier = div()
                .flex().flex_col().gap(px(6.)).px(px(18.)).pt(px(14.))
                .child(
                    div().flex().flex_row().items_center().gap(px(7.))
                        .text_size(px(11.5)).font_weight(FontWeight::BOLD).text_color(rgb(MUTED2))
                        .child("● LIVE")
                        .child(div().text_color(rgb(MUTED2)).child(SharedString::from((work_n + idle_n).to_string()))),
                );
            for (busy, (name, info)) in working.into_iter().map(|r| (true, r))
                .chain(idle.into_iter().map(|r| (false, r)))
            {
                let (glyph, gcol): (&str, u32) = if busy { ("●", GREEN) } else { ("◌", MUTED2) };
                let doing = {
                    let m = info.last_message.trim();
                    if m.is_empty() { (if busy { "working…" } else { "idle" }).to_string() }
                    else { termview::trim(m, 80) }
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
            feed = feed.child(tier);
        }
        // ── ▦ PORTFOLIO — each project's plain-words rollup (docs/019 slice 4):
        // the v1 portfolio. Same Rollup::line() the map header shows, per
        // project, deterministic + memoized by write_gen. Click → open the map.
        if self.scanned {
            let rows: Vec<(String, String, String)> = {
                let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
                let now_secs = orchestrator_core::registry::now_secs();
                let key = (store.write_gen(), now_secs / 60);
                let mut memo = self.standup_rollups.borrow_mut();
                if memo.0 != key {
                    let mut rows = Vec::new();
                    for p in &self.projects {
                        if let Some(r) = deterministic_project_rollup(&store, &p.slug, now_secs) {
                            rows.push((p.slug.clone(), p.name.clone(), r.line()));
                        }
                    }
                    *memo = (key, rows);
                }
                memo.1.clone()
            };
            // #49: a flat "every project, every time" list buried the timeline
            // under rows the user had already read. Split it — what changed
            // since you last opened a project goes up top as ▲ UPDATED, and the
            // quiet remainder collapses behind a count so it stops blocking the
            // feed. Both are newest-activity first.
            //
            // NB: the store lock above is released by here. It has to be —
            // project_unread / project_update_ms take the lock themselves, and
            // the mutex is not reentrant.
            let now_ms = crate::timefmt::now_ms();
            let mut fresh: Vec<(String, String, String, u64)> = Vec::new();
            let mut rest: Vec<(String, String, String, u64)> = Vec::new();
            for (slug, name, line) in rows {
                let ts = self.project_update_ms(&slug);
                if self.project_unread(&slug) {
                    fresh.push((slug, name, line, ts));
                } else {
                    rest.push((slug, name, line, ts));
                }
            }
            fresh.sort_by(|a, b| b.3.cmp(&a.3));
            rest.sort_by(|a, b| b.3.cmp(&a.3));

            if !fresh.is_empty() {
                let mut tier = div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .px(px(18.))
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
                            .child("▲ UPDATED")
                            .child(
                                div()
                                    .text_color(rgb(MUTED2))
                                    .child(SharedString::from(fresh.len().to_string())),
                            ),
                    );
                for (slug, name, line, ts) in fresh {
                    let age = crate::timefmt::age_ms_since(ts, now_ms);
                    tier = tier.child(self.portfolio_row(slug, name, line, Some(age), cx));
                }
                feed = feed.child(tier);
            }

            if !rest.is_empty() {
                let open = self.standup_portfolio_open;
                let n = rest.len();
                let mut tier = div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .px(px(18.))
                    .pt(px(14.))
                    .child(
                        div()
                            .id("pf-toggle")
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(7.))
                            .cursor_pointer()
                            .text_size(px(11.5))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(MUTED2))
                            .hover(|h| h.text_color(rgb(MUTED)))
                            .child(if open { "▾" } else { "▸" })
                            .child("▦ PORTFOLIO")
                            .child(
                                div()
                                    .text_color(rgb(MUTED2))
                                    .child(SharedString::from(n.to_string())),
                            )
                            .when(!open, |h| {
                                h.child(
                                    div()
                                        .text_size(px(10.5))
                                        .font_weight(FontWeight::NORMAL)
                                        .text_color(rgb(MUTED2))
                                        .child("nothing new"),
                                )
                            })
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.standup_portfolio_open = !this.standup_portfolio_open;
                                if let Ok(store) = this.store.lock() {
                                    let _ = store.set_setting(
                                        "standup_portfolio_open",
                                        if this.standup_portfolio_open { "1" } else { "0" },
                                    );
                                }
                                cx.notify();
                            })),
                    );
                if open {
                    for (slug, name, line, _ts) in rest {
                        tier = tier.child(self.portfolio_row(slug, name, line, None, cx));
                    }
                }
                feed = feed.child(tier);
            }
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
                let mut tier = div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .px(px(18.))
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
            let (events, failed) = {
                let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
                let failed: Vec<(String, String)> = store
                    .dead_summary_jobs()
                    .into_iter()
                    .filter(|(_, _, _, _, died_ms)| *died_ms >= day_ago)
                    .map(|(_, cid, _, err, _)| (cid, err))
                    .collect();
                (store.timeline(120), failed)
            };
            let pname = |key: &str| {
                self.projects
                    .iter()
                    .find(|p| p.slug == key)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| key.rsplit(['/', ':']).next().unwrap_or(key).to_string())
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
fn deterministic_project_rollup(
    store: &Store,
    slug: &str,
    now_secs: u64,
) -> Option<cockpit::Rollup> {
    use std::collections::{HashMap, HashSet};
    let parts = store.load_tree(slug).unwrap_or_default();
    if parts.is_empty() {
        return None;
    }
    let tree = orchestrator_store::build_tree(&parts);
    let activity = store.part_activity(slug);
    // derived building from STORED link recency (dispatch/declared, <48h) — no
    // alive stamping, so the count is a deterministic snapshot.
    let mut links: HashMap<PartId, Vec<(returnchannel::LinkRole, u64)>> = HashMap::new();
    for r in store.session_parts(slug) {
        let role = returnchannel::LinkRole::from_str(&r.role);
        if role.is_declared_intent() {
            let last = r.last_touch_secs.unwrap_or(r.at_secs);
            links.entry(r.part_id).or_default().push((role, last));
        }
    }
    let building_ids: Vec<PartId> = links
        .iter()
        .filter_map(|(pid, l)| returnchannel::derived_building(l, now_secs).map(|_| *pid))
        .collect();
    let stars: Vec<PartId> = store
        .get_setting(&format!("map_stars:{slug}"))
        .map(|v| {
            v.split(',')
                .filter_map(|t| t.trim().parse::<PartId>().ok())
                .collect()
        })
        .unwrap_or_default();
    let attention: HashSet<PartId> = store
        .needs_you_flags(slug)
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();
    let blocked: HashSet<PartId> = HashSet::new();
    let ready = cockpit::next_up(&parts, &activity, &stars, &attention, &blocked, usize::MAX);
    let stale: HashSet<PartId> = parts.iter().filter(|p| p.stale).map(|p| p.id).collect();
    let anchor_counts: HashMap<PartId, usize> = HashMap::new();
    let gaps = cockpit::gap_findings(&parts, &activity, now_secs, &stale, &anchor_counts);
    Some(cockpit::rollup(
        &tree,
        &building_ids,
        &[],
        &ready,
        gaps.len(),
    ))
}

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
