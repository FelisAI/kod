use gpui::prelude::FluentBuilder;
use gpui::*;
use crate::*;


impl Orchestrator {
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
        first: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let digest = matches!(density, crate::standup_plan::Density::Digest);
        // ago_label, NOT the raw age: age_ms_since returns MILLISECONDS, so
        // interpolating it directly printed a nine-digit number where "12m ago"
        // belonged.
        let age = crate::timefmt::ago_label(crate::timefmt::age_ms_since(p.newest_ms, now_ms));
        // No `fresh` any more: every project the planner hands back is one you
        // have NOT seen, so a "read" variant of this card would be unreachable.
        let (count, hidden) = (p.total, p.hidden_lines);
        // No kind glyph. ☁/▶/■/◆ marked EVERY line — overwhelmingly ☁, since
        // summaries are the spine — so it was a column of noise that told you
        // nothing you would act on differently.
        let lines: Vec<String> = if digest {
            p.lines
                .first()
                .map(|l| termview::trim(&l.text, 90))
                .into_iter()
                .collect()
        } else {
            p.lines
                .iter()
                .map(|l| termview::trim(&l.text, 120))
                .collect()
        };

        // THE BODY IS THE ONLY THING THAT SHRINKS. Everything else on the row is
        // `flex_none`; this column carries `min_w_0` so `truncate` has somewhere
        // to go when the pane narrows.
        let mut body = div().flex_1().min_w_0().flex().flex_col().gap(px(3.)).child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(7.))
                .child(
                    div()
                        .flex_shrink()
                        .min_w_0()
                        .truncate()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_STRONG))
                        .child(SharedString::from(termview::trim(&name, 30))),
                )
                // The unread mark: a dot AFTER the name, the way every Mac list
                // marks an unread row. It used to lead the line, which put a
                // coloured pip in the position the eye reads as an icon and left
                // the name un-anchored.
                .child(
                    div()
                        .flex_none()
                        .w(px(6.))
                        .h(px(6.))
                        .rounded(px(3.))
                        .bg(rgb(ACCENT)),
                ),
        );
        for text in lines {
            body = body.child(
                div()
                    // WRAPS, deliberately — it does not truncate.
                    //
                    // gpui's `truncate()` only paints its ellipsis when the text
                    // is first MEASURED against a definite width, and with
                    // `whitespace_nowrap` it then caches that measurement
                    // (elements/text.rs: the early return keyed on wrap_width
                    // being None). A flex-sized column is measured indefinitely
                    // first, so the ellipsis never appears and the sentence is
                    // instead chopped mid-word by the group's overflow_hidden —
                    // which reads as a rendering fault, not as "there is more".
                    //
                    // Two wrapped lines is also simply the better row: it is what
                    // Mail and Messages do with a preview, and it needs no
                    // ellipsis to look finished at any width.
                    .line_clamp(2)
                    // w_full, and NO min_w_0().
                    //
                    // These two go together. min-width:0 is what lets a flex-ROW
                    // child shrink below its content; `body` is a flex COLUMN, so
                    // on its children min_w_0 removes the width floor entirely,
                    // the box collapses to zero, and truncate() renders nothing
                    // but an ellipsis — that bug has shipped here twice.
                    //
                    // But WITHOUT w_full the opposite happens: the line sizes to
                    // its content, overflows the column, and gets CLIPPED
                    // mid-word by the group's overflow_hidden — an ellipsis that
                    // never appears because truncate() had no box to work in.
                    // w_full pins it to the column, which is the width that
                    // actually shrinks.
                    .w_full()
                    // NO min_w_0() — and this is the SECOND time that mistake has
                    // shipped here. min-width:0 is what lets a flex-ROW child
                    // shrink below its content. `body` is a flex COLUMN, so on
                    // its children min_w_0 removes the width floor entirely, the
                    // box collapses to zero, and truncate() renders nothing but
                    // the ellipsis. Every line became "…". Same bug, same fix, as
                    // the settings rows.
                    .text_size(px(12.))
                    .text_color(rgb(TEXT))
                    .child(SharedString::from(text)),
            );
        }
        let ekey = p.key.clone();
        if hidden > 0 {
            // CLICKABLE. A count you cannot open is a complaint, not a control —
            // "+53 more" told you what you were missing and gave you no way to
            // see it.
            body = body.child(
                div()
                    .id(SharedString::from(format!("upd-more-{}", p.key)))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.))
                    .pt(px(2.))
                    .w_full()
                    .cursor_pointer()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED2))
                    .hover(|h| h.text_color(rgb(ACCENT)))
                    .child(icon("icons/chevron-down.svg", 10., MUTED2))
                    .child(SharedString::from(format!("{hidden} more")))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.standup_block_open.insert(ekey.clone());
                        cx.notify();
                    })),
            );
        }

        let (kopen, kread) = (p.key.clone(), p.key.clone());
        list_row(first)
            .id(SharedString::from(format!("upd-{}", p.key)))
            .hover(|h| h.bg(rgb(CARD2)))
            // The project's own badge — the same colour and initials the rail
            // uses. The Standup had no visual tie to the sidebar at all, so the
            // two halves of one window looked like two applications.
            .child(project_badge(&name, &p.key, 20.))
            .child(body)
            .child(
                row_trailing(if digest {
                    format!("{count} · {age}")
                } else {
                    format!("{count} update{} · {age}", if count == 1 { "" } else { "s" })
                })
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .child(
                            card_action(
                                SharedString::from(format!("upd-read-{}", p.key)),
                                "Mark read",
                                Some("icons/check.svg"),
                                false,
                            )
                            .on_click(cx.listener(
                                move |this: &mut Orchestrator, _: &ClickEvent, _, cx| {
                                    this.mark_project_read(&kread);
                                    cx.notify();
                                },
                            )),
                        )
                        .child(
                            card_action(
                                SharedString::from(format!("upd-open-{}", p.key)),
                                "Open",
                                Some("icons/chevron-right.svg"),
                                true,
                            )
                            .on_click(cx.listener(
                                move |this: &mut Orchestrator, _: &ClickEvent, _, cx| {
                                    this.select_project(&kopen, cx)
                                },
                            )),
                        ),
                ),
            )
    }

    /// The shape of the Standup without building it — what the rail's Standup
    /// button reports. Same `standup_bucket` the tiers use, so the two cannot
    /// disagree about what is waiting.
    pub(crate) fn standup_counts(&self) -> StandupCounts {
        let mut c = StandupCounts::default();
        if !self.scanned {
            return c;
        }
        for p in &self.projects {
            for info in self.cached_infos(&p.slug) {
                if !info.alive {
                    continue;
                }
                match standup_bucket(
                    info.usage_limit.as_ref().is_some_and(|u| u.hit),
                    info.phase,
                    self.session_unreviewed(info.id),
                ) {
                    Bucket::Blocked => c.blocked += 1,
                    Bucket::Needs => c.needs += 1,
                    Bucket::Working => c.working += 1,
                    Bucket::Ready => c.ready += 1,
                    Bucket::Idle => c.idle += 1,
                }
            }
        }
        c
    }

    /// One ⏎ card: a session that finished a turn while you were elsewhere, what
    /// it said, and how long it has been waiting.
    ///
    /// TWO LINES, and that is a width fix rather than a style choice. On one line
    /// this had a glyph, a 150px name, a 110px project, the message, a clock and
    /// two buttons all competing; the message is the only flexible one, so it was
    /// the only thing that could give — and on a narrower window it collapsed to
    /// nothing while six fixed things sat there jammed together. The sentence you
    /// actually read was the first casualty of every resize.
    ///
    /// So: who/where/when on top, what it said and what you can do about it
    /// below, where the message has a whole line to itself and only the buttons
    /// to yield to.
    fn ready_row(
        &self,
        name: String,
        info: &SessionInfo,
        now_ms: u64,
        first: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The wait is the POINT of this card, so it holds the trailing column
        // rather than being a phase word every row would share.
        let waited = self
            .session_ready_since(info.id)
            .map(|t| crate::timefmt::ago_label(now_ms.saturating_sub(t)))
            .unwrap_or_default();
        let (jslug, jid) = (info.project_slug.clone(), info.id);
        let slug = info.project_slug.clone();
        list_row(first)
            .id(SharedString::from(format!("ready-{}", info.id.0)))
            .hover(|h| h.bg(rgb(CARD2)))
            .child(project_badge(&name, &slug, 20.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(7.))
                            .child(
                                div()
                                    .flex_shrink()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(TEXT_STRONG))
                                    .child(SharedString::from(termview::session_label(info))),
                            )
                            // The state icon rides WITH the title, not in a
                            // column of its own: one glyph per row in a fixed
                            // gutter was a stripe of repeated symbols down the
                            // left edge, which is exactly the noise the badge is
                            // better at carrying.
                            .child(icon("icons/reply.svg", 12., ACCENT)),
                    )
                    .child(
                        div()
                            .w_full()
                            // wraps to two lines rather than clipping — see the
                            // note in `update_block`.
                            .line_clamp(2)
                            .text_size(px(12.))
                            .text_color(rgb(TEXT))
                            .child(SharedString::from(termview::trim(
                                info.last_message.trim(),
                                200,
                            ))),
                    ),
            )
            .child(
                row_trailing(format!("ready {waited}")).child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .child(
                            card_action(
                                SharedString::from(format!("ready-dismiss-{}", info.id.0)),
                                "Dismiss",
                                Some("icons/check.svg"),
                                false,
                            )
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.dismiss_ready(jid, cx)
                            })),
                        )
                        .child(
                            card_action(
                                SharedString::from(format!("ready-open-{}", info.id.0)),
                                "Open",
                                Some("icons/chevron-right.svg"),
                                true,
                            )
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.focus_session(&jslug, jid, window, cx)
                            })),
                        ),
                ),
            )
            .into_any_element()
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
        // Finished a turn while you were elsewhere and not opened since — your
        // move. Pulled OUT of `idle` rather than tagged inside it, so a session
        // still lands in exactly one tier.
        let mut ready: Vec<(String, SessionInfo)> = Vec::new();
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
                    // `live_n` above already counted it, so the "N agents live"
                    // headline is unchanged whichever tier claims it.
                    match standup_bucket(
                        info.usage_limit.as_ref().is_some_and(|u| u.hit),
                        info.phase,
                        self.session_unreviewed(info.id),
                    ) {
                        Bucket::Blocked => blocked.push((p.name.clone(), info.clone())),
                        Bucket::Needs => {
                            needs.push((i, p.name.clone(), p.slug.clone(), info.clone()))
                        }
                        Bucket::Working => working.push((p.name.clone(), info.clone())),
                        Bucket::Ready => ready.push((p.name.clone(), info.clone())),
                        Bucket::Idle => idle.push((p.name.clone(), info.clone())),
                    }
                }
            }
        }
        // oldest ask first — the one you've been ignoring longest leads (#4).
        needs.sort_by_key(|(_, _, _, info)| info.phase_since_ms);
        // LONGEST WAIT FIRST, and that ordering is the whole noise strategy.
        //
        // Measured over 14 days of real history: 47% of turn-ends are continued
        // within five minutes, 19% sit past half an hour, and NO session behaves
        // like a self-continuing loop. So there is no class of turn-end to filter
        // out — the interesting variable is not whether a finished turn is yours
        // to answer, it is how long it has gone unanswered. Sorting by that lets
        // the tier answer the question itself: what you are about to open anyway
        // sinks to the bottom, what you have forgotten rises.
        ready.sort_by_key(|(_, info)| self.session_ready_since(info.id).unwrap_or(u64::MAX));
        let need_n = needs.len();
        let ready_n = ready.len();
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
            // NO GLYPHS. "⚠ 2 need you · ⏎ 4 ready · ● 3 working" put four
            // different dingbats in one sentence, where each one either repeats
            // the word beside it or has to be learned. The words already say it.
            let mut parts = Vec::new();
            if need_n > 0 {
                parts.push(format!("{need_n} need you"));
            }
            if ready_n > 0 {
                parts.push(format!("{ready_n} ready for you"));
            }
            if work_n > 0 {
                parts.push(format!("{work_n} working"));
            }
            if idle_n > 0 {
                parts.push(format!("{idle_n} idle"));
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
                        .child(icon("icons/blocked.svg", 12., 0xE68A8A))
                        .child("BLOCKED")
                        .child(div().text_color(rgb(MUTED2)).child(SharedString::from(blocked.len().to_string()))),
                );
            let mut group = list_group();
            for (bi, (name, info)) in blocked.into_iter().enumerate() {
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
                // …and what Kod will DO about it. Without this the row says when the
                // window reopens and nothing about whether anything will happen then,
                // so a feature that exists to handle exactly this moment is invisible
                // at exactly this moment — which is how its owner concluded it was
                // never built.
                let promise = resume_promise(self.auto_continue, u.reset_at_unix.is_some());
                let (jslug, jid) = (info.project_slug.clone(), info.id);
                let slug = info.project_slug.clone();
                group = group.child(
                    list_row(bi == 0)
                        .id(SharedString::from(format!("blocked-{}", info.id.0)))
                        .hover(|h| h.bg(rgb(0x241A1A)))
                        .child(project_badge(&name, &slug, 20.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(3.))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap(px(7.))
                                        .child(
                                            div()
                                                .flex_shrink()
                                                .min_w_0()
                                                .truncate()
                                                .text_size(px(13.))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(rgb(TEXT_STRONG))
                                                .child(SharedString::from(termview::session_label(
                                                    &info,
                                                ))),
                                        )
                                        .child(icon("icons/blocked.svg", 12., 0xE68A8A)),
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .truncate()
                                        .text_size(px(12.))
                                        .text_color(rgb(0xE0A0A0))
                                        .child(SharedString::from(detail)),
                                )
                                // The PROMISE on its own line. It used to be
                                // glued to the reset time with a "·", which made
                                // the one sentence that says whether Kod will act
                                // look like more timestamp.
                                .child(
                                    div()
                                        .w_full()
                                        .truncate()
                                        .text_size(px(11.))
                                        .text_color(rgb(MUTED2))
                                        .child(SharedString::from(promise.to_string())),
                                ),
                        )
                        .child(
                            row_trailing(String::new()).child(
                                card_action(
                                    SharedString::from(format!("blocked-open-{}", info.id.0)),
                                    "Open",
                                    Some("icons/chevron-right.svg"),
                                    true,
                                )
                                .on_click(cx.listener(
                                    move |this, _: &ClickEvent, window, cx| {
                                        this.focus_session(&jslug, jid, window, cx)
                                    },
                                )),
                            ),
                        ),
                );
            }
            tier = tier.child(group);
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
                    .child(icon("icons/warning.svg", 12., AMBER))
                    .child("NEEDS YOU")
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
        // The cli-session ids waiting on you. ▲ WHAT HAPPENED drops their
        // summaries below, so a finished turn is reported ONCE — on the row that
        // can act on it.
        let ready_sess: std::collections::HashSet<String> = ready
            .iter()
            .filter_map(|(_, i)| i.cli_session_id.clone())
            .collect();
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
            store
                .timeline(120)
                .into_iter()
                // A session sitting in ⏎ READY already reports its own last turn,
                // on a row that opens it. Reporting it again here as project news
                // is the same fact twice, one tier apart, phrased differently —
                // which is what made the two tiers read as duplicates. Only its
                // SUMMARIES are dropped: a decision or a map change from the same
                // session is genuinely other news and stays.
                .filter(|e| {
                    !(e.kind == orchestrator_store::TimelineKind::Summary
                        && ready_sess.contains(&e.sess))
                })
                .collect()
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
            // BOTH halves are now per project, and that is the point. Read-ness
            // always was (`proj_seen_ms`, #50, which is also what bolds an unread
            // title in the rail, so the two surfaces cannot disagree). The reach
            // back was NOT: one global `standup_seen_ms` floor was applied to
            // every project, so glancing at the Standup clamped it to 48 hours and
            // a project never opened, whose only activity was three days ago,
            // vanished — unread, in a tier that claims to show what you have not
            // seen.
            let floor_for = |k: &str| {
                crate::standup_plan::project_floor_ms(self.project_seen_ms(k), now_ms)
            };
            let is_fresh = |k: &str| self.project_unread(k);
            let is_expanded = |k: &str| self.standup_block_open.contains(k);
            let plan = crate::standup_plan::plan_updates(
                &timeline,
                &is_fresh,
                &is_expanded,
                &floor_for,
                self.standup_updates_all,
            );
            // RENDERED WHEN EMPTY TOO, so long as there is a "last looked" to
            // measure from. Being caught up is the answer this tier exists to
            // give, and an all-clear you can read in one line is worth more than
            // the tier silently vanishing — which is indistinguishable from the
            // standup being broken. Before a first check there is nothing
            // truthful to say, so it stays away.
            if ready_n > 0 || !plan.is_empty() || self.standup_divider_ms > 0 {
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
                            .child(icon("icons/feed.svg", 12., ACCENT))
                            .child("WHAT HAPPENED")
                            // Lead with the COUNT, not the window. "what
                            // happened" means "since I last looked" to the
                            // reader; the window is only how far back the planner
                            // reaches, and naming it here answered a question
                            // nobody asked.
                            .child({
                                let seen = crate::timefmt::ago_label(
                                    crate::timefmt::age_ms_since(self.standup_divider_ms, now_ms),
                                );
                                let n = plan.projects.len();
                                // Ready leads, because it is the half you can do
                                // something about.
                                let mut bits: Vec<String> = Vec::new();
                                if ready_n > 0 {
                                    bits.push(format!("{ready_n} ready for you"));
                                }
                                if self.standup_divider_ms == 0 {
                                    bits.push("first look".to_string());
                                } else if n > 0 {
                                    bits.push(format!("{n} new since you last looked, {seen}"));
                                } else if ready_n == 0 {
                                    bits.push(format!("nothing new since you last looked, {seen}"));
                                }
                                div()
                                    .font_weight(FontWeight::NORMAL)
                                    .text_size(px(10.5))
                                    .text_color(rgb(if ready_n > 0 || n > 0 {
                                        ACCENT
                                    } else {
                                        MUTED2
                                    }))
                                    .child(SharedString::from(format!("· {}", bits.join(" · "))))
                            }),
                    );
                // ONE GROUP, TWO SECTIONS.
                //
                // ⏎ READY used to be its own tier above this one, which read as a
                // separate feature rather than as what it is: the part of "what
                // happened" you can act on. Same group, split once — the sessions
                // waiting on you, then everything else that changed and wants
                // nothing.
                //
                // The sub-headings appear ONLY when both halves have content. A
                // heading over the only thing on screen names nothing, and this
                // screen has enough chrome.
                //
                // TWO GROUPED LISTS, not two runs of free-floating cards. Each
                // half is one rounded surface with hairlines BETWEEN its rows,
                // which is what makes a set of rows read as a list rather than as
                // a pile of boxes — and it is the whole reason the sub-headings
                // can be quiet now: the grouping itself does the dividing that
                // two loud labels were being asked to do.
                let split = ready_n > 0 && !plan.is_empty();
                if ready_n > 0 {
                    if split {
                        tier = tier.child(sub_heading("READY FOR YOU", ACCENT));
                    }
                    let ready_now = crate::render_sidebar::wall_now_ms();
                    let mut group = list_group();
                    for (i, (name, info)) in ready.into_iter().enumerate() {
                        group = group.child(self.ready_row(name, &info, ready_now, i == 0, cx));
                    }
                    tier = tier.child(group);
                    if split {
                        tier = tier.child(sub_heading("NOTHING TO ACT ON", MUTED2));
                    }
                }
                // No NEW bar: the header already says "{n} new since you last
                // looked", and a heading that repeats the line above it is chrome.
                if !plan.is_empty() || plan.hidden_projects > 0 {
                    let mut group = list_group();
                    for (i, pp) in plan.projects.iter().enumerate() {
                        group = group.child(self.update_block(
                            pp,
                            pname(&pp.key),
                            plan.density,
                            now_ms,
                            i == 0,
                            cx,
                        ));
                    }
                    if plan.hidden_projects > 0 {
                        // A FOOTER ROW of the same list, not a loose line under
                        // it: "show the rest" belongs to the list it extends, and
                        // as a bare line it read as an unrelated caption.
                        let n = plan.hidden_projects;
                        group = group.child(
                            list_row(plan.projects.is_empty())
                                .id("upd-show-all")
                                .py(px(8.))
                                .items_center()
                                .gap(px(6.))
                                .cursor_pointer()
                                .text_size(px(11.5))
                                .text_color(rgb(MUTED2))
                                .hover(|h| h.bg(rgb(CARD2)).text_color(rgb(ACCENT)))
                                .child(icon("icons/chevron-down.svg", 11., MUTED2))
                                .child(SharedString::from(format!(
                                    "Show {n} more project{}",
                                    if n == 1 { "" } else { "s" }
                                )))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.standup_updates_all = true;
                                    cx.notify();
                                })),
                        );
                    }
                    tier = tier.child(group);
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
                        .child(icon("icons/working.svg", 12., MUTED))
                        .child("LIVE")
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
                        .child(icon(
                            if open {
                                "icons/chevron-down.svg"
                            } else {
                                "icons/chevron-right.svg"
                            },
                            11.,
                            MUTED2,
                        ))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.standup_live_open = !this.standup_live_open;
                            cx.notify();
                        })),
                );
            if open {
                // COMPACT ON PURPOSE. These rows are the ambient half — nothing
                // here wants anything — so they get one line each inside a single
                // group, rather than the two-line treatment the actionable tiers
                // earn. Same grammar, less of it.
                let mut group = list_group();
                for (li, (busy, (name, info))) in working
                    .into_iter()
                    .map(|r| (true, r))
                    .chain(idle.into_iter().map(|r| (false, r)))
                    .enumerate()
                {
                    let gcol = if busy { GREEN } else { MUTED2 };
                    let doing = {
                        let m = info.last_message.trim();
                        if m.is_empty() {
                            (if busy { "working…" } else { "idle" }).to_string()
                        } else {
                            termview::trim(m, 80)
                        }
                    };
                    let (jslug, jid) = (info.project_slug.clone(), info.id);
                    let slug = info.project_slug.clone();
                    group = group.child(
                        list_row(li == 0)
                            .id(SharedString::from(format!("live-{}", jid.0)))
                            .items_center()
                            .py(px(7.))
                            .gap(px(8.))
                            .cursor_pointer()
                            .hover(|h| h.bg(rgb(CARD2)))
                            .child(project_badge(&name, &slug, 16.))
                            .child(dot(gcol))
                            .child(
                                div()
                                    .flex_shrink()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.5))
                                    .text_color(rgb(TEXT_STRONG))
                                    .child(SharedString::from(termview::session_label(&info))),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.))
                                    .text_color(rgb(MUTED))
                                    .child(SharedString::from(doing)),
                            )
                            .when_some(info.usage_limit.clone(), |c, u| {
                                c.child(crate::render_sidebar::usage_chip(&u))
                            })
                            .child(icon("icons/chevron-right.svg", 11., MUTED2))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.focus_session(&jslug, jid, window, cx)
                            })),
                    );
                }
                tier = tier.child(group);
            }
            feed = feed.child(tier);
        }
        // ── ▲ MAP UPDATES — the morning checkmarks (docs/011 slice 3): every
        // pending proposal op across projects (sessions, break-downs, drift),
        // resolvable inline; 'map ▸' jumps to the node for context.
        //
        // GATED. This tier renders proposals the map made, so with the feature off
        // it has no business on screen — and it was drawing anyway, because the
        // gate was applied to everything that CREATES proposals (summaries.rs,
        // agentic.rs, spawn.rs) and to every other surface that shows them, but
        // never here. A build without `--features map` therefore still showed a
        // MAP UPDATES section to anyone whose store held proposals from when the
        // feature was on, which is every existing install.
        if crate::features::MAP_ENABLED {
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
                                    DiffOp::AddDecision {
                                        part: PartRef::Id(id),
                                        ..
                                    } => Some(*id),
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
            // The failures AND whether this exact set has already been waved
            // off, read under ONE lock.
            let (failed, fail_dismissed): (Vec<(String, String)>, Option<String>) = {
                let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
                let f: Vec<(String, String)> = store
                    .dead_summary_jobs()
                    .into_iter()
                    .filter(|(_, _, _, _, died_ms)| *died_ms >= day_ago)
                    .map(|(_, cid, _, err, _)| (cid, err))
                    .collect();
                let d = store.get_setting(FAIL_DISMISSED_KEY);
                (f, d)
            };
            // A SIGNATURE, not a boolean. Dismissing has to mean "I have seen
            // THESE failures", not "never warn me again": keyed on the set of
            // sessions, a new failure tomorrow brings the notice back on its own,
            // while the ones he has already read stay gone — including across a
            // restart, which an in-memory flag would not survive.
            let fail_sig = fail_signature(&failed);
            let fail_seen = fail_dismissed.as_deref() == Some(fail_sig.as_str());
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
            if !failed.is_empty() && !fail_seen {
                // ONE LINE, AND DISMISSIBLE. It was two permanent lines — the
                // sentence and the raw error under it — with no way to clear
                // them, so a summarizer that failed once sat on the Standup
                // taking space from the sessions the screen is FOR. The reason
                // still ships, folded into the same line after an em dash, which
                // is all the room it ever needed.
                let n = failed.len();
                let (_, err) = &failed[n - 1];
                let reason: String = err.chars().take(90).collect();
                let sig = fail_sig.clone();
                thread = thread.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .px(px(8.))
                        .py(px(4.))
                        .child(icon("icons/warning.svg", 11., AMBER))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .line_clamp(1)
                                .text_size(px(11.5))
                                .text_color(rgb(AMBER))
                                .child(SharedString::from(format!(
                                    "{n} session summar{} failed — retrying on a backoff · {reason}",
                                    if n == 1 { "y" } else { "ies" }
                                ))),
                        )
                        .child(
                            div()
                                .id("fail-dismiss")
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .w(px(20.))
                                .h(px(20.))
                                .rounded(px(5.))
                                .cursor_pointer()
                                .hover(|h| h.bg(rgb(CARD2)))
                                .child(icon("icons/close.svg", 10., MUTED2))
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    let _ = this
                                        .store
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .set_setting(FAIL_DISMISSED_KEY, &sig);
                                    cx.notify();
                                })),
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
            // Fold consecutive same-project rows into runs, so a burst stops
            // repeating "atlas · " down twelve lines. The timeline is NOT
            // reordered or collapsed — a run only ever covers rows that were
            // already adjacent, and group_runs breaks on a day boundary or the
            // divider so a heading can never straddle one.
            let runs = crate::standup_plan::group_runs(&events, &|ts| local_day(ts), divider_ms);
            let mut head_len: Vec<usize> = vec![0; events.len()];
            let mut in_run: Vec<bool> = vec![false; events.len()];
            // EVERY run gets a heading, including a run of one. Two styles —
            // inline for singles, heading for bursts — read as a rendering bug
            // rather than a distinction, because the reader has no idea why one
            // row looks different from the row above it.
            for r in &runs {
                head_len[r.at] = r.len;
                for k in r.at..r.at + r.len {
                    in_run[k] = true;
                }
            }
            let mut last_day: i64 = i64::MIN;
            let mut divider_done = divider_ms == 0;
            for (evi, ev) in events.iter().enumerate() {
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
                // AFTER the day header and the divider, so a heading always sits
                // under the rules that bound it rather than above them.
                if head_len[evi] > 0 {
                    let n = head_len[evi];
                    thread = thread.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .pl(px(53.))
                            .mt(px(7.))
                            .when(old, |d| d.opacity(0.45))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(MUTED))
                                    .child(SharedString::from(pname(&ev.project_key))),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED2))
                                    .child(SharedString::from(format!(
                                        "{n} update{}",
                                        if n == 1 { "" } else { "s" }
                                    ))),
                            ),
                    );
                }
                let key = ev.ts_ms ^ ((ev.kind.clone() as u64) << 1) ^ (ev.count as u64);
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
                            SharedString::from(if in_run[evi] {
                                line.clone()
                            } else {
                                format!("{} · {}", pname(&ev.project_key), line)
                            }),
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
                    // No kind glyph. ☁ sat before EVERY row — summaries are the
                    // spine, so it marked almost everything and distinguished
                    // almost nothing. The time and the project heading carry the
                    // scan; a column of weather was noise.
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
        let bslug = slug.clone();
        // DELIBERATELY NOT a list_row. Every other tier is a grouped list
        // because its rows are peers you scan; this one is the interruption, and
        // it earns a card of its own — amber ground, a full-size question, and
        // one button. Flattening it into the same list would have made "an agent
        // is stopped, waiting on you" look exactly like "a project has news".
        div()
            .id(SharedString::from(format!("need-{eid}")))
            .flex()
            .flex_col()
            .gap(px(9.))
            .p(px(13.))
            .rounded(px(10.))
            .bg(rgb(AMBER_INK))
            .border_1()
            .border_color(rgb(AMBER_HAIR))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(9.))
                    .child(project_badge(&name, &bslug, 20.))
                    .child(
                        div()
                            .flex_shrink()
                            .min_w_0()
                            .truncate()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT_STRONG))
                            .child(SharedString::from(termview::session_label(&info))),
                    )
                    .child(icon("icons/warning.svg", 13., AMBER))
                    .child(div().flex_1().min_w_0())
                    .child(
                        div()
                            .flex_none()
                            .whitespace_nowrap()
                            .text_size(px(11.))
                            .text_color(rgb(AMBER))
                            .child(SharedString::from(waiting)),
                    ),
            )
            // THE ASK, at reading size. This is the only text on the Standup the
            // user has to actually answer, so it is the only text allowed to be
            // bigger than the row titles around it.
            .child(
                div()
                    .text_size(px(13.5))
                    .text_color(rgb(TEXT))
                    .child(SharedString::from(
                        ask.chars().take(170).collect::<String>(),
                    )),
            )
            .child(
                div().flex().flex_row().items_center().gap(px(8.)).child(
                    card_action(
                        SharedString::from(format!("term-{eid}")),
                        "Open in terminal",
                        Some("icons/chevron-right.svg"),
                        true,
                    )
                    .h(px(30.))
                    .px(px(13.))
                    .text_size(px(12.5))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.focus_session(&slug, id, window, cx)
                    })),
                ),
            )
            .into_any_element()
    }

}


