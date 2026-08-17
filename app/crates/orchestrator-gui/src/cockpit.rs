//! cockpit — the PURE glance logic of docs/019 slice 4 (the cockpit, C6). The
//! header answers "what needs me · what's moving · where was I · what to start"
//! in PLAIN WORDS. This is the FINAL slice's foundations.
//!
//! Like `returnchannel`, NOTHING here does I/O, touches the `Store`, or draws:
//! every fact is passed in and every result is plain data. The live wiring
//! (header render, Next-up tray, summons pulse, gap drawer, triage sweep) reads
//! these; keeping the arithmetic here means each trust rule is unit-tested
//! without a GPUI render tree — an in-file `#[cfg(test)]` mod in the 8k-line
//! main.rs overflows the macro recursion limit (returnchannel.rs learned this).
//!
//! The house rules this module encodes:
//!   * Status stays ASSERTED-never-inferred. The rollup COMPUTES over asserted
//!     statuses (arithmetic, honest); `building` is derived-only (slice 3) and
//!     arrives here as a set of ids, never re-inferred.
//!   * DESIGN VOCABULARY NEVER SHIPS AS UI COPY (ruling 13). CAN/ARE/SHOULD,
//!     "epistemic tier", "changeset" — none of these strings leave the doc. The
//!     UI says "ready to start", "drifted", "suggestions", "proposal".
//!   * No global task inbox, no priorities, no due dates — EVER (ruling 8). The
//!     ★ is a ≤3 POINTER (a 4th unpins the oldest), never a stored queue;
//!     Next-up is a COMPUTED frontier over asserted statuses, not persisted.
//!   * At most ONE summons pulses in the whole UI — a computed SINGLETON in a
//!     single render path, so two pulsing things is structurally impossible.
//!
//! The pieces, each provable in isolation:
//!   1. ROLLUP        — the plain-words header line (done · working · ready · …).
//!   2. NEXT-UP       — the dispatch-ready frontier, ★ > attention > warmth > age.
//!   3. STAR PINS     — the ≤3 pointer (a 4th unpins the oldest).
//!   4. ONE SUMMONS   — the computed singleton (needs-you > review > start > none).
//!   5. GAP FINDERS   — deterministic SHOULD drawer (no LLM); never color nodes.
//!   6. WARM BAND     — resumption trail (activity ≤7d and NOT live).
//!   7. NEEDS-YOU     — the 7d anti-decay escalation.
//!
//! WIRED (slice 4 UI pass): the header rollup, Next-up tray, ONE-SUMMONS pulse,
//! gap drawer, warm band, triage sweep and Standup portfolio line all read this
//! module, so the foundations' scoped `dead_code` allow is gone.

use std::cmp::Reverse;
use std::collections::HashMap;

use orchestrator_store::{
    build_tree, quiet_building, task_rollup, Kind, Lifecycle, Part, PartId, TreeNode,
};

/// One day in seconds — the unit every cockpit window is expressed in.
const DAY: u64 = 24 * 60 * 60;

// ===========================================================================
// 1. ROLLUP — the plain-words header line (docs/019 commitment 5).
// ===========================================================================
//
// "41% done · 2 working (+1 drifted) · 4 ready to start · 2 suggestions". Reads
// like a person talking; CAN/ARE/SHOULD never appears. Every count is arithmetic
// over asserted state passed in — `building`/`drifted`/`ready` are computed by
// the slice-3 overlays and the Next-up frontier, handed here as id sets so this
// function INFERS nothing. `done_pct` is `task_rollup` over the WHOLE map.

/// The header rollup — five counts + a render to plain words. Distinct from
/// `mapview::Rollup` (that one counts attention hidden by collapse).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rollup {
    /// done/total over descendant TASKS across the whole map, as a percent.
    pub done_pct: u32,
    /// derived-building count (dispatch/declared links live or <48h).
    pub working: usize,
    /// observed-hollow count — the epistemic tier BELOW working (a hidden
    /// observed-only touch must NOT read identically to a dispatched session).
    pub drifted: usize,
    /// dispatch-ready task nodes (the Next-up frontier size, uncapped).
    pub ready: usize,
    /// gap-finder count (the SHOULD drawer).
    pub suggestions: usize,
}

impl Rollup {
    /// The header line in PLAIN WORDS (ruling 13). Same string powers the
    /// Standup per-project rollup, so it stays a stable, scannable four columns.
    pub fn line(&self) -> String {
        let working = if self.drifted > 0 {
            format!("{} working (+{} drifted)", self.working, self.drifted)
        } else {
            format!("{} working", self.working)
        };
        format!(
            "{}% done · {} · {} ready to start · {} suggestion{}",
            self.done_pct,
            working,
            self.ready,
            self.suggestions,
            if self.suggestions == 1 { "" } else { "s" },
        )
    }
}

/// done/total over descendant TASKS across the whole forest (roots included).
/// Reuses `task_rollup` (which counts a node's descendants) and adds each root
/// itself — so a root-level task is never silently dropped from the percent.
pub fn map_done(roots: &[TreeNode]) -> (usize, usize) {
    let mut done = 0usize;
    let mut total = 0usize;
    for r in roots {
        let (d, t) = task_rollup(r);
        done += d;
        total += t;
        if r.part.kind == Kind::Task && r.part.lifecycle != Lifecycle::Idea {
            total += 1;
            if r.part.lifecycle == Lifecycle::Done {
                done += 1;
            }
        }
    }
    (done, total)
}

/// Assemble the header rollup. `working`/`drifted`/`ready` arrive as id slices
/// (the caller's derived-building set, observed-hollow set, and Next-up frontier)
/// so the count is honest arithmetic — this function re-derives nothing.
pub fn rollup(
    roots: &[TreeNode],
    building_ids: &[PartId],
    drifted_ids: &[PartId],
    ready_ids: &[PartId],
    suggestion_count: usize,
) -> Rollup {
    let (done, total) = map_done(roots);
    let done_pct = if total == 0 {
        0
    } else {
        ((done as f64 / total as f64) * 100.0).round() as u32
    };
    Rollup {
        done_pct,
        working: building_ids.len(),
        drifted: drifted_ids.len(),
        ready: ready_ids.len(),
        suggestions: suggestion_count,
    }
}

