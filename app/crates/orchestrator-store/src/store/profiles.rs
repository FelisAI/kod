//! Named per-CLI accounts ("profiles"): the CRUD read/write side of the
//! `profile` table (docs: codex/claude profiles). A profile is an isolated
//! config home (CLAUDE_CONFIG_DIR / CODEX_HOME) plus a default model, extra
//! args, and env overrides a spawn can later adopt. STORE-ONLY foundation —
//! no spawn env-injection or UI wires through here yet.

use std::collections::HashMap;

use rusqlite::params;

use super::{now, ProfileRow, Store};

/// The `profile` columns, in the order `row_to_profile` reads them. Shared by
/// `profile` (by id) and `profiles` (all) so the two selects can never drift.
const PROFILE_COLS: &str = "id,label,cli_kind,config_dir,model,extra_args_json,env_json,color";

/// Map a `profile` row into a `ProfileRow`. The two JSON columns
/// (`extra_args_json`, `env_json`) deserialize with `unwrap_or_default`: they
/// are `NOT NULL DEFAULT '[]'/'{}'`, so a malformed value folds to empty rather
/// than failing the whole read.
fn row_to_profile(r: &rusqlite::Row) -> rusqlite::Result<ProfileRow> {
    let extra_args_json: String = r.get(5)?;
    let env_json: String = r.get(6)?;
    Ok(ProfileRow {
        id: r.get(0)?,
        label: r.get(1)?,
        cli_kind: r.get(2)?,
        config_dir: r.get(3)?,
        model: r.get(4)?,
        extra_args: serde_json::from_str(&extra_args_json).unwrap_or_default(),
        env: serde_json::from_str(&env_json).unwrap_or_default(),
        color: r.get(7)?,
    })
}

