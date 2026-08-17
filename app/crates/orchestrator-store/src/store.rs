//! The SQLite store (docs/016) — persists the DESIGN tree, asserted status,
//! anchors, and an append-only journal (undo + since-you-were-away). Keyed by
//! the registry's canonical_key. The tree exists nowhere else on disk.

#[allow(unused_imports)] // HashMap is used by the `tests` module (store/tests.rs) via `use super::*`.
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};

// The memory + tree type imports below are consumed by the `tests` module
// (store/tests.rs) through `use super::*`; the non-test lib build sees them as
// unused, so the test-only ones carry `#[allow(unused_imports)]`.
#[allow(unused_imports)]
use crate::memory::{
    HumanCorrection, MemoryBackend, MemoryEdge, MemoryEdgeKind, MemoryError, MemoryObject,
    MemoryObjectKind, MemoryObjectState, MemoryResult, MemorySource, MemorySourceKind, MemorySpan,
    Projection, ProjectionItem, ProjectionKind, ProjectionRequest, ProjectionTrust, RetrievalItem,
    RetrievalQuery, RetrievalResult,
};
#[allow(unused_imports)]
use crate::memory_engine::{
    apply_native_memory_candidates, NativeMemoryCandidate, NativeMemoryDecisionKind,
    NativeMemoryEngineReport,
};
#[allow(unused_imports)]
use crate::memory_extract::{MemoryDocument, RuleBackedMemory};
#[allow(unused_imports)]
use crate::memory_llm::parse_llm_memory_candidates;
#[allow(unused_imports)]
use crate::tree::{first_line, DiffOp, Kind, Lifecycle, Part, PartId, PartRef, StatusSource};

mod brain;
mod memory_store;
mod profiles;
mod proposals;
mod schema;
mod sessions;
mod summaries;
mod tree_ops;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedState {
    None,     // no tree yet — offer the seed CTA
    Proposed, // an extraction is pending accept
    Seeded,   // has a tree (seeded or authored)
    Blank,    // user chose start-blank
}

impl SeedState {
    fn as_str(self) -> &'static str {
        match self {
            SeedState::None => "none",
            SeedState::Proposed => "proposed",
            SeedState::Seeded => "seeded",
            SeedState::Blank => "blank",
        }
    }
    fn parse(s: &str) -> SeedState {
        match s {
            "proposed" => SeedState::Proposed,
            "seeded" => SeedState::Seeded,
            "blank" => SeedState::Blank,
            _ => SeedState::None,
        }
    }
}

/// A proposed diff awaiting the user's accept (seed / drift / summary).
#[derive(Debug, Clone)]
pub struct PendingDiff {
    pub id: i64,
    pub kind: String,
    pub ops: Vec<DiffOp>,
    /// Per-op evidence quotes (#10 slice 2), index-aligned with `ops` — the
    /// verbatim summary line that justifies each op. Always `ops.len()` long:
    /// rows from before the column existed load as all-`None`.
    pub evidence: Vec<Option<String>>,
    /// docs/019 slice 1c: the changeset this row belongs to (NULL = a legacy
    /// singleton proposal — seed / drift / summary — reviewed per-op in the
    /// outline). Changeset-linked rows are HIDDEN from the per-node proposal
    /// path and surface only through the grouped changeset review.
    pub changeset_id: Option<i64>,
    /// docs/019 slice 2 (commitment 3 / ruling 4): per-op EVIDENCE FLAG,
    /// index-aligned with `ops`. `true` = the cartographer's verbatim quote did
    /// NOT verify against the real repo file (kept + marked, EXCLUDED from
    /// accept-all — individually accepted only). Rows from before the column
    /// existed load as all-`false` (a legacy/canned op carries no flag).
    pub flagged: Vec<bool>,
}

/// Flatten a changeset's linked pending rows into ONE ops+evidence list, in
/// (row-id, then op) order (docs/019 slice 1c). This ordering IS the changeset
/// review's stable global op index — the position a toggle-off / name-edit
/// keys on, and the position accept rebuilds the kept `Vec<DiffOp>` from. Pure
/// so the index contract is unit-testable.
pub fn flatten_changeset_ops(rows: &[PendingDiff]) -> Vec<(DiffOp, Option<String>)> {
    rows.iter()
        .flat_map(|pd| pd.ops.iter().cloned().zip(pd.evidence.iter().cloned()))
        .collect()
}

