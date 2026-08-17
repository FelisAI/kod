//! Session summaries, the append-only Standup event journal, proposal-window
//! bookkeeping timestamps, and the summary->memory document projection.
//! Extracted verbatim from `store.rs` (decomposition; behavior unchanged).

use rusqlite::params;

use super::{Store, SummaryRow};
use crate::memory::MemorySourceKind;
use crate::memory_extract::MemoryDocument;

/// Map a `session_summary` row into a SummaryRow. Both latest_summaries and
/// summaries_since select these 10 columns in this exact order.
fn map_summary_row(r: &rusqlite::Row) -> rusqlite::Result<SummaryRow> {
    Ok(SummaryRow {
        sess: r.get(0)?,
        project_key: r.get(1)?,
        at_ms: r.get::<_, i64>(2)? as u64,
        thru_at_ms: r.get::<_, i64>(3)? as u64,
        src_bytes: r.get::<_, i64>(4)? as u64,
        src_path: r.get(5)?,
        goal: r.get(6)?,
        headline: r.get(7)?,
        next_action: r.get(8)?,
        detail_json: r.get(9)?,
    })
}

impl Store {
    /// Append a session summary (#16). INSERT OR IGNORE: two workers racing the
    /// same (sess, ms) — cross-instance — keep the first.
    #[allow(clippy::too_many_arguments)]
    pub fn record_summary(
        &self,
        sess: &str,
        project_key: &str,
        at_ms: u64,
        thru_at_ms: u64,
        src_bytes: u64,
        src_path: &str,
        goal: &str,
        headline: &str,
        next_action: &str,
        detail_json: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO session_summary(sess,project_key,at_ms,thru_at_ms,src_bytes,src_path,goal,headline,next_action,detail_json)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![sess, project_key, at_ms as i64, thru_at_ms as i64, src_bytes as i64, src_path, goal, headline, next_action, detail_json],
        )?;
        self.mark_dirty();
        Ok(())
    }

    /// The latest summary per session (the UI view over the append history).
    pub fn latest_summaries(&self) -> rusqlite::Result<Vec<SummaryRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.sess, s.project_key, s.at_ms, s.thru_at_ms, s.src_bytes, s.src_path, s.goal, s.headline, s.next_action, s.detail_json
             FROM session_summary s
             JOIN (SELECT sess, MAX(at_ms) m FROM session_summary GROUP BY sess) x
               ON s.sess = x.sess AND s.at_ms = x.m",
        )?;
        let rows = stmt
            .query_map([], map_summary_row)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Newest recorded event per session — the durable freshness anchor for
    /// summaries (in-memory event seqs reset across restarts; wall-clock doesn't).
    pub fn latest_event_by_sess(&self) -> rusqlite::Result<Vec<(String, u64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT sess, MAX(at_ms) FROM session_event GROUP BY sess")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Record a TurnEnd activity event for the Today digest (idempotent on
    /// (sess, at_ms) so a resume/backfill re-observing a turn doesn't dup).
    pub fn record_event(
        &self,
        sess: &str,
        project_key: &str,
        at_ms: u64,
        summary: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO session_event(sess,project_key,at_ms,summary) VALUES(?1,?2,?3,?4)",
            params![sess, project_key, at_ms as i64, summary],
        )?;
        Ok(())
    }

    /// Activity at/after `since_ms`, newest first — (project_key, at_ms, summary).
    /// Drives the durable Standup "Today" digest.
    pub fn events_since(&self, since_ms: u64) -> rusqlite::Result<Vec<(String, u64, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT project_key,at_ms,summary FROM session_event WHERE at_ms>=?1 ORDER BY at_ms DESC")?;
        let rows = stmt.query_map(params![since_ms as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, String>(2)?,
            ))
        })?;
        rows.collect()
    }

    /// Count durable session_event rows for ONE session strictly after
    /// `since_ms` — the Delta trigger's turn count (docs/019 slice 3: enqueue a
    /// summary every ~12 turns). Durable (survives restarts), unlike the RAM
    /// event ring. Errors fold to 0 (a summarize that under-triggers, never a
    /// panic on the tick path).
    pub fn count_events_since_sess(&self, sess: &str, since_ms: u64) -> u64 {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM session_event WHERE sess=?1 AND at_ms>?2",
                params![sess, since_ms as i64],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n as u64)
            .unwrap_or(0)
    }

    /// Count durable session_event rows for a PROJECT strictly after `since_ms`
    /// — the truth meter's `events_behind` (docs/019 commitment 4: how far
    /// upstream has outrun the last summary). Errors fold to 0.
    pub fn count_events_since(&self, project_key: &str, since_ms: u64) -> u64 {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM session_event WHERE project_key=?1 AND at_ms>?2",
                params![project_key, since_ms as i64],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n as u64)
            .unwrap_or(0)
    }

    /// The newest session_event ts for a PROJECT (0 = none) — the truth meter's
    /// upstream edge (docs/019 commitment 4). Errors fold to 0.
    pub fn latest_event_ms(&self, project_key: &str) -> u64 {
        self.conn
            .query_row(
                "SELECT COALESCE(MAX(at_ms),0) FROM session_event WHERE project_key=?1",
                params![project_key],
                |r| r.get::<_, i64>(0),
            )
            .map(|t| t as u64)
            .unwrap_or(0)
    }

    /// The newest summary ts for a PROJECT (0 = none) — the truth meter's
    /// derived (downstream) edge (docs/019 commitment 4). Errors fold to 0.
    pub fn latest_summary_ms(&self, project_key: &str) -> u64 {
        self.conn
            .query_row(
                "SELECT COALESCE(MAX(at_ms),0) FROM session_summary WHERE project_key=?1",
                params![project_key],
                |r| r.get::<_, i64>(0),
            )
            .map(|t| t as u64)
            .unwrap_or(0)
    }

    // --- proposal bookkeeping (#10 slice 2: one proposer call per window) ---

    /// When the map proposer last ran for `key` (unix secs; 0 = never).
    /// Stored in app_settings under "map_prop_at:<key>" — no new table.
    pub fn last_map_proposal_secs(&self, key: &str) -> u64 {
        self.get_setting(&format!("map_prop_at:{key}"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    pub fn set_last_map_proposal_secs(&self, key: &str, secs: u64) -> rusqlite::Result<()> {
        self.set_setting(&format!("map_prop_at:{key}"), &secs.to_string())
    }

    /// Summaries for one project STRICTLY AFTER `since_ms`, oldest first — the
    /// proposer's grounding window (every new summary since the last proposal
    /// folds into one call).
    pub fn summaries_since(
        &self,
        project_key: &str,
        since_ms: u64,
    ) -> rusqlite::Result<Vec<SummaryRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT sess,project_key,at_ms,thru_at_ms,src_bytes,src_path,goal,headline,next_action,detail_json
             FROM session_summary WHERE project_key=?1 AND at_ms>?2 ORDER BY at_ms ASC",
        )?;
        let rows = stmt
            .query_map(params![project_key, since_ms as i64], map_summary_row)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// When the memory extractor last ran for `key` (unix secs; 0 = never).
    /// Stored in app_settings under "memory_prop_at:<key>".
    pub fn last_memory_proposal_secs(&self, key: &str) -> u64 {
        self.get_setting(&format!("memory_prop_at:{key}"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    pub fn set_last_memory_proposal_secs(&self, key: &str, secs: u64) -> rusqlite::Result<()> {
        self.set_setting(&format!("memory_prop_at:{key}"), &secs.to_string())
    }

    /// Convert durable session summaries into memory-source documents. These
    /// are the first real-session sources for the backend memory graph.
    pub fn summary_memory_documents_since(
        &self,
        project_key: &str,
        since_ms: u64,
    ) -> rusqlite::Result<Vec<MemoryDocument>> {
        Ok(self
            .summaries_since(project_key, since_ms)?
            .iter()
            .map(summary_memory_document)
            .collect())
    }

    /// Session-summary memory documents plus the newest covered summary timestamp.
    /// Workers use this to advance a watermark only for the exact source window
    /// they handed to the memory extractor.
    pub fn summary_memory_documents_since_with_watermark(
        &self,
        project_key: &str,
        since_ms: u64,
    ) -> rusqlite::Result<(Vec<MemoryDocument>, u64)> {
        let rows = self.summaries_since(project_key, since_ms)?;
        let watermark = rows.iter().map(|row| row.at_ms).max().unwrap_or(since_ms);
        let documents = rows.iter().map(summary_memory_document).collect();
        Ok((documents, watermark))
    }
}

fn summary_memory_document(row: &SummaryRow) -> MemoryDocument {
    let details = summary_detail_lines(&row.detail_json);
    let mut text = format!(
        "SESSION SUMMARY\nsession: {}\nproject: {}\nat_ms: {}\ngoal: {}\nheadline: {}\nnext_action: {}\n",
        row.sess, row.project_key, row.at_ms, row.goal, row.headline, row.next_action
    );
    if !details.is_empty() {
        text.push_str("details:\n");
        for detail in details {
            text.push_str("- ");
            text.push_str(&detail);
            text.push('\n');
        }
    }
    MemoryDocument {
        id: format!("summary:{}:{}", row.sess, row.at_ms),
        kind: MemorySourceKind::Session,
        uri: row.src_path.clone(),
        title: Some(if row.headline.trim().is_empty() {
            row.goal.clone()
        } else {
            row.headline.clone()
        }),
        text,
    }
}

fn summary_detail_lines(detail_json: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(detail_json).unwrap_or_else(|_| {
        serde_json::from_str::<serde_json::Value>(detail_json)
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect()
    })
}