/// Does ⛔ BLOCKED claim this session, or does it fall through to the phase tiers?
///
/// Pure because this single rule is the one that used to make Standup disagree
/// with every other needs-you surface in the app — a comment could not prove it
/// no longer does, and a test can.
/// What Kod will do about a blocked session, in a few words, for the row that
/// reports the block.
///
/// Three states and they are genuinely different actions for the reader: turn a
/// switch on, wait, or go and do it yourself. Saying nothing — which is what this
/// row did — collapses all three into "you are blocked", and the one thing the
/// user cannot discover from there is that an auto-resume exists at all.
///
/// `has_reset_instant` is `reset_at_unix.is_some()`, and it is the SAME condition
/// `session::ac_decide` arms on: a limit whose banner carried no resolvable time
/// can never be resumed automatically, however the switch is set. Reading it off
/// the same fact is what keeps this sentence from promising something the gate
/// will then refuse.
/// The identity of a set of failed summary jobs, for the dismiss ledger.
///
/// SORTED, and that is the whole point. `dead_summary_jobs()` makes no ordering
/// promise, so a signature built in iteration order would differ between two
/// renders of the SAME failures — and the notice he just dismissed would come
/// straight back, which is indistinguishable from the dismiss button not
/// working.
pub(crate) fn fail_signature(failed: &[(String, String)]) -> String {
    let mut ids: Vec<&str> = failed.iter().map(|(c, _)| c.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    ids.join(",")
}

/// Which set of failed summary jobs the user has already waved off. A SET, not
/// a flag — see the dismiss handler.
const FAIL_DISMISSED_KEY: &str = "standup_fail_dismissed";

pub(crate) fn resume_promise(auto_on: bool, has_reset_instant: bool) -> &'static str {
    match (auto_on, has_reset_instant) {
        (false, _) => "auto-continue off",
        (true, true) => "Kod will resume it",
        (true, false) => "no reset time — Kod can't resume it",
    }
}

