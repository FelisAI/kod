//! Where a CLI keeps its state — the ONE answer, for every account.
//!
//! A session runs under an account: the ambient one (`~/.claude`, `~/.codex`),
//! or a profile's own config dir, exported as the variable that CLI reads
//! (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`). Everything Kod reads off disk about a
//! session — its transcript, its rollout, its limit telemetry, the projects it
//! ran in — lives under that account's root. Until 2026-09-14 that root was
//! re-derived at each call site, mostly as a hardcoded `~/.claude` / `~/.codex`,
//! with a codex-only "extra roots" patch in the GUI: so a profiled codex
//! session got no limit detection or auto-continue (the daemon polled
//! `~/.codex`), a profiled claude session could not be recovered, summarised,
//! backfilled or discovered at all, and a profiled codex rollout was classed as
//! a claude transcript because its path lacked the substring `/.codex/`.
//!
//! So every lookup takes `&[CliHome]`, and there are exactly three ways to get
//! one: [`CliHome::ambient`], [`CliHome::for_config_dir`] (a profile), and
//! [`CliHome::from_env`] (read back out of a spawn's environment — the daemon,
//! which has no store, learns a session's account this way). [`cli_homes`] is
//! the one enumeration of "every account on this machine".
//!
//! This crate is a leaf linked by BOTH processes (host → core, gui → core), so
//! the daemon and the GUI cannot disagree about where a session's files are.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The CLI a session runs. Lives HERE (host re-exports it) so the one resolver
/// below can name it — and so the daemon's limit poller, the GUI's Recover and
/// the registry scan share a single definition of claude-vs-codex.
///
/// SERIALIZED ON THE DAEMON AND BRIDGE WIRES: moved from `host::session`
/// without changing a variant, its name or its order — the `PROTOCOL_HASH`
/// tripwire pins that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CliKind {
    Claude,
    Codex,
    Shell,
}

impl CliKind {
    pub fn label(self) -> &'static str {
        match self {
            CliKind::Claude => "claude",
            CliKind::Codex => "codex",
            CliKind::Shell => "shell",
        }
    }

    /// The environment variable that puts this CLI under a given account.
    /// THE one mapping: spawning under a profile writes it, and the daemon reads
    /// it back to find the session's files. A shell has no account.
    pub const fn home_env_var(self) -> Option<&'static str> {
        match self {
            CliKind::Claude => Some("CLAUDE_CONFIG_DIR"),
            CliKind::Codex => Some("CODEX_HOME"),
            CliKind::Shell => None,
        }
    }

    /// The account root when the variable is unset, relative to HOME.
    const fn default_home(self) -> Option<&'static str> {
        match self {
            CliKind::Claude => Some(".claude"),
            CliKind::Codex => Some(".codex"),
            CliKind::Shell => None,
        }
    }

    /// Which CLI wrote a transcript, from the CLI's OWN file naming — codex
    /// names every rollout `rollout-<ts>-<id>.jsonl`, claude names a transcript
    /// `<session-id>.jsonl`. Intrinsic to the file, so it cannot be fooled by
    /// where the account lives (the `/.codex/` substring test it replaces
    /// classed `~/.codex-team/sessions/…` as claude) and needs no list of homes
    /// (a profile deleted since the row was written still classifies).
    pub fn of_transcript(path: &Path) -> CliKind {
        let rollout = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("rollout-"));
        if rollout {
            CliKind::Codex
        } else {
            CliKind::Claude
        }
    }
}

/// One account's state root for one CLI.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CliHome {
    kind: CliKind,
    root: PathBuf,
}

