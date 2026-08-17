//! Hosted-session tracking, session<->part links, and the Standup timeline.
//! Extracted verbatim from `store.rs` (decomposition; behavior unchanged).

use std::collections::HashMap;

use rusqlite::params;

use super::{now, HostedSessionRow, SessionPartRow, Store, TimelineEvent, TimelineKind};

impl Store {
    // --- hosted sessions (crash recovery) ---

    /// Record a session we host (fresh or resumed) at spawn time. UPSERT by the
    /// CLI session id (resuming the same session updates the row, keeps it
    /// alive). Written synchronously so a crash leaves a durable row.
    pub fn record_session(
        &self,
        cli_session_id: &str,
        project_key: &str,
        kind: &str,
        cwd: &str,
        profile_id: Option<i64>,
    ) -> rusqlite::Result<()> {
        let n = now() as i64;
        self.conn.execute(
            "INSERT INTO hosted_session(cli_session_id,project_key,kind,cwd,started_secs,last_seen_secs,alive,profile_id)
             VALUES(?1,?2,?3,?4,?5,?5,1,?6)
             ON CONFLICT(cli_session_id) DO UPDATE SET project_key=?2,kind=?3,cwd=?4,last_seen_secs=?5,alive=1,
                 closed_at_secs=NULL,dismissed_at_secs=NULL,profile_id=?6",
            params![cli_session_id, project_key, kind, cwd, n, profile_id],
        )?;
        Ok(())
    }