/// Which Standup tier a live session belongs to. EXACTLY ONE, always.
///
/// A free function because two surfaces now ask: the Standup itself, to build
/// its tiers, and the rail's Standup button, to say what is waiting without
/// opening it. This file already carries a scar from that shape — the comment on
/// the ⛔ branch records the day the Dock badge, the toast, the notification and
/// the sidebar dot each decided for themselves what "needs you" meant, and
/// "Dock-badge 1" and "nothing needs you" were reachable in the same instant. So
/// there is one definition and both callers use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bucket {
    /// a hard usage-limit hit with no ask on top of it — wait it out.
    Blocked,
    /// sitting on a real permission prompt; cannot proceed without you.
    Needs,
    /// working.
    Working,
    /// finished a turn you have not opened — your move.
    Ready,
    /// alive and quiet, wanting nothing.
    Idle,
}

pub(crate) fn standup_bucket(
    limit_hit: bool,
    phase: orchestrator_host::Phase,
    unreviewed: bool,
) -> Bucket {
    if blocked_tier_claims(limit_hit, phase == orchestrator_host::Phase::AwaitingDecision) {
        return Bucket::Blocked;
    }
    match phase {
        orchestrator_host::Phase::AwaitingDecision => Bucket::Needs,
        orchestrator_host::Phase::Busy => Bucket::Working,
        _ if unreviewed => Bucket::Ready,
        _ => Bucket::Idle,
    }
}