// ===========================================================================
// 2. NEXT-UP — the dispatch-ready frontier (docs/019 commitment 5, CAN).
// ===========================================================================
//
// Dispatch-ready TASK nodes (kind=task, lifecycle=todo, unblocked, not stale),
// ranked ★ > attention(needs-you) > warmth(recent activity) > age. This is a
// COMPUTED frontier, never a stored queue (ruling 8): pass the asserted rows in,
// get an ordered id list out. The render list is capped so a huge todo pile
// never becomes the very inbox the map exists to abolish.

/// How many Next-up rows the tray renders. The frontier is computed uncapped
/// (its size is the header's "ready" count); only the RENDER list is bounded.
pub const NEXT_UP_CAP: usize = 7;

/// Is this row eligible for the Next-up frontier? kind=task, lifecycle=todo,
/// not stale, not in the blocked set. `building`/`idea`/`done` never appear —
/// only committed-but-unstarted work a session can pick up right now.
fn is_ready(p: &Part, blocked: &std::collections::HashSet<PartId>) -> bool {
    p.kind == Kind::Task && p.lifecycle == Lifecycle::Todo && !p.stale && !blocked.contains(&p.id)
}

/// The ready frontier, ranked. Priority tiers (highest first):
///   1. ★ pins, in the user's pin order (the ≤3 pointer wins outright).
///   2. attention — nodes carrying a needs-you signal rank above cold work.
///   3. warmth — most-recent activity first (where the user last was).
///   4. age — the oldest todo first (been waiting longest); id breaks ties.
///
/// `attention` and `blocked` are DIFFERENT sets: attention BOOSTS rank, blocked
/// EXCLUDES from candidacy. Returns the capped, ranked id list.
pub fn next_up(
    parts: &[Part],
    activity: &HashMap<PartId, u64>,
    stars: &[PartId],
    attention: &std::collections::HashSet<PartId>,
    blocked: &std::collections::HashSet<PartId>,
    cap: usize,
) -> Vec<PartId> {
    let mut candidates: Vec<&Part> = parts.iter().filter(|p| is_ready(p, blocked)).collect();
    // The sort key is ascending = higher priority. star_key puts pinned rows
    // first in pin order (non-pinned share the max sentinel and fall to the
    // attention/warmth/age tiers). warmth is Reverse so newest sorts first.
    let key = |p: &&Part| {
        let star_key = stars
            .iter()
            .position(|s| *s == p.id)
            .map(|i| i as i64)
            .unwrap_or(i64::MAX);
        let attn_key = if attention.contains(&p.id) { 0u8 } else { 1u8 };
        let warmth = activity.get(&p.id).copied().unwrap_or(0);
        (star_key, attn_key, Reverse(warmth), p.status_at_secs, p.id)
    };
    candidates.sort_by_key(key);
    candidates.into_iter().take(cap).map(|p| p.id).collect()
}

// ===========================================================================
// 3. STAR PINS — the ≤3 pointer (docs/019 commitment 5, verify amendment).
// ===========================================================================
//
// ★ is a user-set "up next" pin, NOT a priority system (ruling 8). At most
// `cap` (3) per project; pinning a 4th unpins the OLDEST. The list is ordered
// oldest-first (front = oldest), so a fresh pin appends to the back.

/// The pin cap: at most three ★ per project.
pub const STAR_CAP: usize = 3;

/// Toggle a pin. If `id` is already pinned, UNPIN it (the ★ is idempotent — a
/// second gesture removes it). Otherwise pin it at the back; if that would
/// exceed `cap`, drop from the front until it fits (the oldest pin gives way —
/// a pointer, never a growing queue). Returns the new pin list, oldest-first.
pub fn star_toggle(current: &[PartId], id: PartId, cap: usize) -> Vec<PartId> {
    if current.contains(&id) {
        return current.iter().copied().filter(|p| *p != id).collect();
    }
    let mut next: Vec<PartId> = current.to_vec();
    next.push(id);
    while next.len() > cap {
        next.remove(0);
    }
    next
}

// ===========================================================================
// 4. ONE SUMMONS — the computed singleton (docs/019 commitment 5).
// ===========================================================================
//
// At most ONE pulsing thing in the entire UI. Because this returns exactly one
// enum value, two things pulsing at once is structurally impossible — the
// singleton is the type, not a convention. Priority:
//   oldest unanswered needs-you  >  a pending changeset review  >  the top
//   ready item  >  nothing (a calm map is a valid state — `None` pulses nothing).
// This is DISTINCT from the chip-pulse (a session burning tokens = status); the
// summons-pulse asks for the USER.

/// The single thing (if any) asking for the user. Exactly one variant or
/// `None`; the hover-explains-why copy is the caller's, drawn from this value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Summons {
    /// A needs-you flag went unanswered longest. `question` renders VERBATIM
    /// (the question IS the payload — ruling: user-set needs-you requires it).
    NeedsYou {
        node: PartId,
        question: String,
        age_secs: u64,
    },
    /// A machine proposal is waiting to be reviewed.
    Review { changeset: i64, title: String },
    /// Nothing needs you and nothing's pending — but there IS work ready; nudge
    /// the top of the frontier.
    Start { node: PartId },
    /// A calm map. Nothing pulses; this is a CORRECT state, not an empty one.
    None,
}

/// Compute the summons. `needs_you` is `(node, question, age_secs)` — age is how
/// long the question has gone unanswered; the OLDEST (largest age) wins, lowest
/// node id breaking a tie so the choice is deterministic. `open_changeset` is
/// `(id, title)` of a pending review; `top_ready` is the head of the Next-up
/// frontier. The order of these `if`s IS the priority contract.
pub fn summons(
    needs_you: &[(PartId, String, u64)],
    open_changeset: Option<(i64, String)>,
    top_ready: Option<PartId>,
) -> Summons {
    if let Some((node, question, age)) = needs_you
        .iter()
        // oldest first: max age; a tie falls to the lowest node id.
        .max_by(|a, b| a.2.cmp(&b.2).then(b.0.cmp(&a.0)))
    {
        return Summons::NeedsYou {
            node: *node,
            question: question.clone(),
            age_secs: *age,
        };
    }
    if let Some((changeset, title)) = open_changeset {
        return Summons::Review { changeset, title };
    }
    if let Some(node) = top_ready {
        return Summons::Start { node };
    }
    Summons::None
}

