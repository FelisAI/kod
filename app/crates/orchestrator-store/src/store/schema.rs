//! Schema migration, kind backfill, and additive-column helpers.
//! Extracted verbatim from `store.rs` (decomposition; behavior unchanged).
//! `migrate` is `pub(crate)` because `Store::open`/`open_in_memory` (the parent
//! module) and the tests (a sibling module) call it across module boundaries.

use std::collections::HashMap;

use rusqlite::params;

use super::{now, Store};
use crate::tree::{DiffOp, Kind};

impl Store {
    pub(crate) fn migrate(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS project (
                key TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                seed_state TEXT NOT NULL DEFAULT 'none',
                last_opened_secs INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS part (
                id INTEGER PRIMARY KEY,
                project_key TEXT NOT NULL,
                parent_id INTEGER,
                name TEXT NOT NULL,
                detail TEXT NOT NULL DEFAULT '',
                lifecycle TEXT NOT NULL DEFAULT 'todo',
                status_source TEXT NOT NULL DEFAULT 'seed',
                status_at_secs INTEGER NOT NULL DEFAULT 0,
                stale INTEGER NOT NULL DEFAULT 0,
                stale_reason TEXT,
                sort_order REAL NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS part_project ON part(project_key);
            CREATE TABLE IF NOT EXISTS part_anchor (
                part_id INTEGER NOT NULL,
                glob TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS anchor_part ON part_anchor(part_id);
            CREATE TABLE IF NOT EXISTS tree_event (
                id INTEGER PRIMARY KEY,
                project_key TEXT NOT NULL,
                accept_id TEXT NOT NULL,
                ts_secs INTEGER NOT NULL,
                ops_json TEXT NOT NULL,
                inverse_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS event_project ON tree_event(project_key);
            CREATE TABLE IF NOT EXISTS pending_diff (
                id INTEGER PRIMARY KEY,
                project_key TEXT NOT NULL,
                kind TEXT NOT NULL,
                ops_json TEXT NOT NULL,
                created_secs INTEGER NOT NULL
            );
            -- Every session WE host (fresh or resumed): the crash-survivable
            -- record. Written at spawn/resume time so a total crash leaves the
            -- row `alive=1`; on the next launch any alive row is necessarily
            -- from a prior process => restorable. `cli_session_id` is the resume
            -- handle (claude --resume <id> / codex resume <id>) we always know.
            CREATE TABLE IF NOT EXISTS hosted_session (
                cli_session_id TEXT PRIMARY KEY,
                project_key TEXT NOT NULL,
                kind TEXT NOT NULL,
                cwd TEXT NOT NULL,
                started_secs INTEGER NOT NULL,
                last_seen_secs INTEGER NOT NULL,
                alive INTEGER NOT NULL DEFAULT 1
            );
            -- Named per-CLI account (codex/claude "profiles"): an isolated
            -- config home (CLAUDE_CONFIG_DIR / CODEX_HOME) plus default model,
            -- extra args, and env overrides a spawn can adopt. STORE-ONLY for
            -- now (no spawn wiring / UI yet) — the crash-recovery row carries a
            -- nullable profile_id so a resumed session can re-adopt its account.
            CREATE TABLE IF NOT EXISTS profile (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                label TEXT NOT NULL,
                cli_kind TEXT NOT NULL,
                config_dir TEXT,
                model TEXT,
                extra_args_json TEXT NOT NULL DEFAULT '[]',
                env_json TEXT NOT NULL DEFAULT '{}',
                color TEXT,
                created_secs INTEGER NOT NULL DEFAULT 0
            );
            -- Manual project-attach: the user's durable INTENT to file a session
            -- (by its on-disk id) under a project, set BEFORE the session is ever
            -- recorded/resumed. Deliberately separate from hosted_session so an
            -- attach never writes alive=1 and pollutes the restore-on-launch set.
            CREATE TABLE IF NOT EXISTS session_project_override (
                cli_session_id TEXT PRIMARY KEY,
                project_key TEXT NOT NULL,
                set_at_secs INTEGER NOT NULL
            );
            -- Persisted activity log for the Standup "Today" digest. The live
            -- event ring is RAM-only (256-capped) so the digest was empty after a
            -- restart; the GUI writes TurnEnd summaries here as it observes them.
            -- Keyed by (cli session id, wall-clock ms) so a resume/backfill that
            -- re-observes the same turn dedupes instead of double-counting.
            CREATE TABLE IF NOT EXISTS session_event (
                sess TEXT NOT NULL,
                project_key TEXT NOT NULL,
                at_ms INTEGER NOT NULL,
                summary TEXT NOT NULL,
                PRIMARY KEY (sess, at_ms)
            );
            CREATE INDEX IF NOT EXISTS session_event_at ON session_event(at_ms);
            -- app-level key/value settings (default claude effort, etc.).
            CREATE TABLE IF NOT EXISTS app_settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            -- LLM session summaries (#16) — APPEND-history on purpose: these are
            -- the first durable records of the memory system (the product map
            -- will read them); the UI shows only the latest per session.
            CREATE TABLE IF NOT EXISTS session_summary (
                sess TEXT NOT NULL,
                project_key TEXT NOT NULL,
                at_ms INTEGER NOT NULL,
                thru_at_ms INTEGER NOT NULL,   -- newest TurnEnd covered (claude freshness)
                src_bytes INTEGER NOT NULL,    -- transcript size at read (codex freshness)
                src_path TEXT NOT NULL,
                goal TEXT NOT NULL,
                headline TEXT NOT NULL,
                next_action TEXT NOT NULL,
                detail_json TEXT NOT NULL,
                PRIMARY KEY (sess, at_ms)
            );
            -- typed memory substrate (docs/020-021): exact source ledger,
            -- source spans, queryable memory objects, graph edges, and human
            -- corrections. The Map is a projection over this substrate.
            CREATE TABLE IF NOT EXISTS memory_source (
                id TEXT PRIMARY KEY,
                project_key TEXT NOT NULL,
                kind TEXT NOT NULL,
                uri TEXT NOT NULL,
                title TEXT,
                captured_at_secs INTEGER NOT NULL,
                content_hash TEXT,
                metadata_json TEXT NOT NULL DEFAULT '{}'
            );
            CREATE INDEX IF NOT EXISTS memory_source_project ON memory_source(project_key);
            CREATE TABLE IF NOT EXISTS memory_span (
                id TEXT PRIMARY KEY,
                source_id TEXT NOT NULL,
                start_ref TEXT,
                end_ref TEXT,
                quote TEXT
            );
            CREATE INDEX IF NOT EXISTS memory_span_source ON memory_span(source_id);
            CREATE TABLE IF NOT EXISTS memory_object (
                id TEXT PRIMARY KEY,
                project_key TEXT NOT NULL,
                kind TEXT NOT NULL,
                title TEXT NOT NULL,
                body_md TEXT NOT NULL,
                state TEXT NOT NULL,
                confidence REAL NOT NULL,
                created_by TEXT NOT NULL,
                created_at_secs INTEGER NOT NULL,
                updated_at_secs INTEGER NOT NULL,
                valid_from_secs INTEGER,
                valid_to_secs INTEGER,
                superseded_by TEXT,
                source_span_ids_json TEXT NOT NULL DEFAULT '[]',
                projection_json TEXT NOT NULL DEFAULT '{}',
                metadata_json TEXT NOT NULL DEFAULT '{}'
            );
            CREATE INDEX IF NOT EXISTS memory_object_project ON memory_object(project_key);
            CREATE TABLE IF NOT EXISTS memory_edge (
                id TEXT PRIMARY KEY,
                project_key TEXT NOT NULL,
                src_id TEXT NOT NULL,
                dst_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                confidence REAL NOT NULL,
                created_at_secs INTEGER NOT NULL,
                source_span_id TEXT,
                metadata_json TEXT NOT NULL DEFAULT '{}'
            );
            CREATE INDEX IF NOT EXISTS memory_edge_project ON memory_edge(project_key);
            CREATE TABLE IF NOT EXISTS memory_correction (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_key TEXT NOT NULL,
                target_id TEXT NOT NULL,
                action TEXT NOT NULL,
                note TEXT NOT NULL,
                corrected_at_secs INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS memory_correction_project ON memory_correction(project_key);
            CREATE TABLE IF NOT EXISTS memory_candidate (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_key TEXT NOT NULL,
                candidate_json TEXT NOT NULL,
                created_by TEXT NOT NULL,
                created_secs INTEGER NOT NULL,
                status TEXT NOT NULL DEFAULT 'open'
            );
            CREATE INDEX IF NOT EXISTS memory_candidate_project ON memory_candidate(project_key, status);
            "#,
        )?;
        self.conn.execute_batch(
            r#"
            -- the DECISION/NOTE log (#10 memory layer): append-only, provenanced.
            -- Answers "what did we decide about X" — a mutable field never can.
            CREATE TABLE IF NOT EXISTS part_note (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                part_id INTEGER NOT NULL,
                project_key TEXT NOT NULL,
                ts_secs INTEGER NOT NULL,
                kind TEXT NOT NULL,      -- decision | note | context
                text TEXT NOT NULL,
                source TEXT NOT NULL     -- user | sess-<cli id>
            );
            CREATE INDEX IF NOT EXISTS part_note_part ON part_note(part_id, ts_secs);
            -- cross-cutting decisions surface on every node they govern.
            CREATE TABLE IF NOT EXISTS note_part (
                note_id INTEGER NOT NULL,
                part_id INTEGER NOT NULL,
                PRIMARY KEY (note_id, part_id)
            );
            "#,
        )?;
        self.conn.execute_batch(
            r#"
            -- session↔node linkage (docs/011): which node a session works on.
            -- role 'dispatch' = declared intent at spawn/relink (renders as a
            -- map chip; at most ONE per session, enforced in code); 'touch' =
            -- observed at summarize time (outline-only, never a chip).
            CREATE TABLE IF NOT EXISTS session_part (
                cli_session_id TEXT NOT NULL,
                part_id INTEGER NOT NULL,
                project_key TEXT NOT NULL,
                role TEXT NOT NULL DEFAULT 'dispatch',
                at_secs INTEGER NOT NULL,
                PRIMARY KEY (cli_session_id, part_id)
            );
            CREATE INDEX IF NOT EXISTS session_part_part ON session_part(part_id);
            CREATE INDEX IF NOT EXISTS session_part_proj ON session_part(project_key);
            "#,
        )?;
        self.conn.execute_batch(
            r#"
            -- docs/019: a named machine proposal reviewed as one unit (edit-
            -- before-accept, one transaction, one ⌘Z). scope_part_id is the
            -- user-pointed fence (NULL = whole map, explicitly granted).
            CREATE TABLE IF NOT EXISTS changeset (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_key TEXT NOT NULL,
                title TEXT NOT NULL,
                instruction TEXT NOT NULL DEFAULT '',
                scope_part_id INTEGER,
                origin_run TEXT NOT NULL DEFAULT '',
                created_secs INTEGER NOT NULL,
                status TEXT NOT NULL DEFAULT 'open'  -- open|accepted|rejected|partial
            );
            CREATE INDEX IF NOT EXISTS changeset_proj ON changeset(project_key, status);
            -- docs/019: the durable summarizer queue (T-END/T-IDLE/T-DELTA).
            -- Survives restarts; 'dead' rows surface as cards, never vanish —
            -- the 981-events→0-summaries death was an in-memory queue.
            CREATE TABLE IF NOT EXISTS summary_job (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                cli_session_id TEXT NOT NULL,
                project_key TEXT NOT NULL,
                trigger TEXT NOT NULL,               -- end|delta|idle|backfill
                enqueued_ms INTEGER NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                last_error TEXT,
                state TEXT NOT NULL DEFAULT 'queued' -- queued|running|done|dead
            );
            CREATE INDEX IF NOT EXISTS summary_job_state ON summary_job(state, enqueued_ms);
            -- docs/019 commitment 5: the user-set needs-you FLAG (orthogonal to
            -- lifecycle). One row per flagged node; `question` renders VERBATIM and
            -- `set_secs` drives the 7d anti-decay escalation. AwaitingDecision
            -- sessions auto-flag WITHOUT a row (that lane is derived, live-only).
            CREATE TABLE IF NOT EXISTS needs_you (
                part_id INTEGER PRIMARY KEY,
                project_key TEXT NOT NULL,
                question TEXT NOT NULL,
                set_secs INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS needs_you_proj ON needs_you(project_key);
            "#,
        )?;
        // recall index (#10): one FTS5 table over nodes + notes + summaries,
        // rebuilt on demand (search_all) — trivial at this scale, zero triggers.
        self.conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS brain_fts USING fts5(kind, project_key, ref_id, title, body);",
        )?;
        // Columns added after the table shipped — SQLite has no ADD COLUMN IF
        // NOT EXISTS, so gate on PRAGMA table_info (a bare ALTER in the batch
        // would fail every open after the first — design critique #4).
        self.add_column_if_missing("hosted_session", "closed_at_secs", "INTEGER")?;
        self.add_column_if_missing("hosted_session", "dismissed_at_secs", "INTEGER")?;
        // The named CLI account (profile) a hosted session was spawned/resumed
        // under. NULL = no profile (today: always, until spawn wiring lands);
        // delete_profile soft-orphans by NULLing this rather than dropping rows.
        self.add_column_if_missing("hosted_session", "profile_id", "INTEGER")?;
        // #10: provenance on the journal + persisted spatial positions.
        self.add_column_if_missing("tree_event", "origin", "TEXT")?;
        self.add_column_if_missing("tree_event", "source_sess", "TEXT")?;
        self.add_column_if_missing("part", "map_x", "REAL")?;
        self.add_column_if_missing("part", "map_y", "REAL")?;
        // #10 slice 2: per-op evidence quotes on proposals — a JSON array of
        // nullable strings, index-aligned with ops_json. NULL on old rows.
        self.add_column_if_missing("pending_diff", "evidence_json", "TEXT")?;
        // docs/019 slice 1a: typed nodes + markdown body + the provenance quad
        // (the permanent "why is this here?" answer — populated by the slice-2
        // cartographer; created_by is stamped from origin on every Add).
        self.add_column_if_missing("part", "kind", "TEXT NOT NULL DEFAULT 'task'")?;
        self.add_column_if_missing("part", "detail_md", "TEXT")?;
        self.add_column_if_missing("part", "created_by", "TEXT NOT NULL DEFAULT 'legacy'")?;
        self.add_column_if_missing("part", "source_file", "TEXT")?;
        self.add_column_if_missing("part", "source_quote", "TEXT")?;
        self.add_column_if_missing("part", "rationale", "TEXT")?;
        // docs/019 (seed / re-ground): the ratified one-line organizing
        // principle of the map. Written when the seed's roots are accepted;
        // injected verbatim into every later expand/rework prompt so the
        // machine can never drift from the map's own grammar.
        self.add_column_if_missing("project", "taxonomy_note", "TEXT")?;
        // #29: the project's OWN DIRECTORY. Set for a project created with
        // "＋ new project" (and for an idea promoted on its first spawn) — the
        // GUI feeds it back into the scan as an Explicit source at that dir, so
        // the project keys `path:<dir>` from birth and its own sessions fold
        // onto it instead of minting a second row. NULL = a path-less idea, or
        // a project the scan already finds on its own (sessions/git).
        self.add_column_if_missing("project", "path", "TEXT")?;
        // docs/019: observed-touch promotion needs accumulating weight and a
        // recency stamp (at_secs stays = first link time).
        self.add_column_if_missing("session_part", "weight", "REAL NOT NULL DEFAULT 0")?;
        self.add_column_if_missing("session_part", "last_touch_secs", "INTEGER")?;
        // docs/019: proposals group under a reviewable changeset (NULL = legacy
        // singleton card).
        self.add_column_if_missing("pending_diff", "changeset_id", "INTEGER")?;
        // The summary job's LAST STATE CHANGE (claim / defer / finish), ms.
        // Two jobs depend on it and neither can be done with enqueued_ms alone:
        //   - a DEAD row's updated_ms is the DEATH time — the anchor of the
        //     escalating cool-off that replaced the permanent dead-job
        //     blacklist. Without a death stamp the cool-off can't expire.
        //   - a RUNNING row's updated_ms is its CLAIM time — the lease that
        //     lets a crash-orphaned 'running' row be reclaimed instead of
        //     wedging its session forever.
        // NULL on rows written before this column: every read COALESCEs to
        // enqueued_ms, which for a dead row is within ~3 minutes of its death
        // (3 attempts, 60s apart), so legacy rows age out correctly with no
        // backfill and no one-time migration flag.
        self.add_column_if_missing("summary_job", "updated_ms", "INTEGER")?;
        // NOT-BEFORE: a DEFERRED job (the world wasn't ready — transcript not
        // written yet, provider rate-limited) is not eligible until this instant,
        // and the claim both filters AND sorts on it. Without it a defer refunded
        // the attempt but left `enqueued_ms` alone, so the job went straight back
        // to the HEAD of a strictly-oldest-first queue and was re-claimed on the
        // next tick, forever — one un-summarizable session starved every other
        // session's standup. NULL on old rows → COALESCE(next_attempt_ms,
        // enqueued_ms), i.e. "ready when enqueued", which is exactly the old
        // behaviour: the migration cannot change how an existing row is claimed.
        self.add_column_if_missing("summary_job", "next_attempt_ms", "INTEGER")?;
        // How many times this job has been deferred. Bounded (MAX_SUMMARY_DEFERS)
        // so an IMMORTAL JOB IS IMPOSSIBLE BY CONSTRUCTION: a defer refunds the
        // attempt, so without a ceiling a job that always defers could never die,
        // no matter how right or wrong the rate-limit classifier happens to be.
        // NULL on old rows → COALESCE(defers,0): every existing job starts with a
        // full allowance, and none of them can be mid-defer (the column didn't
        // exist), so there is nothing to backfill.
        self.add_column_if_missing("summary_job", "defers", "INTEGER")?;
        // docs/019 slice 2: per-op evidence flags — a JSON array of bools,
        // index-aligned with ops_json (true = the cartographer's quote failed
        // verification). NULL on old rows → all-false (a canned/legacy op).
        self.add_column_if_missing("pending_diff", "flagged_json", "TEXT")?;
        // ONE-TIME frame migration (docs/011 slice 2): child pins were placed
        // in the project-root frame; the two-generation canvas reinterprets
        // map_x/map_y as position-on-the-parent's-canvas, so old child pins
        // would land nonsensically — NULL them back to auto-placement.
        // Aspects (roots) keep their pins. The flag makes it fire exactly
        // once; on a fresh DB it flags an empty UPDATE, harmless either way.
        if self.get_setting("map_frame_v2").is_none() {
            self.conn.execute(
                "UPDATE part SET map_x=NULL, map_y=NULL WHERE parent_id IS NOT NULL",
                [],
            )?;
            self.set_setting("map_frame_v2", "1")?;
        }
        // docs/019 commitment 2: `building` is derived-only — write-time
        // coercion (Lifecycle::assertable) stops NEW building rows here in
        // slice 1a. The building→todo data migration for pre-019 rows is
        // DEFERRED to slice 3 (review finding: rewriting them before the
        // derived-building overlay ships would erase visible in-progress
        // state with nothing to replace it; stored rows still parse + render
        // until then).
        // docs/019 slice 1a: one-time kind backfill — has children → area,
        // leaf → task (the column default). Journaled as ONE tree_event per
        // project (origin 'migration') so it is inspectable and ⌘Z-undoable;
        // misclassifications flip with one gesture (SetKind).
        if self.get_setting("kind_backfill_v3").is_none() {
            self.backfill_kinds()?;
            self.set_setting("kind_backfill_v3", "1")?;
        }
        Ok(())
    }

    /// The one-time docs/019 kind backfill (see migrate). Parents become
    /// areas; everything else keeps the 'task' column default.
    fn backfill_kinds(&self) -> rusqlite::Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT p.project_key, p.id FROM part p
             WHERE EXISTS (SELECT 1 FROM part c WHERE c.parent_id = p.id)
             ORDER BY p.project_key, p.id",
        )?;
        let rows: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        let mut by_project: HashMap<String, Vec<i64>> = HashMap::new();
        for (key, id) in rows {
            by_project.entry(key).or_default().push(id);
        }
        for (key, ids) in by_project {
            let ops: Vec<DiffOp> = ids
                .iter()
                .map(|id| DiffOp::SetKind {
                    id: *id,
                    kind: Kind::Area,
                })
                .collect();
            let inverse: Vec<DiffOp> = ids
                .iter()
                .map(|id| DiffOp::SetKind {
                    id: *id,
                    kind: Kind::Task,
                })
                .collect();
            for id in &ids {
                self.conn
                    .execute("UPDATE part SET kind='area' WHERE id=?1", params![id])?;
            }
            self.conn.execute(
                "INSERT INTO tree_event(project_key,accept_id,ts_secs,ops_json,inverse_json,origin) VALUES(?1,?2,?3,?4,?5,'migration')",
                params![
                    key,
                    format!("kind-backfill-{}", now()),
                    now() as i64,
                    serde_json::to_string(&ops).unwrap_or_default(),
                    serde_json::to_string(&inverse).unwrap_or_default(),
                ],
            )?;
        }
        Ok(())
    }

    fn add_column_if_missing(&self, table: &str, col: &str, decl: &str) -> rusqlite::Result<()> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let have: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(|r| r.ok())
            .collect();
        if !have.iter().any(|c| c == col) {
            if let Err(e) = self
                .conn
                .execute(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"), [])
            {
                // two instances can race the check-then-ALTER on first launch
                // after an upgrade; the loser must not fail open() into the
                // silent in-memory fallback (review). Only this error is eaten.
                if !e.to_string().contains("duplicate column") {
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}
