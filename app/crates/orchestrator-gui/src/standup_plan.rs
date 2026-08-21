//! The Standup "▲ WHAT HAPPENED" planner.
//!
//! Every density decision the tier makes lives here as a pure function over
//! timeline events: which projects report, how they split around your last
//! check, whether they render as blocks or as one-line digests, and what gets
//! held back behind "+N more". No gpui, no store, no clock — so all of it is
//! unit-tested with no `App`, no window and nothing spawned.
//!
//! WHY A PLANNER AND NOT INLINE CONDITIONS: the old tiers grew without limit
//! because "how much of this do we show?" was never a decision anyone wrote
//! down — it was the absence of one. Thirty sessions rendered thirty rows.
//! Naming the thresholds here makes them arguable, testable, and changeable in
//! one place.
//!
//! THE INVARIANT THAT MATTERS: height grows with PROJECTS, never with sessions
//! or events. A project running nine sessions and emitting forty events is one
//! block, capped.

use orchestrator_store::{TimelineEvent, TimelineKind};

/// Above this many reporting projects, blocks collapse to one line each.
///
/// MEASURED, not guessed: across 47 days of real history the busiest day ever
/// had 9 projects report and a typical busy day has 2-4. Six full blocks is
/// about one screen, so digest mode is the safety valve for the rare 7+ day
/// rather than the normal case.
pub(crate) const BLOCK_MAX_PROJECTS: usize = 6;
/// Event lines inside one block before the rest become "+N more".
pub(crate) const LINES_PER_BLOCK: usize = 3;
/// Above this many reporting projects the tail collapses entirely. Set just
/// past the observed ceiling (9) so it is a backstop, not a routine amputation.
pub(crate) const PROJECT_CAP: usize = 10;
/// How many survive that collapse.
pub(crate) const PROJECTS_WHEN_CAPPED: usize = 8;

/// Always show at least this much, even if you checked a minute ago — a
/// standup that hides this morning's work because you glanced at 9am is
/// useless.
pub(crate) const MIN_WINDOW_MS: u64 = 48 * 3600 * 1000;
/// Never reach back further than this, however long you have been away. Past a
/// fortnight it stops being a standup and becomes an archive.
pub(crate) const MAX_WINDOW_MS: u64 = 14 * 24 * 3600 * 1000;

/// The oldest event ▲ WHAT HAPPENED will show.
///
/// WHY A FLOOR AT ALL: the timeline is capped by COUNT (120 rows), not by time,
/// and 278 summaries across 47 days of real history means those 120 rows reach
/// back about three weeks. Without a floor the tier would file a project that
/// last reported three weeks ago under "EARLIER", next to this morning.
pub(crate) fn update_floor_ms(seen_ms: u64, now_ms: u64) -> u64 {
    let at_least = now_ms.saturating_sub(MIN_WINDOW_MS);
    let at_most = now_ms.saturating_sub(MAX_WINDOW_MS);
    // seen_ms == 0 is the first-ever visit: fall back to the minimum window
    // rather than showing everything the store has ever held.
    let want = if seen_ms == 0 { at_least } else { seen_ms.min(at_least) };
    want.max(at_most)
}