// ===========================================================================
// 5. GAP FINDERS — the deterministic SHOULD drawer (docs/019 intelligence).
// ===========================================================================
//
// Deterministic finders, no LLM. Findings live in a DRAWER; they NEVER color
// nodes (a suggestion is not a status). Five finders:
//   * quiet zone            — a node cold 14d while a sibling still moves.
//   * committed-never-started — a todo task 45d old with zero sessions.
//   * stale cluster         — ≥2 stale nodes sharing the tightest ancestor.
//   * undermapped           — an anchored node with many files but ≤2 children.
//   * quiet-building drift   — reuses `quiet_building` (a Building node gone quiet).

/// A node is "cold" past this — no activity in its subtree for 14 days.
pub const QUIET_ZONE_SECS: u64 = 14 * DAY;
/// A todo this old with zero sessions reads as committed-but-never-started.
pub const COMMITTED_STALE_SECS: u64 = 45 * DAY;
/// The `quiet_building` drift threshold — a Building node quiet this long drifts.
pub const QUIET_BUILDING_DRIFT_SECS: u64 = 14 * DAY;
/// This many stale nodes under one tightest ancestor is a cluster.
pub const STALE_CLUSTER_MIN: usize = 2;
/// An anchored node carrying at least this many files/commits…
pub const UNDERMAPPED_MIN_ANCHORED: usize = 6;
/// …with at most this many children is undermapped (worth breaking down).
pub const UNDERMAPPED_MAX_CHILDREN: usize = 2;

/// The kind of gap a finder surfaced. The discriminant order is the drawer's
/// sort order (stable, deterministic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapKind {
    QuietZone,
    CommittedNeverStarted,
    StaleCluster,
    Undermapped,
    QuietBuildingDrift,
}

impl GapKind {
    fn ordinal(self) -> u8 {
        match self {
            GapKind::QuietZone => 0,
            GapKind::CommittedNeverStarted => 1,
            GapKind::StaleCluster => 2,
            GapKind::Undermapped => 3,
            GapKind::QuietBuildingDrift => 4,
        }
    }
}

/// One drawer entry: which finder, which node, and a PLAIN-WORDS reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gap {
    pub kind: GapKind,
    pub node: PartId,
    pub detail: String,
}

/// Fill `out[id]` = the newest activity anywhere in the subtree rooted at each
/// node (post-order). "Zone" liveness is subtree-wide: an area whose children
/// still move is not quiet, even if the area row itself was never touched.
fn subtree_max_activity(
    node: &TreeNode,
    activity: &HashMap<PartId, u64>,
    out: &mut HashMap<PartId, u64>,
) -> u64 {
    let mut m = activity.get(&node.part.id).copied().unwrap_or(0);
    for c in &node.children {
        m = m.max(subtree_max_activity(c, activity, out));
    }
    out.insert(node.part.id, m);
    m
}

/// Run every gap finder. Deterministic: the result is sorted by (kind, node id),
/// so repeated sweeps produce byte-identical drawers. `stale` is the set of
/// stale node ids (the reconciler's freshness cache); `anchor_file_counts` maps
/// a node id to the files/commits under its anchors (for undermapped).
pub fn gap_findings(
    parts: &[Part],
    activity: &HashMap<PartId, u64>,
    now: u64,
    stale: &std::collections::HashSet<PartId>,
    anchor_file_counts: &HashMap<PartId, usize>,
) -> Vec<Gap> {
    let tree = build_tree(parts);
    let mut out: Vec<Gap> = Vec::new();

    // subtree-wide liveness for the quiet-zone finder.
    let mut submax: HashMap<PartId, u64> = HashMap::new();
    for r in &tree {
        subtree_max_activity(r, activity, &mut submax);
    }
    let zone_moving = |id: PartId| {
        submax
            .get(&id)
            .is_some_and(|&t| t > 0 && now.saturating_sub(t) <= QUIET_ZONE_SECS)
    };

    // ---- quiet zone: cold node beside a moving sibling ----
    // Process each sibling group (the roots, then each node's children).
    quiet_zone_group(&tree, &zone_moving, &mut out);
    fn quiet_zone_group(sibs: &[TreeNode], moving: &impl Fn(PartId) -> bool, out: &mut Vec<Gap>) {
        let any_moving = sibs.iter().any(|s| moving(s.part.id));
        if any_moving {
            for s in sibs {
                let id = s.part.id;
                // captures (idea kind) and finished work are legitimately quiet.
                if s.part.kind != Kind::Idea && s.part.lifecycle != Lifecycle::Done && !moving(id) {
                    out.push(Gap {
                        kind: GapKind::QuietZone,
                        node: id,
                        detail: "no activity in 14 days while nearby work moves".into(),
                    });
                }
            }
        }
        for s in sibs {
            quiet_zone_group(&s.children, moving, out);
        }
    }

    // ---- committed-never-started: an old todo task with zero sessions ----
    for p in parts {
        if p.kind == Kind::Task
            && p.lifecycle == Lifecycle::Todo
            && p.status_at_secs > 0
            && now.saturating_sub(p.status_at_secs) >= COMMITTED_STALE_SECS
            && activity.get(&p.id).copied().unwrap_or(0) == 0
        {
            out.push(Gap {
                kind: GapKind::CommittedNeverStarted,
                node: p.id,
                detail: "committed 45+ days ago, never started".into(),
            });
        }
    }

    // ---- stale cluster: ≥2 stale nodes sharing their TIGHTEST ancestor ----
    // Flag the lowest node whose subtree holds ≥2 stale, but only when no single
    // child already aggregates the whole cluster (else the cluster is deeper —
    // flag the child). This lands the finding at the real branch point, once.
    let mut stale_counts: HashMap<PartId, usize> = HashMap::new();
    for r in &tree {
        stale_subtree_count(r, stale, &mut stale_counts);
    }
    for r in &tree {
        stale_cluster_walk(r, &stale_counts, &mut out);
    }
    fn stale_subtree_count(
        node: &TreeNode,
        stale: &std::collections::HashSet<PartId>,
        out: &mut HashMap<PartId, usize>,
    ) -> usize {
        let mut c = usize::from(stale.contains(&node.part.id));
        for ch in &node.children {
            c += stale_subtree_count(ch, stale, out);
        }
        out.insert(node.part.id, c);
        c
    }
    fn stale_cluster_walk(node: &TreeNode, counts: &HashMap<PartId, usize>, out: &mut Vec<Gap>) {
        let here = counts.get(&node.part.id).copied().unwrap_or(0);
        let child_absorbs = node
            .children
            .iter()
            .any(|c| counts.get(&c.part.id).copied().unwrap_or(0) >= STALE_CLUSTER_MIN);
        if here >= STALE_CLUSTER_MIN && !child_absorbs {
            out.push(Gap {
                kind: GapKind::StaleCluster,
                node: node.part.id,
                detail: format!("{here} stale items cluster here — worth a sweep"),
            });
        }
        // If a child absorbs the cluster, recurse to place it at the branch point;
        // once flagged here, deeper nodes hold <MIN so recursion is harmless.
        for c in &node.children {
            stale_cluster_walk(c, counts, out);
        }
    }

    // ---- undermapped: an anchored node with many files but ≤2 children ----
    let child_count = |id: PartId| parts.iter().filter(|p| p.parent_id == Some(id)).count();
    for p in parts {
        if p.kind == Kind::Idea {
            continue; // the idea tray / captures are not undermapped areas.
        }
        let files = anchor_file_counts.get(&p.id).copied().unwrap_or(0);
        if !p.anchors.is_empty()
            && files >= UNDERMAPPED_MIN_ANCHORED
            && child_count(p.id) <= UNDERMAPPED_MAX_CHILDREN
        {
            out.push(Gap {
                kind: GapKind::Undermapped,
                node: p.id,
                detail: format!("anchors {files} files but has ≤2 children — worth breaking down"),
            });
        }
    }

    // ---- quiet-building drift: reuse the shipped `quiet_building` ----
    for id in quiet_building(parts, activity, now, QUIET_BUILDING_DRIFT_SECS) {
        out.push(Gap {
            kind: GapKind::QuietBuildingDrift,
            node: id,
            detail: "marked building but quiet for 14+ days".into(),
        });
    }

    out.sort_by(|a, b| {
        a.kind
            .ordinal()
            .cmp(&b.kind.ordinal())
            .then(a.node.cmp(&b.node))
    });
    out
}

