//! changeset — the pure accept-planning for a machine changeset (docs/019
//! slice 1c/2 review surface). Kept as its own module so the load-bearing
//! "Accept all" partition is unit-tested away from the GPUI render code (and
//! so the huge main.rs doesn't grow a test harness).

use std::collections::{HashMap, HashSet};

use orchestrator_store::{DiffOp, Lifecycle, PartRef};

/// A changeset op with the user's edit-before-accept name substituted in
/// (Adds only). ONE definition so what the review card DISPLAYS and what accept
/// APPLIES can never diverge (review: a review surface whose preview drifts
/// from its effect is a trust bug).
pub fn op_with_name_edit(op: &DiffOp, i: usize, names: Option<&HashMap<usize, String>>) -> DiffOp {
    let mut o = op.clone();
    if let DiffOp::Add { name, .. } = &mut o {
        if let Some(edited) = names.and_then(|n| n.get(&i)) {
            *name = edited.clone();
        }
    }
    o
}

/// Whether a changeset op ASSERTS `done` (an Add of a done node, or a
/// SetStatus→done). Done ops are excluded from Accept all unconditionally
/// (docs/019 ruling 6): a verified quote proves a span EXISTS, not that it
/// means "shipped" — so every done enters the tree individually vouched.
pub fn op_asserts_done(op: &DiffOp) -> bool {
    matches!(
        op,
        DiffOp::Add {
            lifecycle: Lifecycle::Done,
            ..
        } | DiffOp::SetStatus {
            lifecycle: Lifecycle::Done,
            ..
        }
    )
}

/// Semantic memory projections are true claims, not ordinary structural
/// cleanup. Like `done`, each one must be vouched individually even when its
/// quote verifies; an exact span proves grounding, not that the interpretation
/// or relation to this node is correct. RemoveDecision is internal undo
/// currency, but is held too if it ever reaches a review row.
pub fn op_requires_individual_review(op: &DiffOp) -> bool {
    op_asserts_done(op)
        || matches!(
            op,
            DiffOp::AddDecision { .. }
                | DiffOp::RemoveDecision { .. }
                | DiffOp::RestoreNoteTarget { .. }
        )
}

/// The primary changeset action must describe what it will actually do. A
/// semantic card has no "Accept all": its rows begin held, then the action
/// applies only the claims the user explicitly confirmed.
pub fn changeset_accept_label(has_individual_rows: bool, kept: usize) -> String {
    if !has_individual_rows {
        "Accept all".into()
    } else if kept == 0 {
        "Confirm rows below".into()
    } else {
        format!(
            "Apply {kept} confirmed row{}",
            if kept == 1 { "" } else { "s" }
        )
    }
}

/// The effective keep-state of a changeset op (docs/019 slice 2 review surface).
/// A plain, verified op is kept by DEFAULT; a FLAGGED (unverified), `done`, or
/// semantic-decision op is EXCLUDED by default (individually accepted only —
/// it never rides Accept all). The user's per-op toggle FLIPS that default, so one
/// toggle mechanic serves both "drop this" and "accept this flagged one anyway".
pub fn changeset_kept(op: &DiffOp, flagged: bool, flipped: bool) -> bool {
    let default_kept = !(flagged || op_requires_individual_review(op));
    default_kept ^ flipped
}

/// The op a changeset op parents under, if it references a not-yet-created node
/// by Temp (Add children, and Moves onto a proposed node).
fn op_temp_parent(op: &DiffOp) -> Option<&str> {
    match op {
        DiffOp::Add {
            parent: PartRef::Temp(t),
            ..
        }
        | DiffOp::Move {
            parent: PartRef::Temp(t),
            ..
        }
        | DiffOp::AddDecision {
            part: PartRef::Temp(t),
            ..
        } => Some(t),
        _ => None,
    }
}

