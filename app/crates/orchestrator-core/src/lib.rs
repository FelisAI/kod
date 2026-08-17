//! orchestrator-core — the typed product model the GUI renders, plus a live
//! scan of the user's real projects from local agent session dirs.
//!
//! The organizing unit is the PROJECT and its anatomy (a tree of Parts), each
//! carrying live STATUS. No "thesis"; structure + status IS the state.

pub mod recap;
pub mod registry;
pub mod scan;

use std::path::PathBuf;
use std::sync::RwLock;

/// The user's PROJECTS FOLDER (#29) — where a NEW project's own directory is
/// created. It was hardcoded as `~/local`; now it is ONE setting, mirrored here
/// from the store at launch so the pure/live layers below (which cannot read the
/// store) keep agreeing on it. Unset → `~/local`, exactly the old behavior.
static PROJECTS_ROOT: RwLock<Option<PathBuf>> = RwLock::new(None);

/// `~/local` — the default when the setting was never chosen.
pub fn default_projects_root() -> PathBuf {
    scan::home().join("local")
}

/// The configured projects folder (or the default). WHERE NEW DIRECTORIES ARE
/// CREATED — not, on its own, what the resolver admits (see `project_roots`).
pub fn projects_root() -> PathBuf {
    PROJECTS_ROOT
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(default_projects_root)
}

/// Every root the resolver admits tier-3 path rows under (and Recover trusts a
/// session's cwd under): the configured projects folder AND the default
/// (`~/local`). ADDITIVE on purpose — retargeting where NEW projects land must
/// never UNMOUNT the projects that already exist. Picking `~/code` used to drop
/// every non-git project under `~/local` from the rail and empty Recover of every
/// session ever run there; discovery is a union, creation is a single root.
pub fn project_roots() -> Vec<PathBuf> {
    let mut v = vec![projects_root()];
    let d = default_projects_root();
    if !v.contains(&d) {
        v.push(d);
    }
    v
}

/// Point the scan/fold at the user's chosen projects folder (GUI: on launch
/// and whenever Settings changes it).
pub fn set_projects_root(root: PathBuf) {
    *PROJECTS_ROOT.write().unwrap_or_else(|e| e.into_inner()) = Some(root);
}

/// A project row the STORE knows about, fed back into the scan. `path: None` is
/// a genuine idea (no code yet); `Some(dir)` is a project that owns a directory
/// (created by "＋ new project", or an idea promoted on its first spawn).
#[derive(Clone, Debug)]
pub struct StoreProject {
    pub key: String,
    pub name: String,
    pub path: Option<PathBuf>,
}

/// A project's top-line state. `color()` is the ONLY accessor the GUI renders
/// (Recover's "file under:" dot) — parts on the map/outline carry the store's
/// own `Lifecycle` with its own glyphs, so a glyph/label pair here would just be
/// a second vocabulary for the same states, free to drift out of sync.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Done,
    Building,
    Todo,
    NeedsYou,
    Blocked,
    /// a path-less store row (`idea:<slug>`) — named before any code exists, so
    /// there is no activity to derive a status from.
    Idea,
}

impl Status {
    /// Focused-Dark color (0xRRGGBB). The bright accent stays reserved for the
    /// recommended action, so states themselves are calm.
    pub fn color(self) -> u32 {
        match self {
            Status::Done => 0x5BB99B,
            Status::Building => 0xE6C07A,
            Status::Todo => 0x7E8694,
            Status::NeedsYou => 0xE6C07A,
            Status::Blocked => 0xE68A8A,
            Status::Idea => 0x5C636F,
        }
    }
    fn rank(self) -> u8 {
        match self {
            Status::NeedsYou => 5,
            Status::Blocked => 4,
            Status::Building => 3,
            Status::Todo => 2,
            Status::Idea => 1,
            Status::Done => 0,
        }
    }
}

/// A part of the product: an area, feature, or task (a tree).
#[derive(Clone, Debug)]
pub struct Part {
    pub name: String,
    pub status: Status,
    pub detail: String,
    pub agent: Option<String>,
    pub progress: f32,
    pub children: Vec<Part>,
}

impl Part {
    pub fn new(name: &str, status: Status, detail: &str) -> Self {
        Part {
            name: name.into(),
            status,
            detail: detail.into(),
            agent: None,
            progress: 0.0,
            children: Vec::new(),
        }
    }
    pub fn agent(mut self, a: &str) -> Self {
        self.agent = Some(a.into());
        self
    }
    pub fn progress(mut self, p: f32) -> Self {
        self.progress = p;
        self
    }
    pub fn children(mut self, c: Vec<Part>) -> Self {
        self.children = c;
        self
    }
}