impl CliHome {
    /// The account a session gets with no profile: the CLI's variable as THIS
    /// process sees it (a spawned CLI inherits it, so that is where it writes),
    /// else `~/.claude` / `~/.codex`. `None` for a shell, or with no HOME.
    pub fn ambient(kind: CliKind) -> Option<CliHome> {
        let var = kind.home_env_var()?;
        if let Some(dir) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            return Some(CliHome { kind, root: PathBuf::from(dir) });
        }
        let home = crate::scan::home();
        if home.as_os_str().is_empty() {
            return None;
        }
        Some(CliHome { kind, root: home.join(kind.default_home()?) })
    }

    /// A profile's account: its config dir, or the ambient account when the
    /// profile does not isolate one (blank dir).
    pub fn for_config_dir(kind: CliKind, dir: Option<&str>) -> Option<CliHome> {
        match dir.map(str::trim).filter(|d| !d.is_empty()) {
            Some(d) if kind.home_env_var().is_some() => {
                Some(CliHome { kind, root: PathBuf::from(d) })
            }
            _ => CliHome::ambient(kind),
        }
    }

    /// The account a spawn will run under, read back out of its environment.
    /// The LAST assignment wins, because that is the one the child process sees
    /// (`SpawnSpec.env` is applied in order). Unset or blank → ambient.
    pub fn from_env(kind: CliKind, env: &[(String, String)]) -> Option<CliHome> {
        let var = kind.home_env_var()?;
        let dir = env.iter().rev().find(|(k, _)| k == var).map(|(_, v)| v.as_str());
        CliHome::for_config_dir(kind, dir)
    }

    pub fn kind(&self) -> CliKind {
        self.kind
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where this account keeps session transcripts — the one statement of each
    /// CLI's layout: `projects/<cwd-encoded>/<id>.jsonl` for claude,
    /// `sessions/YYYY/MM/DD/rollout-<ts>-<id>.jsonl` for codex.
    pub fn transcripts_dir(&self) -> PathBuf {
        match self.kind {
            CliKind::Codex => self.root.join("sessions"),
            _ => self.root.join("projects"),
        }
    }

    /// Find one session's transcript in THIS account, by the id in its filename.
    pub fn transcript_path(&self, session_id: &str) -> Option<PathBuf> {
        if session_id.is_empty() {
            return None;
        }
        // codex date-folders three deep under sessions/; claude one under projects/.
        let depth = if self.kind == CliKind::Codex { 5 } else { 3 };
        crate::scan::find_transcript(&self.transcripts_dir(), session_id, depth)
    }
}

/// Every account on this machine, for every CLI with one: the two ambient homes
/// first, then each profile's — deduped, so a profile that points at the
/// ambient dir is searched once. THE enumeration every whole-machine lookup
/// (Recover, project discovery, transcript resolution) goes through, so adding
/// an account can never again reach one feature and miss the next.
///
/// `profiles` is `(cli kind, config dir)` — the store's profile rows; this
/// crate has no store.
pub fn cli_homes<'a>(profiles: impl IntoIterator<Item = (CliKind, Option<&'a str>)>) -> Vec<CliHome> {
    let mut out: Vec<CliHome> = [CliKind::Claude, CliKind::Codex]
        .into_iter()
        .filter_map(CliHome::ambient)
        .collect();
    for (kind, dir) in profiles {
        if let Some(h) = CliHome::for_config_dir(kind, dir) {
            if !out.contains(&h) {
                out.push(h);
            }
        }
    }
    out
}

/// A session's transcript, searched across every account of its CLI.
pub fn transcript_path(kind: CliKind, session_id: &str, homes: &[CliHome]) -> Option<PathBuf> {
    homes
        .iter()
        .filter(|h| h.kind == kind)
        .find_map(|h| h.transcript_path(session_id))
}

