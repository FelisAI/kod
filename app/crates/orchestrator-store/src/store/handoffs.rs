//! Session handoff lineage (docs/028): which session a session was handed off
//! from/to, and where its packet lives. The `handoff` row survives a restart;
//! a codex target's session id arrives after spawn, so lineage is recorded
//! with a NULL target first and filled when that id is discovered.

use rusqlite::params;

use super::{now, HandoffRow, Store};

/// The `handoff` columns, in the order `row_to_handoff` reads them. Shared by
/// every select so by-id and both directions of lineage can never drift.
const HANDOFF_COLS: &str = "id,from_session,from_kind,from_profile_id,to_session,to_kind,to_profile_id,project_key,cwd,packet_dir,reason,created_secs";

/// Map a `handoff` row into a `HandoffRow`. Nullable profiles mean the CLI's
/// default account; a nullable target means its id has not been discovered.
fn row_to_handoff(r: &rusqlite::Row) -> rusqlite::Result<HandoffRow> {
    Ok(HandoffRow {
        id: r.get(0)?,
        from_session: r.get(1)?,
        from_kind: r.get(2)?,
        from_profile_id: r.get(3)?,
        to_session: r.get(4)?,
        to_kind: r.get(5)?,
        to_profile_id: r.get(6)?,
        project_key: r.get(7)?,
        cwd: r.get(8)?,
        packet_dir: r.get(9)?,
        reason: r.get(10)?,
        created_secs: r.get(11)?,
    })
}

impl Store {
    /// Record a handoff; returns its new id. A codex target can start with no
    /// session id — keep the packet + source durable while discovery catches
    /// up. Bumps the write generation like every GUI-visible write.
    #[allow(clippy::too_many_arguments)]
    pub fn record_handoff(
        &self,
        from_session: &str,
        from_kind: &str,
        from_profile_id: Option<i64>,
        to_session: Option<&str>,
        to_kind: &str,
        to_profile_id: Option<i64>,
        project_key: &str,
        cwd: &str,
        packet_dir: &str,
        reason: &str,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO handoff(from_session,from_kind,from_profile_id,to_session,to_kind,to_profile_id,project_key,cwd,packet_dir,reason,created_secs)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                from_session,
                from_kind,
                from_profile_id,
                to_session,
                to_kind,
                to_profile_id,
                project_key,
                cwd,
                packet_dir,
                reason,
                now() as i64
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        self.bump_gen();
        Ok(id)
    }