/// The per-op EVIDENCE FLAGS for a changeset, in the SAME global (row, op)
/// order as `flatten_changeset_ops` (docs/019 slice 2). Kept a SEPARATE
/// parallel vector rather than widening the flatten tuple so every existing
/// slice-1c caller (and its tests) stays byte-identical; the review surface
/// zips the two by index. `true` = unverifiable → excluded from accept-all.
pub fn flatten_changeset_flags(rows: &[PendingDiff]) -> Vec<bool> {
    rows.iter()
        .flat_map(|pd| pd.flagged.iter().copied())
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub enum TimelineKind {
    Summary,
    Trail,
    Decision,
    Map,
}

/// One Standup-timeline entry (docs/012). `node` = (part_id, name) for
/// node-attributed kinds; `count` = batched op count for Map entries.
#[derive(Debug, Clone)]
pub struct TimelineEvent {
    pub ts_ms: u64,
    pub project_key: String,
    pub kind: TimelineKind,
    pub sess: String,
    pub node: Option<(i64, String)>,
    pub text: String,
    pub next: String,
    pub detail_json: String,
    pub count: usize,
}

/// The latest LLM summary of a session (#16) — read-side of session_summary.
#[derive(Debug, Clone)]
pub struct SummaryRow {
    pub sess: String,
    pub project_key: String,
    pub at_ms: u64,
    pub thru_at_ms: u64,
    pub src_bytes: u64,
    pub src_path: String,
    pub goal: String,
    pub headline: String,
    pub next_action: String,
    pub detail_json: String,
}

/// A staged memory extraction result awaiting human review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryCandidateRow {
    pub id: i64,
    pub project_key: String,
    pub candidate: RuleBackedMemory,
    pub created_by: String,
    pub created_secs: u64,
    pub status: String,
}

/// One append-only decision/note entry on a map node (#10 memory layer).
#[derive(Debug, Clone)]
pub struct NoteRow {
    pub id: i64,
    pub part_id: i64,
    pub ts_secs: u64,
    pub kind: String,
    pub text: String,
    pub source: String,
}

/// A recall hit (⌘K) across the three memory layers.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub kind: String, // node | note | summary
    pub project_key: String,
    pub ref_id: String, // part id or cli session id
    pub title: String,
    pub body: String,
}

/// The provenance quad on a part row (docs/019 commitment 3):
/// (created_by, source_file, source_quote, rationale) — the trust gate and
/// the "Why is this here?" answer are the same stored feature.
pub type Provenance = (String, Option<String>, Option<String>, Option<String>);

/// A session we host, recorded for crash recovery (the resume handle + binding).
#[derive(Debug, Clone)]
pub struct HostedSessionRow {
    pub cli_session_id: String,
    pub project_key: String,
    pub kind: String,
    pub cwd: String,
    pub started_secs: u64,
    pub last_seen_secs: u64,
    /// The named CLI account (profile) this session was spawned/resumed under
    /// (`profile` table). None = no profile — today always, until spawn wiring
    /// lands; `delete_profile` soft-orphans a row back to None.
    pub profile_id: Option<i64>,
}

/// A named per-CLI account ("profile"): an isolated config home
/// (CLAUDE_CONFIG_DIR / CODEX_HOME) plus default model, extra args, and env
/// overrides a spawn can adopt. Read-side of the `profile` table. STORE-ONLY
/// for now — no spawn env-injection or UI reads this yet.
#[derive(Debug, Clone)]
pub struct ProfileRow {
    pub id: i64,
    pub label: String,
    pub cli_kind: String,
    pub config_dir: Option<String>,
    pub model: Option<String>,
    pub extra_args: Vec<String>,
    pub env: std::collections::HashMap<String, String>,
    pub color: Option<String>,
}

/// One session↔node link row (docs/019 slice 3): the whole session_part row the
/// GUI needs per frame to derive building (dispatch/declared recency), tier the
/// observed-touch chips (weight + last_touch), and detect drift. Plain data —
/// the render assembles per-node link lists and joins the live-alive set.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionPartRow {
    pub cli_session_id: String,
    pub part_id: i64,
    pub role: String,
    /// link creation time (secs) — dispatch/declared recency for building.
    pub at_secs: u64,
    /// accumulated observed-touch weight (0 for a pure dispatch/declared link).
    pub weight: f64,
    /// last observed-touch time (secs), None if never touched.
    pub last_touch_secs: Option<u64>,
}

pub struct Store {
    conn: Connection,
    /// the brain_fts index needs a rebuild (a memory-layer write happened).
    /// Rebuild-per-query was measured at 12-34ms past ~3k rows — a per-
    /// keystroke stall in ⌘K (review 1b); dirty-gating makes steady-state
    /// typing sub-ms again. Cell is fine: Store lives behind a Mutex.
    fts_dirty: std::cell::Cell<bool>,
    /// Monotonic write generation (#10 slice 2): bumped on every store write
    /// the GUI renders from (notes/summaries/accepts — i.e. wherever fts_dirty
    /// is set — plus pending-diff add/drop, which the GUI also reads per
    /// frame). The GUI memoizes per-frame reads against it.
    write_gen: std::cell::Cell<u64>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Store> {
        let conn = Connection::open(path)?;
        let s = Store {
            conn,
            fts_dirty: std::cell::Cell::new(true),
            write_gen: std::cell::Cell::new(0),
        };
        s.migrate()?;
        Ok(s)
    }