/// How many live sessions sit in each tier, for a caller that wants the shape of
/// the Standup without building it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StandupCounts {
    pub blocked: usize,
    pub needs: usize,
    pub working: usize,
    pub ready: usize,
    pub idle: usize,
}

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

/// A real button on a card: a chip with a background, a border and a hit target
/// you can actually land on.
///
/// The first version was 10.5px text with 2px of padding and no box — a
/// hyperlink pretending to be a control. It was hard to see and hard to hit,
/// which for the two actions that let you clear this screen without opening
/// anything is the whole value gone.
///
/// 26px tall and 12px of horizontal padding: comfortably clickable with a mouse
/// without turning a dense row into a toolbar. `primary` tints the one that
/// carries the row's main verb; the other stays quiet so the pair reads as an
/// action and its alternative rather than as two equal choices.
/// The right-hand column of a list row: the timestamp, and the controls under
/// it.
///
/// Stacked rather than strung out along the title line, and that is the fix for
/// a bug that shipped twice: a row whose name, project, count, age and two
/// buttons were all `flex_none` on ONE line has no give, so a narrow window
/// pushed the right-hand side off the card instead of truncating anything. Here
/// exactly one thing (the body text) is allowed to shrink, and this column is
/// the only fixed-width element on the row.
fn row_trailing(meta: String) -> Div {
    div()
        .flex_none()
        .flex()
        .flex_col()
        .items_end()
        .gap(px(6.))
        // An empty meta must not reserve its line: a blank text node still
        // occupies a row in the column and pushed the controls down by half a
        // line on exactly the rows that have no timestamp to show.
        .when(!meta.is_empty(), |d| {
            d.child(
                div()
                    .whitespace_nowrap()
                    .text_size(px(10.5))
                    .text_color(rgb(MUTED2))
                    .child(SharedString::from(meta)),
            )
        })
}