    /// Bump a session's last-seen (cheap heartbeat; optional).
    pub fn touch_session(&self, cli_session_id: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE hosted_session SET last_seen_secs=?2 WHERE cli_session_id=?1",
            params![cli_session_id, now() as i64],
        )?;
        Ok(())
    }

    /// Mark a session no longer alive (user closed it gracefully) so it is NOT
    /// offered for restore on the next launch.
    pub fn close_session(&self, cli_session_id: &str) -> rusqlite::Result<()> {
        // closed_at stamps the ENDED tombstone strip (#4); COALESCE keeps the
        // first close time if the row is closed twice.
        self.conn.execute(
            "UPDATE hosted_session SET alive=0, closed_at_secs=COALESCE(closed_at_secs, ?2) WHERE cli_session_id=?1",
            params![cli_session_id, now() as i64],
        )?;
        Ok(())
    }

    /// Recently-ended sessions for the ENDED strip: closed within the window,
    /// not dismissed, newest first. Returns (row, closed_at_secs).
    pub fn recently_closed(
        &self,
        since_secs: u64,
        limit: usize,
    ) -> rusqlite::Result<Vec<(HostedSessionRow, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT cli_session_id,project_key,kind,cwd,started_secs,last_seen_secs,closed_at_secs,profile_id
             FROM hosted_session
             WHERE alive=0 AND dismissed_at_secs IS NULL AND closed_at_secs>=?1
             ORDER BY closed_at_secs DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![since_secs as i64, limit as i64], |r| {
                Ok((
                    HostedSessionRow {
                        cli_session_id: r.get(0)?,
                        project_key: r.get(1)?,
                        kind: r.get(2)?,
                        cwd: r.get(3)?,
                        started_secs: r.get::<_, i64>(4)? as u64,
                        last_seen_secs: r.get::<_, i64>(5)? as u64,
                        profile_id: r.get(7)?,
                    },
                    r.get::<_, i64>(6)? as u64,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Re-home a session's crash record (#10 move-session).
    pub fn rebind_session(&self, cli_session_id: &str, project_key: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE hosted_session SET project_key=?2 WHERE cli_session_id=?1",
            params![cli_session_id, project_key],
        )?;
        Ok(())
    }

    /// Dismiss a tombstone from the ENDED strip — NEVER a DELETE: the row keeps
    /// its project binding for Recover's homing (design critique #4).
    pub fn dismiss_session(&self, cli_session_id: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE hosted_session SET dismissed_at_secs=?2 WHERE cli_session_id=?1",
            params![cli_session_id, now() as i64],
        )?;
        Ok(())
    }

    /// Sessions that were still `alive` — at LAUNCH these are necessarily from a
    /// prior process (a crash, or left open), i.e. the restore-on-launch set.
    /// Newest-first. Codex `kind`s with shells excluded by the caller if wanted.
    pub fn restorable_sessions(&self) -> rusqlite::Result<Vec<HostedSessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT cli_session_id,project_key,kind,cwd,started_secs,last_seen_secs,profile_id
             FROM hosted_session WHERE alive=1 ORDER BY last_seen_secs DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(HostedSessionRow {
                cli_session_id: r.get(0)?,
                project_key: r.get(1)?,
                kind: r.get(2)?,
                cwd: r.get(3)?,
                started_secs: r.get::<_, i64>(4)? as u64,
                last_seen_secs: r.get::<_, i64>(5)? as u64,
                profile_id: r.get(6)?,
            })
        })?;
        rows.collect()
    }

    /// Mark every alive row closed — called once after the restore-on-launch
    /// prompt has been presented, so a dismissed prompt does not re-offer next
    /// launch. Sessions actually restored are re-recorded (alive=1) by the
    /// resume path, so this is safe to call before issuing restores.
    pub fn clear_restorable(&self) -> rusqlite::Result<()> {
        self.conn
            .execute("UPDATE hosted_session SET alive=0 WHERE alive=1", [])?;
        Ok(())
    }

    // --- manual project-attach (durable intent; independent of hosted_session) ---

    /// File a session (by its on-disk id) under a project. UPSERT — re-filing
    /// overwrites. Never touches `hosted_session`, so it can't affect restore.
    pub fn set_override(&self, cli_session_id: &str, project_key: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO session_project_override(cli_session_id,project_key,set_at_secs) VALUES(?1,?2,?3)
             ON CONFLICT(cli_session_id) DO UPDATE SET project_key=?2,set_at_secs=?3",
            params![cli_session_id, project_key, now() as i64],
        )?;
        Ok(())
    }

    /// All manual attachments: cli_session_id -> project_key.
    pub fn overrides_map(&self) -> rusqlite::Result<HashMap<String, String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT cli_session_id,project_key FROM session_project_override")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect()
    }

    pub fn clear_override(&self, cli_session_id: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM session_project_override WHERE cli_session_id=?1",
            params![cli_session_id],
        )?;
        Ok(())
    }

    // --- session↔node linkage (docs/011: dispatch + live attribution) ---

    /// Link a session to a node. Roles (docs/019): "dispatch" (declared intent
    /// at spawn), "declared" (session-asserted mid-flight), "trail" (demoted
    /// prior dispatch), "touch" (observed). The PK holds ONE role per
    /// (session, node), so the upsert applies ROLE PRECEDENCE
    /// dispatch > declared > trail > touch — a declared link arriving after an
    /// observed touch upgrades the row; a touch never downgrades anything.
    pub fn link_session_part(
        &self,
        cli_session_id: &str,
        part_id: i64,
        project_key: &str,
        role: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO session_part(cli_session_id,part_id,project_key,role,at_secs) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(cli_session_id,part_id) DO UPDATE SET role=excluded.role, at_secs=excluded.at_secs
             WHERE (CASE excluded.role WHEN 'dispatch' THEN 4 WHEN 'declared' THEN 3 WHEN 'trail' THEN 2 ELSE 1 END)
                 > (CASE session_part.role WHEN 'dispatch' THEN 4 WHEN 'declared' THEN 3 WHEN 'trail' THEN 2 ELSE 1 END)",
            params![cli_session_id, part_id, project_key, role, now() as i64],
        )?;
        self.bump_gen();
        Ok(())
    }

    /// Record observed file activity on a node (docs/019 slice 3 writes these
    /// live, not at a summarize that never fires). Accumulates weight + stamps
    /// recency; NEVER changes an existing row's role — observation may earn a
    /// hollow chip, it never rewrites intent.
    pub fn record_touch(
        &self,
        cli_session_id: &str,
        part_id: i64,
        project_key: &str,
        weight: f64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO session_part(cli_session_id,part_id,project_key,role,at_secs,weight,last_touch_secs)
             VALUES(?1,?2,?3,'touch',?4,?5,?4)
             ON CONFLICT(cli_session_id,part_id) DO UPDATE SET
                 weight = session_part.weight + excluded.weight,
                 last_touch_secs = excluded.last_touch_secs",
            params![cli_session_id, part_id, project_key, now() as i64, weight],
        )?;
        self.bump_gen();
        Ok(())
    }

    /// Move a session's dispatch link to another node (the one-click relink).
    /// AT MOST ONE dispatch row per session, but the prior dispatch DEMOTES to
    /// 'trail' instead of being deleted (docs/019: the map keeps the history —
    /// where a session came from is part of its truth). The precedence upsert
    /// upgrades any prior touch/trail row on the target.
    pub fn relink_session_part(
        &self,
        cli_session_id: &str,
        part_id: i64,
        project_key: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE session_part SET role='trail' WHERE cli_session_id=?1 AND role='dispatch' AND part_id<>?2",
            params![cli_session_id, part_id],
        )?;
        self.link_session_part(cli_session_id, part_id, project_key, "dispatch")
    }

    /// cli_session_id -> part_id for every DISPATCHED session in a project —
    /// the per-frame join against live session infos that fills the map's
    /// agent chips (memoize against write_gen). Errors fold to empty: a frame
    /// with no chips, never a panic.
    pub fn session_dispatch_map(&self, project_key: &str) -> HashMap<String, i64> {
        let Ok(mut stmt) = self
            .conn
            .prepare("SELECT cli_session_id, part_id FROM session_part WHERE project_key=?1 AND role='dispatch'")
        else {
            return HashMap::new();
        };
        stmt.query_map(params![project_key], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// EVERY session_part row for a project (docs/019 slice 3) — the per-frame
    /// snapshot the map derives building, chip tiers, and drift from. Memoize
    /// against write_gen like session_dispatch_map. Errors fold to empty.
    pub fn session_parts(&self, project_key: &str) -> Vec<SessionPartRow> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT cli_session_id, part_id, role, at_secs, COALESCE(weight,0), last_touch_secs
             FROM session_part WHERE project_key=?1",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![project_key], |r| {
            Ok(SessionPartRow {
                cli_session_id: r.get(0)?,
                part_id: r.get(1)?,
                role: r.get(2)?,
                at_secs: r.get::<_, i64>(3)? as u64,
                weight: r.get(4)?,
                last_touch_secs: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// Sessions linked to a node — (cli_session_id, role, at_secs): dispatch
    /// rows first, then newest-first (the outline's SESSIONS section).
    pub fn sessions_for_part(&self, part_id: i64) -> Vec<(String, String, i64)> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT cli_session_id, role, at_secs FROM session_part WHERE part_id=?1
             ORDER BY (role='dispatch') DESC, at_secs DESC, rowid DESC",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![part_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    /// The node a session was dispatched onto (None: never dispatched, or the
    /// node was removed). `.ok()` folds a missing row to None.
    pub fn dispatched_part(&self, cli_session_id: &str) -> Option<i64> {
        self.conn
            .query_row(
                "SELECT part_id FROM session_part WHERE cli_session_id=?1 AND role='dispatch'",
                params![cli_session_id],
                |r| r.get(0),
            )
            .ok()
    }

    /// The profile (account) a hosted session was recorded under, by its cli id —
    /// so a disk-scanned resume (which carries no store row) can re-adopt the SAME
    /// account it ran under. None = no row / no profile (an imported external
    /// session, or one spawned before the profile picker existed). `delete_profile`
    /// NULLs the column, so a dangling id can never come back here.
    pub fn session_profile_id(&self, cli_session_id: &str) -> Option<i64> {
        self.conn
            .query_row(
                "SELECT profile_id FROM hosted_session WHERE cli_session_id=?1",
                params![cli_session_id],
                |r| r.get(0),
            )
            .ok()
            .flatten()
    }

    /// The Standup timeline (docs/012 §2): one merged, ts-DESC feed over what
    /// the store already records — summaries, dispatch trails, user decisions,
    /// batched map accepts. A VIEW, not a new event system. Infallible per the
    /// per-frame-reader convention (errors fold to empty).
    pub fn timeline(&self, cap: usize) -> Vec<TimelineEvent> {
        let mut out: Vec<TimelineEvent> = Vec::new();
        // ☁ session summaries — the spine.
        if let Ok(mut st) = self.conn.prepare(
            "SELECT at_ms, project_key, sess, headline, next_action, detail_json FROM session_summary ORDER BY at_ms DESC LIMIT ?1",
        ) {
            let rows = st.query_map(params![cap as i64], |r| {
                Ok(TimelineEvent {
                    ts_ms: r.get(0)?,
                    project_key: r.get(1)?,
                    kind: TimelineKind::Summary,
                    sess: r.get(2)?,
                    node: None,
                    text: r.get(3)?,
                    next: r.get(4)?,
                    detail_json: r.get(5)?,
                    count: 1,
                })
            });
            if let Ok(rows) = rows {
                out.extend(rows.flatten());
            }
        }
        // ▶/■ dispatch trail + ◆ user decisions (node-attributed part_notes).
        if let Ok(mut st) = self.conn.prepare(
            "SELECT n.ts_secs, p.project_key, n.kind, n.text, n.source, p.id, p.name
             FROM part_note n JOIN part p ON p.id = n.part_id
             WHERE n.kind = 'session' OR (n.kind = 'decision' AND n.source = 'user')
             ORDER BY n.ts_secs DESC LIMIT ?1",
        ) {
            let rows = st.query_map(params![cap as i64], |r| {
                let kind: String = r.get(2)?;
                let source: String = r.get(4)?;
                Ok(TimelineEvent {
                    ts_ms: r.get::<_, u64>(0)?.saturating_mul(1000),
                    project_key: r.get(1)?,
                    kind: if kind == "decision" {
                        TimelineKind::Decision
                    } else {
                        TimelineKind::Trail
                    },
                    sess: source.strip_prefix("sess-").unwrap_or("").to_string(),
                    node: Some((r.get(5)?, r.get(6)?)),
                    text: r.get(3)?,
                    next: String::new(),
                    detail_json: String::new(),
                    count: 1,
                })
            });
            if let Ok(rows) = rows {
                out.extend(rows.flatten());
            }
        }
        // 🗺 map accepts, BATCHED: same project within 10 min = one entry.
        if let Ok(mut st) = self.conn.prepare(
            "SELECT ts_secs, project_key, ops_json FROM tree_event ORDER BY ts_secs DESC LIMIT ?1",
        ) {
            let raw: Vec<(u64, String, String)> = st
                .query_map(params![cap as i64], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default();
            let mut batches: Vec<TimelineEvent> = Vec::new();
            for (ts, key, ops) in raw {
                let n = serde_json::from_str::<Vec<serde_json::Value>>(&ops)
                    .map(|v| v.len())
                    .unwrap_or(1);
                match batches.last_mut() {
                    Some(b)
                        if b.project_key == key && b.ts_ms.saturating_sub(ts * 1000) < 600_000 =>
                    {
                        b.count += n;
                    }
                    _ => batches.push(TimelineEvent {
                        ts_ms: ts.saturating_mul(1000),
                        project_key: key,
                        kind: TimelineKind::Map,
                        sess: String::new(),
                        node: None,
                        text: String::new(),
                        next: String::new(),
                        detail_json: String::new(),
                        count: n,
                    }),
                }
            }
            out.extend(batches);
        }
        out.sort_by(|a, b| b.ts_ms.cmp(&a.ts_ms));
        out.truncate(cap);
        out
    }

    /// dispatched_part plus the project key — the end-of-life note needs both
    /// (docs/011: "■ session ended" lands on the dispatched node's log).
    pub fn dispatch_of(&self, cli_session_id: &str) -> Option<(i64, String)> {
        self.conn
            .query_row(
                "SELECT part_id, project_key FROM session_part WHERE cli_session_id=?1 AND role='dispatch'",
                params![cli_session_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()
    }

    /// part_id -> last-activity epoch secs across a project: the max of linked
    /// session activity (session_part.at_secs, ANY role — a touch is activity
    /// too) and log entries (part_note.ts_secs). Parts with neither are absent
    /// (= never active). Feeds the quiet_building drift detector (docs/011
    /// slice 3). Errors fold to empty: a sweep that skips, never a panic.
    pub fn part_activity(&self, project_key: &str) -> HashMap<i64, u64> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT part_id, MAX(at) FROM (
                 SELECT part_id, at_secs AS at FROM session_part WHERE project_key=?1
                 UNION ALL
                 SELECT part_id, ts_secs AS at FROM part_note WHERE project_key=?1
             ) GROUP BY part_id",
        ) else {
            return HashMap::new();
        };
        stmt.query_map(params![project_key], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, u64>(1)?))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }
}
