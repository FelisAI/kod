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

/// Above this many reporting projects, blocks collapse to one line each. Eight
/// full blocks is roughly one screen; past that, reading beats browsing.
pub(crate) const BLOCK_MAX_PROJECTS: usize = 8;
/// Event lines inside one block before the rest become "+N more".
pub(crate) const LINES_PER_BLOCK: usize = 3;
/// Above this many reporting projects the tail collapses entirely.
pub(crate) const PROJECT_CAP: usize = 12;
/// How many survive that collapse.
pub(crate) const PROJECTS_WHEN_CAPPED: usize = 10;

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
/// `events` may arrive in any order; `seen_ms` is the last-checked stamp (0 on a
/// first-ever visit, which makes everything fresh — the same rule the seen
/// divider already uses). `show_all` is the user having clicked through a
/// "+N more", and defeats BOTH caps at once: having asked for everything,
/// getting a digest would be a second, invisible cap.
pub(crate) fn plan_updates(events: &[TimelineEvent], seen_ms: u64, show_all: bool) -> UpdatePlan {
    // group by project, newest first within each
    let mut order: Vec<String> = Vec::new();
    let mut by_key: std::collections::HashMap<String, Vec<&TimelineEvent>> =
        std::collections::HashMap::new();
    for e in events {
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
            let newest_ms = evs.first().map(|e| e.ts_ms).unwrap_or(0);
            PlannedProject {
                key,
                newest_ms,
                total: evs.len(),
                fresh: newest_ms > seen_ms,
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

/// Glyph + colour for one event line inside a project block.
///
/// NOTE the thread below the tiers keeps a RICHER variant that also splits
/// Trail into dispatched (▶ green) vs finished (■ grey) by inspecting the text.
/// Here only the kind is known, so Trail renders as the neutral ■ — a block
/// line is a glance, the thread is the record.
pub(crate) fn kind_glyph(k: &TimelineKind) -> (&'static str, u32) {
    match k {
        TimelineKind::Summary => ("☁", crate::theme::ACCENT),
        TimelineKind::Trail => ("■", crate::theme::MUTED2),
        TimelineKind::Decision => ("◆", crate::theme::AMBER),
        TimelineKind::Map => ("🗺", 0x9A7FD1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Projects actually rendered across both groups. Test-only: the render
    /// walks the two groups separately, so nothing in the app needs the sum.
    fn shown(p: &UpdatePlan) -> usize {
        p.fresh.len() + p.earlier.len()
    }

    fn ev(project: &str, ts_ms: u64, text: &str) -> TimelineEvent {
        TimelineEvent {
            ts_ms,
            project_key: project.into(),
            kind: TimelineKind::Summary,
            sess: "s".into(),
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
        let p = plan_updates(&[], 0, false);
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
        let p = plan_updates(&evs, 0, false);
        assert_eq!(p.reporting, 2, "two projects, three events");
        // sorted newest project first
        assert_eq!(p.fresh[0].key, "atlas");
        assert_eq!(p.fresh[0].newest_ms, 900);
        assert_eq!(p.fresh[0].total, 2);
        assert_eq!(p.fresh[0].lines[0].text, "newest", "newest line leads");
        assert_eq!(p.fresh[1].key, "harbor");
    }

    #[test]
    fn height_grows_with_projects_not_events() {
        // The whole point. Forty events on ONE project is still one block.
        let evs: Vec<_> = (0..40).map(|i| ev("atlas", 1000 + i, "x")).collect();
        let p = plan_updates(&evs, 0, false);
        assert_eq!(p.reporting, 1);
        assert_eq!(shown(&p), 1);
        assert_eq!(p.fresh[0].total, 40);
        assert_eq!(p.fresh[0].lines.len(), LINES_PER_BLOCK);
        assert_eq!(p.fresh[0].hidden_lines, 37);
    }

    #[test]
    fn a_project_is_fresh_only_when_it_beats_the_last_check() {
        let evs = vec![ev("new", 900, "a"), ev("old", 100, "b")];
        let p = plan_updates(&evs, 500, false);
        assert_eq!(p.fresh.len(), 1);
        assert_eq!(p.fresh[0].key, "new");
        assert_eq!(p.earlier.len(), 1);
        assert_eq!(p.earlier[0].key, "old");
    }

    #[test]
    fn a_first_ever_visit_makes_everything_fresh() {
        // seen_ms == 0 is the first-visit sentinel the seen divider already uses;
        // dimming the entire history on someone's first look would be wrong.
        let p = plan_updates(&spread(4), 0, false);
        assert_eq!(p.fresh.len(), 4);
        assert!(p.earlier.is_empty());
    }

    #[test]
    fn blocks_up_to_the_threshold_digest_past_it() {
        let at = plan_updates(&spread(BLOCK_MAX_PROJECTS), 0, false);
        assert_eq!(at.density, Density::Blocks);
        let over = plan_updates(&spread(BLOCK_MAX_PROJECTS + 1), 0, false);
        assert_eq!(over.density, Density::Digest);
    }

    #[test]
    fn a_digest_row_carries_exactly_one_line() {
        let mut evs = spread(BLOCK_MAX_PROJECTS + 1);
        evs.push(ev("p0", 99_999, "the newest thing p0 did"));
        let p = plan_updates(&evs, 0, false);
        assert_eq!(p.density, Density::Digest);
        let p0 = p.fresh.iter().find(|x| x.key == "p0").unwrap();
        assert_eq!(p0.lines.len(), 1);
        assert_eq!(p0.lines[0].text, "the newest thing p0 did");
        assert_eq!(p0.hidden_lines, 1);
    }

    #[test]
    fn the_project_cap_drops_the_stalest_and_reports_the_remainder() {
        let p = plan_updates(&spread(PROJECT_CAP + 5), 0, false);
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
        let p = plan_updates(&spread(PROJECT_CAP), 0, false);
        assert_eq!(p.hidden_projects, 0);
        assert_eq!(shown(&p), PROJECT_CAP);
    }

    #[test]
    fn show_all_defeats_both_caps_at_once() {
        // Having asked for everything, being handed digests would be a second
        // cap the user cannot see.
        let p = plan_updates(&spread(PROJECT_CAP + 5), 0, true);
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
        let p = plan_updates(&evs, 500, false);
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
        let p = plan_updates(&[ev("a", 500, "")], 500, false);
        assert!(p.fresh.is_empty());
        assert_eq!(p.earlier.len(), 1);
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

    #[test]
    fn every_event_kind_gets_its_own_glyph() {
        let all = [
            kind_glyph(&TimelineKind::Summary),
            kind_glyph(&TimelineKind::Trail),
            kind_glyph(&TimelineKind::Decision),
            kind_glyph(&TimelineKind::Map),
        ];
        let mut g: Vec<_> = all.iter().map(|(g, _)| *g).collect();
        g.sort_unstable();
        g.dedup();
        assert_eq!(g.len(), 4, "two kinds render as the same glyph");
    }

    #[test]
    fn an_empty_live_summary_knows_it_is_empty() {
        let s = LiveSummary::default();
        assert!(s.is_empty());
        assert_eq!(s.total(), 0);
    }
}
