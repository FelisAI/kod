//! The DESIGN-tree model (docs/016) — pure types shared by the SQLite store
//! and the GUI. The TREE (areas/parts) is owned data; STATUS is split into a
//! stored lifecycle ASSERTION + derived live/freshness (composed at render).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub type PartId = i64;

/// The stored lifecycle ASSERTION — set only by a human, an accepted decision,
/// or an accepted agent proposal. NEVER inferred from code (docs/016). Live
/// (building/needs-you) and freshness (stale/unverified) are derived elsewhere.
///
/// `Building` is DERIVED-ONLY as of docs/019: never stored, computed per frame
/// from live dispatch/declared session links. The variant stays deserializable
/// forever (old journal rows serialize it — removing it would silently corrupt
/// undo history via `unwrap_or_default`); every write path coerces it through
/// `assertable()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lifecycle {
    Idea,
    Todo,
    Building,
    Done,
}

impl Lifecycle {
    pub fn as_str(self) -> &'static str {
        match self {
            Lifecycle::Idea => "idea",
            Lifecycle::Todo => "todo",
            Lifecycle::Building => "building",
            Lifecycle::Done => "done",
        }
    }
    pub fn parse(s: &str) -> Lifecycle {
        match s {
            "done" => Lifecycle::Done,
            "building" => Lifecycle::Building,
            "todo" => Lifecycle::Todo,
            _ => Lifecycle::Idea,
        }
    }
    /// What may actually be STORED (docs/019 commitment 2): `building` is
    /// derived from live session links, so asserting it maps to `todo` — the
    /// underlying commitment the liveness overlay draws on top of.
    pub fn assertable(self) -> Lifecycle {
        match self {
            Lifecycle::Building => Lifecycle::Todo,
            lc => lc,
        }
    }
    /// THE glyph-click cycle (docs/019) — user-settable states only;
    /// `building` is unreachable by hand. One canonical copy: main.rs and
    /// outlinepane.rs both call this (review: a stale duplicate cycled through
    /// Building, which write-coercion turned into a permanent no-op click).
    pub fn cycle(self) -> Lifecycle {
        match self {
            Lifecycle::Idea => Lifecycle::Todo,
            Lifecycle::Todo => Lifecycle::Done,
            Lifecycle::Building => Lifecycle::Done, // legacy stored rows advance out
            Lifecycle::Done => Lifecycle::Idea,
        }
    }
}

/// The node KIND (docs/019 commitment 2): the all-todo desert was a type error
/// — structural areas wearing task lifecycles. Areas roll up arithmetic over
/// descendant tasks; tasks assert lifecycle; ideas float unratified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Kind {
    Area,
    #[default]
    Task,
    Idea,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Area => "area",
            Kind::Task => "task",
            Kind::Idea => "idea",
        }
    }
    pub fn parse(s: &str) -> Kind {
        match s {
            "area" => Kind::Area,
            "idea" => Kind::Idea,
            _ => Kind::Task,
        }
    }
    /// THE kind-chip click cycle (docs/019 slice 1b): the one-gesture backfill
    /// repair — a node misfiled by the heuristic (has-children→area) flips
    /// with clicks, never a form. area → task → idea → area, so any kind is
    /// ≤2 gestures from any other. One canonical copy, like Lifecycle::cycle.
    pub fn cycle(self) -> Kind {
        match self {
            Kind::Area => Kind::Task,
            Kind::Task => Kind::Idea,
            Kind::Idea => Kind::Area,
        }
    }
}

/// Who asserted the current lifecycle (provenance; drives the UI's confidence
/// encoding and the maintenance loop).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatusSource {
    Seed,   // the E1 extraction proposed it (always idea/todo — never done)
    User,   // the user asserted it directly
    Accept, // set by accepting a decision/diff
    Agent,  // an agent proposal the user accepted
    Triage, // a triage-sweep confirmation (docs/019 T7): a human assertion, but
            // stamped distinctly so audits/the stale detector can weight a
            // rapid sweep-tap below a deliberate hand-set status.
}

impl StatusSource {
    pub fn as_str(self) -> &'static str {
        match self {
            StatusSource::Seed => "seed",
            StatusSource::User => "user",
            StatusSource::Accept => "accept",
            StatusSource::Agent => "agent",
            StatusSource::Triage => "human:triage",
        }
    }
    pub fn parse(s: &str) -> StatusSource {
        match s {
            "user" => StatusSource::User,
            "accept" => StatusSource::Accept,
            "agent" => StatusSource::Agent,
            "human:triage" => StatusSource::Triage,
            _ => StatusSource::Seed,
        }
    }
}

/// A stored part (one row). The tree is reconstructed from `parent_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Part {
    pub id: PartId,
    pub parent_id: Option<PartId>,
    pub name: String,
    pub detail: String,
    pub lifecycle: Lifecycle,
    pub status_source: StatusSource,
    pub status_at_secs: u64,
    /// derived freshness cache: the anchored code changed since `status_at`, or
    /// the assertion aged out — confidence LOWERED, never the lifecycle.
    pub stale: bool,
    pub stale_reason: Option<String>,
    pub sort_order: f64,
    /// code paths/globs this part maps to (for auto-attribution + drift).
    pub anchors: Vec<String>,
    /// persisted brain-map canvas position (spatial memory — never auto-moved
    /// once set; None = auto-placed until the user drags it). #10.
    pub map_x: Option<f64>,
    pub map_y: Option<f64>,
    /// area | task | idea (docs/019): drives status semantics — areas roll up,
    /// tasks assert, ideas float.
    #[serde(default)]
    pub kind: Kind,
    /// full markdown body (docs/019); `detail` is its derived first line.
    /// Loaded as COALESCE(detail_md, detail) so pre-migration rows read back
    /// their one-liner.
    #[serde(default)]
    pub detail_md: String,
    /// provenance quad (docs/019 commitment 3, the "why is this here?" answer):
    /// who created it + the cited file/verbatim quote/rationale for a machine
    /// node. Loaded so a Remove inverse can restore it on ⌘Z (review: undo of a
    /// machine node dropped its citation). `created_by` defaults 'legacy'.
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub source_file: Option<String>,
    #[serde(default)]
    pub source_quote: Option<String>,
    #[serde(default)]
    pub rationale: Option<String>,
}

