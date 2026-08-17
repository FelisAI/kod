//! Pending diffs (proposals awaiting accept), changesets, and the summary job
//! queue. Extracted verbatim from `store.rs` (decomposition; behavior unchanged).

use rusqlite::params;

use super::{now, PendingDiff, Store};
use crate::tree::DiffOp;

impl Store {
    // --- pending diffs (proposals awaiting accept) ---

    pub fn add_pending_diff(&self, key: &str, kind: &str, ops: &[DiffOp]) -> rusqlite::Result<i64> {
        self.add_pending_diff_with_evidence(key, kind, ops, &vec![None; ops.len()])
    }

    /// Add a proposal with per-op EVIDENCE (#10 slice 2): `evidence[i]` is the
    /// verbatim summary quote justifying `ops[i]` (None where a source has no
    /// quote, e.g. seed/drift). Stored index-aligned with ops_json. No op is
    /// flagged (the canned / summary / drift lanes carry deterministic ops).
    pub fn add_pending_diff_with_evidence(
        &self,
        key: &str,
        kind: &str,
        ops: &[DiffOp],
        evidence: &[Option<String>],
    ) -> rusqlite::Result<i64> {
        self.add_pending_diff_full(key, kind, ops, evidence, &vec![false; ops.len()])
    }

    /// Add a cartographer proposal with per-op EVIDENCE and per-op FLAGS
    /// (docs/019 slice 2): `flagged[i]` = the verbatim quote for `ops[i]` did
    /// NOT verify against the real repo file, so the review surface marks it
    /// and excludes it from accept-all. All three arrays are stored
    /// index-aligned with ops_json.
    pub fn add_pending_diff_full(
        &self,
        key: &str,
        kind: &str,
        ops: &[DiffOp],
        evidence: &[Option<String>],
        flagged: &[bool],
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO pending_diff(project_key,kind,ops_json,evidence_json,flagged_json,created_secs) VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                key,
                kind,
                serde_json::to_string(ops).unwrap_or_default(),
                serde_json::to_string(evidence).unwrap_or_default(),
                serde_json::to_string(flagged).unwrap_or_default(),
                now() as i64
            ],
        )?;
        self.bump_gen();
        Ok(self.conn.last_insert_rowid())
    }

    pub fn pending_diffs(&self, key: &str) -> rusqlite::Result<Vec<PendingDiff>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id,kind,ops_json,evidence_json,changeset_id,flagged_json FROM pending_diff WHERE project_key=?1 ORDER BY id")?;
        let rows = stmt.query_map(params![key], Self::map_pending_row)?;
        rows.collect()
    }

    /// Row shape shared by pending_diffs / changeset_pending: id,kind,ops_json,
    /// evidence_json,changeset_id,flagged_json in that select order.
    fn map_pending_row(r: &rusqlite::Row) -> rusqlite::Result<PendingDiff> {
        let ops_json: String = r.get(2)?;
        let evidence_json: Option<String> = r.get(3)?;
        let flagged_json: Option<String> = r.get(5)?;
        let ops: Vec<DiffOp> = serde_json::from_str(&ops_json).unwrap_or_default();
        // old rows (pre-column) are NULL; a length mismatch can only come
        // from corruption — normalize to ops.len() so zips never misalign.
        let mut evidence: Vec<Option<String>> = evidence_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        evidence.resize(ops.len(), None);
        let mut flagged: Vec<bool> = flagged_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        flagged.resize(ops.len(), false);
        Ok(PendingDiff {
            id: r.get(0)?,
            kind: r.get(1)?,
            ops,
            evidence,
            changeset_id: r.get(4)?,
            flagged,
        })
    }

    /// The pending_diff rows LINKED to one changeset, oldest first (docs/019
    /// slice 1c). The review surface flattens their ops+evidence into one
    /// diff-of-the-document (`flatten_changeset_ops`); accept re-reads them to
    /// rebuild the kept ops and to drop the rows in the same sweep.
    pub fn changeset_pending(&self, changeset_id: i64) -> rusqlite::Result<Vec<PendingDiff>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id,kind,ops_json,evidence_json,changeset_id,flagged_json FROM pending_diff WHERE changeset_id=?1 ORDER BY id")?;
        let rows = stmt.query_map(params![changeset_id], Self::map_pending_row)?;
        rows.collect()
    }

    pub fn drop_pending_diff(&self, id: i64) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM pending_diff WHERE id=?1", params![id])?;
        self.bump_gen();
        Ok(())
    }

    // --- changesets (docs/019: named machine proposals, reviewed as one unit) ---

    /// Create a changeset shell; its ops ride pending_diff rows linked by
    /// changeset_id. scope_part_id = the user-pointed fence (None = whole
    /// map, explicitly granted).
    pub fn create_changeset(
        &self,
        key: &str,
        title: &str,
        instruction: &str,
        scope_part_id: Option<i64>,
        origin_run: &str,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO changeset(project_key,title,instruction,scope_part_id,origin_run,created_secs) VALUES(?1,?2,?3,?4,?5,?6)",
            params![key, title, instruction, scope_part_id, origin_run, now() as i64],
        )?;
        self.bump_gen();
        Ok(self.conn.last_insert_rowid())
    }

    /// Attach a pending diff to a changeset (grouped review).
    pub fn link_pending_to_changeset(
        &self,
        pending_id: i64,
        changeset_id: i64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE pending_diff SET changeset_id=?2 WHERE id=?1",
            params![pending_id, changeset_id],
        )?;
        self.bump_gen();
        Ok(())
    }

    /// Resolve a changeset: open|accepted|rejected|partial.
    pub fn set_changeset_status(&self, id: i64, status: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE changeset SET status=?2 WHERE id=?1",
            params![id, status],
        )?;
        self.bump_gen();
        Ok(())
    }

    /// Open changesets for a project, oldest first —
    /// (id, title, instruction, scope_part_id, origin_run, created_secs).
    pub fn open_changesets(
        &self,
        key: &str,
    ) -> Vec<(i64, String, String, Option<i64>, String, u64)> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT id,title,instruction,scope_part_id,origin_run,created_secs FROM changeset
             WHERE project_key=?1 AND status='open' ORDER BY id",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![key], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get::<_, i64>(5)? as u64,
            ))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    // --- summary jobs (docs/019: the durable return-channel queue) ---

    /// Enqueue a summarize job, deduped against this session's IN-FLIGHT work.
    ///
    /// A QUEUED job absorbs the trigger (it re-reads the transcript at run
    /// time) and UPGRADES to the stronger one (end > delta > idle), so a queued
    /// delta still carries the session-ended chapter when the session exits.
    ///
    /// A RUNNING job absorbs an EQUAL-OR-WEAKER trigger too. It used not to,
    /// and the cost was measured in the user's live store: `claim` flips the
    /// row to 'running', so on the very next ~500ms tick the same still-firing
    /// idle trigger saw no queued row and inserted a TWIN — 11 twin pairs, every
    /// summary generated (and paid for) twice, halving the shared 20/hr budget.
    /// Nothing is lost by absorbing: the worker anchors `thru`/`src_bytes`
    /// BEFORE it reads the transcript, so work that lands mid-run is not covered
    /// by the stored summary and the trigger simply re-fires on the next tick.
    /// A STRONGER trigger (the session ended while an idle job was running)
    /// still inserts a fresh job — a running worker can no longer see an upgrade,
    /// and the final chapter must never be dropped.
    pub fn enqueue_summary_job(
        &self,
        cli_session_id: &str,
        project_key: &str,
        trigger: &str,
    ) -> rusqlite::Result<i64> {
        if let Ok((id, existing)) = self.conn.query_row(
            "SELECT id, trigger FROM summary_job WHERE cli_session_id=?1 AND state='queued' LIMIT 1",
            params![cli_session_id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        ) {
            if trigger_rank(trigger) > trigger_rank(&existing) {
                self.conn.execute("UPDATE summary_job SET trigger=?2 WHERE id=?1", params![id, trigger])?;
            }
            return Ok(id);
        }
        if let Ok((id, running)) = self.conn.query_row(
            "SELECT id, trigger FROM summary_job WHERE cli_session_id=?1 AND state='running' ORDER BY id DESC LIMIT 1",
            params![cli_session_id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        ) {
            if trigger_rank(trigger) <= trigger_rank(&running) {
                return Ok(id);
            }
        }
        self.conn.execute(
            "INSERT INTO summary_job(cli_session_id,project_key,trigger,enqueued_ms,updated_ms) VALUES(?1,?2,?3,?4,?4)",
            params![cli_session_id, project_key, trigger, (now() as i64) * 1000],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// How many times a job may be DEFERRED before the defer is converted into
    /// an ordinary FAILURE (which marches it to 'dead' and cools the session
    /// off). This is what makes an IMMORTAL JOB IMPOSSIBLE BY CONSTRUCTION: a
    /// defer refunds the attempt, so without a ceiling a job that always defers
    /// (a rate limit that never lifts, or a misclassified permanent failure)
    /// could never die. The bound does not trust the classifier — it holds even
    /// if every classification is wrong.
    ///
    /// 10, with the escalation below, buys ~8.7h of riding out a rate limit
    /// (900s base: 900+1800+3600×8) — comfortably past a provider's 5h usage
    /// window — and ~5h for a not-ready transcript (60s base). Long enough that
    /// a real outage is survived without a death; short enough that a job that
    /// will NEVER succeed is dead within the day.
    pub const MAX_SUMMARY_DEFERS: i64 = 10;
    /// Ceiling on the doubling, so the last defers don't stretch to days.
    pub const DEFER_BACKOFF_CAP_SECS: u64 = 3600;

    /// The n-th defer waits `base << n`, capped. Escalating, because the thing
    /// we are waiting out (a quota window) does not clear on a fixed schedule —
    /// a constant retry-after would either spin through it or over-wait a blip.
    pub fn defer_backoff_secs(defers: i64, base_secs: u64) -> u64 {
        let shift = defers.clamp(0, 16) as u32;
        base_secs
            .saturating_mul(1u64 << shift)
            .min(Self::DEFER_BACKOFF_CAP_SECS)
    }

    /// DEFER a claimed job the WORLD wasn't ready for (transcript not written
    /// yet, provider rate-limited): requeue, give back the speculative attempt
    /// the claim spent, and — this is the load-bearing part — send it to the
    /// BACK OF THE LINE via `next_attempt_ms`.
    ///
    /// The refund alone was a starvation bug. `claim` is strictly oldest-first
    /// and takes ONE job at a time, so a requeued job with an untouched
    /// `enqueued_ms` went straight back to the HEAD of the queue and was
    /// re-claimed on the very next tick, forever: a single session that can
    /// never be summarized blocked EVERY other session's standup indefinitely —
    /// the exact "one bad session blinds everyone" outcome the (deleted)
    /// permanent blacklist was reaching for.
    ///
    /// Returns Ok(true) if the job was deferred, Ok(false) if its defers were
    /// EXHAUSTED and it was failed instead (see `MAX_SUMMARY_DEFERS`).
    pub fn defer_summary_job(
        &self,
        id: i64,
        base_retry_secs: u64,
        reason: &str,
    ) -> rusqlite::Result<bool> {
        let defers: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(defers,0) FROM summary_job WHERE id=?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if defers >= Self::MAX_SUMMARY_DEFERS {
            // out of patience: this is now an ordinary failure, and it marches
            // to 'dead' → the session cools off (escalating, self-expiring).
            self.finish_summary_job(id, Some(reason))?;
            return Ok(false);
        }
        let now_ms = (now() as i64) * 1000;
        let wait_ms = Self::defer_backoff_secs(defers, base_retry_secs) as i64 * 1000;
        self.conn.execute(
            "UPDATE summary_job
             SET state='queued', attempts=MAX(attempts-1,0), defers=?2,
                 next_attempt_ms=?3, updated_ms=?4, last_error=?5
             WHERE id=?1",
            params![id, defers + 1, now_ms + wait_ms, now_ms, reason],
        )?;
        Ok(true)
    }

    /// When this session's summary jobs DIED (ms, ascending). Feeds the pure
    /// cool-off decision (`returnchannel::cooling_off`): recent deaths back the
    /// session off, and the backoff escalates with how many there were.
    ///
    /// This replaced a boolean `session_has_dead_job` that skipped the session
    /// FOREVER on the first death. It cost the user his standup for two days:
    /// one transient codex failure window on Jul 10-11 burned 3 attempts in ~3
    /// minutes, and the resulting dead row blacklisted 10 sessions permanently —
    /// `claude --resume` keeps the cli_session_id, so the blacklist even survived
    /// restarts. A retryable operation must never be given a permanent death.
    ///
    /// COALESCE: rows written before `updated_ms` shipped have no death stamp;
    /// enqueued_ms is within ~3 minutes of their death (3 attempts, 60s apart),
    /// which is far inside the smallest cool-off window, so legacy rows expire
    /// correctly with no backfill.
    pub fn session_death_times(&self, cli_session_id: &str) -> Vec<u64> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT COALESCE(updated_ms, enqueued_ms) FROM summary_job
             WHERE cli_session_id=?1 AND state='dead' ORDER BY 1",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![cli_session_id], |r| r.get::<_, i64>(0))
            .map(|rows| rows.filter_map(|r| r.ok()).map(|v| v.max(0) as u64).collect())
            .unwrap_or_default()
    }

    /// Per-session freshness for a project's truth meter (review finding 5: a
    /// project-level MAX(summary) vs MAX(event) let one session's fresh summary
    /// mask another session that is behind). Returns (latest_event_ms,
    /// latest_summary_ms) per session that has ANY event — the GUI folds it so
    /// the project reads 'blind' if ANY session is behind.
    pub fn project_session_freshness(&self, project_key: &str) -> Vec<(u64, Option<u64>)> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT e.sess, MAX(e.at_ms), (SELECT MAX(s.at_ms) FROM session_summary s WHERE s.sess=e.sess)
             FROM session_event e WHERE e.project_key=?1 GROUP BY e.sess",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![project_key], |r| {
            Ok((
                r.get::<_, i64>(1)? as u64,
                r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
            ))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// How long a claimed job may sit in 'running' before it is presumed
    /// crashed. The summarize call itself is capped at 90s, so 10 minutes only
    /// ever fires on a process that died mid-run.
    const SUMMARY_LEASE_SECS: u64 = 600;

    /// Claim the oldest READY queued job (state → running, attempts+1). Returns
    /// (id, cli_session_id, project_key, trigger).
    ///
    /// READY = `COALESCE(next_attempt_ms, enqueued_ms) <= now`, and that same
    /// expression is the sort key. A DEFERRED job therefore (a) is invisible
    /// until its retry-after elapses and (b) sorts BEHIND every job enqueued
    /// before it becomes due — so it yields to younger, ready work instead of
    /// re-taking the head of the queue every tick and starving the whole queue.
    ///
    /// First RECLAIMS any 'running' row whose lease expired. 'running' had no
    /// writer other than the claim, so killing the app mid-summary stranded the
    /// row there forever — harmless while nothing keyed on it, but now that an
    /// in-flight job absorbs its session's triggers (see `enqueue_summary_job`)
    /// a stranded row would wedge that session's summaries permanently. Self-
    /// healing on the claim path beats a startup hook: no launch to wait for.
    /// The reclaim REFUNDS the attempt the claim charged: a crash is not the
    /// job's fault, and without the refund two crashes mid-summary silently ate
    /// 2 of the job's 3 attempts, so its first genuine failure died on try one
    /// and bought the session an unearned cool-off. Same rule as `defer` — a
    /// requeue may never march a healthy job toward death.
    pub fn claim_summary_job(&self) -> Option<(i64, String, String, String)> {
        let now_ms = (now() as i64) * 1000;
        let lease_ms = now_ms - (Self::SUMMARY_LEASE_SECS as i64) * 1000;
        let _ = self.conn.execute(
            "UPDATE summary_job SET state='queued', attempts=MAX(attempts-1,0), updated_ms=?1
             WHERE state='running' AND COALESCE(updated_ms, enqueued_ms) < ?2",
            params![now_ms, lease_ms],
        );
        let row = self
            .conn
            .query_row(
                "SELECT id,cli_session_id,project_key,trigger FROM summary_job
                 WHERE state='queued' AND COALESCE(next_attempt_ms, enqueued_ms) <= ?1
                 ORDER BY COALESCE(next_attempt_ms, enqueued_ms), id LIMIT 1",
                params![now_ms],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)),
            )
            .ok()?;
        self.conn
            .execute(
                "UPDATE summary_job SET state='running', attempts=attempts+1, updated_ms=?2 WHERE id=?1",
                params![row.0, now_ms],
            )
            .ok()?;
        Some(row)
    }

    /// Finish a claimed job: success → done; failure retries up to 3 attempts,
    /// then → dead. `updated_ms` stamps the transition, so a dead row carries
    /// its DEATH time — the anchor the escalating cool-off expires from.
    ///
    /// Death is no longer terminal for the SESSION (see `session_death_times`);
    /// it is terminal only for this job, and it makes the session back off.
    pub fn finish_summary_job(&self, id: i64, err: Option<&str>) -> rusqlite::Result<()> {
        let now_ms = (now() as i64) * 1000;
        match err {
            None => {
                self.conn.execute(
                    "UPDATE summary_job SET state='done', last_error=NULL, updated_ms=?2 WHERE id=?1",
                    params![id, now_ms],
                )?;
            }
            Some(e) => {
                self.conn.execute(
                    "UPDATE summary_job SET state = CASE WHEN attempts >= 3 THEN 'dead' ELSE 'queued' END, last_error=?2, updated_ms=?3 WHERE id=?1",
                    params![id, e, now_ms],
                )?;
            }
        }
        Ok(())
    }

    /// Dead jobs (attempts exhausted) — (id, cli_session_id, project_key,
    /// last_error, died_ms). These surface; the map may be behind, never
    /// silently stale. `died_ms` lets a reader show only RECENT failures — a
    /// week-old death is history, not news.
    pub fn dead_summary_jobs(&self) -> Vec<(i64, String, String, String, u64)> {
        let Ok(mut stmt) = self
            .conn
            .prepare("SELECT id,cli_session_id,project_key,COALESCE(last_error,''),COALESCE(updated_ms,enqueued_ms) FROM summary_job WHERE state='dead' ORDER BY id")
        else {
            return Vec::new();
        };
        stmt.query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get::<_, i64>(4)?.max(0) as u64,
            ))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }
}

/// Trigger strength for the summary queue: end (session over) beats delta
/// (mid-flight) beats idle. A queued job's trigger is upgraded, never lost.
fn trigger_rank(t: &str) -> u8 {
    match t {
        "end" => 3,
        "delta" => 2,
        "idle" => 1,
        _ => 0,
    }
}