// ===========================================================================
// 6. WARM BAND — the resumption trail (docs/019 commitment 5, verify amendment).
// ===========================================================================
//
// Activity within 7d that is NOT live renders a warm/dashed marker, so after
// the 48h building window expires "where was I" is still on the glass — buried
// nowhere. Distinct from live building (which lights the solid glyph).

/// Activity this recent (and not live) draws the warm band.
pub const WARM_BAND_SECS: u64 = 7 * DAY;

/// The warm state — carries the age for a "touched Nh ago" whisper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WarmState {
    pub age_secs: u64,
}

/// Warm iff there was real activity within 7d AND the node is NOT currently
/// live (a live node is `building`, not `warm` — the two never render alike).
/// `last_activity_secs == 0` (never touched) is never warm.
pub fn warm_band(last_activity_secs: u64, now: u64, is_live: bool) -> Option<WarmState> {
    if is_live || last_activity_secs == 0 {
        return None;
    }
    let age = now.saturating_sub(last_activity_secs);
    (age <= WARM_BAND_SECS).then_some(WarmState { age_secs: age })
}

// ===========================================================================
// 7. NEEDS-YOU — the 7d anti-decay escalation (docs/019 status).
// ===========================================================================
//
// needs-you is an orthogonal FLAG, not a lifecycle. Unlike a proposal (which may
// expire), a user-set needs-you ANTI-decays: an unanswered question does not
// fade — at 7d it ESCALATES, because the map absorbing a decision means holding
// onto it until answered, not letting it rot.

/// After this long unanswered, a needs-you escalates (never fades).
pub const NEEDS_YOU_ESCALATE_SECS: u64 = 7 * DAY;

/// Has this needs-you gone unanswered long enough to escalate? `question_set_secs`
/// is when the user set the blocking question; escalation is at/after 7d.
pub fn needs_you_escalated(question_set_secs: u64, now: u64) -> bool {
    now.saturating_sub(question_set_secs) >= NEEDS_YOU_ESCALATE_SECS
}

// ===========================================================================
// 8. GLANCE STRINGS + TRIAGE WALK — plain-words render glue for the UI pass.
// ===========================================================================
//
// Pure formatting/order helpers the wiring pass reads. Kept here (not in
// main.rs) so the "where was I" whisper and the sweep's j/k order are unit-
// tested without a render tree — same reasoning as the rest of the module.

/// A quiet "where was I" whisper for the warm band (docs/019 resumption): the
/// age of the last touch in PLAIN WORDS. Never the design vocabulary.
pub fn warm_age_label(age_secs: u64) -> String {
    if age_secs < 60 * 60 {
        "seen just now".to_string()
    } else if age_secs < DAY {
        format!("seen {}h ago", age_secs / (60 * 60))
    } else {
        let d = age_secs / DAY;
        format!("seen {d} day{} ago", if d == 1 { "" } else { "s" })
    }
}

/// The triage sweep's j/k walk order (docs/019 T7): pre-order over the tree as
/// it reads top-to-bottom, so `j` steps to the next node the eye would meet and
/// `k` to the previous. Ideas (captures) are included — the sweep can ratify or
/// stamp them too. Deterministic: children follow their parent in sibling order.
pub fn triage_walk(tree: &[TreeNode]) -> Vec<PartId> {
    fn go(nodes: &[TreeNode], out: &mut Vec<PartId>) {
        for n in nodes {
            out.push(n.part.id);
            go(&n.children, out);
        }
    }
    let mut out = Vec::new();
    go(tree, &mut out);
    out
}