/// Plan a changeset "Accept all" (review 2). Partitions the op indices into:
/// - APPLIED: every op the keep-rule includes (verified/non-done minus toggled-
///   off, plus flagged/done toggled IN), DEPENDENCY-CLOSED so a kept child whose
///   Temp parent is held back is NOT applied (it would resolve to a root orphan).
/// - LEFTOVER: unresolved ops that must PERSIST — held flagged/done the user
///   didn't act on, and children deferred because their parent wasn't applied.
///
/// Everything else (a verified op the user explicitly toggled OFF) is a
/// deliberate reject and is neither applied nor kept. Nothing is ever silently
/// dropped (review finding 1) and nothing half-applies to root (finding 2).
pub fn plan_changeset_accept(
    flat: &[(DiffOp, Option<String>)],
    flags: &[bool],
    off: &HashSet<usize>,
) -> (Vec<usize>, Vec<usize>) {
    let flagged = |i: usize| flags.get(i).copied().unwrap_or(false);
    let mut applied: Vec<bool> = (0..flat.len())
        .map(|i| changeset_kept(&flat[i].0, flagged(i), off.contains(&i)))
        .collect();
    // temp name → the index of the Add that defines it.
    let mut temp_idx: HashMap<&str, usize> = HashMap::new();
    for (i, (op, _)) in flat.iter().enumerate() {
        if let DiffOp::Add { temp, .. } = op {
            temp_idx.insert(temp.as_str(), i);
        }
    }
    // dependency closure: defer any op whose Temp parent isn't itself applied.
    loop {
        let mut changed = false;
        for i in 0..flat.len() {
            if !applied[i] {
                continue;
            }
            if let Some(t) = op_temp_parent(&flat[i].0) {
                let parent_ok = temp_idx.get(t).map(|&pi| applied[pi]).unwrap_or(false);
                if !parent_ok {
                    applied[i] = false;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let applied_idx: Vec<usize> = (0..flat.len()).filter(|&i| applied[i]).collect();
    let leftover_idx: Vec<usize> = (0..flat.len())
        .filter(|&i| {
            if applied[i] {
                return false;
            }
            // an explicitly toggled-off VERIFIED op is a deliberate reject —
            // dropped, not kept. Held/deferred ops persist.
            let default_kept =
                !(flagged(i) || op_requires_individual_review(&flat[i].0));
            let explicitly_rejected = default_kept && off.contains(&i);
            !explicitly_rejected
        })
        .collect();
    (applied_idx, leftover_idx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator_store::Kind;

    fn add(temp: &str, parent: PartRef) -> DiffOp {
        DiffOp::Add {
            temp: temp.into(),
            parent,
            name: temp.into(),
            detail: String::new(),
            lifecycle: Lifecycle::Todo,
            anchors: vec![],
            kind: Kind::Task,
            detail_md: None,
            sort_order: None,
            source_file: None,
            source_quote: None,
            rationale: None,
        }
    }
    fn flat(ops: Vec<DiffOp>) -> Vec<(DiffOp, Option<String>)> {
        ops.into_iter().map(|o| (o, None)).collect()
    }

    fn decision(part: PartRef) -> DiffOp {
        DiffOp::AddDecision {
            part,
            text: "Map is the only memory surface".into(),
            source_memory_id: "memory-map-surface".into(),
            source_revision_id: "revision-1".into(),
        }
    }

    #[test]
    fn accept_all_applies_verified_and_persists_flagged() {
        // g1 verified, g2 flagged — Accept all (no toggles) applies g1, keeps g2.
        let f = flat(vec![add("g1", PartRef::Root), add("g2", PartRef::Root)]);
        let flags = vec![false, true];
        let (applied, leftover) = plan_changeset_accept(&f, &flags, &HashSet::new());
        assert_eq!(applied, vec![0], "only the verified op is applied");
        assert_eq!(
            leftover,
            vec![1],
            "the flagged op PERSISTS — never silently dropped"
        );
    }

    #[test]
    fn kept_child_of_held_parent_is_deferred_not_orphaned() {
        // g1 (parent) FLAGGED → held; g2 verified child under Temp(g1).
        let f = flat(vec![
            add("g1", PartRef::Root),
            add("g2", PartRef::Temp("g1".into())),
        ]);
        let flags = vec![true, false];
        let (applied, leftover) = plan_changeset_accept(&f, &flags, &HashSet::new());
        // the child must NOT be applied (its Temp parent isn't) — it would land
        // at root. Both persist so the user can accept the parent first.
        assert!(applied.is_empty(), "child deferred, never orphaned to root");
        assert_eq!(leftover, vec![0, 1], "parent + child both persist");
    }

    #[test]
    fn flipping_the_parent_in_lets_the_child_apply() {
        let f = flat(vec![
            add("g1", PartRef::Root),
            add("g2", PartRef::Temp("g1".into())),
        ]);
        let flags = vec![true, false];
        let off: HashSet<usize> = [0].into_iter().collect(); // flip g1 IN
        let (applied, leftover) = plan_changeset_accept(&f, &flags, &off);
        assert_eq!(
            applied,
            vec![0, 1],
            "parent flipped in → child applies under it"
        );
        assert!(leftover.is_empty());
    }

    #[test]
    fn explicitly_rejected_verified_op_is_dropped_not_kept() {
        let f = flat(vec![add("g1", PartRef::Root), add("g2", PartRef::Root)]);
        let flags = vec![false, false];
        let off: HashSet<usize> = [1].into_iter().collect(); // toggle g2 OFF = reject
        let (applied, leftover) = plan_changeset_accept(&f, &flags, &off);
        assert_eq!(applied, vec![0]);
        assert!(
            leftover.is_empty(),
            "an explicitly rejected verified op is dropped, not persisted"
        );
    }

    #[test]
    fn verified_decision_is_held_until_individually_confirmed() {
        let f = flat(vec![decision(PartRef::Id(7))]);
        let (applied, leftover) = plan_changeset_accept(&f, &[false], &HashSet::new());
        assert!(applied.is_empty());
        assert_eq!(leftover, vec![0]);

        let confirmed: HashSet<usize> = [0].into_iter().collect();
        let (applied, leftover) = plan_changeset_accept(&f, &[false], &confirmed);
        assert_eq!(applied, vec![0]);
        assert!(leftover.is_empty());
        assert_eq!(changeset_accept_label(true, 0), "Confirm rows below");
        assert_eq!(
            changeset_accept_label(true, 1),
            "Apply 1 confirmed row"
        );
        assert_eq!(
            changeset_accept_label(true, 3),
            "Apply 3 confirmed rows"
        );
        assert_eq!(changeset_accept_label(false, 3), "Accept all");
    }

    #[test]
    fn decision_for_held_temp_parent_is_dependency_deferred() {
        let f = flat(vec![
            add("g1", PartRef::Root),
            decision(PartRef::Temp("g1".into())),
        ]);
        let confirmed: HashSet<usize> = [1].into_iter().collect();
        let (applied, leftover) = plan_changeset_accept(&f, &[true, false], &confirmed);
        assert!(applied.is_empty());
        assert_eq!(leftover, vec![0, 1]);
    }

    #[test]
    fn name_edit_applies_only_to_adds() {
        let names: HashMap<usize, String> = [(0, "Renamed".to_string())].into_iter().collect();
        let edited = op_with_name_edit(&add("g1", PartRef::Root), 0, Some(&names));
        assert!(matches!(&edited, DiffOp::Add { name, .. } if name == "Renamed"));
    }
}
