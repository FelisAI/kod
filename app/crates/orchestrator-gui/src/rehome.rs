//! Re-home live sessions whose project binding names a rail project that no
//! longer exists.
//!
//! How sessions went invisible (2026-09-14: 8 of the 19 restored after the
//! panic): a restore binds a session to its STORED project key. When that key
//! was not a current rail slug, `ensure_home_for` minted an ad-hoc `path:<dir>`
//! home — git-blind, so for a repo with a remote it minted
//! `path:/…/alpha` while the scan keys that same directory
//! `github:acme/alpha`. The next scan dropped the phantom (it owned no
//! directory, so `rekey_moved_projects` never saw it either), and because the
//! rail only ever asks the host for sessions BY rail slug (`infos_for`), a
//! session bound to a vanished slug was fetched by nothing: not the rail, not
//! the Standup, not Recover's "already running" check — which is why resuming
//! one got the host's refusal ("session is already running") instead of a jump.
//!
//! Two halves. `translate_stale_key` maps a dead key to what its directory
//! resolves to TODAY, so restores and user overrides land on the live row.
//! `rehome_orphaned_sessions` walks every live session after each scan and
//! rebinds the ones nobody can reach, then does the same for closed rows and
//! pulls each session's HISTORY (summaries, events — what ▲ WHAT HAPPENED
//! groups by) onto its current binding, since those rows carry the slug they
//! were written under. Nothing here merges projects: a key is only ever
//! translated to a slug the rail already has.

use std::path::Path;

use crate::Orchestrator;

/// A stored project key → the rail slug it means today. `Some(key)` when it
/// already is one; for a `path:<dir>` key whose dir still exists, the key that
/// dir resolves to now — but ONLY if the rail has that slug. Never invents a
/// project: the caller mints a home from the session's cwd on `None`.
pub(crate) fn translate_stale_key(
    key: &str,
    is_slug: impl Fn(&str) -> bool,
    resolve_dir: impl Fn(&Path) -> Option<String>,
) -> Option<String> {
    if is_slug(key) {
        return Some(key.to_string());
    }
    let dir = Path::new(key.strip_prefix("path:")?);
    let now = resolve_dir(dir)?;
    is_slug(&now).then_some(now)
}

/// Where a live session bound to `slug` belongs; `None` = leave it. The user's
/// override (a deliberate move) wins whenever it translates to a rail slug.
/// Otherwise a session whose slug is still a rail project stays put, and an
/// orphan goes to whatever its dead key resolves to.
pub(crate) fn desired_home(
    slug: &str,
    override_key: Option<&str>,
    is_slug: impl Fn(&str) -> bool,
    translate: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    if let Some(t) = override_key.and_then(|k| translate(k)) {
        return (t != slug).then_some(t);
    }
    if is_slug(slug) {
        return None;
    }
    translate(slug)
}

impl Orchestrator {
    fn is_rail_slug(&self, key: &str) -> bool {
        self.projects.iter().any(|p| p.slug == key)
    }

    /// `translate_stale_key` against the current rail, resolving a directory
    /// the way the scan does (one git probe; only reached for a dead key).
    pub(crate) fn current_home_key(&self, key: &str) -> Option<String> {
        translate_stale_key(
            key,
            |k| self.is_rail_slug(k),
            |dir| dir.is_dir().then(|| orchestrator_core::scan::key_for_dir(dir)),
        )
    }

    /// After every scan: rebind the live sessions the rail can no longer reach.
    /// Walks `host.infos()` — every session, not per-slug — the one enumeration
    /// that cannot miss an orphan. Then closed rows, then history. Returns how
    /// many sessions moved.
    pub(crate) fn rehome_orphaned_sessions(&mut self) -> usize {
        let mut moved = 0;
        let mut handled: std::collections::HashSet<String> = std::collections::HashSet::new();
        for info in self.host.infos() {
            if !info.alive {
                continue;
            }
            let cli = info.cli_session_id.clone();
            if let Some(c) = &cli {
                handled.insert(c.clone());
            }
            let ov = cli.as_ref().and_then(|c| self.overrides.get(c)).cloned();
            let dest = desired_home(
                &info.project_slug,
                ov.as_deref(),
                |k| self.is_rail_slug(k),
                |k| self.current_home_key(k),
            )
            .or_else(|| {
                // a dead key whose directory is gone, or resolves to no rail
                // project: home the session by its recorded cwd, minting one.
                if self.is_rail_slug(&info.project_slug) {
                    return None;
                }
                let cwd = cli.as_ref().and_then(|c| {
                    self.store
                        .lock()
                        .ok()
                        .and_then(|s| s.hosted_session_of(c))
                        .map(|(_, _, cwd, _)| std::path::PathBuf::from(cwd))
                })?;
                cwd.is_dir().then(|| self.ensure_home_for(&cwd))
            });
            let Some(dest) = dest else { continue };
            self.host.rebind(info.id, &dest);
            if let Some(c) = &cli {
                if let Ok(store) = self.store.lock() {
                    let _ = store.rebind_session(c, &dest);
                    // a stale override is corrected in place: the user's move
                    // stands, under the project's current identity.
                    if ov.as_deref().is_some_and(|o| o != dest) {
                        let _ = store.set_override(c, &dest);
                    }
                }
                if ov.is_some() {
                    self.overrides.insert(c.clone(), dest.clone());
                }
            }
            moved += 1;
        }
        // Closed rows: a crashed or finished session bound to a twin key would
        // be restored under it (and its history filed there) next launch. Only
        // TRANSLATE here — never mint a project for a session that is not
        // running, or every dead twin would come back as a rail row.
        let rows = self
            .store
            .lock()
            .ok()
            .and_then(|s| s.hosted_session_keys().ok())
            .unwrap_or_default();
        for (cli, key) in rows {
            if handled.contains(&cli) {
                continue;
            }
            let ov = self.overrides.get(&cli).cloned();
            let Some(dest) = desired_home(
                &key,
                ov.as_deref(),
                |k| self.is_rail_slug(k),
                |k| self.current_home_key(k),
            ) else {
                continue;
            };
            if let Ok(store) = self.store.lock() {
                let _ = store.rebind_session(&cli, &dest);
                if ov.as_deref().is_some_and(|o| o != dest) {
                    let _ = store.set_override(&cli, &dest);
                }
            }
            if ov.is_some() {
                self.overrides.insert(cli, dest);
            }
            moved += 1;
        }
        // History written before a rebind carried it: pull every session's
        // rows onto its current key — guarded to keys the rail has.
        let slugs: Vec<String> = self.projects.iter().map(|p| p.slug.clone()).collect();
        if let Ok(store) = self.store.lock() {
            let _ = store.reconcile_session_history(&slugs);
        }
        moved
    }
}