/// Step the triage cursor by `delta` (+1 = `j`/next, -1 = `k`/prev) through the
/// walk order, clamped at the ends (a sweep never wraps — the user always
/// knows where the top and bottom are). `None` cursor starts at the first node.
pub fn triage_step(order: &[PartId], cursor: Option<PartId>, delta: i64) -> Option<PartId> {
    if order.is_empty() {
        return None;
    }
    let pos = cursor.and_then(|c| order.iter().position(|id| *id == c));
    let next = match pos {
        None => 0,
        Some(i) => {
            let n = i as i64 + delta;
            n.clamp(0, order.len() as i64 - 1) as usize
        }
    };
    Some(order[next])
}

// ===========================================================================
// TESTS
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn mk(id: PartId, parent: Option<PartId>, kind: Kind, lc: Lifecycle) -> Part {
        Part {
            id,
            parent_id: parent,
            name: format!("n{id}"),
            kind,
            lifecycle: lc,
            sort_order: id as f64,
            ..Part::default()
        }
    }

    // ---- 1. ROLLUP ----------------------------------------------------------

    #[test]
    fn rollup_done_pct_counts_tasks_across_the_whole_map_including_roots() {
        // root task done + area>[task done, task todo] + a root-level todo task.
        let parts = vec![
            mk(1, None, Kind::Task, Lifecycle::Done),
            mk(2, None, Kind::Area, Lifecycle::Todo),
            mk(3, Some(2), Kind::Task, Lifecycle::Done),
            mk(4, Some(2), Kind::Task, Lifecycle::Todo),
            mk(5, None, Kind::Task, Lifecycle::Todo),
        ];
        let roots = build_tree(&parts);
        // tasks counted: 1(done),3(done),4(todo),5(todo) => 2/4 = 50%.
        let (done, total) = map_done(&roots);
        assert_eq!((done, total), (2, 4), "root-level tasks are not dropped");
        let r = rollup(&roots, &[10, 11], &[12], &[4, 5, 20, 21], 2);
        assert_eq!(r.done_pct, 50);
        assert_eq!((r.working, r.drifted, r.ready, r.suggestions), (2, 1, 4, 2));
    }

    #[test]
    fn rollup_empty_map_is_zero_percent_not_a_panic() {
        let r = rollup(&[], &[], &[], &[], 0);
        assert_eq!(r.done_pct, 0);
        assert_eq!(
            r.line(),
            "0% done · 0 working · 0 ready to start · 0 suggestions"
        );
    }

    #[test]
    fn rollup_line_is_plain_words_never_the_design_vocabulary() {
        let r = Rollup {
            done_pct: 41,
            working: 2,
            drifted: 1,
            ready: 4,
            suggestions: 2,
        };
        let line = r.line();
        assert_eq!(
            line,
            "41% done · 2 working (+1 drifted) · 4 ready to start · 2 suggestions"
        );
        for banned in [
            "CAN",
            "ARE",
            "SHOULD",
            "epistemic",
            "changeset",
            "hollow",
            "observed",
        ] {
            assert!(
                !line.contains(banned),
                "ruling 13: '{banned}' must not ship as UI copy"
            );
        }
    }

    #[test]
    fn rollup_line_drops_the_drifted_parenthetical_when_zero_and_singularizes_suggestion() {
        let r = Rollup {
            done_pct: 100,
            working: 1,
            drifted: 0,
            ready: 0,
            suggestions: 1,
        };
        assert_eq!(
            r.line(),
            "100% done · 1 working · 0 ready to start · 1 suggestion"
        );
    }

    #[test]
    fn rollup_done_pct_rounds_to_nearest() {
        // 1 of 3 done = 33.33% -> 33.
        let parts = vec![
            mk(1, None, Kind::Task, Lifecycle::Done),
            mk(2, None, Kind::Task, Lifecycle::Todo),
            mk(3, None, Kind::Task, Lifecycle::Todo),
        ];
        let roots = build_tree(&parts);
        assert_eq!(rollup(&roots, &[], &[], &[], 0).done_pct, 33);
    }

    // ---- 2. NEXT-UP ---------------------------------------------------------

    #[test]
    fn next_up_only_dispatch_ready_task_nodes() {
        let mut parts = vec![
            mk(1, None, Kind::Task, Lifecycle::Todo), // ready
            mk(2, None, Kind::Area, Lifecycle::Todo), // area: never
            mk(3, None, Kind::Task, Lifecycle::Done), // done: never
            mk(4, None, Kind::Task, Lifecycle::Idea), // idea: never
            mk(5, None, Kind::Task, Lifecycle::Todo), // stale: never
        ];
        parts[4].stale = true;
        let got = next_up(
            &parts,
            &HashMap::new(),
            &[],
            &HashSet::new(),
            &HashSet::new(),
            NEXT_UP_CAP,
        );
        assert_eq!(got, vec![1]);
    }

    #[test]
    fn next_up_blocked_set_excludes_candidates() {
        let parts = vec![
            mk(1, None, Kind::Task, Lifecycle::Todo),
            mk(2, None, Kind::Task, Lifecycle::Todo),
        ];
        let blocked: HashSet<PartId> = [1].into_iter().collect();
        assert_eq!(
            next_up(
                &parts,
                &HashMap::new(),
                &[],
                &HashSet::new(),
                &blocked,
                NEXT_UP_CAP
            ),
            vec![2]
        );
    }

    #[test]
    fn next_up_ranks_star_then_attention_then_warmth_then_age() {
        // all four are ready todos; each should win its own tier.
        let mut parts = vec![
            mk(1, None, Kind::Task, Lifecycle::Todo), // warm (activity 900)
            mk(2, None, Kind::Task, Lifecycle::Todo), // attention
            mk(3, None, Kind::Task, Lifecycle::Todo), // starred
            mk(4, None, Kind::Task, Lifecycle::Todo), // cold, but oldest todo
        ];
        parts[0].status_at_secs = 500;
        parts[1].status_at_secs = 400;
        parts[2].status_at_secs = 300;
        parts[3].status_at_secs = 100; // oldest
        let activity: HashMap<PartId, u64> = [(1, 900u64)].into_iter().collect();
        let attention: HashSet<PartId> = [2].into_iter().collect();
        let got = next_up(
            &parts,
            &activity,
            &[3],
            &attention,
            &HashSet::new(),
            NEXT_UP_CAP,
        );
        // star(3) > attention(2) > warmth(1) > age(4).
        assert_eq!(got, vec![3, 2, 1, 4]);
    }

    #[test]
    fn next_up_preserves_star_pin_order_over_other_signals() {
        let parts = vec![
            mk(1, None, Kind::Task, Lifecycle::Todo),
            mk(2, None, Kind::Task, Lifecycle::Todo),
            mk(3, None, Kind::Task, Lifecycle::Todo),
        ];
        // node 3 has attention + warmth, but pins order the starred rows first.
        let activity: HashMap<PartId, u64> = [(3, 999u64)].into_iter().collect();
        let attention: HashSet<PartId> = [3].into_iter().collect();
        let got = next_up(
            &parts,
            &activity,
            &[2, 1],
            &attention,
            &HashSet::new(),
            NEXT_UP_CAP,
        );
        assert_eq!(
            got,
            vec![2, 1, 3],
            "pin order (2 then 1) wins; 3 falls below"
        );
    }

    #[test]
    fn next_up_caps_the_render_list() {
        let parts: Vec<Part> = (1..=10)
            .map(|i| mk(i, None, Kind::Task, Lifecycle::Todo))
            .collect();
        let got = next_up(
            &parts,
            &HashMap::new(),
            &[],
            &HashSet::new(),
            &HashSet::new(),
            3,
        );
        assert_eq!(got.len(), 3);
        assert_eq!(got, vec![1, 2, 3], "tie-broken by id ascending");
    }

    // ---- 3. STAR PINS -------------------------------------------------------

    #[test]
    fn star_toggle_pins_and_unpins() {
        assert_eq!(star_toggle(&[], 5, STAR_CAP), vec![5]);
        assert_eq!(
            star_toggle(&[5], 5, STAR_CAP),
            Vec::<PartId>::new(),
            "second gesture unpins"
        );
        assert_eq!(star_toggle(&[5, 6], 7, STAR_CAP), vec![5, 6, 7]);
    }

    #[test]
    fn star_toggle_a_fourth_unpins_the_oldest() {
        // front is oldest; pinning a 4th drops the front.
        assert_eq!(star_toggle(&[1, 2, 3], 4, STAR_CAP), vec![2, 3, 4]);
    }

    #[test]
    fn star_toggle_unpin_from_the_middle_keeps_order() {
        assert_eq!(star_toggle(&[1, 2, 3], 2, STAR_CAP), vec![1, 3]);
    }

    // ---- 4. ONE SUMMONS -----------------------------------------------------

    #[test]
    fn summons_needs_you_beats_review_and_start() {
        let ny = vec![(9, "ship the beta?".to_string(), 100u64)];
        let s = summons(&ny, Some((7, "Restructure".into())), Some(3));
        assert_eq!(
            s,
            Summons::NeedsYou {
                node: 9,
                question: "ship the beta?".into(),
                age_secs: 100
            }
        );
    }

    #[test]
    fn summons_picks_the_oldest_unanswered_needs_you_verbatim() {
        let ny = vec![
            (1, "A?".to_string(), 50u64),
            (2, "B?".to_string(), 200u64),
            (3, "C?".to_string(), 120u64),
        ];
        assert_eq!(
            summons(&ny, None, None),
            Summons::NeedsYou {
                node: 2,
                question: "B?".into(),
                age_secs: 200
            }
        );
    }

    #[test]
    fn summons_needs_you_ties_break_on_lowest_node_id() {
        let ny = vec![
            (5, "five".to_string(), 100u64),
            (2, "two".to_string(), 100u64),
        ];
        assert_eq!(
            summons(&ny, None, None),
            Summons::NeedsYou {
                node: 2,
                question: "two".into(),
                age_secs: 100
            }
        );
    }

    #[test]
    fn summons_review_beats_start() {
        assert_eq!(
            summons(&[], Some((7, "Dissolve Tech".into())), Some(3)),
            Summons::Review {
                changeset: 7,
                title: "Dissolve Tech".into()
            }
        );
    }

    #[test]
    fn summons_start_when_only_ready_work() {
        assert_eq!(summons(&[], None, Some(42)), Summons::Start { node: 42 });
    }

    #[test]
    fn summons_none_is_a_valid_calm_state() {
        assert_eq!(summons(&[], None, None), Summons::None);
    }

    // ---- 5. GAP FINDERS -----------------------------------------------------

    #[test]
    fn gap_quiet_zone_flags_cold_node_beside_a_moving_sibling() {
        let parts = vec![
            mk(1, None, Kind::Area, Lifecycle::Todo),
            mk(2, Some(1), Kind::Task, Lifecycle::Todo), // moving
            mk(3, Some(1), Kind::Task, Lifecycle::Todo), // cold -> flagged
        ];
        let now = 100 * DAY;
        let activity: HashMap<PartId, u64> = [(2, now)].into_iter().collect(); // 3 never active
        let gaps = gap_findings(&parts, &activity, now, &HashSet::new(), &HashMap::new());
        assert_eq!(
            gaps.iter()
                .filter(|g| g.kind == GapKind::QuietZone)
                .map(|g| g.node)
                .collect::<Vec<_>>(),
            vec![3]
        );
    }

    #[test]
    fn gap_quiet_zone_silent_when_whole_group_is_cold() {
        // no sibling moves -> not a "zone gone quiet relative to peers".
        let parts = vec![
            mk(1, None, Kind::Task, Lifecycle::Todo),
            mk(2, None, Kind::Task, Lifecycle::Todo),
        ];
        let now = 100 * DAY;
        let gaps = gap_findings(
            &parts,
            &HashMap::new(),
            now,
            &HashSet::new(),
            &HashMap::new(),
        );
        assert!(gaps.iter().all(|g| g.kind != GapKind::QuietZone));
    }

    #[test]
    fn gap_quiet_zone_uses_subtree_liveness_not_just_the_row() {
        // area 1 row is never touched, but its child moves -> the area is NOT cold.
        let parts = vec![
            mk(1, None, Kind::Area, Lifecycle::Todo), // row cold, subtree hot
            mk(2, Some(1), Kind::Task, Lifecycle::Todo),
            mk(3, None, Kind::Task, Lifecycle::Todo), // truly cold root -> flagged
        ];
        let now = 100 * DAY;
        let activity: HashMap<PartId, u64> = [(2, now)].into_iter().collect();
        let gaps = gap_findings(&parts, &activity, now, &HashSet::new(), &HashMap::new());
        let zone: Vec<PartId> = gaps
            .iter()
            .filter(|g| g.kind == GapKind::QuietZone)
            .map(|g| g.node)
            .collect();
        assert_eq!(
            zone,
            vec![3],
            "area 1 spared by subtree liveness; root 3 flagged"
        );
    }

    #[test]
    fn gap_committed_never_started() {
        let mut parts = vec![
            mk(1, None, Kind::Task, Lifecycle::Todo), // old, no sessions -> flagged
            mk(2, None, Kind::Task, Lifecycle::Todo), // old, but has a session
            mk(3, None, Kind::Task, Lifecycle::Todo), // recent
        ];
        let now = 100 * DAY;
        parts[0].status_at_secs = now - 50 * DAY;
        parts[1].status_at_secs = now - 50 * DAY;
        parts[2].status_at_secs = now - 1 * DAY;
        let activity: HashMap<PartId, u64> = [(2, now)].into_iter().collect();
        let gaps = gap_findings(&parts, &activity, now, &HashSet::new(), &HashMap::new());
        assert_eq!(
            gaps.iter()
                .filter(|g| g.kind == GapKind::CommittedNeverStarted)
                .map(|g| g.node)
                .collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn gap_committed_ignores_rows_without_a_stamp() {
        // status_at_secs == 0: age unknown, so we don't cry wolf.
        let parts = vec![mk(1, None, Kind::Task, Lifecycle::Todo)];
        let now = 100 * DAY;
        let gaps = gap_findings(
            &parts,
            &HashMap::new(),
            now,
            &HashSet::new(),
            &HashMap::new(),
        );
        assert!(gaps
            .iter()
            .all(|g| g.kind != GapKind::CommittedNeverStarted));
    }

    #[test]
    fn gap_stale_cluster_flags_the_tightest_ancestor_once() {
        // area 1 > [area 2 > [stale 4, stale 5], task 3]. The cluster lives under 2.
        let parts = vec![
            mk(1, None, Kind::Area, Lifecycle::Todo),
            mk(2, Some(1), Kind::Area, Lifecycle::Todo),
            mk(3, Some(1), Kind::Task, Lifecycle::Todo),
            mk(4, Some(2), Kind::Task, Lifecycle::Todo),
            mk(5, Some(2), Kind::Task, Lifecycle::Todo),
        ];
        let stale: HashSet<PartId> = [4, 5].into_iter().collect();
        let gaps = gap_findings(&parts, &HashMap::new(), 100 * DAY, &stale, &HashMap::new());
        let cl: Vec<PartId> = gaps
            .iter()
            .filter(|g| g.kind == GapKind::StaleCluster)
            .map(|g| g.node)
            .collect();
        assert_eq!(
            cl,
            vec![2],
            "flagged at the branch point, not the root, not twice"
        );
    }

    #[test]
    fn gap_stale_cluster_branch_point_when_two_subtrees_each_hold_one() {
        // root 1 > [child 2 > stale 4, child 3 > stale 5]: neither child has 2,
        // so the tightest common ancestor is root 1.
        let parts = vec![
            mk(1, None, Kind::Area, Lifecycle::Todo),
            mk(2, Some(1), Kind::Area, Lifecycle::Todo),
            mk(3, Some(1), Kind::Area, Lifecycle::Todo),
            mk(4, Some(2), Kind::Task, Lifecycle::Todo),
            mk(5, Some(3), Kind::Task, Lifecycle::Todo),
        ];
        let stale: HashSet<PartId> = [4, 5].into_iter().collect();
        let gaps = gap_findings(&parts, &HashMap::new(), 100 * DAY, &stale, &HashMap::new());
        let cl: Vec<PartId> = gaps
            .iter()
            .filter(|g| g.kind == GapKind::StaleCluster)
            .map(|g| g.node)
            .collect();
        assert_eq!(cl, vec![1]);
    }

    #[test]
    fn gap_stale_cluster_silent_below_the_minimum() {
        let parts = vec![
            mk(1, None, Kind::Area, Lifecycle::Todo),
            mk(2, Some(1), Kind::Task, Lifecycle::Todo),
        ];
        let stale: HashSet<PartId> = [2].into_iter().collect();
        let gaps = gap_findings(&parts, &HashMap::new(), 100 * DAY, &stale, &HashMap::new());
        assert!(gaps.iter().all(|g| g.kind != GapKind::StaleCluster));
    }

    #[test]
    fn gap_undermapped_anchored_node_with_many_files_and_few_children() {
        let mut parts = vec![
            mk(1, None, Kind::Area, Lifecycle::Todo), // anchored, many files, 1 child
            mk(2, Some(1), Kind::Task, Lifecycle::Todo),
            mk(3, None, Kind::Area, Lifecycle::Todo), // anchored but many children
            mk(4, Some(3), Kind::Task, Lifecycle::Todo),
            mk(5, Some(3), Kind::Task, Lifecycle::Todo),
            mk(6, Some(3), Kind::Task, Lifecycle::Todo),
        ];
        parts[0].anchors = vec!["src/big/**".into()];
        parts[2].anchors = vec!["src/small/**".into()];
        let counts: HashMap<PartId, usize> = [(1, 20usize), (3, 20usize)].into_iter().collect();
        let gaps = gap_findings(&parts, &HashMap::new(), 100 * DAY, &HashSet::new(), &counts);
        let um: Vec<PartId> = gaps
            .iter()
            .filter(|g| g.kind == GapKind::Undermapped)
            .map(|g| g.node)
            .collect();
        assert_eq!(um, vec![1], "3 has >2 children so it's mapped enough");
    }

    #[test]
    fn gap_undermapped_needs_an_anchor() {
        // many files attributed but no anchor on the row -> not undermapped.
        let parts = vec![mk(1, None, Kind::Area, Lifecycle::Todo)];
        let counts: HashMap<PartId, usize> = [(1, 50usize)].into_iter().collect();
        let gaps = gap_findings(&parts, &HashMap::new(), 100 * DAY, &HashSet::new(), &counts);
        assert!(gaps.iter().all(|g| g.kind != GapKind::Undermapped));
    }

    #[test]
    fn gap_quiet_building_drift_reuses_quiet_building() {
        let mut parts = vec![
            mk(1, None, Kind::Task, Lifecycle::Building),
            mk(2, None, Kind::Task, Lifecycle::Building),
        ];
        parts[0].status_source = orchestrator_store::StatusSource::Seed;
        let now = 100 * DAY;
        // node 1 active recently; node 2 quiet -> drift.
        let activity: HashMap<PartId, u64> = [(1, now)].into_iter().collect();
        let gaps = gap_findings(&parts, &activity, now, &HashSet::new(), &HashMap::new());
        let drift: Vec<PartId> = gaps
            .iter()
            .filter(|g| g.kind == GapKind::QuietBuildingDrift)
            .map(|g| g.node)
            .collect();
        assert_eq!(drift, vec![2]);
    }

    #[test]
    fn gap_findings_are_sorted_deterministically_by_kind_then_node() {
        // engineer one of several kinds and assert the drawer order is stable.
        let mut parts = vec![
            mk(1, None, Kind::Area, Lifecycle::Todo), // undermapped
            mk(2, Some(1), Kind::Task, Lifecycle::Building), // quiet-building drift
            mk(10, None, Kind::Task, Lifecycle::Todo), // committed-never-started
        ];
        parts[0].anchors = vec!["a/**".into()];
        parts[2].status_at_secs = 1; // very old todo
        let now = 100 * DAY;
        let counts: HashMap<PartId, usize> = [(1, 30usize)].into_iter().collect();
        let gaps = gap_findings(&parts, &HashMap::new(), now, &HashSet::new(), &counts);
        let order: Vec<GapKind> = gaps.iter().map(|g| g.kind).collect();
        // committed(1) < undermapped(3) < quiet-building(4) by ordinal.
        let idx = |k: GapKind| order.iter().position(|x| *x == k);
        assert!(idx(GapKind::CommittedNeverStarted) < idx(GapKind::Undermapped));
        assert!(idx(GapKind::Undermapped) < idx(GapKind::QuietBuildingDrift));
    }

    #[test]
    fn gap_findings_never_color_a_node_they_are_only_drawer_entries() {
        // a node can carry MULTIPLE findings; the drawer lists each, none mutate.
        let mut parts = vec![mk(1, None, Kind::Task, Lifecycle::Building)];
        parts[0].status_at_secs = 1;
        let before = parts.clone();
        let now = 100 * DAY;
        let _ = gap_findings(
            &parts,
            &HashMap::new(),
            now,
            &HashSet::new(),
            &HashMap::new(),
        );
        assert_eq!(
            parts, before,
            "gap_findings is pure — it never mutates a part"
        );
    }

    // ---- 6. WARM BAND -------------------------------------------------------

    #[test]
    fn warm_band_within_7d_and_not_live() {
        let now = 100 * DAY;
        assert_eq!(
            warm_band(now - 3 * DAY, now, false),
            Some(WarmState { age_secs: 3 * DAY })
        );
    }

    #[test]
    fn warm_band_none_when_live() {
        let now = 100 * DAY;
        assert_eq!(
            warm_band(now - 1 * DAY, now, true),
            None,
            "live is building, not warm"
        );
    }

    #[test]
    fn warm_band_none_past_the_window_or_never_active() {
        let now = 100 * DAY;
        assert_eq!(warm_band(now - 8 * DAY, now, false), None);
        assert_eq!(warm_band(0, now, false), None, "never active is not warm");
    }

    #[test]
    fn warm_band_edge_exactly_7d_is_still_warm() {
        let now = 100 * DAY;
        assert_eq!(
            warm_band(now - WARM_BAND_SECS, now, false),
            Some(WarmState {
                age_secs: WARM_BAND_SECS
            })
        );
    }

    // ---- 7. NEEDS-YOU escalation -------------------------------------------

    #[test]
    fn needs_you_escalates_at_7d_and_not_before() {
        let now = 100 * DAY;
        assert!(!needs_you_escalated(now - 6 * DAY, now));
        assert!(
            needs_you_escalated(now - 7 * DAY, now),
            "7d anti-decay: escalates, never fades"
        );
        assert!(needs_you_escalated(now - 30 * DAY, now));
    }

    #[test]
    fn needs_you_escalation_is_clock_skew_safe() {
        // a question stamped in the future must not underflow into escalation.
        assert!(!needs_you_escalated(200, 100));
    }

    // ---- 8. GLANCE STRINGS + TRIAGE WALK -----------------------------------

    #[test]
    fn warm_age_label_is_plain_words_by_magnitude() {
        assert_eq!(warm_age_label(0), "seen just now");
        assert_eq!(warm_age_label(30 * 60), "seen just now");
        assert_eq!(warm_age_label(3 * 60 * 60), "seen 3h ago");
        assert_eq!(warm_age_label(DAY), "seen 1 day ago");
        assert_eq!(warm_age_label(3 * DAY), "seen 3 days ago");
    }

    #[test]
    fn triage_walk_is_preorder_top_to_bottom() {
        // area 1 > [task 2, area 3 > [task 4]], root task 5.
        let parts = vec![
            mk(1, None, Kind::Area, Lifecycle::Todo),
            mk(2, Some(1), Kind::Task, Lifecycle::Todo),
            mk(3, Some(1), Kind::Area, Lifecycle::Todo),
            mk(4, Some(3), Kind::Task, Lifecycle::Todo),
            mk(5, None, Kind::Task, Lifecycle::Todo),
        ];
        let tree = build_tree(&parts);
        assert_eq!(triage_walk(&tree), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn triage_step_clamps_at_both_ends_and_starts_at_top() {
        let order = vec![10, 20, 30];
        assert_eq!(triage_step(&order, None, 1), Some(10), "no cursor → top");
        assert_eq!(triage_step(&order, Some(10), 1), Some(20));
        assert_eq!(
            triage_step(&order, Some(30), 1),
            Some(30),
            "j past bottom stays"
        );
        assert_eq!(
            triage_step(&order, Some(10), -1),
            Some(10),
            "k past top stays"
        );
        assert_eq!(triage_step(&[], None, 1), None, "empty map has no cursor");
    }
}
