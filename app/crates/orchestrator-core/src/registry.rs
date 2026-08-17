//! The identity resolver (docs/015) — folds the user's Claude + Codex sessions
//! into one deduped registry of real projects.
//!
//! `resolve(&ScanSnapshot) -> Registry` is PURE: a recorded snapshot of
//! per-source facts in, a deduped registry out. All fs/git I/O lives in
//! the live layer (`scan.rs`), so the resolver is unit-testable with RULE ZERO
//! (no live git/fs in tests). Grounded in real data — see docs/015 for the
//! gold-standard registry that is the test oracle.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    Claude,
    Codex,
    Explicit,
    /// store-backed idea project — a project the user named before any code
    /// exists. PATH-LESS by definition: it owns no directory until its first
    /// spawn makes one, at which point `promote_project` re-keys it `path:`/git
    /// and it stops being a Store source. Core can't read the store (dep
    /// direction is store→core), so the GUI injects these into the snapshot
    /// from `project` rows keyed `idea:<slug>`.
    Store,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitFacts {
    pub is_git: bool,
    pub toplevel: Option<PathBuf>,
    pub remote: Option<String>,
    pub common_dir: Option<PathBuf>,
}

/// One recorded discovery source (a Claude session file, a Codex rollout, or
/// an explicit path). The live layer fills these; the resolver only reads them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanSource {
    pub kind: SourceKind,
    pub source_ref: String,
    /// lossy ENCODED Claude dir name (Claude only) — drives de-encode.
    pub claude_dir: Option<String>,
    /// fs-probed de-encode of `claude_dir` — the dir the user ran `claude` in.
    pub launch_root: Option<PathBuf>,
    /// most-frequent `.cwd` in THIS jsonl/rollout (dominant, not first).
    pub dominant_cwd: Option<PathBuf>,
    /// explicit path.
    pub location: Option<PathBuf>,
    pub git: Option<GitFacts>,
    pub session_count: u32,
    /// seconds since the unix epoch (serde-stable; SystemTime isn't).
    pub last_activity_secs: u64,
    /// last assistant message from this session (the DID feed). None for
    /// no-jsonl / explicit sources.
    #[serde(default)]
    pub last_message: Option<String>,
    /// store project key (`idea:<slug>`) — Store only; becomes the row's
    /// canonical key VERBATIM (never re-derived from the name, which the store
    /// may refresh while the key stays fixed).
    #[serde(default)]
    pub store_key: Option<String>,
    /// user-given name — Store only (a path-less row has no basename).
    #[serde(default)]
    pub display_name: Option<String>,
}