impl Default for Part {
    fn default() -> Part {
        Part {
            id: 0,
            parent_id: None,
            name: String::new(),
            detail: String::new(),
            lifecycle: Lifecycle::Todo,
            status_source: StatusSource::User,
            status_at_secs: 0,
            stale: false,
            stale_reason: None,
            sort_order: 0.0,
            anchors: vec![],
            map_x: None,
            map_y: None,
            kind: Kind::Task,
            detail_md: String::new(),
            created_by: "legacy".into(),
            source_file: None,
            source_quote: None,
            rationale: None,
        }
    }
}

/// How an op references a part (lets one diff add a whole subtree: children
/// reference their parent by a temp id before it exists).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PartRef {
    Id(PartId),
    Temp(String),
    Root,
}

/// The universal diff currency (docs/016): the E1 seed, drift proposals, the
/// ⌘K conversation, and agent results ALL reduce to a `Vec<DiffOp>` rendered
/// as the same accept-diff card and applied in one transaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DiffOp {
    Add {
        temp: String,
        parent: PartRef,
        name: String,
        #[serde(default)]
        detail: String,
        lifecycle: Lifecycle,
        #[serde(default)]
        anchors: Vec<String>,
        /// docs/019: area | task | idea. serde default (Task) keeps every
        /// pre-019 journal row deserializable.
        #[serde(default)]
        kind: Kind,
        /// full markdown body; None = same as `detail`. Carried on Remove
        /// inverses so undo restores prose, not just the one-liner.
        #[serde(default)]
        detail_md: Option<String>,
        /// explicit position among siblings; None = append (next_order).
        /// Carried on Remove inverses so a multi-child subtree undo restores
        /// the exact sibling ORDER (review: reversed replay + next_order
        /// re-added children last-first).
        #[serde(default)]
        sort_order: Option<f64>,
        /// docs/019 commitment 3: the provenance quad rides the Add so an
        /// accepted machine node lands with its "why is this here?" answer.
        /// serde-default Option keeps every pre-019 journal row + literal valid
        /// (human/breakdown/undo Adds carry None). `source_file` is a REPO file
        /// path (a part_note never satisfies the evidence bar); `source_quote`
        /// is the verbatim span the Rust verifier string-matched.
        #[serde(default)]
        source_file: Option<String>,
        #[serde(default)]
        source_quote: Option<String>,
        #[serde(default)]
        rationale: Option<String>,
    },
    SetStatus {
        id: PartId,
        lifecycle: Lifecycle,
        source: StatusSource,
    },
    Rename {
        id: PartId,
        name: String,
        detail: String,
    },
    Move {
        id: PartId,
        parent: PartRef,
        sort_order: f64,
    },
    Remove {
        id: PartId,
    },
    /// Edit the markdown body (docs/019). The stored one-liner (`detail`) is
    /// re-derived from the first non-empty line on apply.
    SetDetail {
        id: PartId,
        detail_md: String,
    },
    /// Flip a node's kind (docs/019) — one-gesture repair for backfill misses.
    SetKind {
        id: PartId,
        kind: Kind,
    },
}

impl DiffOp {
    /// A short human label for the accept-diff card.
    pub fn summary(&self, name_of: impl Fn(PartId) -> String) -> String {
        match self {
            DiffOp::Add { name, .. } => format!("+ area “{name}”"),
            DiffOp::SetStatus { id, lifecycle, .. } => {
                format!("→ {} is {}", name_of(*id), lifecycle.as_str())
            }
            DiffOp::Rename { id, name, .. } => format!("rename {} → {name}", name_of(*id)),
            DiffOp::Move { id, .. } => format!("move {}", name_of(*id)),
            DiffOp::Remove { id } => format!("− remove {}", name_of(*id)),
            DiffOp::SetDetail { id, .. } => format!("✎ describe {}", name_of(*id)),
            DiffOp::SetKind { id, kind } => format!("⊙ {} becomes {}", name_of(*id), kind.as_str()),
        }
    }
}

