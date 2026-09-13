//! Tree loading and diff application (accept / undo / status / staleness),
//! the raw op-apply engine, and its ref/cycle/order helpers.
//! Extracted verbatim from `store.rs` (decomposition; behavior unchanged).

use std::collections::HashMap;

use rusqlite::{params, OptionalExtension};

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
        self.accept_diff_from_at(key, ops, origin, source_sess, now())
    }

    /// Apply a diff at an explicit logical time. Sealed replays and shadow
    /// simulations use this to prove byte-for-byte repeatability; live actions
    /// should use [`Store::accept_diff_from`] so wall-clock time is captured.
    /// One timestamp is shared by the accept id, notes, assertions, and journal
    /// row, avoiding internally inconsistent accepts that straddle a second.
    pub fn accept_diff_from_at(
        &mut self,
        key: &str,
        ops: &[DiffOp],
        origin: &str,
        source_sess: Option<&str>,
        at_secs: u64,
    ) -> rusqlite::Result<String> {
        let before = self.load_tree(key)?;
        let by_id: HashMap<PartId, Part> = before.iter().map(|p| (p.id, p.clone())).collect();
        let accept_id = format!("acc-{at_secs}-{}", self.conn.last_insert_rowid());
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
                            at_secs as i64,
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
                        params![id, lifecycle.assertable().as_str(), source.as_str(), at_secs as i64],
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
                        // Memory-engine rows are removable projections, not the
                        // append-only user/session log. Carry them through the
                        // node's undo and remove the old projection now; leaving
                        // one pointed at a dead part would both orphan the UI row
                        // and make this source revision collide on re-projection.
                        for (text, memory_id, revision_id) in
                            take_decision_projections_for_part(&tx, key, *id)?
                        {
                            inverse.push(DiffOp::AddDecision {
                                part: PartRef::Temp(format!("undo-{id}")),
                                text,
                                source_memory_id: memory_id,
                                source_revision_id: revision_id,
                            });
                        }
                        let (primary_notes, linked_notes) =
                            retained_note_targets_for_removed_part(&tx, key, *id)?;
                        for note_id in primary_notes {
                            inverse.push(DiffOp::RestoreNoteTarget {
                                note_id,
                                part: PartRef::Temp(format!("undo-{id}")),
                                primary: true,
                            });
                        }
                        for note_id in linked_notes {
                            inverse.push(DiffOp::RestoreNoteTarget {
                                note_id,
                                part: PartRef::Temp(format!("undo-{id}")),
                                primary: false,
                            });
                        }
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
                    // Cross-cutting links may point at an append-only note whose
                    // primary node survives. The note remains; only its dead
                    // secondary target is removed.
                    tx.execute("DELETE FROM note_part WHERE part_id=?1", params![id])?;
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
                DiffOp::AddDecision {
                    part,
                    text,
                    source_memory_id,
                    source_revision_id,
                } => {
                    let part_id = resolve_decision_part(&tx, key, part, &temp_to_real)?;
                    if let Some(note_id) = insert_decision_projection(
                        &tx,
                        key,
                        part_id,
                        text,
                        source_memory_id,
                        source_revision_id,
                        at_secs,
                    )? {
                        inverse.push(DiffOp::RemoveDecision { note_id });
                    }
                }
                DiffOp::RemoveDecision { note_id } => {
                    if let Some((part_id, text, source_memory_id, source_revision_id)) =
                        delete_decision_projection(&tx, key, *note_id)?
                    {
                        inverse.push(DiffOp::AddDecision {
                            part: undo_ref(Some(part_id)),
                            text,
                            source_memory_id,
                            source_revision_id,
                        });
                    }
                }
                DiffOp::RestoreNoteTarget { .. } => {
                    return Err(invalid_decision(
                        "RestoreNoteTarget is an internal undo operation",
                    ));
                }
            }
        }

        tx.execute(
            "INSERT INTO tree_event(project_key,accept_id,ts_secs,ops_json,inverse_json,origin,source_sess) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                key,
                accept_id,
                at_secs as i64,
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
        let replay_at_secs = now();
        let mut temp_to_real: HashMap<String, PartId> = HashMap::new();
        // (row id, unresolved temp name) — re-parented after the loop.
        let mut deferred: Vec<(PartId, String)> = Vec::new();
        // A decision inverse can target a part restored later in the replay.
        // Defer it just like a Move's parent fixup instead of ever attaching it
        // to Root or losing it because of inverse ordering.
        let mut deferred_decisions: Vec<(PartRef, String, String, String)> = Vec::new();
        let mut deferred_note_targets: Vec<(i64, PartRef, bool)> = Vec::new();
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
                        params![key, parent_id, name, detail, lifecycle.assertable().as_str(), StatusSource::User.as_str(), replay_at_secs as i64, order, kind.as_str(), body, source_file, source_quote, rationale],
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
                    tx.execute("UPDATE part SET lifecycle=?2,status_source=?3,status_at_secs=?4 WHERE id=?1", params![id, lifecycle.assertable().as_str(), source.as_str(), replay_at_secs as i64])?;
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
                    let _ = take_decision_projections_for_part(tx, key, *id)?;
                    tx.execute("DELETE FROM part_anchor WHERE part_id=?1", params![id])?;
                    tx.execute("DELETE FROM session_part WHERE part_id=?1", params![id])?;
                    tx.execute("DELETE FROM needs_you WHERE part_id=?1", params![id])?;
                    tx.execute("DELETE FROM note_part WHERE part_id=?1", params![id])?;
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
                DiffOp::AddDecision {
                    part,
                    text,
                    source_memory_id,
                    source_revision_id,
                } => {
                    if matches!(part, PartRef::Temp(t) if !temp_to_real.contains_key(t)) {
                        deferred_decisions.push((
                            part.clone(),
                            text.clone(),
                            source_memory_id.clone(),
                            source_revision_id.clone(),
                        ));
                        continue;
                    }
                    let part_id = resolve_decision_part(tx, key, part, &temp_to_real)?;
                    let _ = insert_decision_projection(
                        tx,
                        key,
                        part_id,
                        text,
                        source_memory_id,
                        source_revision_id,
                        replay_at_secs,
                    )?;
                }
                DiffOp::RemoveDecision { note_id } => {
                    let _ = delete_decision_projection(tx, key, *note_id)?;
                }
                DiffOp::RestoreNoteTarget {
                    note_id,
                    part,
                    primary,
                } => {
                    if matches!(part, PartRef::Temp(t) if !temp_to_real.contains_key(t)) {
                        deferred_note_targets.push((*note_id, part.clone(), *primary));
                        continue;
                    }
                    let part_id = resolve_decision_part(tx, key, part, &temp_to_real)?;
                    restore_note_target(tx, key, *note_id, part_id, *primary)?;
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
        for (part, text, source_memory_id, source_revision_id) in deferred_decisions {
            let part_id = resolve_decision_part(tx, key, &part, &temp_to_real)?;
            let _ = insert_decision_projection(
                tx,
                key,
                part_id,
                &text,
                &source_memory_id,
                &source_revision_id,
                replay_at_secs,
            )?;
        }
        for (note_id, part, primary) in deferred_note_targets {
            let part_id = resolve_decision_part(tx, key, &part, &temp_to_real)?;
            restore_note_target(tx, key, note_id, part_id, primary)?;
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

// Deliberately named for the ROLE, not for whichever engine is computing the
// memory today: this prefix is a persisted data format (it lands in
// part_note.source), so a vendor name here would be a migration to undo, and
// the port above is engine-agnostic on purpose.
const MEMORY_NOTE_PREFIX: &str = "memory:";

fn invalid_decision(message: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::InvalidParameterName(message.into())
}

fn memory_note_source(memory_id: &str, revision_id: &str) -> rusqlite::Result<String> {
    if memory_id.trim().is_empty() || revision_id.trim().is_empty() {
        return Err(invalid_decision(
            "memory decision source ids must both be non-empty",
        ));
    }
    if memory_id.len() > 1024 || revision_id.len() > 1024 {
        return Err(invalid_decision(
            "memory decision source ids must be at most 1024 bytes",
        ));
    }
    let encoded = serde_json::to_string(&(memory_id, revision_id))
        .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?;
    Ok(format!("{MEMORY_NOTE_PREFIX}{encoded}"))
}

fn parse_memory_note_source(source: &str) -> Option<(String, String)> {
    let encoded = source.strip_prefix(MEMORY_NOTE_PREFIX)?;
    let (memory_id, revision_id): (String, String) = serde_json::from_str(encoded).ok()?;
    (!memory_id.trim().is_empty() && !revision_id.trim().is_empty())
        .then_some((memory_id, revision_id))
}

fn resolve_decision_part(
    tx: &rusqlite::Transaction,
    key: &str,
    part: &PartRef,
    temp_to_real: &HashMap<String, PartId>,
) -> rusqlite::Result<PartId> {
    let part_id = resolve_ref(part, temp_to_real).ok_or_else(|| {
        invalid_decision("a memory decision must target an existing map node")
    })?;
    tx.query_row(
        "SELECT id FROM part WHERE id=?1 AND project_key=?2",
        params![part_id, key],
        |row| row.get(0),
    )
}

/// Insert one application-facing projection of a memory-engine decision. The
/// source revision is the idempotency key: replaying the exact same projection
/// is a no-op, while reusing it for different text/placement is rejected as a
/// provenance collision instead of silently changing accepted memory.
fn insert_decision_projection(
    tx: &rusqlite::Transaction,
    key: &str,
    part_id: PartId,
    text: &str,
    memory_id: &str,
    revision_id: &str,
    at_secs: u64,
) -> rusqlite::Result<Option<i64>> {
    if text.trim().is_empty() {
        return Err(invalid_decision("a memory decision cannot be empty"));
    }
    if text.len() > 16 * 1024 {
        return Err(invalid_decision(
            "a memory decision cannot exceed 16384 bytes",
        ));
    }
    let source = memory_note_source(memory_id, revision_id)?;
    let existing: Option<(i64, PartId, String)> = tx
        .query_row(
            "SELECT id,part_id,text FROM part_note
             WHERE project_key=?1 AND kind='decision' AND source=?2
             ORDER BY id LIMIT 1",
            params![key, source],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if let Some((_note_id, existing_part, existing_text)) = existing {
        if existing_part == part_id && existing_text == text {
            return Ok(None);
        }
        return Err(invalid_decision(format!(
            "memory revision {revision_id} is already projected differently"
        )));
    }
    tx.execute(
        "INSERT INTO part_note(part_id,project_key,ts_secs,kind,text,source)
         VALUES(?1,?2,?3,'decision',?4,?5)",
        params![part_id, key, at_secs as i64, text, source],
    )?;
    Ok(Some(tx.last_insert_rowid()))
}

/// Delete only a machine projection carrying a valid memory-engine source pointer.
/// User and session notes cannot be targeted by the internal undo variant.
fn delete_decision_projection(
    tx: &rusqlite::Transaction,
    key: &str,
    note_id: i64,
) -> rusqlite::Result<Option<(PartId, String, String, String)>> {
    let row: Option<(PartId, String, String)> = tx
        .query_row(
            "SELECT part_id,text,source FROM part_note
             WHERE id=?1 AND project_key=?2 AND kind='decision'",
            params![note_id, key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((part_id, text, source)) = row else {
        return Ok(None);
    };
    let (memory_id, revision_id) = parse_memory_note_source(&source).ok_or_else(|| {
        invalid_decision("RemoveDecision may only remove a memory-engine projection")
    })?;
    tx.execute("DELETE FROM note_part WHERE note_id=?1", params![note_id])?;
    tx.execute("DELETE FROM part_note WHERE id=?1", params![note_id])?;
    Ok(Some((part_id, text, memory_id, revision_id)))
}

/// Remove every memory-engine display projection whose primary target is a part
/// being deleted, returning enough durable identity to re-project it if the
/// node deletion is undone. User and session notes are deliberately untouched.
fn take_decision_projections_for_part(
    tx: &rusqlite::Transaction,
    key: &str,
    part_id: PartId,
) -> rusqlite::Result<Vec<(String, String, String)>> {
    let rows = {
        let mut statement = tx.prepare(
            "SELECT id,text,source FROM part_note
             WHERE part_id=?1 AND project_key=?2 AND kind='decision' AND source LIKE 'memory:%'
             ORDER BY id",
        )?;
        let rows = statement
            .query_map(params![part_id, key], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let mut removed = Vec::with_capacity(rows.len());
    for (note_id, text, source) in rows {
        let (memory_id, revision_id) = parse_memory_note_source(&source).ok_or_else(|| {
            invalid_decision("a reserved memory projection has invalid source identity")
        })?;
        tx.execute("DELETE FROM note_part WHERE note_id=?1", params![note_id])?;
        tx.execute("DELETE FROM part_note WHERE id=?1", params![note_id])?;
        removed.push((text, memory_id, revision_id));
    }
    Ok(removed)
}

/// Capture append-only notes and links whose target id is about to disappear.
/// The rows themselves are never deleted. Undo uses the returned identities to
/// point them at the replacement node id; a permanent node deletion leaves the
/// primary notes as durable history rather than silently erasing them.
fn retained_note_targets_for_removed_part(
    tx: &rusqlite::Transaction,
    key: &str,
    part_id: PartId,
) -> rusqlite::Result<(Vec<i64>, Vec<i64>)> {
    let primary = {
        let mut statement = tx.prepare(
            "SELECT id,project_key FROM part_note WHERE part_id=?1 ORDER BY id",
        )?;
        let rows = statement
            .query_map(params![part_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if rows.iter().any(|(_, project_key)| project_key != key) {
            return Err(invalid_decision(
                "a part note crossed the project boundary before node removal",
            ));
        }
        rows.into_iter().map(|(note_id, _)| note_id).collect()
    };
    let linked = {
        let mut statement =
            tx.prepare("SELECT note_id FROM note_part WHERE part_id=?1 ORDER BY note_id")?;
        let rows = statement
            .query_map(params![part_id], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    Ok((primary, linked))
}

fn restore_note_target(
    tx: &rusqlite::Transaction,
    key: &str,
    note_id: i64,
    part_id: PartId,
    primary: bool,
) -> rusqlite::Result<()> {
    if primary {
        let changed = tx.execute(
            "UPDATE part_note SET part_id=?3
             WHERE id=?1 AND project_key=?2 AND source NOT LIKE 'memory:%'",
            params![note_id, key, part_id],
        )?;
        if changed != 1 {
            return Err(invalid_decision(
                "append-only note disappeared or changed identity before undo",
            ));
        }
    } else {
        let exists = tx
            .query_row(
                "SELECT 1 FROM part_note WHERE id=?1",
                params![note_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            return Err(invalid_decision(
                "linked note disappeared before node-removal undo",
            ));
        }
        tx.execute(
            "INSERT OR IGNORE INTO note_part(note_id,part_id) VALUES(?1,?2)",
            params![note_id, part_id],
        )?;
    }
    Ok(())
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
