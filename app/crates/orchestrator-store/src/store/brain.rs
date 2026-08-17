//! The "brain" memory layer: node notes, node positions, provenance, full-text
//! recall (search_all / FTS), idea projects, and the needs-you user flags.
//! Extracted verbatim from `store.rs` (decomposition; behavior unchanged).

use rusqlite::params;

use super::{now, NoteRow, Provenance, SearchHit, Store};
use crate::tree::PartId;

impl Store {
    /// Append a decision/note to a node's log (#10). LLM callers must pass
    /// source="sess-<cli id>"; user edits pass "user". Entries are never
    /// updated or deleted — the log IS the memory.
    pub fn add_note(
        &self,
        project_key: &str,
        part_id: i64,
        kind: &str,
        text: &str,
        source: &str,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO part_note(part_id,project_key,ts_secs,kind,text,source) VALUES(?1,?2,?3,?4,?5,?6)",
            params![part_id, project_key, now() as i64, kind, text, source],
        )?;
        self.mark_dirty();
        Ok(self.conn.last_insert_rowid())
    }

    /// A node's log, newest first (also entries LINKED via note_part).
    pub fn notes_for_part(&self, part_id: i64) -> rusqlite::Result<Vec<NoteRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT n.id, n.part_id, n.ts_secs, n.kind, n.text, n.source
             FROM part_note n LEFT JOIN note_part l ON l.note_id = n.id
             WHERE n.part_id = ?1 OR l.part_id = ?1
             ORDER BY n.ts_secs DESC, n.id DESC",
        )?;
        let rows = stmt
            .query_map(params![part_id], |r| {
                Ok(NoteRow {
                    id: r.get(0)?,
                    part_id: r.get(1)?,
                    ts_secs: r.get::<_, i64>(2)? as u64,
                    kind: r.get(3)?,
                    text: r.get(4)?,
                    source: r.get(5)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Link a note to an additional node (cross-cutting decisions, #10).
    pub fn link_note(&self, note_id: i64, part_id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO note_part(note_id,part_id) VALUES(?1,?2)",
            params![note_id, part_id],
        )?;
        Ok(())
    }

    /// Persist a node's spatial position (the brain-map canvas; spatial memory
    /// — positions never auto-move once set, docs/008).
    pub fn set_part_pos(&self, id: i64, x: f64, y: f64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE part SET map_x=?2, map_y=?3 WHERE id=?1",
            params![id, x, y],
        )?;
        Ok(())
    }

    /// Un-pin a node (docs/019 CANVAS context menu): NULL map_x/map_y hands it
    /// back to auto-layout. Spatial memory, not structure — direct and
    /// unjournaled, exactly like `set_part_pos` (⌘Z never replays a pin).
    pub fn clear_part_pos(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE part SET map_x=NULL, map_y=NULL WHERE id=?1",
            params![id],
        )?;
        Ok(())
    }

    /// The provenance quad (docs/019 commitment 3) for the "Why is this here?"
    /// popover: (created_by, source_file, source_quote, rationale). created_by
    /// defaults 'legacy' (pre-provenance rows) — the popover renders the
    /// re-ground CTA for those instead of pretending the row can explain
    /// itself. None = the part doesn't exist.
    pub fn part_provenance(&self, id: PartId) -> Option<Provenance> {
        self.conn
            .query_row(
                "SELECT COALESCE(created_by,'legacy'), source_file, source_quote, rationale FROM part WHERE id=?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok()
    }

    /// ⌘K recall across nodes + notes + session summaries. The FTS index is
    /// rebuilt per call — hundreds of rows at this scale, sub-ms; measure
    /// before optimizing (his rule) if it ever grows teeth.
    pub fn search_all(&self, query: &str, limit: usize) -> rusqlite::Result<Vec<SearchHit>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        if self.fts_dirty.replace(false) {
            self.rebuild_fts()?;
        }
        Ok(self.query_fts(q, limit))
    }

    fn rebuild_fts(&self) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM brain_fts", [])?;
        self.conn.execute_batch(
            "INSERT INTO brain_fts(kind, project_key, ref_id, title, body)
               SELECT 'node', project_key, CAST(id AS TEXT), name, detail FROM part;
             INSERT INTO brain_fts(kind, project_key, ref_id, title, body)
               SELECT 'note', project_key, CAST(part_id AS TEXT), kind, text FROM part_note;
             INSERT INTO brain_fts(kind, project_key, ref_id, title, body)
               SELECT 'summary', project_key, sess, goal, headline || ' ' || next_action FROM session_summary;",
        )
    }

    fn query_fts(&self, q: &str, limit: usize) -> Vec<SearchHit> {
        // quote each term to keep FTS syntax chars (-, ") from erroring.
        let fts_q: String = q
            .split_whitespace()
            .map(|t| format!("\"{}\"", t.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" ");
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT kind, project_key, ref_id, title, body FROM brain_fts WHERE brain_fts MATCH ?1 ORDER BY rank LIMIT ?2",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![fts_q, limit as i64], |r| {
            Ok(SearchHit {
                kind: r.get(0)?,
                project_key: r.get(1)?,
                ref_id: r.get(2)?,
                title: r.get(3)?,
                body: r.get(4)?,
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// The store-backed projects the scan cannot find on its own, for registry
    /// injection on every scan (#10, #29): path-less IDEAS (`idea:*`) and
    /// projects that OWN A DIRECTORY (`path` set — created by "＋ new project",
    /// or an idea promoted on its first spawn). A project with neither is
    /// discovered from its sessions/git like any other, so injecting it
    /// would be redundant. Returns `(key, name, path)`.
    #[allow(clippy::type_complexity)]
    pub fn store_projects(&self) -> rusqlite::Result<Vec<(String, String, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT key,name,path FROM project
             WHERE key LIKE 'idea:%' OR (path IS NOT NULL AND path <> '')
             ORDER BY name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        rows.collect()
    }

    /// Set (or replace) the user's needs-you flag on a node (docs/019 C5):
    /// the one-line blocking `question` IS the payload; `set_secs` stamps when
    /// it was set so an unanswered flag can ANTI-decay (escalate at 7d). Bumping
    /// the gen keeps the map's per-frame memoization honest.
    pub fn set_needs_you(
        &self,
        project_key: &str,
        part_id: PartId,
        question: &str,
        set_secs: u64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO needs_you(part_id,project_key,question,set_secs) VALUES(?1,?2,?3,?4)",
            params![part_id, project_key, question, set_secs as i64],
        )?;
        self.bump_gen();
        Ok(())
    }

    /// Clear a user-set needs-you flag (the user answered / cancelled).
    pub fn clear_needs_you(&self, part_id: PartId) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM needs_you WHERE part_id=?1", params![part_id])?;
        self.bump_gen();
        Ok(())
    }

    /// Every user-set needs-you flag for a project: (part_id, question,
    /// set_secs). The summons singleton and the header rollup fold these in.
    pub fn needs_you_flags(&self, project_key: &str) -> Vec<(PartId, String, u64)> {
        let Ok(mut stmt) = self
            .conn
            .prepare("SELECT part_id,question,set_secs FROM needs_you WHERE project_key=?1 ORDER BY set_secs, part_id")
        else {
            return Vec::new();
        };
        stmt.query_map(params![project_key], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? as u64,
            ))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// The user-set needs-you flag on ONE node, if any — for prefilling the
    /// re-flag editor and rendering the question verbatim on the node.
    pub fn needs_you_for(&self, part_id: PartId) -> Option<(String, u64)> {
        self.conn
            .query_row(
                "SELECT question,set_secs FROM needs_you WHERE part_id=?1",
                params![part_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)),
            )
            .ok()
    }
}