/// First non-empty line of a markdown body — the derived one-liner (`detail`)
/// shown on canvas cards.
pub fn first_line(md: &str) -> String {
    md.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Replace the first non-empty line of a markdown body (used by Rename to
/// keep the `detail = first_line(detail_md)` invariant — review: Rename
/// updated `detail` alone and the two silently diverged). Empty body → the
/// new line becomes the body.
pub fn replace_first_line(md: &str, new_line: &str) -> String {
    let mut lines: Vec<&str> = md.lines().collect();
    match lines.iter().position(|l| !l.trim().is_empty()) {
        Some(i) => {
            lines[i] = new_line;
            lines.join("\n")
        }
        None => new_line.to_string(),
    }
}

/// A node in the rendered tree (built from flat `Part` rows for the GUI).
#[derive(Debug, Clone)]
pub struct TreeNode {
    pub part: Part,
    pub children: Vec<TreeNode>,
}

/// Build the nested tree from flat rows, ordered by `sort_order`.
pub fn build_tree(parts: &[Part]) -> Vec<TreeNode> {
    fn children_of(parent: Option<PartId>, parts: &[Part]) -> Vec<TreeNode> {
        let mut kids: Vec<&Part> = parts.iter().filter(|p| p.parent_id == parent).collect();
        kids.sort_by(|a, b| {
            a.sort_order
                .partial_cmp(&b.sort_order)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        kids.into_iter()
            .map(|p| TreeNode {
                part: p.clone(),
                children: children_of(Some(p.id), parts),
            })
            .collect()
    }
    children_of(None, parts)
}

/// Parent progress = done-ratio of descendants (no fabricated per-leaf %).
pub fn done_ratio(node: &TreeNode) -> f32 {
    let mut done = 0;
    let mut total = 0;
    fn walk(n: &TreeNode, done: &mut i32, total: &mut i32) {
        for c in &n.children {
            *total += 1;
            if c.part.lifecycle == Lifecycle::Done {
                *done += 1;
            }
            walk(c, done, total);
        }
    }
    walk(node, &mut done, &mut total);
    if total == 0 {
        if node.part.lifecycle == Lifecycle::Done {
            1.0
        } else {
            0.0
        }
    } else {
        done as f32 / total as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(id: PartId, parent: Option<PartId>, lc: Lifecycle, order: f64) -> Part {
        Part {
            id,
            parent_id: parent,
            name: format!("p{id}"),
            lifecycle: lc,
            status_source: StatusSource::Seed,
            sort_order: order,
            ..Part::default()
        }
    }

    #[test]
    fn builds_ordered_nested_tree() {
        let parts = vec![
            part(1, None, Lifecycle::Building, 2.0),
            part(2, None, Lifecycle::Done, 1.0),
            part(3, Some(1), Lifecycle::Done, 1.0),
        ];
        let tree = build_tree(&parts);
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].part.id, 2); // sort_order 1 first
        assert_eq!(tree[1].part.id, 1);
        assert_eq!(tree[1].children.len(), 1);
        assert_eq!(tree[1].children[0].part.id, 3);
    }

    #[test]
    fn done_ratio_from_children() {
        let parts = vec![
            part(1, None, Lifecycle::Building, 1.0),
            part(2, Some(1), Lifecycle::Done, 1.0),
            part(3, Some(1), Lifecycle::Todo, 2.0),
        ];
        let tree = build_tree(&parts);
        assert_eq!(done_ratio(&tree[0]), 0.5);
    }

    #[test]
    fn quiet_building_flags_only_stale_building_parts() {
        let parts = vec![
            part(4, None, Lifecycle::Building, 4.0), // never active (unsorted input on purpose)
            part(1, None, Lifecycle::Building, 1.0), // active long ago
            part(2, None, Lifecycle::Building, 2.0), // recently active
            part(3, None, Lifecycle::Done, 3.0),     // old activity but not Building
            part(5, None, Lifecycle::Todo, 5.0),     // never active but not Building
        ];
        let mut act = HashMap::new();
        act.insert(1, 100u64);
        act.insert(2, 950);
        act.insert(3, 100);
        let now = 1000u64;
        assert_eq!(
            quiet_building(&parts, &act, now, 200),
            vec![1, 4],
            "sorted by id"
        );
        // exactly at threshold is NOT quiet (strictly older-than): 1 is 900s old
        assert_eq!(quiet_building(&parts, &act, now, 900), vec![4]);
        // never-active folds to 0, so even it ages out of a huge threshold
        assert_eq!(
            quiet_building(&parts, &act, now, 1000),
            Vec::<PartId>::new()
        );
    }

    #[test]
    fn lifecycle_roundtrips() {
        for lc in [
            Lifecycle::Idea,
            Lifecycle::Todo,
            Lifecycle::Building,
            Lifecycle::Done,
        ] {
            assert_eq!(Lifecycle::parse(lc.as_str()), lc);
        }
    }

    /// docs/019 slice 1c auto-seed: the husk detector fires on the exact
    /// `build_aspect_migration` artifact and NOTHING else — a user-meant
    /// "Tech" area, a childless node, or a nested Tech are all left alone.
    #[test]
    fn dissolve_tech_target_finds_only_the_husk() {
        let husk = |detail: &str| -> Part {
            Part {
                id: 1,
                name: "TECH".into(),
                detail: detail.into(),
                kind: Kind::Area,
                ..Part::default()
            }
        };
        // the real husk: root "Tech" + the migration's words + children (app id 25).
        let mut mess = vec![husk("the codebase — one aspect of the product")];
        mess.push(part(2, Some(1), Lifecycle::Todo, 1.0));
        mess.push(part(3, Some(1), Lifecycle::Todo, 2.0));
        assert_eq!(
            dissolve_tech_target(&mess),
            Some(1),
            "case-insensitive name + migration words + children"
        );

        // a real "Tech" area the user MEANT (no migration words) is not a husk.
        let mut real = vec![Part {
            id: 1,
            name: "Tech".into(),
            detail: "our platform team".into(),
            ..Part::default()
        }];
        real.push(part(2, Some(1), Lifecycle::Todo, 1.0));
        assert_eq!(
            dissolve_tech_target(&real),
            None,
            "no migration words → not the husk"
        );

        // a user-meant Tech whose description merely MENTIONS the codebase
        // must NOT trip the detector (review: the false-positive we must avoid).
        let mut user = vec![Part {
            id: 1,
            name: "Tech".into(),
            detail: "owns our codebase and CI".into(),
            ..Part::default()
        }];
        user.push(part(2, Some(1), Lifecycle::Todo, 1.0));
        assert_eq!(
            dissolve_tech_target(&user),
            None,
            "a 'codebase' mention is not the migration husk"
        );

        // a childless husk has nothing to unwrap.
        assert_eq!(
            dissolve_tech_target(&[husk("the codebase — one aspect of the product")]),
            None,
            "no children → nothing to dissolve"
        );

        // a NESTED Tech is out of scope — the husk is a ROOT wrapper.
        let nested = vec![
            part(1, None, Lifecycle::Todo, 1.0),
            Part {
                id: 2,
                parent_id: Some(1),
                name: "Tech".into(),
                detail: "the codebase".into(),
                ..Part::default()
            },
            part(3, Some(2), Lifecycle::Todo, 1.0),
        ];
        assert_eq!(
            dissolve_tech_target(&nested),
            None,
            "husk is a ROOT wrapper"
        );
    }
}

/// Dissolve a container node (docs/019 slice 1c — the canned "Dissolve Tech"
/// changeset): MOVE its children out to its own parent (order preserved), then
/// REMOVE the empty husk. The exact inverse of the old aspect-migration wrap.
/// Deterministic, no LLM; flows through the normal changeset review.
pub fn dissolve_node_ops(parts: &[Part], id: PartId) -> Vec<DiffOp> {
    let Some(node) = parts.iter().find(|p| p.id == id) else {
        return Vec::new();
    };
    let dest = node.parent_id.map(PartRef::Id).unwrap_or(PartRef::Root);
    let mut kids: Vec<&Part> = parts.iter().filter(|p| p.parent_id == Some(id)).collect();
    kids.sort_by(|a, b| {
        a.sort_order
            .partial_cmp(&b.sort_order)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let base = parts
        .iter()
        .filter(|p| p.parent_id == node.parent_id)
        .map(|p| p.sort_order)
        .fold(0.0f64, f64::max);
    let mut ops: Vec<DiffOp> = kids
        .iter()
        .enumerate()
        .map(|(i, k)| DiffOp::Move {
            id: k.id,
            parent: dest.clone(),
            sort_order: base + (i + 1) as f64,
        })
        .collect();
    ops.push(DiffOp::Remove { id });
    ops
}

/// Detect the legacy `build_aspect_migration` husk (docs/019 slice 1c auto-seed,
/// Panel ruling 2 — the machine cleans its own Tech mess as the review UI's
/// first customer): a ROOT node named "Tech" (case-insensitive) whose body
/// still carries the migration's own words ("codebase" / "one aspect") AND
/// wraps at least one child. `build_aspect_migration` was deleted in slice 1a,
/// but the user's tree still carries the husk it left — this is the exact
/// predicate that decides whether to OFFER a Dissolve-Tech changeset. `None`
/// where there is no such mess (a real "Tech" area the user meant, a
/// childless node, or a nested Tech), so the offer never fires spuriously.
pub fn dissolve_tech_target(parts: &[Part]) -> Option<PartId> {
    parts
        .iter()
        .find(|p| {
            p.parent_id.is_none()
                && p.name.eq_ignore_ascii_case("tech")
                && {
                    // Match the migration's OWN verbatim detail, not a loose
                    // keyword (review: "owns our codebase and CI" is a user's
                    // legitimate Tech area — auto-proposing to destroy it is the
                    // exact false-positive we must avoid). build_aspect_migration
                    // stamped "the codebase — one aspect of the product"; that
                    // full phrase never occurs in a hand-written description.
                    let body = format!("{} {}", p.detail, p.detail_md).to_ascii_lowercase();
                    body.contains("one aspect of the product")
                }
                && parts.iter().any(|c| c.parent_id == Some(p.id))
        })
        .map(|p| p.id)
}

/// Ops that remove a whole subtree, LEAF-FIRST (docs/019: `Remove` is a
/// single-row op; removing a parent before its children strands them as
/// orphans invisible to `build_tree`). Every delete gesture goes through this.
pub fn subtree_removal_ops(parts: &[Part], id: PartId) -> Vec<DiffOp> {
    fn walk(parts: &[Part], id: PartId, out: &mut Vec<DiffOp>) {
        for c in parts.iter().filter(|p| p.parent_id == Some(id)) {
            walk(parts, c.id, out);
        }
        out.push(DiffOp::Remove { id });
    }
    if !parts.iter().any(|p| p.id == id) {
        return Vec::new();
    }
    let mut ops = Vec::new();
    walk(parts, id, &mut ops);
    ops
}

// ---- the OUTLINE edit grammar's pure ordering (docs/019 slice 1b) ----
// Every keyboard verb (Enter/Tab/⇧Tab/⌥↑/⌥↓) reduces to ONE DiffOp computed
// here from the flat rows — main.rs only routes keys and accepts the op on
// the human lane. Impossible gestures (indent the first sibling, reorder past
// an edge, outdent a root) return None: a calm no-op, never an error.

/// A node's siblings-in-order under `parent` (same sort as build_tree).
fn ordered_children(parts: &[Part], parent: Option<PartId>) -> Vec<&Part> {
    let mut v: Vec<&Part> = parts.iter().filter(|p| p.parent_id == parent).collect();
    v.sort_by(|a, b| {
        a.sort_order
            .partial_cmp(&b.sort_order)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    v
}

/// The sort_order that lands BETWEEN two neighbours (docs/019: moves/inserts
/// never renumber the row set — one op per gesture, one clean inverse for ⌘Z).
/// Open ends step by 1.0 so plain appends stay integral; an empty set → 1.0.
pub fn midpoint_order(before: Option<f64>, after: Option<f64>) -> f64 {
    match (before, after) {
        (Some(b), Some(a)) => (b + a) / 2.0,
        (Some(b), None) => b + 1.0,
        (None, Some(a)) => a - 1.0,
        (None, None) => 1.0,
    }
}

/// Tab: indent = Move under the PREVIOUS sibling, appended as its last child
/// (the Workflowy contract — indentation never reorders what you can see).
pub fn indent_op(parts: &[Part], id: PartId) -> Option<DiffOp> {
    let node = parts.iter().find(|p| p.id == id)?;
    let sibs = ordered_children(parts, node.parent_id);
    let pos = sibs.iter().position(|p| p.id == id)?;
    let prev = sibs.get(pos.checked_sub(1)?)?;
    let last = ordered_children(parts, Some(prev.id))
        .last()
        .map(|p| p.sort_order);
    Some(DiffOp::Move {
        id,
        parent: PartRef::Id(prev.id),
        sort_order: midpoint_order(last, None),
    })
}

/// Lay `order` out as children of `dest` with integer sort_orders 1..=N,
/// emitting a Move ONLY for rows whose parent or order actually changes. The
/// antidote to float-bisection collapse (review): a structural reorder that
/// would otherwise halve a shrinking gap instead resets the whole sibling
/// group to integers, so repeated edits can never converge two sort_orders.
fn lay_out(
    parts: &[Part],
    order: &[PartId],
    dest: &PartRef,
    dest_id: Option<PartId>,
) -> Vec<DiffOp> {
    order
        .iter()
        .enumerate()
        .filter_map(|(i, &pid)| {
            let want = (i + 1) as f64;
            let cur = parts.iter().find(|p| p.id == pid)?;
            (cur.parent_id != dest_id || cur.sort_order != want).then(|| DiffOp::Move {
                id: pid,
                parent: dest.clone(),
                sort_order: want,
            })
        })
        .collect()
}

/// ⇧Tab: outdent = Move to the grandparent, landing right AFTER the current
/// parent. RENUMBERS the grandparent's children to integers with the node
/// spliced in (never bisects → can't collapse — review). Empty (calm no-op)
/// when there is no grandparent level: a root has nowhere to climb.
pub fn outdent_op(parts: &[Part], id: PartId) -> Vec<DiffOp> {
    let Some(node) = parts.iter().find(|p| p.id == id) else {
        return Vec::new();
    };
    let Some(parent) = parts.iter().find(|p| Some(p.id) == node.parent_id) else {
        return Vec::new();
    };
    let dest_id = parent.parent_id;
    let dest = dest_id.map(PartRef::Id).unwrap_or(PartRef::Root);
    // grandparent's children in order, minus the mover, with `node` reinserted
    // immediately after `parent`.
    let mut order: Vec<PartId> = Vec::new();
    for p in ordered_children(parts, dest_id) {
        if p.id == id {
            continue;
        }
        order.push(p.id);
        if p.id == parent.id {
            order.push(id);
        }
    }
    lay_out(parts, &order, &dest, dest_id)
}

/// ⌥↑/⌥↓: reorder among siblings by EXCHANGING sort_orders with the neighbour
/// (reuses existing distinct values → never bisects, can't collapse — review).
/// Two Moves, same parent. Empty (calm no-op) at a list edge.
pub fn reorder_op(parts: &[Part], id: PartId, up: bool) -> Vec<DiffOp> {
    let Some(node) = parts.iter().find(|p| p.id == id) else {
        return Vec::new();
    };
    let sibs = ordered_children(parts, node.parent_id);
    let Some(pos) = sibs.iter().position(|p| p.id == id) else {
        return Vec::new();
    };
    let other = if up {
        pos.checked_sub(1)
    } else {
        (pos + 1 < sibs.len()).then_some(pos + 1)
    };
    let Some(j) = other else { return Vec::new() };
    let dest = node.parent_id.map(PartRef::Id).unwrap_or(PartRef::Root);
    vec![
        DiffOp::Move {
            id: node.id,
            parent: dest.clone(),
            sort_order: sibs[j].sort_order,
        },
        DiffOp::Move {
            id: sibs[j].id,
            parent: dest,
            sort_order: node.sort_order,
        },
    ]
}

/// The CANVAS reparent verb (docs/019 slice 1b: ⌥-drag drop, and the menu's
/// Move-to once the palette picker lands): ONE Move landing `id` as the LAST
/// child of `new_parent` (None = root) — a drop appends, it never wedges
/// itself between siblings the user ordered. Calm None on: an unknown id
/// or destination, a same-parent drop (nothing to say), and a drop on itself
/// or its own descendant (the cycle would detach the subtree from the tree).
pub fn reparent_op(parts: &[Part], id: PartId, new_parent: Option<PartId>) -> Option<DiffOp> {
    let node = parts.iter().find(|p| p.id == id)?;
    if node.parent_id == new_parent {
        return None;
    }
    if let Some(np) = new_parent {
        // walk UP from the destination: reaching `id` means the drop point
        // lives inside the dragged subtree. A broken chain (missing row)
        // rejects too — never Move under a node we can't place.
        let mut cur = Some(np);
        while let Some(c) = cur {
            if c == id {
                return None;
            }
            cur = parts.iter().find(|p| p.id == c)?.parent_id;
        }
    }
    let last = ordered_children(parts, new_parent)
        .last()
        .map(|p| p.sort_order);
    let dest = new_parent.map(PartRef::Id).unwrap_or(PartRef::Root);
    Some(DiffOp::Move {
        id,
        parent: dest,
        sort_order: midpoint_order(last, None),
    })
}

/// Enter's landing plan (docs/019 OUTLINE: new sibling BELOW the focused
/// node). RENUMBERS the existing siblings to integers, opening the slot right
/// after the anchor for the new node (never bisects → 50 rapid Enters can't
/// collapse the gap — review). `renumber` are the sibling Moves; the caller's
/// Add carries `parent`/`order`. None when the anchor vanished mid-edit — the
/// caller falls back to an append rather than dropping the typed name.
pub struct SiblingInsert {
    pub renumber: Vec<DiffOp>,
    pub parent: PartRef,
    pub order: f64,
}
pub fn sibling_below_plan(parts: &[Part], after: PartId) -> Option<SiblingInsert> {
    let node = parts.iter().find(|p| p.id == after)?;
    let dest_id = node.parent_id;
    let dest = dest_id.map(PartRef::Id).unwrap_or(PartRef::Root);
    let sibs = ordered_children(parts, dest_id);
    let apos = sibs.iter().position(|p| p.id == after)?;
    // rows up to and including the anchor keep ranks 1..=apos+1; rows below it
    // shift up by one, freeing rank apos+2 for the new node.
    let mut renumber = Vec::new();
    for (i, s) in sibs.iter().enumerate() {
        let want = if i <= apos {
            (i + 1) as f64
        } else {
            (i + 2) as f64
        };
        if s.sort_order != want {
            renumber.push(DiffOp::Move {
                id: s.id,
                parent: dest.clone(),
                sort_order: want,
            });
        }
    }
    Some(SiblingInsert {
        renumber,
        parent: dest,
        order: (apos + 2) as f64,
    })
}

/// Area rollup (docs/019 commitment 2): done/total over descendant TASKS —
/// regardless of intermediate kinds — excluding lifecycle-Idea captures (the
/// countable_ratio rule). Areas have no asserted lifecycle; this arithmetic
/// over assertions IS their state.
pub fn task_rollup(node: &TreeNode) -> (usize, usize) {
    let mut done = 0usize;
    let mut total = 0usize;
    fn walk(n: &TreeNode, done: &mut usize, total: &mut usize) {
        if n.part.kind == Kind::Task && n.part.lifecycle != Lifecycle::Idea {
            *total += 1;
            if n.part.lifecycle == Lifecycle::Done {
                *done += 1;
            }
        }
        for c in &n.children {
            walk(c, done, total);
        }
    }
    for c in &node.children {
        walk(c, &mut done, &mut total);
    }
    (done, total)
}

/// done/total over a subtree counting ONLY committed work (todo/building/done)
/// — Idea nodes are captured thoughts, not unfinished work; putting them in
/// the denominator punishes capture (design critique #10).
pub fn countable_ratio(node: &TreeNode) -> (usize, usize) {
    let mut done = 0usize;
    let mut total = 0usize;
    fn walk(n: &TreeNode, done: &mut usize, total: &mut usize) {
        match n.part.lifecycle {
            Lifecycle::Idea => {}
            lc => {
                *total += 1;
                if lc == Lifecycle::Done {
                    *done += 1;
                }
            }
        }
        for c in &n.children {
            walk(c, done, total);
        }
    }
    for c in &node.children {
        walk(c, &mut done, &mut total);
    }
    (done, total)
}

/// The idea tray's canonical name (docs/019 commitment 5, user amendment):
/// captures with no meaningful selection land under a node named "ideas".
pub const IDEA_TRAY_NAME: &str = "ideas";

/// Find the per-project idea tray: looked up by NAME+KIND (case-insensitive —
/// the user may retitle-case it), root rows preferred (the tray is born a
/// root but survives being filed somewhere by hand — a deliberate Move must
/// not spawn a second tray). Lowest id breaks a duplicate tie so repeated
/// captures always land in the same one. None = not yet created; the first
/// capture lazily adds it.
pub fn idea_tray_id(parts: &[Part]) -> Option<PartId> {
    let tray = |root_only: bool| {
        parts
            .iter()
            .filter(|p| p.kind == Kind::Idea && p.name.trim().eq_ignore_ascii_case(IDEA_TRAY_NAME))
            .filter(|p| !root_only || p.parent_id.is_none())
            .map(|p| p.id)
            .min()
    };
    tray(true).or_else(|| tray(false))
}

/// The quiet-Building drift detector (docs/011 slice 3): Building parts whose
/// last linked activity — dispatch/touch links and log entries, see
/// `Store::part_activity` — is strictly older than `threshold_secs`. Absent
/// from `activity` = never active (0), so pre-dispatch-era parts surface too.
/// Pure: the caller supplies the activity map and the clock. Sorted by id so
/// repeated sweeps propose in a stable order.
pub fn quiet_building(
    parts: &[Part],
    activity: &HashMap<PartId, u64>,
    now_secs: u64,
    threshold_secs: u64,
) -> Vec<PartId> {
    let mut quiet: Vec<PartId> = parts
        .iter()
        .filter(|p| p.lifecycle == Lifecycle::Building)
        .filter(|p| {
            now_secs.saturating_sub(activity.get(&p.id).copied().unwrap_or(0)) > threshold_secs
        })
        .map(|p| p.id)
        .collect();
    quiet.sort_unstable();
    quiet
}

#[cfg(test)]
mod aspect_tests {
    use super::*;

    #[test]
    fn countable_ratio_excludes_ideas() {
        fn part(id: i64, lc: Lifecycle) -> Part {
            Part {
                id,
                parent_id: Some(0),
                name: format!("n{id}"),
                lifecycle: lc,
                sort_order: id as f64,
                ..Part::default()
            }
        }
        let root = Part {
            id: 0,
            name: "Growth".into(),
            lifecycle: Lifecycle::Building,
            kind: Kind::Area,
            ..Part::default()
        };
        let parts = vec![
            root,
            part(1, Lifecycle::Done),
            part(2, Lifecycle::Todo),
            part(3, Lifecycle::Idea),
            part(4, Lifecycle::Idea),
        ];
        let tree = build_tree(&parts);
        let (done, total) = countable_ratio(&tree[0]);
        assert_eq!((done, total), (1, 2), "ideas are not unfinished work");
    }

    #[test]
    fn task_rollup_counts_only_tasks_through_any_depth() {
        // Area > [task done, Area > [task todo, idea-kind node, lifecycle-idea task], task under a TASK]
        let mk = |id: i64, parent: Option<i64>, kind: Kind, lc: Lifecycle| Part {
            id,
            parent_id: parent,
            name: format!("n{id}"),
            kind,
            lifecycle: lc,
            sort_order: id as f64,
            ..Part::default()
        };
        let parts = vec![
            mk(1, None, Kind::Area, Lifecycle::Todo),
            mk(2, Some(1), Kind::Task, Lifecycle::Done),
            mk(3, Some(1), Kind::Area, Lifecycle::Todo),
            mk(4, Some(3), Kind::Task, Lifecycle::Todo),
            mk(5, Some(3), Kind::Idea, Lifecycle::Todo), // kind=idea: never counted
            mk(6, Some(3), Kind::Task, Lifecycle::Idea), // captured thought: not unfinished work
            mk(7, Some(2), Kind::Task, Lifecycle::Todo), // task under a task still counts
        ];
        let tree = build_tree(&parts);
        assert_eq!(
            task_rollup(&tree[0]),
            (1, 3),
            "done=1 (n2), total=3 (n2,n4,n7)"
        );
    }

    // ---- the OUTLINE grammar's pure ordering (docs/019 slice 1b) ----
    // fixture: root 1 (1.0) > [2 (1.0) > 5 (1.0), 3 (2.0)], root 4 (2.0)
    fn outline_fixture() -> Vec<Part> {
        let mk = |id: i64, parent: Option<i64>, order: f64| Part {
            id,
            parent_id: parent,
            name: format!("n{id}"),
            sort_order: order,
            ..Part::default()
        };
        vec![
            mk(1, None, 1.0),
            mk(2, Some(1), 1.0),
            mk(5, Some(2), 1.0),
            mk(3, Some(1), 2.0),
            mk(4, None, 2.0),
        ]
    }

    #[test]
    fn midpoint_lands_between_neighbours_and_steps_past_open_ends() {
        assert_eq!(midpoint_order(Some(1.0), Some(2.0)), 1.5);
        assert_eq!(midpoint_order(Some(3.0), None), 4.0); // append
        assert_eq!(midpoint_order(None, Some(3.0)), 2.0); // prepend
        assert_eq!(midpoint_order(None, None), 1.0); // empty row set
    }

    #[test]
    fn indent_goes_under_previous_sibling_as_last_child() {
        let parts = outline_fixture();
        // 3 indents under its previous sibling 2, AFTER 2's existing child 5
        // (order 1.0) — indentation appends, it never reorders (docs/019).
        let op = indent_op(&parts, 3).unwrap();
        assert!(
            matches!(op, DiffOp::Move { id: 3, parent: PartRef::Id(2), sort_order } if sort_order == 2.0)
        );
        // first-of-row has no previous sibling → calm no-op, never an error.
        assert!(indent_op(&parts, 2).is_none());
        assert!(indent_op(&parts, 99).is_none(), "unknown id → no op");
    }

    #[test]
    fn outdent_renumbers_grandparent_with_node_after_old_parent() {
        let parts = outline_fixture();
        // 5 climbs out of 2 into 1 (siblings 2@1.0, 3@2.0): new order [2,5,3]
        // renumbered 1,2,3 — 5 becomes rank 2, 3 shifts 2.0→3.0, 2 unchanged.
        let ops = outdent_op(&parts, 5);
        assert!(ops.contains(&DiffOp::Move {
            id: 5,
            parent: PartRef::Id(1),
            sort_order: 2.0
        }));
        assert!(ops.contains(&DiffOp::Move {
            id: 3,
            parent: PartRef::Id(1),
            sort_order: 3.0
        }));
        assert!(
            !ops.iter().any(|o| matches!(o, DiffOp::Move { id: 2, .. })),
            "already-correct row emits no Move"
        );
        // a child of a ROOT outdents to Root — order [1,3,4] renumbered.
        let ops = outdent_op(&parts, 3);
        assert!(ops.contains(&DiffOp::Move {
            id: 3,
            parent: PartRef::Root,
            sort_order: 2.0
        }));
        assert!(ops.contains(&DiffOp::Move {
            id: 4,
            parent: PartRef::Root,
            sort_order: 3.0
        }));
        assert!(
            outdent_op(&parts, 1).is_empty(),
            "roots have no grandparent"
        );
    }

    #[test]
    fn reorder_swaps_sort_orders_and_stops_at_edges() {
        let parts = outline_fixture();
        // 3 up swaps with 2: 3 takes 2's order (1.0), 2 takes 3's (2.0).
        let ops = reorder_op(&parts, 3, true);
        assert_eq!(
            ops,
            vec![
                DiffOp::Move {
                    id: 3,
                    parent: PartRef::Id(1),
                    sort_order: 1.0
                },
                DiffOp::Move {
                    id: 2,
                    parent: PartRef::Id(1),
                    sort_order: 2.0
                },
            ]
        );
        // 2 down swaps with 3 → identical exchange from the other side.
        let ops = reorder_op(&parts, 2, false);
        assert_eq!(
            ops,
            vec![
                DiffOp::Move {
                    id: 2,
                    parent: PartRef::Id(1),
                    sort_order: 2.0
                },
                DiffOp::Move {
                    id: 3,
                    parent: PartRef::Id(1),
                    sort_order: 1.0
                },
            ]
        );
        // roots swap against Root as the parent ref.
        let ops = reorder_op(&parts, 4, true);
        assert!(ops.iter().all(|o| matches!(
            o,
            DiffOp::Move {
                parent: PartRef::Root,
                ..
            }
        )));
        assert!(reorder_op(&parts, 2, true).is_empty(), "already first");
        assert!(reorder_op(&parts, 3, false).is_empty(), "already last");
    }

    #[test]
    fn sibling_below_renumbers_and_reserves_the_slot() {
        let parts = outline_fixture();
        // Enter on 2 (siblings 2@1.0, 3@2.0): 3 shifts to rank 3, new node
        // takes the freed rank 2, under parent 1.
        let plan = sibling_below_plan(&parts, 2).unwrap();
        assert_eq!(plan.parent, PartRef::Id(1));
        assert_eq!(plan.order, 2.0);
        assert!(plan.renumber.contains(&DiffOp::Move {
            id: 3,
            parent: PartRef::Id(1),
            sort_order: 3.0
        }));
        assert!(
            !plan
                .renumber
                .iter()
                .any(|o| matches!(o, DiffOp::Move { id: 2, .. })),
            "anchor already correct"
        );
        // Enter on the LAST root: nothing shifts, new node appends past it.
        let plan = sibling_below_plan(&parts, 4).unwrap();
        assert_eq!(plan.parent, PartRef::Root);
        assert_eq!(plan.order, 3.0);
        assert!(
            plan.renumber.is_empty(),
            "no rows below the anchor to shift"
        );
        assert!(
            sibling_below_plan(&parts, 99).is_none(),
            "anchor deleted mid-edit"
        );
    }

    #[test]
    fn repeated_sibling_inserts_never_collapse_sort_orders() {
        // the review's exact scenario: 60 rapid Enters below the same anchor.
        // The renumber-plan resets siblings to integers each time, so no two
        // sort_orders ever converge (float bisection would collapse by ~52).
        let mut parts = vec![
            Part {
                id: 1,
                name: "root".into(),
                sort_order: 1.0,
                ..Part::default()
            },
            Part {
                id: 2,
                parent_id: Some(1),
                name: "a".into(),
                sort_order: 1.0,
                ..Part::default()
            },
            Part {
                id: 3,
                parent_id: Some(1),
                name: "b".into(),
                sort_order: 2.0,
                ..Part::default()
            },
        ];
        let mut next_id = 4;
        let mut anchor = 2; // always insert below the freshly-created row
        for _ in 0..60 {
            let plan = sibling_below_plan(&parts, anchor).expect("anchor alive");
            for op in &plan.renumber {
                if let DiffOp::Move { id, sort_order, .. } = op {
                    parts.iter_mut().find(|p| p.id == *id).unwrap().sort_order = *sort_order;
                }
            }
            parts.push(Part {
                id: next_id,
                parent_id: Some(1),
                name: format!("n{next_id}"),
                sort_order: plan.order,
                ..Part::default()
            });
            anchor = next_id;
            next_id += 1;
        }
        // every sibling has a distinct order — a strict total order survives.
        let mut orders: Vec<f64> = parts
            .iter()
            .filter(|p| p.parent_id == Some(1))
            .map(|p| p.sort_order)
            .collect();
        orders.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for w in orders.windows(2) {
            assert!(
                w[1] - w[0] >= 0.5,
                "orders stay integer-spaced, never collapse: {:?}",
                w
            );
        }
        assert_eq!(orders.len(), 62, "60 inserts + 2 seed rows");
    }

    #[test]
    fn subtree_removal_is_leaf_first_and_dissolve_moves_children_out() {
        let mk = |id: i64, parent: Option<i64>, order: f64| Part {
            id,
            parent_id: parent,
            name: format!("n{id}"),
            sort_order: order,
            ..Part::default()
        };
        // 1 > 2 > 3, 1 > 4
        let parts = vec![
            mk(1, None, 1.0),
            mk(2, Some(1), 1.0),
            mk(3, Some(2), 1.0),
            mk(4, Some(1), 2.0),
        ];
        let ops = subtree_removal_ops(&parts, 1);
        let ids: Vec<PartId> = ops
            .iter()
            .map(|o| match o {
                DiffOp::Remove { id } => *id,
                _ => panic!("only removes"),
            })
            .collect();
        assert_eq!(ids, vec![3, 2, 4, 1], "leaves first, root last");
        assert!(
            subtree_removal_ops(&parts, 99).is_empty(),
            "unknown id → no ops"
        );
        // dissolve node 2: its child 3 moves up under 1, husk removed
        let ops = dissolve_node_ops(&parts, 2);
        assert_eq!(ops.len(), 2);
        assert!(matches!(
            &ops[0],
            DiffOp::Move {
                id: 3,
                parent: PartRef::Id(1),
                ..
            }
        ));
        assert!(matches!(&ops[1], DiffOp::Remove { id: 2 }));
        // dissolving a root sends children to Root
        let ops = dissolve_node_ops(&parts, 1);
        assert!(matches!(
            &ops[0],
            DiffOp::Move {
                id: 2,
                parent: PartRef::Root,
                ..
            }
        ));
        assert!(matches!(
            &ops[1],
            DiffOp::Move {
                id: 4,
                parent: PartRef::Root,
                ..
            }
        ));
        assert!(matches!(&ops[2], DiffOp::Remove { id: 1 }));
    }

    #[test]
    fn reparent_appends_last_and_rejects_cycles() {
        // fixture: root 1 (1.0) > [2 (1.0) > 5 (1.0), 3 (2.0)], root 4 (2.0)
        let parts = outline_fixture();
        // 4 drops onto 2 → appended AFTER 2's existing child 5 (1.0 + 1.0).
        let op = reparent_op(&parts, 4, Some(2)).unwrap();
        assert!(
            matches!(op, DiffOp::Move { id: 4, parent: PartRef::Id(2), sort_order } if sort_order == 2.0)
        );
        // 5 drops on the background of an undrilled canvas → root, appended
        // past the last root (4 @ 2.0).
        let op = reparent_op(&parts, 5, None).unwrap();
        assert!(
            matches!(op, DiffOp::Move { id: 5, parent: PartRef::Root, sort_order } if sort_order == 3.0)
        );
        // calm no-ops (docs/019: impossible gestures are silence, not errors):
        assert!(reparent_op(&parts, 5, Some(2)).is_none(), "same parent");
        assert!(reparent_op(&parts, 1, None).is_none(), "already a root");
        assert!(reparent_op(&parts, 1, Some(1)).is_none(), "onto itself");
        assert!(
            reparent_op(&parts, 1, Some(5)).is_none(),
            "onto own grandchild = cycle"
        );
        assert!(reparent_op(&parts, 99, Some(1)).is_none(), "unknown id");
        assert!(
            reparent_op(&parts, 4, Some(99)).is_none(),
            "destination vanished"
        );
    }

    #[test]
    fn idea_tray_matches_name_plus_kind_and_prefers_roots() {
        let mk = |id: i64, parent: Option<i64>, name: &str, kind: Kind| Part {
            id,
            parent_id: parent,
            name: name.into(),
            kind,
            sort_order: id as f64,
            ..Part::default()
        };
        // a TASK named "ideas" is not the tray — kind is half the lookup key
        // (docs/019 commitment 5), otherwise any list-of-ideas node captures.
        assert_eq!(idea_tray_id(&[mk(1, None, "ideas", Kind::Task)]), None);
        // case-insensitive + trimmed: the user retitling " Ideas " must
        // not fork a second tray on the next capture.
        assert_eq!(idea_tray_id(&[mk(2, None, " Ideas ", Kind::Idea)]), Some(2));
        // roots preferred over a filed tray; duplicates tie-break on lowest id.
        let parts = vec![
            mk(3, Some(9), "ideas", Kind::Idea),
            mk(5, None, "ideas", Kind::Idea),
            mk(4, None, "ideas", Kind::Idea),
        ];
        assert_eq!(idea_tray_id(&parts), Some(4));
        // a tray the user MOVED under a node still counts — no second tray.
        assert_eq!(
            idea_tray_id(&[mk(3, Some(9), "ideas", Kind::Idea)]),
            Some(3)
        );
        assert_eq!(idea_tray_id(&[]), None);
    }

    #[test]
    fn kind_cycle_reaches_every_kind_and_returns() {
        // docs/019 slice 1b: the chip is the ONE gesture that fixes a
        // backfill miss — the cycle must visit all three kinds.
        assert_eq!(Kind::Area.cycle(), Kind::Task);
        assert_eq!(Kind::Task.cycle(), Kind::Idea);
        assert_eq!(Kind::Idea.cycle(), Kind::Area);
    }

    #[test]
    fn building_is_never_assertable_and_first_line_derives() {
        assert_eq!(Lifecycle::Building.assertable(), Lifecycle::Todo);
        assert_eq!(Lifecycle::Done.assertable(), Lifecycle::Done);
        // Building still parses + roundtrips (journal compat) — it is only
        // rejected at write time, never at read time.
        assert_eq!(Lifecycle::parse("building"), Lifecycle::Building);
        assert_eq!(
            first_line("\n\n  What it is.  \nMore prose."),
            "What it is."
        );
        assert_eq!(first_line(""), "");
    }
}