/// What the tier is showing, in words, so the window is never a guess.
///
/// The window rule is not complicated, but it IS invisible — and an unlabelled
/// "what happened" leaves you unable to tell whether you are looking at the last
/// hour or the last fortnight. Every branch of `update_floor_ms` gets a phrase.
pub(crate) fn window_label(seen_ms: u64, now_ms: u64) -> String {
    let floor = update_floor_ms(seen_ms, now_ms);
    if floor <= now_ms.saturating_sub(MAX_WINDOW_MS) {
        return "the last 14 days".to_string();
    }
    // Ask which rule WON, not how it compares to the stamp: when you have been
    // away five days the floor EQUALS the stamp, so `floor >= seen_ms` is true
    // there too and would mislabel it as the 48h minimum.
    if floor >= now_ms.saturating_sub(MIN_WINDOW_MS) {
        return "the last 48 hours".to_string();
    }
    format!(
        "since your last check, {}",
        crate::timefmt::ago_label(now_ms.saturating_sub(seen_ms))
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Density {
    /// Project header + up to `LINES_PER_BLOCK` event lines.
    Blocks,
    /// Project header + its single newest line, on one row.
    Digest,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlannedEvent {
    pub kind: TimelineKind,
    pub text: String,
    pub ts_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlannedProject {
    pub key: String,
    /// Newest event timestamp — what the tier sorts on and what dates the row.
    pub newest_ms: u64,
    /// Every event this project has in the window, before capping.
    pub total: usize,
    pub lines: Vec<PlannedEvent>,
    pub hidden_lines: usize,
    /// Its newest event lands after your last check.
    pub fresh: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UpdatePlan {
    /// Projects whose newest event is newer than the last-check stamp.
    pub fresh: Vec<PlannedProject>,
    pub earlier: Vec<PlannedProject>,
    pub density: Density,
    /// Projects dropped by the cap — the "+N more projects" footer.
    pub hidden_projects: usize,
    /// Reporting projects before any capping.
    pub reporting: usize,
}

impl UpdatePlan {
    pub(crate) fn is_empty(&self) -> bool {
        self.fresh.is_empty() && self.earlier.is_empty()
    }
}

/// Build the tier from the raw timeline.
///
/// `events` may arrive in any order. `is_fresh` decides, PER PROJECT, whether
/// its updates count as unread.
///
/// WHY A PREDICATE AND NOT THE GLOBAL STAMP: reading happens per project, so a
/// single "you left Standup at 14:55" stamp cannot say what you have actually
/// read. Open a project, read it, and the global stamp still calls it new — and
/// the rail, which uses the per-project stamp, would disagree with this tier on
/// screen. The global stamp still decides `floor_ms`, i.e. how far back to look;
/// only what counts as NEW moved to the per-project ledger.
///
/// `show_all` is the user having clicked through a "+N more", and defeats BOTH
/// caps at once: having asked for everything, getting a digest would be a
/// second, invisible cap.
/// A discriminant for TimelineKind, which is neither Copy nor Hash.
fn kind_ord(k: &TimelineKind) -> u8 {
    match k {
        TimelineKind::Summary => 0,
        TimelineKind::Trail => 1,
        TimelineKind::Decision => 2,
        TimelineKind::Map => 3,
    }
}

pub(crate) fn plan_updates(
    events: &[TimelineEvent],
    is_fresh: &dyn Fn(&str) -> bool,
    is_expanded: &dyn Fn(&str) -> bool,
    floor_ms: u64,
    show_all: bool,
) -> UpdatePlan {
    // group by project, newest first within each — dropping anything older than
    // the floor BEFORE grouping, so a project whose only activity is ancient
    // does not report at all.
    let mut order: Vec<String> = Vec::new();
    let mut by_key: std::collections::HashMap<String, Vec<&TimelineEvent>> =
        std::collections::HashMap::new();
    for e in events.iter().filter(|e| e.ts_ms >= floor_ms) {
        let slot = by_key.entry(e.project_key.clone()).or_insert_with(|| {
            order.push(e.project_key.clone());
            Vec::new()
        });
        slot.push(e);
    }

    let mut projects: Vec<PlannedProject> = order
        .into_iter()
        .map(|key| {
            let mut evs = by_key.remove(&key).unwrap_or_default();
            evs.sort_by(|a, b| b.ts_ms.cmp(&a.ts_ms));
            // ONE LINE PER THREAD, not per turn.
            //
            // The summariser writes a fresh summary every turn: measured, a
            // single session produced 54 summaries over 34.7 hours — one every
            // 39 minutes. Those are not 54 updates, they are one session's state
            // restated 54 times, and treating them as events is what produced a
            // block reading "+56 more". Keeping only the NEWEST per session
            // turns that project into one line saying where it actually got to.
            //
            // Events with no session (map batches) never collapse — they have no
            // thread to be the latest of.
            let mut seen_thread: std::collections::HashSet<(u8, &str)> =
                std::collections::HashSet::new();
            evs.retain(|e| {
                if e.sess.is_empty() {
                    return true;
                }
                seen_thread.insert((kind_ord(&e.kind), e.sess.as_str()))
            });
            let newest_ms = evs.first().map(|e| e.ts_ms).unwrap_or(0);
            let fresh = is_fresh(&key);
            PlannedProject {
                key,
                newest_ms,
                total: evs.len(),
                fresh,
                lines: evs
                    .iter()
                    .map(|e| PlannedEvent {
                        kind: e.kind.clone(),
                        text: e.text.clone(),
                        ts_ms: e.ts_ms,
                    })
                    .collect(),
                hidden_lines: 0,
            }
        })
        .collect();

    // newest project first — so the cap below can only ever drop the STALEST,
    // never a project that just reported.
    projects.sort_by(|a, b| b.newest_ms.cmp(&a.newest_ms));

    let reporting = projects.len();
    let density = if show_all || reporting <= BLOCK_MAX_PROJECTS {
        Density::Blocks
    } else {
        Density::Digest
    };

    let hidden_projects = if !show_all && reporting > PROJECT_CAP {
        projects.truncate(PROJECTS_WHEN_CAPPED);
        reporting - PROJECTS_WHEN_CAPPED
    } else {
        0
    };

    let keep = match density {
        Density::Blocks => LINES_PER_BLOCK,
        Density::Digest => 1,
    };
    for p in &mut projects {
        // An expanded project keeps ALL of its lines: "+53 more" was a dead end
        // until this, and a count you cannot open is just a complaint.
        if is_expanded(&p.key) {
            p.hidden_lines = 0;
            continue;
        }
        p.hidden_lines = p.total.saturating_sub(keep);
        p.lines.truncate(keep);
    }

    let (fresh, earlier) = projects.into_iter().partition(|p| p.fresh);
    UpdatePlan {
        fresh,
        earlier,
        density,
        hidden_projects,
        reporting,
    }
}

/// The one-line ambient summary that replaces the ● LIVE row list. Live sessions
/// are reassurance, not information: "nine things are running and fine" is a
/// sentence, not nine rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct LiveSummary {
    pub working: usize,
    pub idle: usize,
    pub projects: usize,
}

impl LiveSummary {
    pub(crate) fn total(&self) -> usize {
        self.working + self.idle
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.total() == 0
    }
    /// `6 working · 5 idle across 9 projects`. Zero-count halves are dropped
    /// rather than printed as "0 idle", matching how the subline already joins.
    pub(crate) fn line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.working > 0 {
            parts.push(format!("{} working", self.working));
        }
        if self.idle > 0 {
            parts.push(format!("{} idle", self.idle));
        }
        let head = if parts.is_empty() {
            "nothing running".to_string()
        } else {
            parts.join(" · ")
        };
        format!(
            "{head} across {} project{}",
            self.projects,
            if self.projects == 1 { "" } else { "s" }
        )
    }
}

/// A run of CONSECUTIVE timeline events from one project.
///
/// The timeline stays a timeline: a run only ever covers rows that were already
/// adjacent in time, so nothing is reordered and nothing is hidden. What it
/// removes is the repetition — twelve rows that each begin "atlas · " become one
/// "atlas" heading with twelve lines under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Run {
    pub key: String,
    /// Index into the events slice where this run starts.
    pub at: usize,
    pub len: usize,
}

/// Fold consecutive same-project events into runs.
///
/// A run is BROKEN by anything the reader can see between two rows, or the
/// heading would appear to cover rows it does not:
///   - a different project,
///   - a day boundary (the TODAY / YESTERDAY rules),
///   - the last-checked divider.
/// `day_of` is supplied by the caller so runs and day headers cannot disagree
/// about where a day starts — they must use the same clock and the same offset.
pub(crate) fn group_runs(
    events: &[TimelineEvent],
    day_of: &dyn Fn(u64) -> i64,
    divider_ms: u64,
) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for (i, e) in events.iter().enumerate() {
        let extend = match (runs.last(), events.get(i.wrapping_sub(1))) {
            (Some(r), Some(prev)) if i > 0 => {
                r.key == e.project_key
                    && day_of(prev.ts_ms) == day_of(e.ts_ms)
                    // the divider sits between prev and this one when prev is
                    // still newer than it and this one is not
                    && !(divider_ms > 0 && prev.ts_ms > divider_ms && e.ts_ms <= divider_ms)
            }
            _ => false,
        };
        if extend {
            if let Some(r) = runs.last_mut() {
                r.len += 1;
            }
        } else {
            runs.push(Run {
                key: e.project_key.clone(),
                at: i,
                len: 1,
            });
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Projects actually rendered across both groups. Test-only: the render
    /// walks the two groups separately, so nothing in the app needs the sum.
    fn shown(p: &UpdatePlan) -> usize {
        p.fresh.len() + p.earlier.len()
    }

    /// A DISTINCT thread per event by default. The old fixture used a constant
    /// `sess`, which made every synthetic event look like the same session — so
    /// once thread-dedup landed, tests counting events silently counted one.
    fn ev(project: &str, ts_ms: u64, text: &str) -> TimelineEvent {
        TimelineEvent {
            ts_ms,
            project_key: project.into(),
            kind: TimelineKind::Summary,
            sess: format!("s{ts_ms}"),
            node: None,
            text: text.into(),
            next: String::new(),
            detail_json: String::new(),
            count: 1,
        }
    }
    /// n projects, one event each, newest first at ts = n, n-1, ...
    fn spread(n: usize) -> Vec<TimelineEvent> {
        (0..n)
            .map(|i| ev(&format!("p{i}"), (n - i) as u64 * 1000, "did a thing"))
            .collect()
    }

    #[test]
    fn empty_input_is_an_empty_plan() {
        let p = plan_updates(&[], &|_| true, &|_| false, 0, false);
        assert!(p.is_empty());
        assert_eq!(p.reporting, 0);
        assert_eq!(p.hidden_projects, 0);
    }

    #[test]
    fn events_group_by_project_and_the_newest_dates_the_row() {
        let evs = vec![
            ev("atlas", 100, "older"),
            ev("atlas", 900, "newest"),
            ev("harbor", 400, "only"),
        ];
        let p = plan_updates(&evs, &|_| true, &|_| false, 0, false);
        assert_eq!(p.reporting, 2, "two projects, three events");
        // sorted newest project first
        assert_eq!(p.fresh[0].key, "atlas");
        assert_eq!(p.fresh[0].newest_ms, 900);
        assert_eq!(p.fresh[0].total, 2);
        assert_eq!(p.fresh[0].lines[0].text, "newest", "newest line leads");
        assert_eq!(p.fresh[1].key, "harbor");
    }

    fn ev_sess(project: &str, sess: &str, ts_ms: u64, text: &str) -> TimelineEvent {
        let mut e = ev(project, ts_ms, text);
        e.sess = sess.into();
        e
    }

    #[test]
    fn one_session_restating_itself_collapses_to_its_latest() {
        // MEASURED: one real session produced 54 summaries over 34.7h — one
        // every 39 minutes. That is a rolling status, not 54 things that
        // happened, and rendering it as events is what produced "+56 more".
        let evs: Vec<_> = (0..54)
            .map(|i| ev_sess("ai-video", "s1", 9_000 - i, "where it got to"))
            .collect();
        let p = plan_updates(&evs, &|_| true, &|_| false, 0, false);
        assert_eq!(p.fresh[0].total, 1, "one thread, one line");
        assert_eq!(p.fresh[0].hidden_lines, 0, "and so nothing to hide");
        assert_eq!(p.fresh[0].lines[0].ts_ms, 9_000, "the NEWEST is what it says");
    }

    #[test]
    fn separate_sessions_keep_separate_lines() {
        let evs = vec![
            ev_sess("atlas", "s1", 900, "a-new"),
            ev_sess("atlas", "s1", 800, "a-old"),
            ev_sess("atlas", "s2", 700, "b"),
        ];
        let p = plan_updates(&evs, &|_| true, &|_| false, 0, false);
        assert_eq!(p.fresh[0].total, 2);
        assert_eq!(p.fresh[0].lines[0].text, "a-new");
        assert_eq!(p.fresh[0].lines[1].text, "b");
    }

    #[test]
    fn sessionless_events_never_collapse() {
        // map batches carry no session — they have no thread to be the latest of.
        let nosess = |ts, t| {
            let mut e = ev("atlas", ts, t);
            e.sess = String::new();
            e
        };
        let evs = vec![nosess(900, "map 1"), nosess(800, "map 2"), nosess(700, "map 3")];
        let p = plan_updates(&evs, &|_| true, &|_| false, 0, false);
        assert_eq!(p.fresh[0].total, 3);
    }

    #[test]
    fn height_grows_with_projects_not_events() {
        // The whole point. Forty events on ONE project is still one block.
        let evs: Vec<_> = (0..40).map(|i| ev("atlas", 1000 + i, "x")).collect();
        let p = plan_updates(&evs, &|_| true, &|_| false, 0, false);
        assert_eq!(p.reporting, 1);
        assert_eq!(shown(&p), 1);
        assert_eq!(p.fresh[0].total, 40);
        assert_eq!(p.fresh[0].lines.len(), LINES_PER_BLOCK);
        assert_eq!(p.fresh[0].hidden_lines, 37);
    }

    #[test]
    fn the_caller_decides_per_project_what_counts_as_new() {
        // The whole reason for the predicate: "have I read this?" is answered
        // per project (proj_seen_ms), not by one global "you left Standup" stamp.
        let evs = vec![ev("new", 900, "a"), ev("old", 100, "b")];
        let p = plan_updates(&evs, &|k| k == "new", &|_| false, 0, false);
        assert_eq!(p.fresh.len(), 1);
        assert_eq!(p.fresh[0].key, "new");
        assert_eq!(p.earlier.len(), 1);
        assert_eq!(p.earlier[0].key, "old");
    }

    #[test]
    fn a_first_ever_visit_makes_everything_fresh() {
        // A project never opened has no proj_seen stamp, so project_unread says
        // unread — dimming someone's entire first look would be wrong.
        let p = plan_updates(&spread(4), &|_| true, &|_| false, 0, false);
        assert_eq!(p.fresh.len(), 4);
        assert!(p.earlier.is_empty());
    }

    #[test]
    fn blocks_up_to_the_threshold_digest_past_it() {
        let at = plan_updates(&spread(BLOCK_MAX_PROJECTS), &|_| true, &|_| false, 0, false);
        assert_eq!(at.density, Density::Blocks);
        let over = plan_updates(&spread(BLOCK_MAX_PROJECTS + 1), &|_| true, &|_| false, 0, false);
        assert_eq!(over.density, Density::Digest);
    }

    #[test]
    fn a_digest_row_carries_exactly_one_line() {
        let mut evs = spread(BLOCK_MAX_PROJECTS + 1);
        evs.push(ev("p0", 99_999, "the newest thing p0 did"));
        let p = plan_updates(&evs, &|_| true, &|_| false, 0, false);
        assert_eq!(p.density, Density::Digest);
        let p0 = p.fresh.iter().find(|x| x.key == "p0").unwrap();
        assert_eq!(p0.lines.len(), 1);
        assert_eq!(p0.lines[0].text, "the newest thing p0 did");
        assert_eq!(p0.hidden_lines, 1);
    }

    #[test]
    fn the_project_cap_drops_the_stalest_and_reports_the_remainder() {
        let p = plan_updates(&spread(PROJECT_CAP + 5), &|_| true, &|_| false, 0, false);
        assert_eq!(p.reporting, PROJECT_CAP + 5);
        assert_eq!(shown(&p), PROJECTS_WHEN_CAPPED);
        assert_eq!(p.hidden_projects, PROJECT_CAP + 5 - PROJECTS_WHEN_CAPPED);
        // spread() gives p0 the newest stamp, so the survivors are p0..p9 —
        // the cap must never drop a project that just reported in favour of
        // one that has been quiet for hours.
        assert_eq!(p.fresh.first().unwrap().key, "p0");
        assert!(!p.fresh.iter().any(|x| x.key == "p14"));
    }

    #[test]
    fn exactly_at_the_cap_nothing_is_hidden() {
        let p = plan_updates(&spread(PROJECT_CAP), &|_| true, &|_| false, 0, false);
        assert_eq!(p.hidden_projects, 0);
        assert_eq!(shown(&p), PROJECT_CAP);
    }

    #[test]
    fn show_all_defeats_both_caps_at_once() {
        // Having asked for everything, being handed digests would be a second
        // cap the user cannot see.
        let p = plan_updates(&spread(PROJECT_CAP + 5), &|_| true, &|_| false, 0, true);
        assert_eq!(p.density, Density::Blocks);
        assert_eq!(p.hidden_projects, 0);
        assert_eq!(shown(&p), PROJECT_CAP + 5);
    }

    #[test]
    fn the_split_keeps_recency_order_inside_each_group() {
        let evs = vec![
            ev("a", 900, ""),
            ev("b", 800, ""),
            ev("c", 200, ""),
            ev("d", 100, ""),
        ];
        let p = plan_updates(&evs, &|k| k == "a" || k == "b", &|_| false, 0, false);
        assert_eq!(
            p.fresh.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(
            p.earlier.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(),
            ["c", "d"]
        );
    }

    #[test]
    fn a_project_whose_newest_event_ties_the_stamp_is_not_fresh() {
        // Strictly newer, matching project_unread's `last_update > seen`.
        let p = plan_updates(&[ev("a", 500, "")], &|_| false, &|_| false, 0, false);
        assert!(p.fresh.is_empty());
        assert_eq!(p.earlier.len(), 1);
    }

    // ── the time floor ────────────────────────────────────────────────────
    const H: u64 = 3600 * 1000;
    const NOW: u64 = 1_000 * 24 * H; // an arbitrary "now" far from zero

    #[test]
    fn checking_a_minute_ago_still_shows_the_last_two_days() {
        // The failure this prevents: you glance at Standup at 9am, and at 10am
        // it has hidden everything that happened overnight because it is "seen".
        let f = update_floor_ms(NOW - 60_000, NOW);
        assert_eq!(f, NOW - MIN_WINDOW_MS);
    }

    #[test]
    fn being_away_a_while_reaches_back_to_your_last_look() {
        let f = update_floor_ms(NOW - 5 * 24 * H, NOW);
        assert_eq!(f, NOW - 5 * 24 * H);
    }

    #[test]
    fn being_away_a_long_time_is_clamped_to_a_fortnight() {
        // Past this it stops being a standup and becomes an archive.
        let f = update_floor_ms(NOW - 90 * 24 * H, NOW);
        assert_eq!(f, NOW - MAX_WINDOW_MS);
    }

    #[test]
    fn a_first_visit_gets_the_minimum_window_not_all_of_history() {
        assert_eq!(update_floor_ms(0, NOW), NOW - MIN_WINDOW_MS);
    }

    #[test]
    fn the_window_says_which_rule_it_used() {
        // checked recently -> the 48h minimum is what you are seeing
        assert_eq!(window_label(NOW - 60_000, NOW), "the last 48 hours");
        assert_eq!(window_label(0, NOW), "the last 48 hours");
        // away longer -> it reaches back to your last look, and says so
        assert_eq!(
            window_label(NOW - 5 * 24 * H, NOW),
            "since your last check, 5d ago"
        );
        // away far too long -> clamped, and says THAT rather than lying about
        // showing everything since your last check
        assert_eq!(window_label(NOW - 90 * 24 * H, NOW), "the last 14 days");
    }

    #[test]
    fn events_older_than_the_floor_do_not_report_at_all() {
        // THE bug this fixes: timeline() is capped by COUNT, not time — 120 rows
        // is about three weeks of real history — so without a floor a project
        // that last spoke three weeks ago was filed under "EARLIER", next to
        // this morning's work.
        let evs = vec![
            ev("recent", NOW - H, "today"),
            ev("ancient", NOW - 30 * 24 * H, "three weeks ago"),
        ];
        let floor = update_floor_ms(0, NOW);
        let p = plan_updates(&evs, &|_| true, &|_| false, floor, false);
        assert_eq!(p.reporting, 1, "the ancient project must not report");
        assert_eq!(p.fresh[0].key, "recent");
    }

    // ── calibrated to production, not to imagination ──────────────────────
    /// The busiest day that has ACTUALLY occurred in 47 days of real history:
    /// 9 projects, 45 summaries. Numbers measured from the store, not invented —
    /// a fixture built from a guess would have validated the guess.
    #[test]
    fn the_busiest_real_day_still_reports_every_project() {
        let mut evs = Vec::new();
        for i in 0..9 {
            for k in 0..5 {
                evs.push(ev(
                    &format!("p{i}"),
                    NOW - (i as u64 * H) - (k as u64 * 600_000),
                    // real headlines average 67 chars, max 120
                    "Split the ingest pipeline into three stages; replay is now four times faster.",
                ));
            }
        }
        let p = plan_updates(&evs, &|_| true, &|_| false, update_floor_ms(0, NOW), false);
        assert_eq!(p.reporting, 9, "all nine projects report");
        assert_eq!(shown(&p), 9, "and none is capped away — 9 is under PROJECT_CAP");
        assert_eq!(p.hidden_projects, 0);
        assert_eq!(
            p.density,
            Density::Digest,
            "9 projects is past BLOCK_MAX_PROJECTS, so the safety valve opens"
        );
        assert!(p.fresh.iter().all(|x| x.lines.len() == 1 && x.hidden_lines == 4));
    }

    /// The TYPICAL day: 2 projects. This is what the screen looks like almost
    /// every morning, and it must be blocks — digest mode on two projects would
    /// throw away detail for no reason.
    #[test]
    fn a_typical_day_renders_as_full_blocks() {
        let evs = vec![
            ev("atlas", NOW - H, "a"),
            ev("atlas", NOW - 2 * H, "b"),
            ev("harbor", NOW - 3 * H, "c"),
        ];
        let p = plan_updates(&evs, &|_| true, &|_| false, update_floor_ms(0, NOW), false);
        assert_eq!(p.reporting, 2);
        assert_eq!(p.density, Density::Blocks);
    }

    #[test]
    fn live_summary_reads_as_a_sentence_and_pluralises() {
        assert_eq!(
            LiveSummary { working: 6, idle: 5, projects: 9 }.line(),
            "6 working · 5 idle across 9 projects"
        );
        assert_eq!(
            LiveSummary { working: 1, idle: 0, projects: 1 }.line(),
            "1 working across 1 project"
        );
        assert_eq!(
            LiveSummary { working: 0, idle: 3, projects: 2 }.line(),
            "3 idle across 2 projects",
            "a zero half is dropped, never printed as '0 working'"
        );
    }

    // ── timeline runs ─────────────────────────────────────────────────────
    /// every event on its own day-0, so only the project can break a run
    fn same_day(_ts: u64) -> i64 {
        0
    }

    #[test]
    fn consecutive_same_project_events_become_one_run() {
        let evs = vec![
            ev("atlas", 900, "a"),
            ev("atlas", 800, "b"),
            ev("atlas", 700, "c"),
        ];
        let runs = group_runs(&evs, &same_day, 0);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].len, 3);
    }

    #[test]
    fn a_different_project_breaks_the_run() {
        let evs = vec![
            ev("atlas", 900, "a"),
            ev("harbor", 800, "b"),
            ev("atlas", 700, "c"),
        ];
        let runs = group_runs(&evs, &same_day, 0);
        // NOT two atlas runs merged: that would reorder the timeline.
        assert_eq!(runs.len(), 3);
        assert!(runs.iter().all(|r| r.len == 1));
        assert_eq!(runs.iter().map(|r| r.at).collect::<Vec<_>>(), [0, 1, 2]);
    }

    #[test]
    fn a_day_boundary_breaks_the_run() {
        // otherwise one heading would straddle TODAY and YESTERDAY.
        let evs = vec![ev("atlas", 900, "a"), ev("atlas", 100, "b")];
        let runs = group_runs(&evs, &|ts| if ts > 500 { 1 } else { 0 }, 0);
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn the_last_checked_divider_breaks_the_run() {
        // the divider is drawn BETWEEN rows; a run spanning it would put the
        // heading above the line and half its rows below.
        let evs = vec![ev("atlas", 900, "a"), ev("atlas", 100, "b")];
        let runs = group_runs(&evs, &same_day, 500);
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn a_lone_event_is_still_a_run() {
        // It gets a heading like any other. Two styles — inline for singles,
        // heading for bursts — read as a bug, not a distinction.
        let runs = group_runs(&[ev("atlas", 900, "a")], &same_day, 0);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].len, 1);
    }

    #[test]
    fn runs_cover_every_event_exactly_once() {
        // the invariant that keeps the timeline honest: nothing dropped, nothing
        // duplicated, order preserved.
        let evs = vec![
            ev("a", 900, ""), ev("a", 800, ""), ev("b", 700, ""),
            ev("b", 600, ""), ev("a", 500, ""),
        ];
        let runs = group_runs(&evs, &same_day, 0);
        let covered: usize = runs.iter().map(|r| r.len).sum();
        assert_eq!(covered, evs.len());
        let mut next = 0;
        for r in &runs {
            assert_eq!(r.at, next, "runs must be contiguous and in order");
            next += r.len;
        }
    }

    #[test]
    fn a_real_burst_collapses_to_one_heading() {
        // measured: one session produced 60 summaries, and a real day had 34
        // rows across 2 projects. That burst is the mess this removes.
        let evs: Vec<_> = (0..34).map(|i| ev("atlas", 1000 - i, "x")).collect();
        let runs = group_runs(&evs, &same_day, 0);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].len, 34);
    }

    #[test]
    fn an_empty_live_summary_knows_it_is_empty() {
        let s = LiveSummary::default();
        assert!(s.is_empty());
        assert_eq!(s.total(), 0);
    }
}