impl ScanSource {
    /// A store-backed idea source: no path, no git, no sessions — just the
    /// stable key + the user's name. `key` comes from `idea_key()` at creation.
    pub fn idea(key: &str, name: &str) -> ScanSource {
        ScanSource {
            kind: SourceKind::Store,
            source_ref: format!("store:{key}"),
            claude_dir: None,
            launch_root: None,
            dominant_cwd: None,
            location: None,
            git: None,
            session_count: 0,
            last_activity_secs: 0,
            last_message: None,
            store_key: Some(key.to_string()),
            display_name: Some(name.to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanSnapshot {
    pub captured_at_secs: u64,
    pub home: PathBuf,
    /// roots under which tier-3 path rows are allowed (e.g. `~/local`).
    pub project_roots: Vec<PathBuf>,
    pub sources: Vec<ScanSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RowOrigin {
    Sessions,
    Explicit,
    Mixed,
    /// store-only idea row — exists in the app's store, nowhere on disk yet.
    Store,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRow {
    pub canonical_key: String,
    pub display_name: String,
    /// `None` only for store-origin idea rows (no code on disk yet).
    pub primary_path: Option<PathBuf>,
    pub is_git: bool,
    pub source_refs: Vec<String>,
    pub session_count: u32,
    pub last_activity_secs: u64,
    pub origin: RowOrigin,
    /// the DID line: last assistant message from the most-recent session.
    pub did: Option<String>,
    /// claude / codex session counts (for the activity string).
    pub claude_sessions: u32,
    pub codex_sessions: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CandidateReason {
    NoJsonlDirDecode,
    UnattachedCwd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateRow {
    pub proposed_path: PathBuf,
    pub reason: CandidateReason,
    pub source_refs: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    pub rows: Vec<ProjectRow>,
    pub candidates: Vec<CandidateRow>,
}

// ---- the pure algorithm -------------------------------------------------

/// Normalize a git remote URL to a stable `host:owner/repo` key.
/// `git@github.com:acme/alpha.git` and `https://github.com/acme/alpha` →
/// `github:acme/alpha`.
pub fn normalize_remote(url: &str) -> String {
    let u = url.trim();
    let strip = |s: &str| s.trim_end_matches('/').trim_end_matches(".git").to_string();
    // scp form: git@github.com:owner/repo.git
    if let Some(rest) = u.strip_prefix("git@") {
        if let Some((host, path)) = rest.split_once(':') {
            return format!("{}:{}", short_host(host), strip(path));
        }
    }
    // url form: scheme://[user@]host/owner/repo(.git)
    if let Some(idx) = u.find("://") {
        let after = &u[idx + 3..];
        let after = after.split_once('@').map(|(_, h)| h).unwrap_or(after);
        if let Some((host, path)) = after.split_once('/') {
            return format!("{}:{}", short_host(host), strip(path));
        }
    }
    strip(u)
}

fn short_host(host: &str) -> &str {
    // github.com → github, gitlab.com → gitlab
    host.split('.').next().unwrap_or(host)
}

/// The canonical store key for a user-named idea project: `idea:<slug>`.
/// Slug = lowercase, whitespace→dash, everything non-alphanumeric-or-dash
/// stripped, dashes deduped + edge-trimmed. Disjoint from `path:`/`github:` so
/// idea rows can never collide with scan-derived keys. The key is minted ONCE
/// at creation and stored — later renames refresh the name, never the key.
pub fn idea_key(name: &str) -> String {
    let mut slug = String::new();
    for c in name.to_lowercase().chars() {
        if c.is_alphanumeric() {
            slug.push(c);
        } else if (c.is_whitespace() || c == '-') && !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-'); // spaces/dashes collapse to ONE dash, never leading
        }
        // everything else (emoji, punctuation, '_') is stripped
    }
    if slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("untitled"); // all-symbol name must still key stably
    }
    format!("idea:{slug}")
}

#[derive(Clone)]
enum Key {
    Remote(String),
    Path(PathBuf),
    /// store key carried VERBATIM (`idea:<slug>`) — path-less by definition.
    Store(String),
}

impl Key {
    fn as_string(&self) -> String {
        match self {
            Key::Remote(r) => format!("github:{r}").replacen("github:github:", "github:", 1),
            Key::Path(p) => format!("path:{}", p.display()),
            Key::Store(k) => k.clone(),
        }
    }
    fn remote_str(r: &str) -> Key {
        // r is already a normalized host:owner/repo
        Key::Remote(r.to_string())
    }
}

fn is_under(p: &Path, root: &Path) -> bool {
    p == root || p.starts_with(root)
}

/// Strict path-component ancestor: `root` is a proper prefix of `p` at a
/// component boundary (not equal).
fn is_strict_ancestor(root: &Path, p: &Path) -> bool {
    p != root && p.starts_with(root)
}

/// The canonical working path a source points at (before keying).
fn cand_path(src: &ScanSource) -> Option<PathBuf> {
    src.dominant_cwd
        .clone()
        .or_else(|| src.launch_root.clone())
        .or_else(|| src.location.clone())
}

pub fn resolve(snap: &ScanSnapshot) -> Registry {
    let home = &snap.home;

    // hard filters: a cwd here is never a project (always dropped).
    let hard_filtered = |p: &Path| -> bool {
        p == home.as_path()
            || is_under(p, Path::new("/tmp"))
            || is_under(p, Path::new("/private/tmp"))
            || is_under(p, Path::new("/private/var/folders"))
    };

    // a "container" is a path with ≥2 distinct child projects under it (~/local).
    // We compute distinct children over GIT-TOPLEVEL-collapsed paths so that two
    // subdirs of ONE repo (beta/app/backend + beta/quantconnect) count as the
    // single repo, not two children — otherwise a project root with ≥2 session
    // subdirs would be wrongly treated as a container and its root sessions
    // dropped. A git repo root is never a container.
    let collapse = |p: &Path| -> PathBuf {
        snap.sources
            .iter()
            .find_map(|s| {
                let g = s.git.as_ref()?;
                let t = g.toplevel.as_deref()?;
                if g.is_git && is_under(p, t) {
                    Some(t.to_path_buf())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| p.to_path_buf())
    };
    let all_paths: Vec<PathBuf> = snap.sources.iter().filter_map(cand_path).map(|p| collapse(&p)).collect();
    let is_container = |p: &Path| -> bool {
        let children = all_paths
            .iter()
            .filter(|q| q.as_path() != p && is_strict_ancestor(p, q))
            .filter_map(|q| q.strip_prefix(p).ok().and_then(|r| r.components().next()).map(|c| c.as_os_str().to_owned()))
            .collect::<std::collections::BTreeSet<_>>();
        children.len() >= 2
    };

    // fold a non-git tier-3 path to `project_root / first-component` so every
    // subdir of a project collapses to the project dir (epsilon/apps/api →
    // epsilon) without needing a session launched at exactly that dir.
    let fold_nongit = |p: &Path| -> PathBuf {
        for root in &snap.project_roots {
            if let Ok(rel) = p.strip_prefix(root) {
                if let Some(first) = rel.components().next() {
                    return root.join(first.as_os_str());
                }
            }
        }
        p.to_path_buf()
    };

    let key_of = |src: &ScanSource, eff_path: &Path| -> Key {
        if let Some(g) = &src.git {
            if g.is_git {
                if let Some(r) = &g.remote {
                    return Key::remote_str(&normalize_remote(r));
                }
                if let Some(t) = &g.toplevel {
                    return Key::Path(t.clone()); // git toplevel is final
                }
            }
        }
        // non-git: fold the cwd to the project's first-level dir
        Key::Path(fold_nongit(eff_path))
    };

    struct Resolved<'a> {
        src: &'a ScanSource,
        eff_path: PathBuf,
        key: Key,
        dropped: bool,
    }
    let mut resolved: Vec<Resolved> = Vec::new();
    for src in &snap.sources {
        // store-backed idea sources are path-less by definition: they key on
        // the stored key verbatim and skip every path filter/fold (none of
        // which can apply without a path). A Store source without a key is
        // malformed — skipped, never a row.
        if src.kind == SourceKind::Store {
            if let Some(key) = &src.store_key {
                resolved.push(Resolved { src, eff_path: PathBuf::new(), key: Key::Store(key.clone()), dropped: false });
            }
            continue;
        }
        let base = match cand_path(src) {
            Some(p) => p,
            None => continue,
        };
        // A source is exempt from the container drop when it resolves INTO a git
        // repo (a repo root is never a container) or is user-declared (explicit).
        // This is what lets beta's root sessions survive even though beta has
        // ≥2 session subdirs.
        let in_git_repo = src
            .git
            .as_ref()
            .map(|g| g.is_git && g.toplevel.as_deref().map(|t| is_under(&base, t)).unwrap_or(false))
            .unwrap_or(false);
        let backed = src.kind == SourceKind::Explicit;
        let exempt = in_git_repo || backed;
        let bad = |p: &Path| hard_filtered(p) || (!exempt && is_container(p));

        // dominant-cwd rescue: launch path bad but a real dominant cwd → re-key.
        let eff_path = if bad(&base) {
            match &src.dominant_cwd {
                Some(d) if !bad(d) => d.clone(),
                _ => {
                    resolved.push(Resolved { src, eff_path: base, key: Key::Path(PathBuf::new()), dropped: true });
                    continue;
                }
            }
        } else {
            base
        };

        let key = key_of(src, &eff_path);

        // tier-3 path rows must live under a project root (drops stray scratch
        // cwds like ~/Documents/Codex); git/explicit are exempt.
        if let Key::Path(ref p) = key {
            let under_root = snap.project_roots.iter().any(|r| is_under(p, r));
            let backed = src.kind == SourceKind::Explicit;
            let git_keyed = src.git.as_ref().map(|g| g.is_git).unwrap_or(false);
            if !under_root && !backed && !git_keyed {
                resolved.push(Resolved { src, eff_path, key, dropped: true });
                continue;
            }
        }
        resolved.push(Resolved { src, eff_path, key, dropped: false });
    }

    // group surviving sources by final key.
    let mut groups: BTreeMap<String, Vec<&Resolved>> = BTreeMap::new();
    for r in &resolved {
        if r.dropped {
            continue;
        }
        groups.entry(r.key.as_string()).or_default().push(r);
    }

    // a group that is ONLY a no-jsonl-dir decode (zero sessions, non-git, no
    // explicit) is a "New project?" candidate, not a silent row.
    let mut candidates: Vec<CandidateRow> = Vec::new();

    // emit rows.
    let mut rows: Vec<ProjectRow> = Vec::new();
    for (key, srcs) in groups {
        // orphan no-jsonl decode (no sessions, non-git, no explicit) →
        // candidate, never a silent row (selfhost).
        let only_nojsonl = srcs.iter().all(|r| {
            r.src.kind == SourceKind::Claude && r.src.session_count == 0 && r.src.dominant_cwd.is_none()
        });
        let backed = srcs.iter().any(|r| r.src.kind == SourceKind::Explicit)
            || srcs.iter().any(|r| r.src.git.as_ref().map(|g| g.is_git).unwrap_or(false));
        if only_nojsonl && !backed {
            let proposed = key.strip_prefix("path:").map(PathBuf::from).unwrap_or_default();
            candidates.push(CandidateRow {
                proposed_path: proposed,
                reason: CandidateReason::NoJsonlDirDecode,
                source_refs: srcs.iter().map(|r| r.src.source_ref.clone()).collect(),
            });
            continue;
        }
        // store-backed idea group: path-less by definition. Key is the stored
        // key verbatim; name is the user's, taken from the source (a path-less
        // row has no basename to derive one from).
        if srcs.iter().all(|r| r.src.kind == SourceKind::Store) {
            let display_name = srcs
                .iter()
                .find_map(|r| r.src.display_name.clone())
                .unwrap_or_else(|| key.clone());
            let last_activity_secs = srcs.iter().map(|r| r.src.last_activity_secs).max().unwrap_or(0);
            rows.push(ProjectRow {
                canonical_key: key,
                display_name,
                primary_path: None,
                is_git: false,
                source_refs: srcs.iter().map(|r| r.src.source_ref.clone()).collect(),
                session_count: 0,
                last_activity_secs,
                origin: RowOrigin::Store,
                did: None,
                claude_sessions: 0,
                codex_sessions: 0,
            });
            continue;
        }
        let mut primary_path = srcs
            .iter()
            .find_map(|r| r.src.git.as_ref().and_then(|g| g.toplevel.clone()))
            .unwrap_or_else(|| {
                // shallowest effective path in the group
                srcs.iter().map(|r| r.eff_path.clone()).min_by_key(|p| p.components().count()).unwrap_or_default()
            });
        // a Path key carries the canonical folded path
        if let Some(p) = key.strip_prefix("path:") {
            primary_path = PathBuf::from(p);
        }
        let is_git = srcs.iter().any(|r| r.src.git.as_ref().map(|g| g.is_git).unwrap_or(false));
        let session_count = srcs.iter().map(|r| r.src.session_count).sum();
        let last_activity_secs = srcs.iter().map(|r| r.src.last_activity_secs).max().unwrap_or(0);
        let has_session = srcs.iter().any(|r| matches!(r.src.kind, SourceKind::Claude | SourceKind::Codex));
        let has_explicit = srcs.iter().any(|r| r.src.kind == SourceKind::Explicit);
        let origin = match (has_session, has_explicit) {
            (true, true) => RowOrigin::Mixed,
            (true, false) => RowOrigin::Sessions,
            _ => RowOrigin::Explicit,
        };
        let claude_sessions = srcs.iter().filter(|r| r.src.kind == SourceKind::Claude).map(|r| r.src.session_count).sum();
        let codex_sessions = srcs.iter().filter(|r| r.src.kind == SourceKind::Codex).map(|r| r.src.session_count).sum();
        // DID = last message of the most-recently-active session in the group.
        let did = srcs
            .iter()
            .filter(|r| r.src.last_message.is_some())
            .max_by_key(|r| r.src.last_activity_secs)
            .and_then(|r| r.src.last_message.clone());
        // prefer the group's EXPLICIT display_name (the user's typed project name,
        // carried by store_path_source) over the slugified directory basename —
        // else the first rescan permanently relabels "Cinematic Check-in" to
        // "cinematic-check-in" (and ensure_project then overwrites the stored name).
        let display_name = srcs
            .iter()
            .find_map(|r| r.src.display_name.clone())
            .unwrap_or_else(|| basename(&primary_path));
        rows.push(ProjectRow {
            canonical_key: key,
            display_name,
            primary_path: Some(primary_path),
            is_git,
            source_refs: srcs.iter().map(|r| r.src.source_ref.clone()).collect(),
            session_count,
            last_activity_secs,
            origin,
            did,
            claude_sessions,
            codex_sessions,
        });
    }

    // newest activity first; quiet zero-session rows sink. Store-backed idea
    // rows sort AFTER every path row unconditionally (activity alone won't do
    // it — a zero-session explicit path row also has activity 0 and would
    // interleave), stable by name among themselves.
    rows.sort_by(|a, b| {
        let (sa, sb) = (a.origin == RowOrigin::Store, b.origin == RowOrigin::Store);
        sa.cmp(&sb).then_with(|| {
            if sa {
                a.display_name.cmp(&b.display_name) // both store: name only
            } else {
                b.last_activity_secs.cmp(&a.last_activity_secs).then_with(|| a.display_name.cmp(&b.display_name))
            }
        })
    });
    Registry { rows, candidates }
}

fn basename(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

/// Helper for the live layer / GUI: now in epoch seconds.
pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Fold a non-git cwd to its project dir (`<project_root>/<first-component>`),
/// the same rule `resolve()` uses for a non-git tier-3 path. An ad-hoc home
/// minted from a NESTED orphan cwd (e.g. ~/local/epsilon/apps/api) must use
/// this so its slug equals the canonical_key a later scan emits and folds onto
/// the same project instead of duplicating it.
pub fn fold_to_project_dir(cwd: &Path) -> PathBuf {
    let local = std::env::var("HOME").map(|h| PathBuf::from(h).join("local")).unwrap_or_default();
    if !local.as_os_str().is_empty() {
        if let Ok(rel) = cwd.strip_prefix(&local) {
            if let Some(first) = rel.components().next() {
                return local.join(first.as_os_str());
            }
        }
    }
    cwd.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(kind: SourceKind, refn: &str) -> ScanSource {
        ScanSource {
            kind,
            source_ref: refn.into(),
            claude_dir: None,
            launch_root: None,
            dominant_cwd: None,
            location: None,
            git: None,
            session_count: if matches!(kind, SourceKind::Claude | SourceKind::Codex) { 1 } else { 0 },
            last_activity_secs: 1000,
            last_message: None,
            store_key: None,
            display_name: None,
        }
    }
    fn git_remote(remote: &str, top: &str) -> Option<GitFacts> {
        Some(GitFacts { is_git: true, toplevel: Some(top.into()), remote: Some(remote.into()), common_dir: None })
    }
    fn snap(sources: Vec<ScanSource>) -> ScanSnapshot {
        ScanSnapshot {
            captured_at_secs: 0,
            home: "/Users/me".into(),
            project_roots: vec!["/Users/me/local".into()],
            sources,
        }
    }

    #[test]
    fn normalize_remote_forms() {
        assert_eq!(normalize_remote("git@github.com:acme/alpha.git"), "github:acme/alpha");
        assert_eq!(normalize_remote("https://github.com/acme/alpha"), "github:acme/alpha");
        assert_eq!(normalize_remote("https://github.com/acme/alpha.git"), "github:acme/alpha");
    }

    #[test]
    fn alpha_fold_gate() {
        // claude (1 real + 1 no-jsonl) + codex, one shared remote → ONE row
        let mut a = src(SourceKind::Claude, "claude/alpha/x.jsonl");
        a.dominant_cwd = Some("/Users/me/local/alpha".into());
        a.git = git_remote("git@github.com:acme/alpha.git", "/Users/me/local/alpha");
        let mut b = src(SourceKind::Claude, "claude/alpha-app/(no-jsonl)");
        b.session_count = 0;
        b.launch_root = Some("/Users/me/local/alpha/alpha-app".into());
        b.dominant_cwd = None;
        b.git = git_remote("git@github.com:acme/alpha.git", "/Users/me/local/alpha");
        let mut c = src(SourceKind::Codex, "codex/rollout-1.jsonl");
        c.dominant_cwd = Some("/Users/me/local/alpha".into());
        c.git = git_remote("git@github.com:acme/alpha.git", "/Users/me/local/alpha");

        let reg = resolve(&snap(vec![a, b, c]));
        let alpha: Vec<_> = reg.rows.iter().filter(|r| r.display_name == "alpha").collect();
        assert_eq!(alpha.len(), 1, "alpha must fold to exactly one row, got {:?}", reg.rows.iter().map(|r| &r.display_name).collect::<Vec<_>>());
        assert_eq!(alpha[0].canonical_key, "github:acme/alpha");
        assert_eq!(alpha[0].origin, RowOrigin::Sessions);
    }

    #[test]
    fn no_tmp_row_gate() {
        let mut t = src(SourceKind::Claude, "claude/tmp.jsonl");
        t.dominant_cwd = Some("/private/tmp/claude-501/sigma-1234".into());
        let reg = resolve(&snap(vec![t]));
        assert!(reg.rows.is_empty(), "a /tmp cwd must never produce a row: {:?}", reg.rows);
    }

    #[test]
    fn non_git_project_gate() {
        // teams: non-git, real sessions, under ~/local → tier-3 path row
        let mut a = src(SourceKind::Claude, "claude/teams.jsonl");
        a.dominant_cwd = Some("/Users/me/local/teams".into());
        a.git = Some(GitFacts::default());
        let reg = resolve(&snap(vec![a]));
        assert_eq!(reg.rows.len(), 1);
        assert_eq!(reg.rows[0].display_name, "teams");
        assert_eq!(reg.rows[0].canonical_key, "path:/Users/me/local/teams");
    }

    #[test]
    fn a_retargeted_projects_folder_does_not_unmount_the_default_root() {
        // findings 7/9: the snapshot used to carry ONE root — the CONFIGURED one
        // INSTEAD of the default. Picking ~/code in Settings therefore dropped
        // every non-git project under ~/local (teams included) from the rail
        // entirely: not a row, not even a "New project?" candidate, its map,
        // journal and memory unreachable. The roots are a UNION now: creation uses
        // the configured folder, DISCOVERY admits both.
        let mut teams = src(SourceKind::Claude, "claude/teams.jsonl");
        teams.dominant_cwd = Some("/Users/me/local/teams".into());
        teams.git = Some(GitFacts::default()); // non-git → tier-3, gated on roots
        let mut fresh = src(SourceKind::Claude, "claude/fresh.jsonl");
        fresh.dominant_cwd = Some("/Users/me/code/fresh".into());
        fresh.git = Some(GitFacts::default());
        // a stray cwd under NEITHER root is still dropped — the gate still works.
        let mut stray = src(SourceKind::Claude, "claude/stray.jsonl");
        stray.dominant_cwd = Some("/Users/me/Documents/Codex".into());
        stray.git = Some(GitFacts::default());

        let mut s = snap(vec![teams, fresh, stray]);
        s.project_roots = vec!["/Users/me/code".into(), "/Users/me/local".into()];
        let reg = resolve(&s);
        let keys: Vec<&str> = reg.rows.iter().map(|r| r.canonical_key.as_str()).collect();
        assert!(keys.contains(&"path:/Users/me/local/teams"), "{keys:?}");
        assert!(keys.contains(&"path:/Users/me/code/fresh"), "{keys:?}");
        assert!(
            !keys.iter().any(|k| k.contains("Documents")),
            "the tier-3 gate must still drop stray cwds: {keys:?}"
        );
    }

    #[test]
    fn dominant_cwd_rescue_for_buried_sessions() {
        // a session launched in ~/local (filtered ancestor) but whose dominant
        // cwd is ~/local/sample-spike → re-keys to sample-spike.
        let mut a = src(SourceKind::Claude, "claude/-Users-me-local/x.jsonl");
        a.launch_root = Some("/Users/me/local".into());
        a.dominant_cwd = Some("/Users/me/local/sample-spike".into());
        a.git = Some(GitFacts::default());
        let reg = resolve(&snap(vec![a]));
        assert_eq!(reg.rows.len(), 1);
        assert_eq!(reg.rows[0].display_name, "sample-spike");
    }

    #[test]
    fn ancestor_of_many_is_filtered() {
        // ~/local hosts ≥2 projects → ~/local itself is never a row
        let mk = |cwd: &str, r: &str| {
            let mut s = src(SourceKind::Claude, r);
            s.dominant_cwd = Some(cwd.into());
            s.git = Some(GitFacts::default());
            s
        };
        let bare = mk("/Users/me/local", "bare");
        let m = mk("/Users/me/local/alpha", "m");
        let t = mk("/Users/me/local/teams", "t");
        let reg = resolve(&snap(vec![bare, m, t]));
        assert!(!reg.rows.iter().any(|r| r.primary_path.as_deref() == Some(Path::new("/Users/me/local"))), "~/local must be filtered");
        assert_eq!(reg.rows.len(), 2);
    }

    #[test]
    fn stray_scratch_cwd_dropped() {
        // a codex rollout in ~/Documents/Codex/... (not under a project root,
        // no git/explicit) → dropped, no phantom row.
        let mut s = src(SourceKind::Codex, "codex/stray.jsonl");
        s.dominant_cwd = Some("/Users/me/Documents/Codex/2026-05-21/hi".into());
        let reg = resolve(&snap(vec![s]));
        assert!(reg.rows.is_empty(), "stray non-project cwd must be dropped: {:?}", reg.rows);
    }

    #[test]
    fn idea_key_slugifies() {
        assert_eq!(idea_key("Cinematic Check-in"), "idea:cinematic-check-in");
        // runs of spaces/dashes collapse to ONE dash; edges trimmed
        assert_eq!(idea_key("  --Weird -- Name-- "), "idea:weird-name");
        // non-alnum-non-dash (punctuation, '_') is stripped, not dashed
        assert_eq!(idea_key("snake_case v2.0!"), "idea:snakecase-v20");
        // unicode letters are alphanumeric → kept (lowercased)
        assert_eq!(idea_key("Café Idée"), "idea:café-idée");
        assert_eq!(idea_key("文字 map"), "idea:文字-map");
        // emoji stripped; surrounding spaces never become edge dashes
        assert_eq!(idea_key("🚀 rocket app 🚀"), "idea:rocket-app");
        // all-symbol name still keys stably
        assert_eq!(idea_key("🚀🔥"), "idea:untitled");
        assert_eq!(idea_key(""), "idea:untitled");
    }

    #[test]
    fn store_idea_rows_are_pathless_and_sort_after_path_rows() {
        // real path project with activity …
        let mut m = src(SourceKind::Claude, "claude/alpha.jsonl");
        m.dominant_cwd = Some("/Users/me/local/alpha".into());
        m.git = Some(GitFacts::default());
        // … plus a ZERO-activity explicit path row (the ordering trap: activity
        // alone would interleave it with the ideas) …
        let mut g = src(SourceKind::Explicit, "explicit:gamma");
        g.location = Some("/Users/me/local/gamma".into());
        g.git = git_remote("git@github.com:acme/gamma.git", "/Users/me/local/gamma");
        g.last_activity_secs = 0;
        // … plus two store ideas, injected out of name order.
        let zeta = ScanSource::idea(&idea_key("zeta thing"), "zeta thing");
        let alpha = ScanSource::idea(&idea_key("Alpha Idea"), "Alpha Idea");

        let reg = resolve(&snap(vec![zeta, m, alpha, g]));
        let names: Vec<_> = reg.rows.iter().map(|r| r.display_name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "gamma", "Alpha Idea", "zeta thing"], "ideas AFTER all path rows, stable by name");
        let a = reg.rows.iter().find(|r| r.display_name == "Alpha Idea").unwrap();
        assert_eq!(a.canonical_key, "idea:alpha-idea");
        assert_eq!(a.primary_path, None);
        assert_eq!(a.origin, RowOrigin::Store);
        assert_eq!(a.session_count, 0);
        // every non-store row still carries a path (the Option is Store-only)
        assert!(reg.rows.iter().filter(|r| r.origin != RowOrigin::Store).all(|r| r.primary_path.is_some()));
    }

    #[test]
    fn store_source_without_key_never_rows() {
        let mut bad = ScanSource::idea("idea:x", "x");
        bad.store_key = None; // malformed injection
        let reg = resolve(&snap(vec![bad]));
        assert!(reg.rows.is_empty() && reg.candidates.is_empty(), "keyless store source must be skipped: {:?}", reg.rows);
    }

    #[test]
    fn non_git_subdir_fold() {
        // epsilon (non-git): apps/api sub-cwd folds into epsilon.
        let mk = |cwd: &str, r: &str| {
            let mut s = src(SourceKind::Claude, r);
            s.dominant_cwd = Some(cwd.into());
            s.git = Some(GitFacts::default());
            s
        };
        let root = mk("/Users/me/local/epsilon", "root");
        let api = mk("/Users/me/local/epsilon/apps/api", "api");
        let reg = resolve(&snap(vec![root, api]));
        let hp: Vec<_> = reg.rows.iter().filter(|r| r.display_name == "epsilon").collect();
        assert_eq!(hp.len(), 1, "apps/api must fold into epsilon: {:?}", reg.rows.iter().map(|r| &r.primary_path).collect::<Vec<_>>());
    }
}