    /// Fill the target once codex's minted id is known. The original row keeps
    /// its id + timestamp so discovery does not reorder the source's history.
    pub fn set_handoff_target(&self, id: i64, to_session: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE handoff SET to_session=?2 WHERE id=?1",
            params![id, to_session],
        )?;
        self.bump_gen();
        Ok(())
    }

    /// A single handoff by id (None if the row is gone). `.ok()` folds the
    /// missing-row error to None, matching the store's other by-id readers.
    pub fn handoff(&self, id: i64) -> Option<HandoffRow> {
        self.conn
            .query_row(
                &format!("SELECT {HANDOFF_COLS} FROM handoff WHERE id=?1"),
                params![id],
                row_to_handoff,
            )
            .ok()
    }

    /// The handoff a session began from. Newest id wins if several rows point
    /// here — timestamps have second precision, so they cannot order a tie.
    pub fn handoff_into(&self, to_session: &str) -> Option<HandoffRow> {
        self.conn
            .query_row(
                &format!("SELECT {HANDOFF_COLS} FROM handoff WHERE to_session=?1 ORDER BY id DESC LIMIT 1"),
                params![to_session],
                row_to_handoff,
            )
            .ok()
    }

    /// Where each handed-off session came from, for its subhead ("↩ from codex ·
    /// codex2"): (target session, source CLI, source profile label — None for the
    /// CLI's own login, packet dir). One join for the GUI tick rather than a query
    /// per session per repaint. Oldest first, so a map built from it keeps the
    /// newest handoff into a session.
    pub fn handoff_sources(&self) -> Vec<(String, String, Option<String>, String)> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT h.to_session, h.from_kind, p.label, h.packet_dir FROM handoff h
             LEFT JOIN profile p ON p.id = h.from_profile_id
             WHERE h.to_session IS NOT NULL ORDER BY h.id",
        ) else {
            return Vec::new();
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    /// Every handoff from a session, newest first. Order by id so two handoffs
    /// recorded in the same second still have a stable lineage order.
    pub fn handoffs_from(&self, from_session: &str) -> Vec<HandoffRow> {
        let Ok(mut stmt) = self
            .conn
            .prepare(&format!("SELECT {HANDOFF_COLS} FROM handoff WHERE from_session=?1 ORDER BY id DESC"))
        else {
            return Vec::new();
        };
        stmt.query_map(params![from_session], row_to_handoff)
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_codex_handoff_target_starts_unknown_and_is_found_after_discovery() {
        // store test convention: a tempdir, never a real path. now() has second
        // precision, so each test gets a child directory to isolate parallel runs.
        let dir = std::env::temp_dir().join(format!(
            "orch-handoffs-test-{}-{}",
            std::process::id(),
            now()
        )).join("codex-target");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("store.sqlite");
        let s = Store::open(&db).unwrap();

        assert!(s.handoff(1).is_none());
        assert!(s.handoff_into("codex-1").is_none());
        let generation = s.write_gen();
        let before = now() as i64;
        let id = s
            .record_handoff(
                "claude-1", "claude", Some(7), None, "codex", Some(9),
                "proj-a", "/tmp/a", "/tmp/packet-a", "usage_limit",
            )
            .unwrap();
        assert_eq!(s.write_gen(), generation + 1);

        // Every field round-trips, including the pending target; creation time
        // belongs to the handoff, not the later target-id discovery.
        let mut expected = s.handoff(id).expect("handoff exists after record");
        assert!(expected.created_secs >= before && expected.created_secs <= now() as i64);
        assert_eq!(expected, HandoffRow {
            id,
            from_session: "claude-1".to_string(),
            from_kind: "claude".to_string(),
            from_profile_id: Some(7),
            to_session: None,
            to_kind: "codex".to_string(),
            to_profile_id: Some(9),
            project_key: "proj-a".to_string(),
            cwd: "/tmp/a".to_string(),
            packet_dir: "/tmp/packet-a".to_string(),
            reason: "usage_limit".to_string(),
            created_secs: expected.created_secs,
        });
        assert!(s.handoff_into("codex-1").is_none());

        s.set_handoff_target(id, "codex-1").unwrap();
        assert_eq!(s.write_gen(), generation + 2);
        expected.to_session = Some("codex-1".to_string());
        assert_eq!(s.handoff(id), Some(expected.clone()));
        assert_eq!(s.handoff_into("codex-1"), Some(expected));

        drop(s);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn handoffs_from_lists_newest_first() {
        let dir = std::env::temp_dir().join(format!(
            "orch-handoffs-test-{}-{}",
            std::process::id(),
            now()
        )).join("newest-first");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("store.sqlite");
        let s = Store::open(&db).unwrap();

        let first = s
            .record_handoff(
                "source", "codex", None, Some("target"), "claude", None,
                "proj-a", "/tmp/a", "/tmp/packet-1", "manual",
            )
            .unwrap();
        let second = s
            .record_handoff(
                "source", "codex", None, Some("target"), "claude", None,
                "proj-a", "/tmp/a", "/tmp/packet-2", "usage_limit",
            )
            .unwrap();
        s.record_handoff(
            "unrelated", "claude", None, Some("elsewhere"), "codex", None,
            "proj-b", "/tmp/b", "/tmp/packet-3", "manual",
        ).unwrap();

        // No sleeps: id orders even same-second writes. An unrelated newer
        // row must not leak into either direction of this session's lineage.
        let rows = s.handoffs_from("source");
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![second, first]);
        assert_eq!(rows[0].from_profile_id, None);
        assert_eq!(rows[0].to_profile_id, None);
        assert_eq!(s.handoff_into("target"), Some(rows[0].clone()));
        assert!(s.handoffs_from("missing").is_empty());

        drop(s);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opening_the_same_handoff_database_twice_preserves_the_row() {
        let dir = std::env::temp_dir().join(format!(
            "orch-handoffs-test-{}-{}",
            std::process::id(),
            now()
        )).join("reopen");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("store.sqlite");
        let s = Store::open(&db).unwrap();

        let id = s
            .record_handoff(
                "codex-1", "codex", None, Some("claude-1"), "claude", Some(4),
                "proj-a", "/tmp/a", "/tmp/packet-a", "manual",
            )
            .unwrap();
        let expected = s.handoff(id).expect("handoff exists before reopen");
        drop(s);

        // Store::open runs migrate again: both table + indexes must be
        // idempotent, and the durable packet/lineage row must survive intact.
        let s = Store::open(&db).unwrap();
        assert_eq!(s.handoff(id), Some(expected.clone()));
        assert_eq!(s.handoff_into("claude-1"), Some(expected.clone()));
        assert_eq!(s.handoffs_from("codex-1"), vec![expected]);
        // The subhead's join: no source profile reads as the CLI's own login.
        assert_eq!(
            s.handoff_sources(),
            vec![("claude-1".to_string(), "codex".to_string(), None, "/tmp/packet-a".to_string())]
        );

        drop(s);
        std::fs::remove_dir_all(&dir).ok();
    }
}