#[derive(Clone, Debug)]
pub struct Project {
    pub slug: String,
    pub name: String,
    pub status: Status,
    pub parts: Vec<Part>,
    /// Canonical working dir where sessions for this project spawn. `None`
    /// when we only know the project from a seed and not a real path yet.
    pub path: Option<std::path::PathBuf>,
    // --- real Standup fields (M4, from the registry) ---
    pub did: Option<String>,
    pub claude_sessions: u32,
    pub codex_sessions: u32,
    pub last_activity_secs: u64,
}

impl Project {
    pub fn overall_progress(&self) -> f32 {
        if self.parts.is_empty() {
            return 0.0;
        }
        let done = self.parts.iter().filter(|p| p.status == Status::Done).count();
        done as f32 / self.parts.len() as f32
    }
    pub fn count(&self, s: Status) -> usize {
        self.parts.iter().filter(|p| p.status == s).count()
    }
    /// The first part (if any) that wants the user's attention.
    pub fn needs_you(&self) -> Option<&Part> {
        self.parts.iter().find(|p| p.status == Status::NeedsYou || p.status == Status::Blocked)
    }

    /// An ad-hoc home for a recovered session whose cwd maps to no registered
    /// project (orphan import). The cwd is FOLDED to its project dir first
    /// (`<root>/<first-component>`) so the slug equals the registry's non-git
    /// `canonical_key` even for a NESTED cwd — a later real scan then FOLDS onto
    /// the same project rather than duplicating it.
    pub fn adhoc(path: std::path::PathBuf) -> Project {
        let path = registry::fold_to_project_dir(&path);
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Project {
            slug: format!("path:{}", path.display()),
            name: name.clone(),
            status: Status::Building,
            parts: Vec::new(),
            path: Some(path),
            did: None,
            claude_sessions: 0,
            codex_sessions: 0,
            last_activity_secs: registry::now_secs(),
        }
    }

    /// A project that OWNS A DIRECTORY (#29) — "＋ new project", and the shape an
    /// idea takes when it's promoted on its first spawn. The OPTIMISTIC insert
    /// right after `store.ensure_project` + `set_project_path`: the next scan
    /// re-derives an identical row from the `Explicit` store source, so the slug
    /// here must equal `resolve()`'s key for that dir — pinned by
    /// `at_dir_matches_resolver_path_row_shape`.
    ///
    /// `key` MUST come from `scan::key_for_dir(&path)` (the resolver's own rule:
    /// `path:<dir>`, or the git key when the dir is a repo). That is the whole
    /// anti-duplicate invariant: the project is born with the key its own
    /// sessions will produce, so they land in ONE registry group.
    pub fn at_dir(key: &str, path: std::path::PathBuf, name: &str) -> Project {
        let now = registry::now_secs();
        project_from_row(
            registry::ProjectRow {
                canonical_key: key.to_string(),
                display_name: name.to_string(),
                primary_path: Some(path),
                is_git: false,
                source_refs: Vec::new(),
                session_count: 0,
                last_activity_secs: now,
                origin: registry::RowOrigin::Explicit,
                did: None,
                claude_sessions: 0,
                codex_sessions: 0,
            },
            now,
        )
    }

    /// A store-backed idea project: a project the user named before any code
    /// exists, so it carries no path until its first spawn makes it a directory.
    /// Used for the OPTIMISTIC insert right after `store.ensure_project(key,
    /// name)` — the next scan re-derives an identical row via `ScanSource::idea`
    /// injection, so slug/shape here must match `resolve()`'s Store rows.
    pub fn idea(key: &str, name: &str) -> Project {
        Project {
            slug: key.to_string(),
            name: name.to_string(),
            status: Status::Idea,
            parts: Vec::new(),
            path: None,
            did: None,
            claude_sessions: 0,
            codex_sessions: 0,
            last_activity_secs: 0,
        }
    }
}

// ---- status derivation ----

fn derive_status(parts: &[Part], mins: u64) -> Status {
    if let Some(worst) = parts.iter().map(|p| p.status).max_by_key(|s| s.rank()) {
        if worst.rank() > Status::Done.rank() {
            return worst;
        }
    }
    if mins < 60 {
        Status::Building
    } else if mins < 1440 {
        Status::Todo
    } else {
        Status::Idea
    }
}

/// The REAL portfolio (M4): the identity resolver over the user's Claude+Codex
/// sessions → deduped projects, each with real activity + DID +
/// seeded structure where we have it. Blocking + I/O-heavy — run off the UI
/// thread. Falls back to the seed if the scan finds nothing (offline/fresh).
pub fn live_projects() -> Vec<Project> {
    live_projects_with_store(&[])
}