    pub fn open_in_memory() -> rusqlite::Result<Store> {
        let conn = Connection::open_in_memory()?;
        let s = Store {
            conn,
            fts_dirty: std::cell::Cell::new(true),
            write_gen: std::cell::Cell::new(0),
        };
        s.migrate()?;
        Ok(s)
    }

    /// The current write generation — strictly increases on every store write
    /// the GUI renders from. Compare against a remembered value to skip
    /// re-reading notes/pending/summaries on frames where nothing changed.
    pub fn write_gen(&self) -> u64 {
        self.write_gen.get()
    }

    /// Bump the write generation (a GUI-visible write happened).
    fn bump_gen(&self) {
        self.write_gen.set(self.write_gen.get() + 1);
    }

    /// A memory-layer write: the FTS index needs a rebuild AND the GUI's
    /// per-frame memoization key advances. Every fts_dirty site goes here.
    fn mark_dirty(&self) {
        self.fts_dirty.set(true);
        self.bump_gen();
    }


    /// UPSERT a project row from the registry (refreshes label; never touches
    /// the tree).
    pub fn ensure_project(&self, key: &str, name: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO project(key,name,last_opened_secs) VALUES(?1,?2,?3)
             ON CONFLICT(key) DO UPDATE SET name=?2",
            params![key, name, now()],
        )?;
        Ok(())
    }


    /// Is there a project row under this key? (The promotion guard: a key that
    /// already belongs to someone must never be merged into blindly.)
    pub fn project_exists(&self, key: &str) -> bool {
        self.conn
            .query_row("SELECT 1 FROM project WHERE key=?1", params![key], |_| Ok(()))
            .is_ok()
    }

    /// The stored display name for a project key, if the row exists. Used by
    /// "Open folder…" (#34) to RE-ATTACH a dormant project to its folder under
    /// its OWN name — never the folder basename, which `ensure_project` would
    /// wrongly write over the survivor's name.
    pub fn project_name(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT name FROM project WHERE key=?1", params![key], |r| {
                r.get::<_, String>(0)
            })
            .ok()
    }

    /// Record the project's OWN DIRECTORY (#29). Also what makes the next scan
    /// inject an Explicit source at that dir, so the row survives a rescan even
    /// before it has a single session.
    pub fn set_project_path(&self, key: &str, path: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE project SET path=?2 WHERE key=?1",
            params![key, path],
        )?;
        self.bump_gen();
        Ok(())
    }

    /// Rewrite a project's CANONICAL KEY across the whole store — the ONE place
    /// a project's identity moves (idea → path promotion on its first spawn,
    /// #29). Everything the user owns is keyed by that slug, so this is
    /// table-driven off the LIVE SCHEMA, not a hardcoded list: every table with
    /// a `project_key` column is swept, so a table added next year cannot
    /// silently orphan his map, journal, notes, memory, summaries or sessions.
    ///
    /// Also moves the slug-suffixed `app_settings` keys (`map_root:<slug>`,
    /// `map_stars:<slug>`, `memory_prop_thru:<slug>`, …) and the `project_order`
    /// rail array. One transaction: a half-migration is impossible.
    ///
    /// NEVER MERGES. If `new` already belongs to a project this REFUSES — moving
    /// onto an occupied key would fuse two maps into one tree and delete the
    /// victim's `project` row (name, seed_state, taxonomy_note), irreversibly and
    /// without a prompt. Callers check first (`project_exists`); this is the last
    /// line, because there is no `DELETE FROM project` anywhere: a project whose
    /// transcripts aged out still owns its key, its map and its memory forever.
    pub fn rename_project_key(&mut self, old: &str, new: &str) -> rusqlite::Result<()> {
        if old == new || old.is_empty() || new.is_empty() {
            return Ok(());
        }
        if self.project_exists(new) {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY),
                Some(format!("{new} already belongs to another project")),
            ));
        }
        let tables = self.tables_with_project_key()?;
        let tx = self.conn.transaction()?;
        for t in &tables {
            tx.execute(
                &format!("UPDATE {t} SET project_key=?2 WHERE project_key=?1"),
                params![old, new],
            )?;
        }
        // plain UPDATE, never `OR REPLACE`: the guard above proves `new` is free,
        // and `OR REPLACE` would DELETE the row sitting there if it weren't.
        tx.execute("UPDATE project SET key=?2 WHERE key=?1", params![old, new])?;
        // settings whose KEY embeds the slug. Rewritten in Rust, never with a
        // LIKE — a path slug can contain `_`/`%`, which are LIKE wildcards.
        //
        // ANCHORED at the FIRST `:` (review). Every project-scoped setting is
        // `<name>:<slug>` with a colon-free name (`map_root`, `map_stars`,
        // `memory_prop_thru`, …) while the slug is ITSELF colon-schemed
        // (`idea:…`, `path:…`, `github:…`). Matching the slug as an unanchored
        // suffix therefore read `map_root:idea:api` as `map_root:idea:` + `api`
        // and rewrote it when an UNRELATED project keyed `api` was renamed —
        // one project's promotion silently moved another project's map root.
        let renames: Vec<(String, String, String)> = {
            let mut stmt = tx.prepare("SELECT key,value FROM app_settings")?;
            let rows: Vec<(String, String)> = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .filter_map(|r| r.ok())
                .collect();
            rows.into_iter()
                .filter_map(|(k, v)| {
                    let (name, slug) = k.split_once(':')?; // global settings have no `:`
                    (slug == old).then(|| (k.clone(), format!("{name}:{new}"), v))
                })
                .collect()
        };
        for (from, to, val) in renames {
            tx.execute("DELETE FROM app_settings WHERE key=?1", params![from])?;
            tx.execute(
                "INSERT OR REPLACE INTO app_settings(key,value) VALUES(?1,?2)",
                params![to, val],
            )?;
        }
        // the rail order is a JSON array OF SLUGS (#28) — rewrite the element.
        let order: Option<String> = tx
            .query_row(
                "SELECT value FROM app_settings WHERE key='project_order'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok();
        if let Some(json) = order {
            if let Ok(mut slugs) = serde_json::from_str::<Vec<String>>(&json) {
                if slugs.iter().any(|s| s == old) {
                    for s in slugs.iter_mut() {
                        if s == old {
                            *s = new.to_string();
                        }
                    }
                    if let Ok(next) = serde_json::to_string(&slugs) {
                        tx.execute(
                            "INSERT OR REPLACE INTO app_settings(key,value) VALUES('project_order',?1)",
                            params![next],
                        )?;
                    }
                }
            }
        }
        tx.commit()?;
        // brain_fts carries project_key but is CONTENTLESS-by-rebuild (search_all
        // re-derives it from the base tables) — marking it dirty is the migration.
        self.mark_dirty();
        Ok(())
    }

    /// Every table carrying a `project_key` column, read from the live schema.
    /// `brain_fts*` is excluded on purpose: it is rebuilt wholesale from the
    /// base tables on the next search (see `rename_project_key`).
    fn tables_with_project_key(&self) -> rusqlite::Result<Vec<String>> {
        let names: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        let mut out = Vec::new();
        for n in names {
            if n.starts_with("brain_fts") {
                continue;
            }
            let mut ti = self.conn.prepare(&format!("PRAGMA table_info({n})"))?;
            let has = ti
                .query_map([], |r| r.get::<_, String>(1))?
                .filter_map(|r| r.ok())
                .any(|c| c == "project_key");
            drop(ti);
            if has {
                out.push(n);
            }
        }
        Ok(out)
    }

    /// An app setting value (None if unset). `.ok()` folds a missing row to None.
    pub fn get_setting(&self, key: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT value FROM app_settings WHERE key=?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .ok()
    }

    /// Set (UPSERT) an app setting.
    pub fn set_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO app_settings(key,value) VALUES(?1,?2)",
            params![key, value],
        )?;
        Ok(())
    }



    pub fn seed_state(&self, key: &str) -> SeedState {
        self.conn
            .query_row(
                "SELECT seed_state FROM project WHERE key=?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .map(|s| SeedState::parse(&s))
            .unwrap_or(SeedState::None)
    }

    pub fn set_seed_state(&self, key: &str, state: SeedState) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE project SET seed_state=?2 WHERE key=?1",
            params![key, state.as_str()],
        )?;
        Ok(())
    }

    /// The ratified organizing principle for this project's map (docs/019 seed
    /// / re-ground). None = never seeded with provenance (no note to inject).
    /// Empty string collapses to None so callers can `if let Some(note)` freely.
    pub fn taxonomy_note(&self, key: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT taxonomy_note FROM project WHERE key=?1",
                params![key],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
            .filter(|s| !s.trim().is_empty())
    }

    /// Record the map's organizing principle (one sentence). Written when the
    /// seed's roots are accepted; re-ground overwrites it. Injected into every
    /// later expand/rework prompt.
    pub fn set_taxonomy_note(&self, key: &str, note: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE project SET taxonomy_note=?2 WHERE key=?1",
            params![key, note],
        )?;
        self.bump_gen();
        Ok(())
    }

}





