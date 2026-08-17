//! Tree loading and diff application (accept / undo / status / staleness),
//! the raw op-apply engine, and its ref/cycle/order helpers.
//! Extracted verbatim from `store.rs` (decomposition; behavior unchanged).

use std::collections::HashMap;

use rusqlite::params;

use super::{now, SeedState, Store};
use crate::tree::{first_line, DiffOp, Kind, Lifecycle, Part, PartId, PartRef, StatusSource};

impl Store {
    /// Load a project's parts (with anchors), flat.
    pub fn load_tree(&self, key: &str) -> rusqlite::Result<Vec<Part>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,parent_id,name,detail,lifecycle,status_source,status_at_secs,stale,stale_reason,sort_order,map_x,map_y,kind,COALESCE(detail_md,detail),COALESCE(created_by,'legacy'),source_file,source_quote,rationale
             FROM part WHERE project_key=?1",
        )?;
        let rows = stmt.query_map(params![key], |r| {
            Ok(Part {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                name: r.get(2)?,
                detail: r.get(3)?,
                lifecycle: Lifecycle::parse(&r.get::<_, String>(4)?),
                status_source: StatusSource::parse(&r.get::<_, String>(5)?),
                status_at_secs: r.get::<_, i64>(6)? as u64,
                stale: r.get::<_, i64>(7)? != 0,
                stale_reason: r.get(8)?,
                sort_order: r.get(9)?,
                map_x: r.get(10)?,
                map_y: r.get(11)?,
                kind: Kind::parse(&r.get::<_, String>(12)?),
                detail_md: r.get(13)?,
                created_by: r.get(14)?,
                source_file: r.get(15)?,
                source_quote: r.get(16)?,
                rationale: r.get(17)?,
                anchors: vec![],
            })
        })?;
        let mut parts: Vec<Part> = rows.collect::<rusqlite::Result<_>>()?;
        for p in &mut parts {
            let mut a = self
                .conn
                .prepare("SELECT glob FROM part_anchor WHERE part_id=?1")?;
            p.anchors = a
                .query_map(params![p.id], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?;
        }
        Ok(parts)
    }

    /// Apply a diff in one transaction: insert/update rows + write the inverse
    /// to the journal (undo). `source` tags new status assertions. Returns the
    /// accept id.
    pub fn accept_diff(&mut self, key: &str, ops: &[DiffOp]) -> rusqlite::Result<String> {
        self.accept_diff_from(key, ops, "user", None)
    }

    /// accept_diff with PROVENANCE (#10): origin = user | seed | summary;
    /// source_sess carries the proposing session's cli id when origin=summary.
    ///
    /// Inverse discipline (docs/019 slice 1a): inverses are recorded in FORWARD
    /// op order and applied REVERSED by undo_last. Any inverse that references
    /// a part this same diff removes uses PartRef::Temp("undo-<old id>") — the
    /// reversed replay re-Adds that part first (under a new id) and the temp
    /// map threads the reference. Without both, undoing a compound diff like
    /// dissolve-Tech (N Moves + 1 Remove) orphans every moved child.
    pub fn accept_diff_from(
        &mut self,
        key: &str,
        ops: &[DiffOp],
        origin: &str,
        source_sess: Option<&str>,
    ) -> rusqlite::Result<String> {
        let before = self.load_tree(key)?;
        let by_id: HashMap<PartId, Part> = before.iter().map(|p| (p.id, p.clone())).collect();
        let accept_id = format!("acc-{}-{}", now(), self.conn.last_insert_rowid());
        // every id this diff removes — inverses referencing one must go through
        // the temp map (the id is dead by the time the inverse replays).
        let removed: std::collections::HashSet<PartId> = ops
            .iter()
            .filter_map(|op| match op {
                DiffOp::Remove { id } => Some(*id),
                _ => None,
            })
            .collect();
        let undo_ref = |parent_id: Option<PartId>| -> PartRef {
            match parent_id {
                Some(pid) if removed.contains(&pid) => PartRef::Temp(format!("undo-{pid}")),
                Some(pid) => PartRef::Id(pid),
                None => PartRef::Root,
            }
        };
        let tx = self.conn.transaction()?;
        let mut temp_to_real: HashMap<String, PartId> = HashMap::new();
        let mut inverse: Vec<DiffOp> = Vec::new();

        for op in ops {
            match op {
                DiffOp::Add {
                    temp,
                    parent,
                    name,
                    detail,
                    lifecycle,
                    anchors,
                    kind,
                    detail_md,
                    sort_order,
                    source_file,
                    source_quote,
                    rationale,
                } => {
                    let parent_id = resolve_ref(parent, &temp_to_real);
                    let body = detail_md.clone().unwrap_or_else(|| detail.clone());
                    let order = match sort_order {
                        Some(o) => *o,
                        None => next_order(&tx, key, parent_id)?,
                    };
                    // docs/019 commitment 3: an accepted machine Add lands with
                    // its verified provenance quad (created_by from origin; the
                    // {source_file, source_quote, rationale} trio from the op).
                    tx.execute(
                        "INSERT INTO part(project_key,parent_id,name,detail,lifecycle,status_source,status_at_secs,sort_order,kind,detail_md,created_by,source_file,source_quote,rationale)
                         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                        params![
                            key,
                            parent_id,
                            name,
                            detail,
                            lifecycle.assertable().as_str(),
                            StatusSource::Seed.as_str(),
                            now() as i64,
                            order,
                            kind.as_str(),
                            body,
                            origin,
                            source_file,
                            source_quote,
                            rationale,
                        ],
                    )?;
                    let real = tx.last_insert_rowid();
                    temp_to_real.insert(temp.clone(), real);
                    for g in anchors {
                        tx.execute(
                            "INSERT INTO part_anchor(part_id,glob) VALUES(?1,?2)",
                            params![real, g],
                        )?;
                    }
                    inverse.push(DiffOp::Remove { id: real });
                }
                DiffOp::SetStatus {
                    id,
                    lifecycle,
                    source,
                } => {
                    if let Some(old) = by_id.get(id) {
                        inverse.push(DiffOp::SetStatus {
                            id: *id,
                            lifecycle: old.lifecycle,
                            source: old.status_source,
                        });
                    }
                    tx.execute(
                        "UPDATE part SET lifecycle=?2,status_source=?3,status_at_secs=?4,stale=0,stale_reason=NULL WHERE id=?1",
                        params![id, lifecycle.assertable().as_str(), source.as_str(), now() as i64],
                    )?;
                }
                DiffOp::Rename { id, name, detail } => {
                    if let Some(old) = by_id.get(id) {
                        inverse.push(DiffOp::Rename {
                            id: *id,
                            name: old.name.clone(),
                            detail: old.detail.clone(),
                        });
                    }
                    // keep the detail = first_line(detail_md) invariant: a
                    // non-empty one-liner rewrite also rewrites the body's
                    // first line (review: they silently diverged). The Rename
                    // inverse round-trips exactly when the invariant held.
                    if detail.is_empty() {
                        tx.execute(
                            "UPDATE part SET name=?2,detail=?3 WHERE id=?1",
                            params![id, name, detail],
                        )?;
                    } else {
                        let body = by_id
                            .get(id)
                            .map(|old| crate::tree::replace_first_line(&old.detail_md, detail))
                            .unwrap_or_else(|| detail.clone());
                        tx.execute(
                            "UPDATE part SET name=?2,detail=?3,detail_md=?4 WHERE id=?1",
                            params![id, name, detail, body],
                        )?;
                    }
                }
                DiffOp::Move {
                    id,
                    parent,
                    sort_order,
                } => {
                    let parent_id = resolve_ref(parent, &temp_to_real);
                    // SKIP a move that would corrupt the tree (the node stays
                    // put): a cycle (unreachable pair), or a reparent onto a
                    // parent that no longer EXISTS — e.g. a stale rework snapshot
                    // moving onto a node the user deleted mid-run would set
                    // parent_id to a dangling id, orphaning the node (review 2b).
                    if would_create_cycle(&tx, *id, parent_id) || !parent_exists(&tx, parent_id) {
                        continue;
                    }
                    if let Some(old) = by_id.get(id) {
                        inverse.push(DiffOp::Move {
                            id: *id,
                            parent: undo_ref(old.parent_id),
                            sort_order: old.sort_order,
                        });
                    }
                    tx.execute(
                        "UPDATE part SET parent_id=?2,sort_order=?3 WHERE id=?1",
                        params![id, parent_id, sort_order],
                    )?;
                }
                DiffOp::Remove { id } => {
                    // A Remove must NEVER orphan a child (review: a toggled-off
                    // Move, or a child added under a container after a dissolve
                    // was seeded, left a row parented to a deleted node —
                    // invisible to build_tree, silent data loss). PROMOTE any
                    // still-present children to the removed node's CURRENT
                    // parent (earlier Moves in this diff may already have
                    // reparented the container). A true subtree delete uses
                    // leaf-first Removes, so the container has no survivors by
                    // the time it is removed and this is a no-op; for a
                    // dissolve, promoting a surviving child IS the intent.
                    let cur_parent: Option<PartId> = tx
                        .query_row("SELECT parent_id FROM part WHERE id=?1", params![id], |r| {
                            r.get(0)
                        })
                        .ok()
                        .flatten();
                    let survivors: Vec<(PartId, f64)> = {
                        let mut st =
                            tx.prepare("SELECT id, sort_order FROM part WHERE parent_id=?1")?;
                        let rows = st
                            .query_map(params![id], |r| {
                                Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
                            })?
                            .filter_map(|r| r.ok())
                            .collect();
                        rows
                    };
                    if let Some(old) = by_id.get(id) {
                        // inverse: move survivors back under the re-Added node,
                        // THEN re-Add it. After undo_last reverses the whole
                        // inverse list the Add runs first (survivors target it
                        // by Temp; apply_raw's deferred-temp fixup covers any
                        // residual ordering). sort_order preserved for exact
                        // restore. A removed PARENT threads via the temp map so
                        // leaf-first subtree removals undo to the same shape.
                        for (sid, sord) in &survivors {
                            inverse.push(DiffOp::Move {
                                id: *sid,
                                parent: PartRef::Temp(format!("undo-{id}")),
                                sort_order: *sord,
                            });
                        }
                        inverse.push(DiffOp::Add {
                            temp: format!("undo-{id}"),
                            parent: undo_ref(old.parent_id),
                            name: old.name.clone(),
                            detail: old.detail.clone(),
                            lifecycle: old.lifecycle,
                            anchors: old.anchors.clone(),
                            kind: old.kind,
                            detail_md: Some(old.detail_md.clone()),
                            sort_order: Some(old.sort_order),
                            // carry the ORIGINAL citation so ⌘Z of a machine node
                            // restores its "why is this here?" answer (review:
                            // load_tree now hydrates the quad, so undo no longer
                            // drops it). created_by still becomes 'undo' — the
                            // current row was materialized by the undo.
                            source_file: old.source_file.clone(),
                            source_quote: old.source_quote.clone(),
                            rationale: old.rationale.clone(),
                        });
                    }
                    for (sid, _) in &survivors {
                        tx.execute(
                            "UPDATE part SET parent_id=?2 WHERE id=?1",
                            params![sid, cur_parent],
                        )?;
                    }
                    tx.execute("DELETE FROM part_anchor WHERE part_id=?1", params![id])?;
                    // linkage hygiene (docs/011): a removed node keeps no
                    // session links; undo re-adds under a NEW id, so linkage
                    // is deliberately not resurrected.
                    tx.execute("DELETE FROM session_part WHERE part_id=?1", params![id])?;
                    // needs_you hygiene (review 4): an orphaned flag would pulse
                    // the one-summons forever for a node that no longer exists,
                    // suppressing every real summons and un-dismissable.
                    tx.execute("DELETE FROM needs_you WHERE part_id=?1", params![id])?;
                    tx.execute("DELETE FROM part WHERE id=?1", params![id])?;
                }
                DiffOp::SetDetail { id, detail_md } => {
                    if let Some(old) = by_id.get(id) {
                        // inverse carries the full old body, capped at 16KB so
                        // giant prose can't bloat the journal (docs/019).
                        let mut old_md = old.detail_md.clone();
                        if old_md.len() > 16 * 1024 {
                            let mut cut = 16 * 1024;
                            while cut > 0 && !old_md.is_char_boundary(cut) {
                                cut -= 1;
                            }
                            old_md.truncate(cut);
                            old_md.push_str("\n… [truncated for undo]");
                        }
                        inverse.push(DiffOp::SetDetail {
                            id: *id,
                            detail_md: old_md,
                        });
                    }
                    tx.execute(
                        "UPDATE part SET detail_md=?2, detail=?3 WHERE id=?1",
                        params![id, detail_md, first_line(detail_md)],
                    )?;
                }
                DiffOp::SetKind { id, kind } => {
                    if let Some(old) = by_id.get(id) {
                        inverse.push(DiffOp::SetKind {
                            id: *id,
                            kind: old.kind,
                        });
                    }
                    tx.execute(
                        "UPDATE part SET kind=?2 WHERE id=?1",
                        params![id, kind.as_str()],
                    )?;
                }
            }
        }

        tx.execute(
            "INSERT INTO tree_event(project_key,accept_id,ts_secs,ops_json,inverse_json,origin,source_sess) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                key,
                accept_id,
                now() as i64,
                serde_json::to_string(ops).unwrap_or_default(),
                serde_json::to_string(&inverse).unwrap_or_default(),
                origin,
                source_sess,
            ],
        )?;
        tx.commit()?;
        self.mark_dirty();
        // any project that gains a tree is no longer in the seed CTA state.
        if !ops.is_empty() {
            let _ = self.set_seed_state(key, SeedState::Seeded);
        }
        Ok(accept_id)
    }

    /// Direct user assertion (immediate, journaled, uncarded — the user is the
    /// authority, docs/016). A thin wrapper over accept_diff.
    pub fn set_status(
        &mut self,
        key: &str,
        id: PartId,
        lifecycle: Lifecycle,
    ) -> rusqlite::Result<()> {
        self.accept_diff(
            key,
            &[DiffOp::SetStatus {
                id,
                lifecycle,
                source: StatusSource::User,
            }],
        )?;
        Ok(())
    }

    /// Mark a part stale (derived freshness — LOWERS confidence, never flips the
    /// lifecycle). Not journaled (it's a cache).
    pub fn set_stale(&self, id: PartId, stale: bool, reason: Option<&str>) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE part SET stale=?2, stale_reason=?3 WHERE id=?1",
            params![id, stale as i64, reason],
        )?;
        // The GUI renders the stale (◌) overlay from the tree and memoizes that
        // per-frame read against write_gen; bump it so the reconciler's change
        // repaints promptly instead of waiting for an unrelated write.
        self.bump_gen();
        Ok(())
    }

    /// Reconcile every asserted-`done` part in `key` against the code it anchors:
    /// mark it stale (◌) when an anchored file changed after the assertion, clear
    /// stale when it didn't. `root` is the project's working directory. Returns
    /// how many parts changed. Cache-only (not journaled). Run off the UI thread —
    /// it stats the filesystem.
    pub fn reconcile_staleness(
        &self,
        key: &str,
        root: &std::path::Path,
    ) -> rusqlite::Result<usize> {
        let mut changed = 0;
        for p in self.load_tree(key)? {
            if !crate::reconcile::is_candidate(&p) {
                continue; // skip the fs walk for todos/seeds/unanchored parts
            }
            let newest = crate::reconcile::newest_anchor_mtime(root, &p.anchors);
            if let Some(stale) = crate::reconcile::staleness(&p, newest) {
                if stale != p.stale {
                    let reason = stale.then_some("anchored code changed since you marked it done");
                    self.set_stale(p.id, stale, reason)?;
                    changed += 1;
                }
            }
        }
        Ok(changed)
    }

    /// Undo the most recent accept group for a project. Migration events are
    /// SKIPPED, never undone (review: the kind backfill journaled itself as
    /// the newest event, so the first post-upgrade ⌘Z silently reverted it —
    /// ⌘Z must only ever reach edits a human or an accepted diff made).
    pub fn undo_last(&mut self, key: &str) -> rusqlite::Result<bool> {
        let last: Option<(i64, String)> = self
            .conn
            .query_row(
                "SELECT id, inverse_json FROM tree_event
                 WHERE project_key=?1 AND (origin IS NULL OR origin<>'migration')
                 ORDER BY id DESC LIMIT 1",
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let Some((event_id, inverse_json)) = last else {
            return Ok(false);
        };
        let inverse: Vec<DiffOp> = serde_json::from_str(&inverse_json).unwrap_or_default();
        // apply the inverse REVERSED (docs/019 slice 1a): inverses are recorded
        // in forward op order, so replaying them backwards unwinds the diff —
        // a removed parent re-Adds BEFORE the Moves that reference it via
        // Temp("undo-<id>") resolve. Forward replay orphaned compound diffs.
        let reversed: Vec<DiffOp> = inverse.into_iter().rev().collect();
        // ONE transaction for the pair (review): the inverse used to commit in
        // apply_raw's own tx and the event was consumed by a SEPARATE statement
        // after it. A crash or a failing write in between left the tree and the
        // journal disagreeing — the consumed event was still the newest, so the
        // next ⌘Z replayed the same undo and ate the edit before it (or, the
        // other way, a real edit vanished with its event already gone).
        let tx = self.conn.transaction()?;
        // apply the inverse WITHOUT re-journaling it, then drop the event.
        Self::apply_raw_in_tx(&tx, key, &reversed)?;
        tx.execute("DELETE FROM tree_event WHERE id=?1", params![event_id])?;
        tx.commit()?;
        // undo mutates FTS-indexed part rows and the GUI's per-frame view —
        // without this an undone rename stayed in ⌘K until the next write.
        self.mark_dirty();
        Ok(true)
    }

    /// Apply ops directly without journaling (used by undo), inside a
    /// CALLER-OWNED transaction: undo must consume the journal event in the
    /// same tx as the ops it replays, so the commit point belongs to the
    /// caller, not here.
    ///
    /// ORDER-INSENSITIVE temp resolution (review): an op may reference a
    /// Temp("undo-<id>") whose re-Add appears LATER in the list (e.g. a diff
    /// that removed root-first). Unresolved refs apply as Root and are
    /// recorded; a fixup pass re-parents them once the whole list has run —
    /// so no ordering can scatter restored children.
    fn apply_raw_in_tx(
        tx: &rusqlite::Transaction,
        key: &str,
        ops: &[DiffOp],
    ) -> rusqlite::Result<()> {
        let mut temp_to_real: HashMap<String, PartId> = HashMap::new();
        // (row id, unresolved temp name) — re-parented after the loop.
        let mut deferred: Vec<(PartId, String)> = Vec::new();
        let defer = |r: &PartRef,
                     row: PartId,
                     temp_to_real: &HashMap<String, PartId>,
                     deferred: &mut Vec<(PartId, String)>| {
            if let PartRef::Temp(t) = r {
                if !temp_to_real.contains_key(t) {
                    deferred.push((row, t.clone()));
                }
            }
        };
        for op in ops {
            match op {
                DiffOp::Add {
                    temp,
                    parent,
                    name,
                    detail,
                    lifecycle,
                    anchors,
                    kind,
                    detail_md,
                    sort_order,
                    source_file,
                    source_quote,
                    rationale,
                } => {
                    let parent_id = resolve_ref(parent, &temp_to_real);
                    let body = detail_md.clone().unwrap_or_else(|| detail.clone());
                    let order = match sort_order {
                        Some(o) => *o,
                        None => next_order(tx, key, parent_id)?,
                    };
                    // created_by='undo' (a re-Add is its own authorship), but any
                    // provenance the inverse carried is preserved so a round-trip
                    // never loses the "why is this here?" quad.
                    tx.execute(
                        "INSERT INTO part(project_key,parent_id,name,detail,lifecycle,status_source,status_at_secs,sort_order,kind,detail_md,created_by,source_file,source_quote,rationale)
                         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'undo',?11,?12,?13)",
                        params![key, parent_id, name, detail, lifecycle.assertable().as_str(), StatusSource::User.as_str(), now() as i64, order, kind.as_str(), body, source_file, source_quote, rationale],
                    )?;
                    let real = tx.last_insert_rowid();
                    temp_to_real.insert(temp.clone(), real);
                    defer(parent, real, &temp_to_real, &mut deferred);
                    for g in anchors {
                        tx.execute(
                            "INSERT INTO part_anchor(part_id,glob) VALUES(?1,?2)",
                            params![real, g],
                        )?;
                    }
                }
                DiffOp::SetStatus {
                    id,
                    lifecycle,
                    source,
                } => {
                    tx.execute("UPDATE part SET lifecycle=?2,status_source=?3,status_at_secs=?4 WHERE id=?1", params![id, lifecycle.assertable().as_str(), source.as_str(), now() as i64])?;
                }
                DiffOp::Rename { id, name, detail } => {
                    if detail.is_empty() {
                        tx.execute(
                            "UPDATE part SET name=?2,detail=?3 WHERE id=?1",
                            params![id, name, detail],
                        )?;
                    } else {
                        // same invariant as the journaled arm: the one-liner
                        // is the body's first line.
                        let old_md: String = tx
                            .query_row(
                                "SELECT COALESCE(detail_md,detail) FROM part WHERE id=?1",
                                params![id],
                                |r| r.get(0),
                            )
                            .unwrap_or_default();
                        let body = crate::tree::replace_first_line(&old_md, detail);
                        tx.execute(
                            "UPDATE part SET name=?2,detail=?3,detail_md=?4 WHERE id=?1",
                            params![id, name, detail, body],
                        )?;
                    }
                }
                DiffOp::Move {
                    id,
                    parent,
                    sort_order,
                } => {
                    let parent_id = resolve_ref(parent, &temp_to_real);
                    // same cycle guard as the forward path — a valid inverse
                    // never cycles, but undo replay must never corrupt either.
                    if would_create_cycle(tx, *id, parent_id) {
                        continue;
                    }
                    tx.execute(
                        "UPDATE part SET parent_id=?2,sort_order=?3 WHERE id=?1",
                        params![id, parent_id, sort_order],
                    )?;
                    defer(parent, *id, &temp_to_real, &mut deferred);
                }
                DiffOp::Remove { id } => {
                    tx.execute("DELETE FROM part_anchor WHERE part_id=?1", params![id])?;
                    tx.execute("DELETE FROM session_part WHERE part_id=?1", params![id])?;
                    tx.execute("DELETE FROM needs_you WHERE part_id=?1", params![id])?;
                    tx.execute("DELETE FROM part WHERE id=?1", params![id])?;
                }
                DiffOp::SetDetail { id, detail_md } => {
                    tx.execute(
                        "UPDATE part SET detail_md=?2, detail=?3 WHERE id=?1",
                        params![id, detail_md, first_line(detail_md)],
                    )?;
                }
                DiffOp::SetKind { id, kind } => {
                    tx.execute(
                        "UPDATE part SET kind=?2 WHERE id=?1",
                        params![id, kind.as_str()],
                    )?;
                }
            }
        }
        // fixup: temps that resolved only after their referencing op ran.
        for (row, t) in deferred {
            if let Some(real) = temp_to_real.get(&t) {
                tx.execute(
                    "UPDATE part SET parent_id=?2 WHERE id=?1",
                    params![row, real],
                )?;
            }
        }
        Ok(())
    }

}