/// A cli-kind parse of the store's `profile.cli_kind` / `hosted_session.kind`
/// text — the inverse of [`CliKind::label`].
pub fn kind_from_label(label: &str) -> Option<CliKind> {
    [CliKind::Claude, CliKind::Codex, CliKind::Shell]
        .into_iter()
        .find(|k| k.label() == label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "kod-cli-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_env_a_profile_writes_is_the_home_the_daemon_reads_back() {
        // THE invariant this module exists for: spawning under a profile and
        // locating that session's files must be one mapping, not two.
        for kind in [CliKind::Claude, CliKind::Codex] {
            let var = kind.home_env_var().unwrap();
            let env = vec![
                ("TERM".to_string(), "xterm".to_string()),
                (var.to_string(), "/acct/one".to_string()),
            ];
            let h = CliHome::from_env(kind, &env).unwrap();
            assert_eq!(h, CliHome::for_config_dir(kind, Some("/acct/one")).unwrap());
            assert_eq!(h.root(), Path::new("/acct/one"));
            assert_eq!(h.kind(), kind);
        }
        // the OTHER CLI's variable is not this CLI's account.
        let env = vec![("CODEX_HOME".to_string(), "/acct/codex".to_string())];
        assert_eq!(
            CliHome::from_env(CliKind::Claude, &env),
            CliHome::ambient(CliKind::Claude)
        );
    }

    #[test]
    fn the_last_assignment_wins_like_the_child_process_sees_it() {
        let env = vec![
            ("CODEX_HOME".to_string(), "/first".to_string()),
            ("CODEX_HOME".to_string(), "/second".to_string()),
        ];
        assert_eq!(
            CliHome::from_env(CliKind::Codex, &env).unwrap().root(),
            Path::new("/second")
        );
    }

    #[test]
    fn a_blank_profile_dir_is_the_ambient_account_and_a_shell_has_none() {
        assert_eq!(
            CliHome::for_config_dir(CliKind::Codex, Some("  ")),
            CliHome::ambient(CliKind::Codex)
        );
        assert_eq!(CliHome::for_config_dir(CliKind::Codex, None), CliHome::ambient(CliKind::Codex));
        assert_eq!(CliHome::for_config_dir(CliKind::Shell, Some("/x")), None);
        assert_eq!(CliHome::ambient(CliKind::Shell), None);
        assert_eq!(CliKind::Shell.home_env_var(), None);
    }

    #[test]
    fn a_transcript_is_classified_by_its_own_name_not_its_directory() {
        // the exact path shape the old `/.codex/` substring test misclassified.
        let profiled = Path::new("/Users/me/.codex-team/sessions/2026/09/08/rollout-2026-09-08T11-16-09-01a08236.jsonl");
        assert_eq!(CliKind::of_transcript(profiled), CliKind::Codex);
        let ambient = Path::new("/Users/me/.codex/sessions/2026/07/14/rollout-2026-07-14T09-16-40-019f616a.jsonl");
        assert_eq!(CliKind::of_transcript(ambient), CliKind::Codex);
        let claude = Path::new("/Users/me/.claude-work/projects/-Users-me-local-kod/8f0589a5.jsonl");
        assert_eq!(CliKind::of_transcript(claude), CliKind::Claude);
        // a claude project dir that merely CONTAINS the word is still claude.
        let tricky = Path::new("/Users/me/.claude/projects/-Users-me-rollout-tool/abc.jsonl");
        assert_eq!(CliKind::of_transcript(tricky), CliKind::Claude);
    }

    #[test]
    fn cli_homes_lists_every_account_once_ambient_first() {
        let homes = cli_homes([
            (CliKind::Codex, Some("/acct/codex2")),
            (CliKind::Claude, Some("/acct/claude2")),
            (CliKind::Codex, Some("/acct/codex2")), // duplicate profile dir
            (CliKind::Codex, Some("")),             // non-isolating → ambient, already listed
            (CliKind::Shell, Some("/nope")),        // no account
        ]);
        let ambient: Vec<CliHome> = [CliKind::Claude, CliKind::Codex]
            .into_iter()
            .filter_map(CliHome::ambient)
            .collect();
        assert_eq!(&homes[..ambient.len()], &ambient[..], "ambient homes lead");
        let rest: Vec<(CliKind, &Path)> =
            homes[ambient.len()..].iter().map(|h| (h.kind(), h.root())).collect();
        assert_eq!(
            rest,
            vec![
                (CliKind::Codex, Path::new("/acct/codex2")),
                (CliKind::Claude, Path::new("/acct/claude2")),
            ]
        );
    }

    #[test]
    fn a_transcript_is_found_in_whichever_account_holds_it() {
        let codex_acct = tmpdir("codex");
        let claude_acct = tmpdir("claude");
        let day = codex_acct.join("sessions/2026/09/08");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-2026-09-08T11-16-09-01a08236-aaaa.jsonl"), "{}\n").unwrap();
        let proj = claude_acct.join("projects/-Users-me-local-kod");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("5f0589a5-bbbb.jsonl"), "{}\n").unwrap();

        let codex = CliHome::for_config_dir(CliKind::Codex, codex_acct.to_str()).unwrap();
        let claude = CliHome::for_config_dir(CliKind::Claude, claude_acct.to_str()).unwrap();
        let homes = vec![claude.clone(), codex.clone()];

        assert!(transcript_path(CliKind::Codex, "01a08236-aaaa", &homes).is_some());
        assert!(transcript_path(CliKind::Claude, "5f0589a5-bbbb", &homes).is_some());
        // searched by KIND: a codex id is never looked for in a claude account.
        assert!(transcript_path(CliKind::Claude, "01a08236-aaaa", &homes).is_none());
        // and without the account, it is not found — the bug this closes.
        assert!(transcript_path(CliKind::Codex, "01a08236-aaaa", &[claude]).is_none());
        assert!(codex.transcript_path("").is_none());

        let _ = std::fs::remove_dir_all(codex_acct);
        let _ = std::fs::remove_dir_all(claude_acct);
    }

    #[test]
    fn labels_round_trip() {
        for k in [CliKind::Claude, CliKind::Codex, CliKind::Shell] {
            assert_eq!(kind_from_label(k.label()), Some(k));
        }
        assert_eq!(kind_from_label("gemini"), None);
    }
}