impl Store {
    /// Create a named profile; returns its new id. `extra_args`/`env` are stored
    /// as JSON. Bumps the write generation like every GUI-visible write.
    pub fn create_profile(
        &self,
        label: &str,
        cli_kind: &str,
        config_dir: Option<&str>,
        model: Option<&str>,
        extra_args: &[String],
        env: &HashMap<String, String>,
        color: Option<&str>,
    ) -> rusqlite::Result<i64> {
        let extra_args_json = serde_json::to_string(extra_args).unwrap_or_else(|_| "[]".to_string());
        let env_json = serde_json::to_string(env).unwrap_or_else(|_| "{}".to_string());
        self.conn.execute(
            "INSERT INTO profile(label,cli_kind,config_dir,model,extra_args_json,env_json,color,created_secs)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                label,
                cli_kind,
                config_dir,
                model,
                extra_args_json,
                env_json,
                color,
                now() as i64
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        self.bump_gen();
        Ok(id)
    }

    /// A single profile by id (None if the row is gone). `.ok()` folds the
    /// missing-row error to None, matching the store's other by-id readers.
    pub fn profile(&self, id: i64) -> Option<ProfileRow> {
        self.conn
            .query_row(
                &format!("SELECT {PROFILE_COLS} FROM profile WHERE id=?1"),
                params![id],
                row_to_profile,
            )
            .ok()
    }

    /// Every profile, oldest first (created_secs, then id as a stable tiebreak).
    pub fn profiles(&self) -> Vec<ProfileRow> {
        let Ok(mut stmt) = self
            .conn
            .prepare(&format!("SELECT {PROFILE_COLS} FROM profile ORDER BY created_secs, id"))
        else {
            return Vec::new();
        };
        stmt.query_map([], row_to_profile)
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    /// Overwrite every field of a profile (created_secs is immutable).
    pub fn update_profile(
        &self,
        id: i64,
        label: &str,
        cli_kind: &str,
        config_dir: Option<&str>,
        model: Option<&str>,
        extra_args: &[String],
        env: &HashMap<String, String>,
        color: Option<&str>,
    ) -> rusqlite::Result<()> {
        let extra_args_json = serde_json::to_string(extra_args).unwrap_or_else(|_| "[]".to_string());
        let env_json = serde_json::to_string(env).unwrap_or_else(|_| "{}".to_string());
        self.conn.execute(
            "UPDATE profile SET label=?2,cli_kind=?3,config_dir=?4,model=?5,extra_args_json=?6,env_json=?7,color=?8
             WHERE id=?1",
            params![
                id,
                label,
                cli_kind,
                config_dir,
                model,
                extra_args_json,
                env_json,
                color
            ],
        )?;
        self.bump_gen();
        Ok(())
    }

    /// Delete a profile and SOFT-ORPHAN any hosted-session rows that adopted it
    /// (`profile_id` NULLed, row kept) — a crash-recovery record must outlive
    /// its profile, never cascade-delete with it.
    pub fn delete_profile(&self, id: i64) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM profile WHERE id=?1", params![id])?;
        self.conn.execute(
            "UPDATE hosted_session SET profile_id=NULL WHERE profile_id=?1",
            params![id],
        )?;
        self.bump_gen();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_roundtrip_create_read_update_delete() {
        // store test convention: a tempdir, never a real path.
        let dir = std::env::temp_dir().join(format!(
            "orch-profiles-test-{}-{}",
            std::process::id(),
            now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("store.sqlite");
        let s = Store::open(&db).unwrap();

        let mut env = HashMap::new();
        env.insert("ANTHROPIC_MODEL".to_string(), "opus".to_string());
        env.insert("FOO".to_string(), "bar".to_string());
        let extra = vec![
            "--dangerously-skip-permissions".to_string(),
            "-v".to_string(),
        ];

        let id = s
            .create_profile(
                "work",
                "claude",
                Some("/home/me/.claude-work"),
                Some("opus"),
                &extra,
                &env,
                Some("#ff8800"),
            )
            .unwrap();

        // read back: every field, incl. the env map + extra_args, round-trips.
        let got = s.profile(id).expect("profile exists after create");
        assert_eq!(got.id, id);
        assert_eq!(got.label, "work");
        assert_eq!(got.cli_kind, "claude");
        assert_eq!(got.config_dir.as_deref(), Some("/home/me/.claude-work"));
        assert_eq!(got.model.as_deref(), Some("opus"));
        assert_eq!(got.extra_args, extra);
        assert_eq!(got.env, env);
        assert_eq!(got.color.as_deref(), Some("#ff8800"));

        // listing surfaces it.
        let all = s.profiles();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, id);

        // update overwrites every field, including clearing the optionals.
        let mut env2 = HashMap::new();
        env2.insert("CODEX_HOME".to_string(), "/tmp/ch".to_string());
        s.update_profile(id, "personal", "codex", None, None, &[], &env2, None)
            .unwrap();
        let got = s.profile(id).expect("still exists after update");
        assert_eq!(got.label, "personal");
        assert_eq!(got.cli_kind, "codex");
        assert_eq!(got.config_dir, None);
        assert_eq!(got.model, None);
        assert!(got.extra_args.is_empty());
        assert_eq!(got.env, env2);
        assert_eq!(got.color, None);

        // a hosted-session row that adopted this profile...
        s.record_session("sess-1", "proj-a", "codex", "/tmp/a", Some(id))
            .unwrap();
        assert_eq!(s.restorable_sessions().unwrap()[0].profile_id, Some(id));

        // delete → profile gone, but the session row SURVIVES with profile_id NULLed.
        s.delete_profile(id).unwrap();
        assert!(s.profile(id).is_none());
        assert!(s.profiles().is_empty());
        let rows = s.restorable_sessions().unwrap();
        assert_eq!(rows.len(), 1, "session row soft-orphaned, not deleted");
        assert_eq!(rows[0].profile_id, None, "profile_id NULLed on delete");

        std::fs::remove_dir_all(&dir).ok();
    }
}