#[cfg(test)]
mod tests {
    // Selective imports (crate-wide pattern; a `use super::*` re-globs `crate::*`
    // and trips the crate recursion limit — see kickoff.rs's mod tests).
    use super::{desired_home, translate_stale_key};
    use std::path::Path;

    fn rail<'a>(slugs: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |k| slugs.contains(&k)
    }

    #[test]
    fn a_current_slug_passes_through_without_a_probe() {
        let t = translate_stale_key("github:acme/alpha", rail(&["github:acme/alpha"]), |_| {
            panic!("a live slug must not cost a git probe")
        });
        assert_eq!(t.as_deref(), Some("github:acme/alpha"));
    }

    #[test]
    fn a_path_twin_of_a_repo_translates_to_the_repo_key() {
        let t = translate_stale_key(
            "path:/Users/me/local/alpha",
            rail(&["github:acme/alpha"]),
            |d| {
                assert_eq!(d, Path::new("/Users/me/local/alpha"));
                Some("github:acme/alpha".to_string())
            },
        );
        assert_eq!(t.as_deref(), Some("github:acme/alpha"));
    }

    #[test]
    fn never_invents_a_project_the_rail_lacks() {
        // resolves to a key the rail doesn't have → None (the caller mints).
        assert_eq!(
            translate_stale_key("path:/x", rail(&[]), |_| Some("github:acme/x".to_string())),
            None
        );
        // the directory is gone → None.
        assert_eq!(
            translate_stale_key("path:/gone", rail(&["github:acme/gone"]), |_| None),
            None
        );
        // a dead key that isn't `path:` (an idea:) carries no directory → None.
        assert_eq!(
            translate_stale_key("idea:old", rail(&[]), |_| Some("github:acme/old".to_string())),
            None
        );
    }

    #[test]
    fn the_users_move_wins_and_is_translated() {
        let is = rail(&["github:acme/kod", "path:/Users/me/local/teams"]);
        let tr = |k: &str| match k {
            "path:/Users/me/local/orchestrator" => Some("github:acme/kod".to_string()),
            "path:/Users/me/local/teams" => Some(k.to_string()),
            _ => None,
        };
        // visible under teams, but the user moved it to orchestrator — recorded
        // under orchestrator's OLD key.
        assert_eq!(
            desired_home(
                "path:/Users/me/local/teams",
                Some("path:/Users/me/local/orchestrator"),
                &is,
                &tr
            )
            .as_deref(),
            Some("github:acme/kod")
        );
        // already there → nothing to do.
        assert_eq!(
            desired_home("github:acme/kod", Some("path:/Users/me/local/orchestrator"), &is, &tr),
            None
        );
        // an override that translates to nothing does not strand the session:
        // it falls through to the slug rule.
        assert_eq!(
            desired_home("path:/Users/me/local/teams", Some("idea:gone"), &is, &tr),
            None
        );
    }

    #[test]
    fn a_visible_session_without_an_override_stays_put() {
        let is = rail(&["github:acme/alpha"]);
        assert_eq!(
            desired_home("github:acme/alpha", None, &is, |_| panic!("no translation needed")),
            None
        );
    }

    #[test]
    fn an_orphan_goes_where_its_dead_key_resolves() {
        let is = rail(&["github:t/hyatt"]);
        let tr = |k: &str| (k == "path:/Users/me/local/hyatt").then(|| "github:t/hyatt".to_string());
        assert_eq!(
            desired_home("path:/Users/me/local/hyatt", None, &is, &tr).as_deref(),
            Some("github:t/hyatt")
        );
        // untranslatable → None; the caller mints a home from the cwd.
        assert_eq!(desired_home("path:/Users/me/local/gone", None, &is, &tr), None);
    }
}