/// `live_projects` + the store's own project rows. Core can't read the store
/// (dep direction is store→core), so the GUI passes them in and we inject them
/// as scan sources before resolving:
///
/// * `path: None` (an idea, no code yet) → `ScanSource::idea` → a path-less
///   `RowOrigin::Store` row, sorted after every path project. Unchanged.
/// * `path: Some(dir)` (#29 — the project owns a directory) → an `Explicit`
///   source AT that dir, which keys `path:<dir>` (or the git key) — the SAME key
///   any session in that dir produces. The two sources therefore fold into ONE
///   group, so the folder the user's agent works in can never mint a second,
///   separate rail row for the project they just created.
pub fn live_projects_with_store(rows: &[StoreProject]) -> Vec<Project> {
    let mut snap = scan::build_snapshot();
    for r in rows {
        snap.sources.push(match &r.path {
            Some(p) => scan::store_path_source(&r.key, &r.name, p),
            None => registry::ScanSource::idea(&r.key, &r.name),
        });
    }
    let reg = registry::resolve(&snap);
    if reg.rows.is_empty() {
        return seed_projects();
    }
    let now = registry::now_secs();
    reg.rows.into_iter().map(|r| project_from_row(r, now)).collect()
}

/// Pure per-row mapping (unit-testable): registry row → renderable Project.
fn project_from_row(r: registry::ProjectRow, now: u64) -> Project {
    // a store-backed idea row IS the Idea status — no activity to derive from.
    let status = if r.origin == registry::RowOrigin::Store {
        Status::Idea
    } else {
        // top-level status is ACTIVITY-derived only — the seeded parts'
        // mock NeedsYou must not masquerade as real needs-you on the
        // Standup (real needs-you comes from a live hosted decision).
        derive_status(&[], now.saturating_sub(r.last_activity_secs) / 60)
    };
    Project {
        // slug = the STABLE canonical key (session binding + selection
        // survive a rescan); name = the human label.
        slug: r.canonical_key,
        name: r.display_name,
        status,
        parts: Vec::new(),
        path: r.primary_path,
        did: r.did,
        claude_sessions: r.claude_sessions,
        codex_sessions: r.codex_sessions,
        last_activity_secs: r.last_activity_secs,
    }
}

/// Kept for the offline fallback path.
pub fn projects() -> Vec<Project> {
    live_projects()
}