fn resolve_ref(r: &PartRef, temp: &HashMap<String, PartId>) -> Option<PartId> {
    match r {
        PartRef::Id(id) => Some(*id),
        PartRef::Temp(t) => temp.get(t).copied(),
        PartRef::Root => None,
    }
}

/// Would moving `id` under `parent_id` place it inside its OWN subtree — a cycle
/// that makes both nodes unreachable from any root (invisible to build_tree),
/// i.e. silent data loss? Walks up from parent_id within the tx (review 2b: the
/// first machine source of arbitrary multi-Moves could emit `A→B` + `B→A`; the
/// parser flags it, this is the store's last-line guard, like orphan-proof
/// Remove). A None/root parent never cycles; a pre-existing cycle is refused.
/// Does the Move's target parent still EXIST? Root (None) always does; a real
/// id must be a live row (review 2b: a stale snapshot could move onto a deleted
/// node, orphaning the moved row since part.parent_id has no FK).
fn parent_exists(tx: &rusqlite::Transaction, parent_id: Option<PartId>) -> bool {
    match parent_id {
        None => true,
        Some(pid) => tx
            .query_row("SELECT 1 FROM part WHERE id=?1", params![pid], |_| Ok(()))
            .is_ok(),
    }
}

fn would_create_cycle(tx: &rusqlite::Transaction, id: PartId, parent_id: Option<PartId>) -> bool {
    let mut cur = parent_id;
    let mut guard = 0;
    while let Some(c) = cur {
        if c == id {
            return true;
        }
        guard += 1;
        if guard > 100_000 {
            return true;
        }
        cur = tx
            .query_row("SELECT parent_id FROM part WHERE id=?1", params![c], |r| {
                r.get(0)
            })
            .ok()
            .flatten();
    }
    false
}

fn next_order(
    tx: &rusqlite::Transaction,
    key: &str,
    parent: Option<PartId>,
) -> rusqlite::Result<f64> {
    let max: Option<f64> = tx
        .query_row(
            "SELECT MAX(sort_order) FROM part WHERE project_key=?1 AND parent_id IS ?2",
            params![key, parent],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    Ok(max.unwrap_or(0.0) + 1.0)
}