/// A section label INSIDE a tier — lighter than a tier heading, because it
/// divides one group rather than announcing another.
fn sub_heading(text: &'static str, color: u32) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .pt(px(4.))
        .text_size(px(10.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(color))
        .child(text)
        .child(div().flex_1().h(px(1.)).bg(rgb(HAIR_SOFT)))
}

/// The standup's one grey line (pure — no store, no window).
#[cfg(test)]
mod tests {
    use super::{resume_promise, standup_bucket, Bucket};

    /// ONE definition, two surfaces. This file already carries the scar: the ⛔
    /// branch records the day the Dock badge, the toast, the notification and the
    /// sidebar dot each decided for themselves what "needs you" meant, and
    /// "Dock-badge 1" and "nothing needs you" were reachable in the same instant.
    /// The dismiss has to survive a reshuffle, and must NOT survive a new
    /// failure.
    #[test]
    fn dismissing_the_summary_warning_sticks_until_something_new_fails() {
        let f = |v: &[(&str, &str)]| -> Vec<(String, String)> {
            v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
        };
        let a = super::fail_signature(&f(&[("s2", "boom"), ("s1", "boom")]));
        let b = super::fail_signature(&f(&[("s1", "other text"), ("s2", "boom")]));
        assert_eq!(a, b, "same sessions in a different order must dismiss once");

        let c = super::fail_signature(&f(&[("s1", "x"), ("s2", "x"), ("s3", "x")]));
        assert_ne!(a, c, "a NEW failing session must bring the notice back");

        assert_eq!(super::fail_signature(&[]), "");
        // A duplicate row for one session must not read as a second failure.
        assert_eq!(
            super::fail_signature(&f(&[("s1", "a"), ("s1", "b")])),
            super::fail_signature(&f(&[("s1", "a")]))
        );
    }

    #[test]
    fn every_live_session_lands_in_exactly_one_tier() {
        use orchestrator_host::Phase;
        let b = standup_bucket;

        // A hard limit hit waits it out — UNLESS there is a real ask on top of
        // it, which you can clear in seconds and which therefore wins.
        assert_eq!(b(true, Phase::Idle, false), Bucket::Blocked);
        assert_eq!(b(true, Phase::Busy, false), Bucket::Blocked);
        assert_eq!(b(true, Phase::AwaitingDecision, false), Bucket::Needs);

        // Blocked outranks ready: a capped session is not your move.
        assert_eq!(b(true, Phase::Idle, true), Bucket::Blocked);
        // …and so does working. A session mid-turn is not waiting on you, even
        // if it finished an earlier one — that is the retraction the ledger does,
        // asserted here so the two cannot drift apart.
        assert_eq!(b(false, Phase::Busy, true), Bucket::Working);

        assert_eq!(b(false, Phase::AwaitingDecision, false), Bucket::Needs);
        assert_eq!(b(false, Phase::Idle, true), Bucket::Ready);
        assert_eq!(b(false, Phase::Idle, false), Bucket::Idle);
        // Spawning is not idle-with-nothing-to-say, but it wants nothing either.
        assert_eq!(b(false, Phase::Spawning, false), Bucket::Idle);
    }

    /// A blocked row that does not say what will happen is why its owner believed
    /// auto-continue had never been built: the feature's whole job is this moment,
    /// and this moment said nothing about it.
    #[test]
    fn a_blocked_row_says_which_of_the_three_things_will_happen() {
        // Off: the switch is the news, because it is the only one the user acts on.
        assert_eq!(resume_promise(false, true), "auto-continue off");
        assert_eq!(resume_promise(false, false), "auto-continue off");
        // On, with a resolvable instant: the one case where waiting is correct.
        assert_eq!(resume_promise(true, true), "Kod will resume it");
        // On, but the banner carried no time. `ac_decide` arms only on
        // `reset_at.is_some()`, so promising a resume here would be a lie the gate
        // then refuses — and the user would wait for something that never comes.
        assert_eq!(resume_promise(true, false), "no reset time — Kod can't resume it");
        // Three states, three sentences: none may collapse into another.
        let all = [
            resume_promise(false, true),
            resume_promise(true, true),
            resume_promise(true, false),
        ];
        assert_eq!(
            all.iter().collect::<std::collections::HashSet<_>>().len(),
            3,
            "two of the three read the same, so one of them is unactionable"
        );
    }

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