/// Fallback portfolio when no live projects are found (offline / fresh machine).
///
/// A brand-new user with no Claude/Codex history starts EMPTY and uses
/// New project / Open folder to add their first project — so there is no
/// canned portfolio to show a stranger on first launch.
pub fn seed_projects() -> Vec<Project> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adhoc_slug_matches_registry_canonical_key() {
        // a path OUTSIDE ~/local isn't folded → slug is the raw path: key.
        let p = Project::adhoc(std::path::PathBuf::from("/Users/me/local/orphan"));
        assert_eq!(p.slug, "path:/Users/me/local/orphan");
        assert_eq!(p.name, "orphan");
        assert_eq!(p.path, Some(std::path::PathBuf::from("/Users/me/local/orphan")));
    }

    fn row(key: &str, name: &str, origin: registry::RowOrigin, path: Option<&str>, last: u64) -> registry::ProjectRow {
        registry::ProjectRow {
            canonical_key: key.into(),
            display_name: name.into(),
            primary_path: path.map(std::path::PathBuf::from),
            is_git: false,
            source_refs: vec![],
            session_count: 0,
            last_activity_secs: last,
            origin,
            did: None,
            claude_sessions: 0,
            codex_sessions: 0,
        }
    }

    #[test]
    fn store_row_maps_to_pathless_idea_project() {
        let now = registry::now_secs();
        let p = project_from_row(row("idea:alpha", "Alpha Idea", registry::RowOrigin::Store, None, 0), now);
        assert_eq!(p.slug, "idea:alpha");
        assert_eq!(p.name, "Alpha Idea");
        assert_eq!(p.status, Status::Idea);
        assert_eq!(p.path, None);
        assert!(p.parts.is_empty(), "an idea named like a seeded project must not inherit its mock parts");
    }

    #[test]
    fn path_row_maps_with_activity_status() {
        let now = registry::now_secs();
        let p = project_from_row(row("path:/x/y", "y", registry::RowOrigin::Sessions, Some("/x/y"), now), now);
        assert_eq!(p.path, Some(std::path::PathBuf::from("/x/y")));
        assert_eq!(p.status, Status::Building); // active <60m ago
    }

    #[test]
    fn idea_constructor_matches_resolver_store_row_shape() {
        // the optimistic insert and the next rescan must agree, or the row
        // would flicker/duplicate on adopt_projects.
        let key = registry::idea_key("Alpha Idea");
        let optimistic = Project::idea(&key, "Alpha Idea");
        let rescanned = project_from_row(row(&key, "Alpha Idea", registry::RowOrigin::Store, None, 0), registry::now_secs());
        assert_eq!(optimistic.slug, rescanned.slug);
        assert_eq!(optimistic.name, rescanned.name);
        assert_eq!(optimistic.status, rescanned.status);
        assert_eq!(optimistic.path, rescanned.path);
        assert_eq!(optimistic.last_activity_secs, rescanned.last_activity_secs);
    }

    #[test]
    fn at_dir_matches_resolver_path_row_shape() {
        // #29: the optimistic insert for a NEW project (its own directory) and
        // the row the next rescan derives from its Explicit store source must
        // agree — slug, name, path — or the rail would flicker or DOUBLE.
        let dir = std::path::PathBuf::from("/nowhere/local/my-app");
        let key = format!("path:{}", dir.display()); // what key_for_dir gives a non-git dir
        let optimistic = Project::at_dir(&key, dir.clone(), "My App");
        let now = registry::now_secs();
        let rescanned = project_from_row(
            row(
                &key,
                "My App", // the resolver prefers the store source's display_name
                registry::RowOrigin::Explicit,
                Some(&dir.display().to_string()),
                now,
            ),
            now,
        );
        assert_eq!(optimistic.slug, rescanned.slug);
        assert_eq!(optimistic.name, rescanned.name);
        assert_eq!(optimistic.path, rescanned.path);
        assert_eq!(optimistic.status, rescanned.status);
        assert!(!optimistic.slug.starts_with("idea:"), "a project with a dir is path-keyed, never an idea");

        // and for an existing GIT repo folder the key is the git one — the same
        // key its sessions produce, so adoption can't orphan the project's data.
        let g = Project::at_dir("github:acme/my-app", dir.clone(), "My App");
        assert_eq!(g.slug, "github:acme/my-app");
        assert_eq!(g.path, Some(dir));
    }

    #[test]
    fn key_for_dir_matches_the_resolver_for_a_plain_directory() {
        // a non-git dir under the projects root keys exactly like a session
        // there would (registry: non_git_project_gate).
        let dir = default_projects_root().join("brand-new");
        assert_eq!(
            scan::key_for_dir(&dir),
            format!("path:{}", dir.display()),
            "a fresh project dir must key like its own future sessions"
        );
    }

    /// The ONE test that touches the PROJECTS_ROOT global (it's process-wide, so
    /// the unset-default and the configured cases must not race each other in
    /// separate tests — they live here, in order).
    #[test]
    fn projects_root_defaults_to_home_local_and_roots_stay_additive() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(default_projects_root(), std::path::PathBuf::from(format!("{home}/local")));
        // unset → the default (the GUI mirrors the setting in at launch).
        assert_eq!(projects_root(), default_projects_root());
        assert_eq!(project_roots(), vec![default_projects_root()], "no duplicate root");

        // findings 7/9: retargeting where NEW projects land must not UNMOUNT the
        // projects that already exist. `projects_root` (creation) moves; the roots
        // the resolver and Recover admit (discovery) are the UNION.
        let code = std::path::PathBuf::from(format!("{home}/code"));
        set_projects_root(code.clone());
        assert_eq!(projects_root(), code, "new directories land in the chosen folder");
        assert_eq!(
            project_roots(),
            vec![code, default_projects_root()],
            "the default root must survive a retarget, or every non-git project under ~/local vanishes"
        );
        // restore, so nothing downstream in this process sees a moved root.
        set_projects_root(default_projects_root());
    }

    #[test]
    fn adhoc_folds_nested_cwd_under_home_local() {
        // a NESTED cwd under ~/local must FOLD to <root>/<first-component> so the
        // slug equals the registry canonical_key and a later scan folds, not dups.
        let home = std::env::var("HOME").unwrap();
        let nested = std::path::PathBuf::from(&home).join("local/epsilon/apps/api");
        let p = Project::adhoc(nested);
        assert_eq!(p.slug, format!("path:{home}/local/epsilon"));
        assert_eq!(p.name, "epsilon");
        // folding is idempotent: an already-folded path stays put.
        let folded = std::path::PathBuf::from(&home).join("local/epsilon");
        assert_eq!(Project::adhoc(folded.clone()).slug, format!("path:{home}/local/epsilon"));
    }
}
