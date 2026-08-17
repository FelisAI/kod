//! A project's OWN DIRECTORY (#29) — the user's complaint: "a new project
//! doesn't have its own directory when I start new session".
//!
//! Two jobs:
//!   * mint a safe directory name from a free-text project name, and create the
//!     directory under the configured projects folder;
//!   * PROMOTE a path-less project (an idea, a pre-scan seed row) to a real
//!     `path:<dir>` identity — moving every slug-keyed row in the store, every
//!     slug-keyed map in RAM, and every live session's host binding with it.
//!
//! Identity rule (the anti-duplicate invariant): a project that owns a directory
//! is keyed `path:<dir>` — the SAME key the registry derives from a session run
//! in that dir. So the folder can never mint a second rail row for the project.
//! `idea:` stays reserved for projects with no directory at all.

use gpui::*;
use crate::*;

/// The directory name for a project called `name` — the idea slug (lowercase,
/// dashes, punctuation stripped) with a length cap.
///
/// This is the ONLY thing standing between a free-text name and the filesystem,
/// so it must be total: `idea_key` keeps only alphanumerics and single interior
/// dashes, which means `/`, `.`, `~` and whitespace are all GONE — `../../etc`
/// slugifies to `etc`, `/tmp/x` to `tmpx`, and an all-symbol name to `untitled`.
/// A traversal or an absolute-path injection is therefore impossible by
/// construction, not by validation.
pub(crate) fn dir_name_for(name: &str) -> String {
    let slug = orchestrator_core::registry::idea_key(name)
        .strip_prefix("idea:")
        .unwrap_or("untitled")
        .to_string();
    // cap the component (HFS+/APFS allow 255 bytes; be conservative and cut on
    // a char boundary so a long pasted title can't blow up create_dir_all).
    let capped: String = slug.chars().take(64).collect();
    let trimmed = capped.trim_matches('-');
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed.to_string()
    }
}

/// The EMPTY-PORTFOLIO SENTINEL — `main.rs::empty_project()`, the row `project()`
/// falls back to when the rail holds NOTHING at all (a fresh OSS user's first
/// launch, no Claude/Codex history, no projects). It is a render placeholder, and
/// it must never reach the filesystem: its name slugifies to the perfectly
/// creatable `no-projects-yet`, so the FIRST keystroke of the FIRST launch (⌘T)
/// used to mint `<projects_root>/no-projects-yet`, a store project literally
/// called "No projects yet", and a session filed under the phantom key "welcome".
///
/// Matched on IDENTITY (the singleton's address) — not "has no path", and not the
/// name. A real path-less idea IS a legitimate spawn target (it promotes onto its
/// own folder), including one the user happened to name "No projects yet".
/// `termgeom::empty_project_sentinel_is_safe` pins the singleton this relies on.
pub(crate) fn is_no_project_sentinel(proj: &orchestrator_core::Project) -> bool {
    std::ptr::eq(proj, crate::empty_project())
}

/// Where a PATH-LESS project's own directory gets made on its first spawn — and
/// `None` for the sentinel, which owns no name the filesystem may ever see. The
/// promotion target is computed HERE and nowhere else, so a new spawn path cannot
/// re-derive it from the name and route around the guard above.
pub(crate) fn promotion_dir_for(
    proj: &orchestrator_core::Project,
    root: &std::path::Path,
) -> Option<std::path::PathBuf> {
    (!is_no_project_sentinel(proj)).then(|| root.join(dir_name_for(&proj.name)))
}

/// What the app says when there is no project AT ALL. `spawn_cwd` answers the
/// keystroke with the ＋New menu instead — an affordance beats a sentence — so
/// this is the wording for the paths that can only put a line on screen.
pub(crate) const NO_PROJECT_YET: &str = "no project yet — add one with ＋New in the sidebar";

/// …and what it says while the FIRST SCAN is still running, which looks exactly
/// the same from `project()`'s side and is not the same thing at all. Compared by
/// VALUE in `adopt_projects`, which clears it the moment the reason expires.
pub(crate) const STILL_SCANNING: &str = "still finding your projects — one moment, then try again";

/// The decision behind `no_project_reason`, pure so the RACE is a test and not a
/// stopwatch: "the sentinel is selected" is true both for a genuinely empty
/// portfolio AND for the first seconds of EVERY launch, because boot seeds the
/// rail empty and the scan fills it from a background thread. Reading only the
/// sentinel refuses a legitimate ⌘T from a user with 30 projects.
pub(crate) fn no_project_reason_for(is_sentinel: bool, scanned: bool) -> Option<&'static str> {
    if !is_sentinel {
        return None;
    }
    Some(if scanned { NO_PROJECT_YET } else { STILL_SCANNING })
}

/// Create (or adopt) `dir`. Never clobbers: an existing directory is reused
/// as-is (we only ever spawn INTO it — the app writes nothing there), and an
/// existing FILE at that path is an error, not something to overwrite.
/// A mkdir failure is returned, never swallowed.
pub(crate) fn create_dir(dir: &std::path::Path) -> Result<(), String> {
    if dir.is_file() {
        return Err(format!("{} is a file, not a folder", dir.display()));
    }
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("couldn't create {} — {e}", dir.display()))
}

/// Is the directory non-empty (an existing folder we'd be ADOPTING, not making)?
pub(crate) fn is_nonempty_dir(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

/// Re-key a slug-keyed map in place (promotion moves the project's identity).
fn remap<V>(m: &mut std::collections::HashMap<String, V>, old: &str, new: &str) {
    if let Some(v) = m.remove(old) {
        m.insert(new.to_string(), v);
    }
}

/// Who already owns the directory we're about to take.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    /// a project the LAST SCAN SAW owns this dir (or its key) — carries its name.
    Scanned(String),
    /// a project row exists under this key that the scan did NOT see.
    Forgotten,
    /// nobody: safe to create/promote onto.
    Free,
}

/// Is `dir`/`key` already spoken for? The one gate in front of every promotion
/// and every "＋ new project" — pure, so the fusion hazard is testable without a
/// gpui App.
///
/// TWO registries, because they disagree and only their UNION is the truth:
///
///   * the RAIL (`projects`) — only what the last scan saw. It catches a scanned
///     project that owns the dir under a DIFFERENT key (a git-keyed row owns its
///     directory too, and a dir nested inside an existing repo resolves to that
///     repo's key with a different path).
///   * the STORE (`key_taken_in_store`) — the owner of record. There is no
///     `DELETE FROM project` anywhere in the app, so a project whose claude
///     transcripts aged out (~30d) or whose folder was deleted is ABSENT FROM
///     THE RAIL while still owning its key, its map, its memory and its notes,
///     forever. The rail check alone would sail straight past it and
///     `rename_project_key` would then re-key 18 tables onto its row — fusing two
///     product maps into one tree and deleting the victim's project row. No
///     prompt, no undo.
///
/// `self_slug` is the project doing the asking: its own row is never a collision
/// (the legitimate self-keyed no-op promotion). Callers must likewise pass
/// `key_taken_in_store = false` when the key IS the caller's own slug.
pub(crate) fn owner_of(
    projects: &[orchestrator_core::Project],
    dir: &std::path::Path,
    key: &str,
    key_taken_in_store: bool,
    self_slug: Option<&str>,
) -> Owner {
    if let Some(other) = projects.iter().find(|p| {
        Some(p.slug.as_str()) != self_slug
            && (p.slug == key || p.path.as_deref() == Some(dir))
    }) {
        return Owner::Scanned(other.name.clone());
    }
    if key_taken_in_store {
        return Owner::Forgotten;
    }
    Owner::Free
}

/// The PATH-LESS project that would land on this very directory — it is not a
/// namesake, it IS this project, and it must be PROMOTED rather than forked.
///
/// Matched on the DIRECTORY, not on `idea_key(name)`: `dir_name_for` caps the
/// slug at 64 chars, so two long names can share a directory without sharing an
/// idea key — and two rows over one directory is the duplicate we're preventing.
pub(crate) fn pathless_owner(
    projects: &[orchestrator_core::Project],
    name: &str,
) -> Option<String> {
    let want = dir_name_for(name);
    projects
        .iter()
        .find(|p| p.path.is_none() && dir_name_for(&p.name) == want)
        .map(|p| p.slug.clone())
}

/// The user-facing name for a folder OPENED IN PLACE (#34): its real
/// basename, verbatim — "epsilon", "Foo Bar" — NOT slugified. Unlike a
/// project we CREATE (whose folder name we mint from a typed title), an opened
/// folder already has a name on disk; the rail should echo it. Falls back to the
/// full path, then "project", so it is total.
pub(crate) fn name_for_dir(dir: &std::path::Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let s = dir.to_string_lossy().into_owned();
            if s.is_empty() { "project".to_string() } else { s }
        })
}

/// What "Open folder…" (#34) should DO with a picked directory — decided PURELY
/// from (the rail, the dir, its resolver key, whether the store already holds
/// that key, and the stored name if so). No gpui, no store, no fs: the
/// anti-duplicate / anti-fusion guarantee is unit-tested without an App.
///
/// It is `owner_of` translated into an action, so the row the user opens and
/// the rows the folder's own sessions produce share ONE key — never two rows.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OpenFolderAction {
    /// the folder is ALREADY a project in the rail → just select it. Graceful
    /// "you already have this open", never an error.
    Select(String),
    /// the folder IS a known-but-dormant project (its store row + map survived a
    /// scan that no longer sees it) → RE-ATTACH its EXISTING key to this path,
    /// under the STORED name. No new row, no `ensure_project`, no rename: only a
    /// `set_project_path`, so the survivor's map/memory/name are untouched.
    Reattach { key: String, name: String },
    /// brand-new folder → create it: `ensure_project(key,name)` +
    /// `set_project_path`, then optimistically insert the path-backed row.
    Create { key: String, name: String },
    /// can't proceed (e.g. the folder vanished, or a key is taken but we can't
    /// read its name) → surface `msg` in the rail.
    Refuse(String),
}

/// See `OpenFolderAction`. `key` MUST be `scan::key_for_dir(dir)` and
/// `key_taken_in_store` MUST be `store.project_exists(key)` — computed by the
/// thin caller, injected here so this stays pure and testable.
pub(crate) fn open_folder_action(
    projects: &[orchestrator_core::Project],
    dir: &std::path::Path,
    key: &str,
    key_taken_in_store: bool,
    stored_name: Option<&str>,
) -> OpenFolderAction {
    // `self_slug = None`: opening a folder is never a self-keyed no-op — there is
    // no "asking" project, so every existing owner (rail OR store) counts.
    match owner_of(projects, dir, key, key_taken_in_store, None) {
        Owner::Scanned(_) => {
            // owner_of matched a rail row by key OR by path — select THAT row.
            let slug = projects
                .iter()
                .find(|p| p.slug == key || p.path.as_deref() == Some(dir))
                .map(|p| p.slug.clone())
                // Scanned ⇒ such a row exists; `key` is the only sane fallback.
                .unwrap_or_else(|| key.to_string());
            OpenFolderAction::Select(slug)
        }
        // the key belongs to a project the last scan never SAW. Blindly creating
        // would `ensure_project`-UPSERT the folder basename over the survivor's
        // name and inherit its map (the HIGH bug #29 fixed). But the folder
        // GENUINELY resolves to this key, so it genuinely IS that project — so
        // re-attach IT (under its own name), never fork or rename. Only if we
        // can't read the stored name do we refuse rather than invent one.
        Owner::Forgotten => match stored_name {
            Some(name) => OpenFolderAction::Reattach {
                key: key.to_string(),
                name: name.to_string(),
            },
            None => OpenFolderAction::Refuse(format!(
                "{} already belongs to another project",
                dir.display()
            )),
        },
        Owner::Free => OpenFolderAction::Create {
            key: key.to_string(),
            name: name_for_dir(dir),
        },
    }
}

impl Orchestrator {
    /// The directory this project's sessions run in — MATERIALIZED if it doesn't
    /// exist yet. The one cwd source for every spawn path (#29 part 3): there is
    /// no `$HOME` fallback anywhere, because a project without a folder gets one
    /// (and a real identity) instead of loosing an agent in the home folder.
    ///
    /// * has a live directory → that.
    /// * has a recorded directory that VANISHED (moved/deleted) → re-create it
    ///   at the same path, so the canonical key (`path:<dir>`) and every map /
    ///   memory row filed under it survive.
    /// * has no directory (an idea — the user's first spawn on it) → create
    ///   `<projects_root>/<slug>` and PROMOTE the project onto it.
    pub(crate) fn ensure_project_dir(&mut self) -> Result<std::path::PathBuf, String> {
        let proj = self.project();
        let (slug, name, path) = (proj.slug.clone(), proj.name.clone(), proj.path.clone());
        // Computed while `proj` is still borrowed: the empty-portfolio sentinel is
        // recognised by IDENTITY, so it can no longer be told apart once the name
        // above has been cloned out of it.
        let minted = promotion_dir_for(proj, &self.projects_root);
        if let Some(p) = path {
            create_dir(&p)?;
            return Ok(p);
        }
        // No project at all (first launch): there is nothing to make a folder FOR.
        // `spawn_cwd` catches this first and shows ＋New; this is the backstop that
        // keeps a future caller of ensure_project_dir off the filesystem.
        let Some(dir) = minted else {
            return Err(NO_PROJECT_YET.to_string());
        };
        // The key the REGISTRY will give this dir — not one of our invention.
        // Fresh folder → `path:<dir>`; a folder that's already a git repo → the
        // git key, i.e. the key its own sessions produce.
        let new_key = orchestrator_core::scan::key_for_dir(&dir);
        // Never merge into someone else's identity: promoting onto a directory
        // that ALREADY belongs to a project would fuse two maps into one tree.
        // (The user's live store holds exactly this pair: the idea `omega` and
        // the real project at ~/local/omega.) Refusing still means no $HOME spawn,
        // which is the thing that must never happen. See `owner_of` for why the
        // rail alone is not enough. `new_key == slug` is the legitimate
        // self-keyed no-op promotion — the project already IS this key, it just
        // never recorded its path — so its own store row must not count.
        let taken = new_key != slug && {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            store.project_exists(&new_key)
        };
        match owner_of(&self.projects, &dir, &new_key, taken, Some(&slug)) {
            Owner::Scanned(other) => {
                return Err(format!(
                    "{} is already the project “{}” — start your session there",
                    dir.display(),
                    other
                ))
            }
            Owner::Forgotten => {
                return Err(format!(
                    "{} already belongs to another project — start your session there",
                    dir.display()
                ))
            }
            Owner::Free => {}
        }
        // adopting (never clobbering) a folder that already has content is
        // legitimate — a repo he cloned yesterday, say — but the agent is about to
        // get write access INSIDE it, with a pre-submitted prompt via "▶ work on
        // this". Say so. Same contract as commit_new_project. Computed BEFORE
        // create_dir, which returns Ok on an existing dir and can't be asked after.
        let adopting = dir.is_dir() && is_nonempty_dir(&dir);
        create_dir(&dir)?;
        self.promote_project(&slug, &new_key, &dir, &name)?;
        if adopting {
            self.term_error = Some(format!("using the existing folder {}", dir.display()));
        }
        Ok(dir)
    }

    /// Move a project onto its own directory: `idea:<slug>` (or a pre-scan seed
    /// key) → `path:<dir>`. Everything the user owns is keyed by that slug,
    /// so all four layers move in lockstep:
    ///   1. the STORE (one transaction, schema-driven — map, journal, notes,
    ///      memory, summaries, sessions, per-project settings, rail order),
    ///   2. the project row itself (+ its recorded path, which is what makes the
    ///      next scan re-derive the SAME `path:` key instead of a fresh idea),
    ///   3. the slug-keyed RAM caches,
    ///   4. any LIVE session's host binding (else it detaches from its own rail
    ///      row mid-flight).
    pub(crate) fn promote_project(
        &mut self,
        old: &str,
        new_key: &str,
        dir: &std::path::Path,
        name: &str,
    ) -> Result<(), String> {
        let promoted = orchestrator_core::Project::at_dir(new_key, dir.to_path_buf(), name);
        let dir_s = dir.to_string_lossy().into_owned();
        {
            let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            if new_key != old {
                // refuses (never merges) if `new_key` is taken — the caller has
                // already checked, this is the last line of defence.
                store
                    .rename_project_key(old, new_key)
                    .map_err(|e| format!("couldn't move this project's data: {e}"))?;
            }
            let _ = store.ensure_project(new_key, name);
            let _ = store.set_project_path(new_key, &dir_s);
        }
        if new_key == old {
            return Ok(());
        }
        if let Some(p) = self.projects.iter_mut().find(|p| p.slug == old) {
            *p = promoted;
        }
        self.rekey_in_memory(old, new_key);
        Ok(())
    }

    /// The IN-MEMORY half of a key move: every slug-keyed RAM cache, the rail
    /// order, the session overrides, the dispatch memo and any LIVE session's
    /// host binding (else it detaches from its own rail row mid-flight).
    ///
    /// Shared by BOTH movers — the idea→path promotion above and the rescan
    /// re-key in `adopt_projects` (a promoted project keys `github:…` the moment
    /// the agent adds a git remote). One copy, so neither can forget a cache.
    pub(crate) fn rekey_in_memory(&mut self, old: &str, new_key: &str) {
        remap(&mut self.active_session, old, new_key);
        remap(&mut self.infos_cache, old, new_key);
        remap(&mut self.outline_open_cache, old, new_key);
        remap(&mut self.map_root_cache, old, new_key);
        for s in self.project_order.iter_mut() {
            if s == old {
                *s = new_key.to_string();
            }
        }
        for v in self.overrides.values_mut() {
            if v == old {
                *v = new_key.to_string();
            }
        }
        // the dispatch-chip memo is keyed by (write_gen, slug) — invalidate it.
        *self.dispatch_memo.borrow_mut() =
            (u64::MAX, String::new(), std::collections::HashMap::new());
        for info in self.host.infos() {
            if info.project_slug == old {
                self.host.rebind(info.id, new_key);
            }
        }
    }

    /// WHY nothing can be written to (or spawned into) a project right now —
    /// `None` = there IS one. Two states look identical through `project()` and
    /// mean opposite things, so every refusal reads them from here rather than
    /// re-deriving "the sentinel means empty" and getting the boot window wrong.
    pub(crate) fn no_project_reason(&self) -> Option<&'static str> {
        no_project_reason_for(is_no_project_sentinel(self.project()), self.scanned)
    }

    /// `ensure_project_dir` for the spawn paths: on failure, surface the reason
    /// in the drawer and DON'T spawn. (Before #29 this was a silent
    /// `unwrap_or($HOME)` — an agent with file-write access in the home folder.)
    ///
    /// The Err arm LANDS THE USER where `term_error` is actually drawn
    /// (render_workspace / render_map). Setting it from Standup, Settings or the
    /// Agent stage used to make ⌘T a completely dead key: no spawn, no message,
    /// nothing — for a name collision AND for a genuine mkdir failure.
    ///
    /// It is also THE CHOKE POINT for the first-run case: every spawn entrance —
    /// ⌘T/⇧⌘T/⌥⌘T, every "+" menu row (plain, per-profile, ambient) and "▶ work on
    /// this" — comes through here, so the empty-portfolio guard is written once
    /// instead of once per call site.
    pub(crate) fn spawn_cwd(&mut self, cx: &mut Context<Self>) -> Option<std::path::PathBuf> {
        // FIRST RUN: the portfolio is empty, so `project()` is the sentinel and
        // there is nothing to spawn INTO. Answer the keystroke with the thing that
        // fixes it — the rail's ＋New menu, which spells out each way to make a
        // project — rather than an error banner: on an empty app "make a project
        // first" is the next step, not a failure. (Before this, ⌘T on first launch
        // silently created <projects_root>/no-projects-yet from the sentinel's own
        // name and filed the session under the phantom key "welcome".)
        //
        // But NOT-YET-SCANNED reads identically here: boot seeds the rail EMPTY
        // (`seed_projects()` is a `Vec::new()`) and only `adopt_projects` fills it,
        // seconds later on a big machine — so for the whole startup window a user
        // with 30 projects also has the sentinel selected. Keying the guard on the
        // sentinel ALONE refused his ⌘T and put "ADD A PROJECT" in his face.
        // `scanned` is the distinguisher: same state, opposite answers.
        if is_no_project_sentinel(self.project()) {
            if self.scanned {
                // the portfolio really is empty → the menu, which IS the fix.
                // The rail draws the ＋New NAME FIELD or the menu, never both: with
                // a field already open IT is the affordance, and opening the menu
                // underneath would just pop out later, when the field closes. The
                // keystroke still gets an answer, ON the field — an unacknowledged
                // ⌘T is the very bug this function's doc says it exists to kill.
                if self.rail_new.is_none() {
                    self.rail_new_menu_open = true;
                    self.spawn_menu_open = false; // two open menus would fight over Esc
                } else {
                    self.rail_new_err =
                        Some("name this project first — ⏎ creates it, then ⌘T".to_string());
                }
            } else {
                // …the scan just hasn't landed. Say WAIT, never "add a project":
                // his projects exist and arrive on their own. The banner lands on
                // the Workspace because that is the only screen that draws it;
                // `adopt_projects` clears it when the reason expires.
                self.term_error = Some(STILL_SCANNING.to_string());
                self.screen = Screen::Workspace;
                self.mode = crate::default_workspace_mode();
            }
            cx.notify();
            return None;
        }
        match self.ensure_project_dir() {
            Ok(dir) => Some(dir),
            Err(e) => {
                self.term_error = Some(e);
                self.screen = Screen::Workspace;
                // land where term_error is drawn — the map stage normally, the
                // Agent stage in the OSS build (both render term_error).
                self.mode = crate::default_workspace_mode();
                cx.notify();
                None
            }
        }
    }

    /// "Open folder…" (#34): register an ARBITRARY existing folder as a project
    /// EXACTLY where it lives — no move, no copy. `projects_root` governs only
    /// where NEW project directories are MADE; a folder that already exists keeps
    /// its home, keyed to its real path. Native picker → `commit_open_folder`.
    /// $HOME itself (or an ancestor) is refused there — #29's PtyProcess backstop
    /// only catches a *non-existent* cwd, and $HOME.is_dir() is true, so it would
    /// sail through and root every session in the home folder. Cancelling the
    /// picker (Err / Ok(None)) is a clean no-op.
    pub(crate) fn open_folder(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(dir) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| this.commit_open_folder(dir, cx));
                }
            }
        })
        .detach();
    }

    /// The store/rail half of `open_folder` — thin plumbing around the PURE
    /// `open_folder_action`, which owns the anti-duplicate / anti-fusion decision.
    fn commit_open_folder(&mut self, dir: std::path::PathBuf, cx: &mut Context<Self>) {
        self.rail_new_err = None;
        // the folder could have moved/vanished between the pick and here.
        if !dir.is_dir() {
            self.rail_new_err = Some(format!("{} is no longer a folder", dir.display()));
            cx.notify();
            return;
        }
        // $HOME (or any ancestor: /, /Users) would root every session in this
        // project at the home folder — an agent with file-write access loose in
        // ~. Refuse it out loud, exactly as the projects-root picker does
        // (settings::choose_projects_root). #29's cwd.is_dir() backstop can't
        // catch this: $HOME exists, so it passes.
        let home = orchestrator_core::scan::home();
        if home.starts_with(&dir) {
            self.rail_new_err = Some(format!(
                "{} would run every session in your home folder — pick a subfolder",
                dir.display()
            ));
            cx.notify();
            return;
        }
        // the key the registry derives for this dir — the SAME key its own
        // sessions produce (scan::key_for_dir). Born canonical ⇒ never a 2nd row.
        let key = orchestrator_core::scan::key_for_dir(&dir);
        let (taken, stored_name) = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            (store.project_exists(&key), store.project_name(&key))
        };
        match open_folder_action(&self.projects, &dir, &key, taken, stored_name.as_deref()) {
            // already open — land on it, never fork a second row over one dir.
            OpenFolderAction::Select(slug) => self.select_project(&slug, cx),
            // a dormant project whose folder this genuinely is — re-attach its
            // EXISTING key to this path under its own stored name. `set_project_path`
            // only (NEVER `ensure_project`), so the survivor's name and map are
            // untouched — no rename, no fusion.
            OpenFolderAction::Reattach { key, name } => {
                {
                    let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = store.set_project_path(&key, &dir.to_string_lossy());
                }
                // optimistic insert; the next rescan re-derives an identical row
                // (core test at_dir_matches_resolver_path_row_shape). owner_of
                // proved the rail holds NO row under this key/path, so no dup.
                self.projects
                    .push(orchestrator_core::Project::at_dir(&key, dir.clone(), &name));
                self.select_project(&key, cx);
            }
            // brand-new folder — create the project IN PLACE, at its own path.
            OpenFolderAction::Create { key, name } => {
                {
                    let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = store.ensure_project(&key, &name);
                    // the recorded path is what makes the next scan inject an
                    // Explicit source at this dir — without it the row would
                    // vanish on rescan.
                    let _ = store.set_project_path(&key, &dir.to_string_lossy());
                }
                self.projects
                    .push(orchestrator_core::Project::at_dir(&key, dir.clone(), &name));
                self.select_project(&key, cx);
            }
            OpenFolderAction::Refuse(msg) => {
                self.rail_new_err = Some(msg);
                cx.notify();
            }
        }
    }

    /// Persist + apply the projects folder (Settings → Projects folder). Core's
    /// scan/fold read it too (they can't reach the store), so mirror it there.
    pub(crate) fn set_projects_root(&mut self, root: std::path::PathBuf, cx: &mut Context<Self>) {
        if let Ok(store) = self.store.lock() {
            let _ = store.set_setting("projects_root", &root.to_string_lossy());
        }
        orchestrator_core::set_projects_root(root.clone());
        self.projects_root = root;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    // gpui is glob-imported above; import selectively (house rule).
    use super::{
        dir_name_for, is_no_project_sentinel, name_for_dir, no_project_reason_for,
        open_folder_action, owner_of, pathless_owner, promotion_dir_for, OpenFolderAction, Owner,
        NO_PROJECT_YET, STILL_SCANNING,
    };
    use orchestrator_core::Project;

    fn idea(name: &str) -> Project {
        Project::idea(&orchestrator_core::registry::idea_key(name), name)
    }
    fn at(key: &str, dir: &str, name: &str) -> Project {
        Project::at_dir(key, std::path::PathBuf::from(dir), name)
    }

    #[test]
    fn a_promotion_can_never_fuse_two_projects() {
        // findings 1/3/4. The rail is only what the LAST SCAN SAW; there is no
        // `DELETE FROM project` anywhere, so a project whose transcripts aged out
        // (claude prunes at ~30d) or whose folder was deleted is absent from the
        // rail while still owning its key, its map and its memory. Guarding on the
        // rail alone let `rename_project_key` re-key 18 tables onto its row —
        // fusing two product maps and deleting the victim's project row.
        let dir = std::path::Path::new("/Users/me/local/epsilon");
        let key = "path:/Users/me/local/epsilon";
        let me = idea("Epsilon");

        // 1. the rail is EMPTY of the victim (its sessions aged out) but the STORE
        //    still holds its row → the store's answer is what saves him.
        assert_eq!(
            owner_of(&[me.clone()], dir, key, true, Some(&me.slug)),
            Owner::Forgotten,
            "a forgotten project's key must block the promotion"
        );
        // 2. the scan DID see it → blocked by name, both on key and on path.
        let victim = at(key, "/Users/me/local/epsilon", "Epsilon (old)");
        assert_eq!(
            owner_of(&[me.clone(), victim], dir, key, false, Some(&me.slug)),
            Owner::Scanned("Epsilon (old)".into())
        );
        // a GIT-keyed row owns its directory too — different key, same folder.
        let repo = at("github:acme/epsilon", "/Users/me/local/epsilon", "epsilon");
        assert_eq!(
            owner_of(&[me.clone(), repo], dir, key, false, Some(&me.slug)),
            Owner::Scanned("epsilon".into())
        );
        // 3. nobody owns it → the promotion proceeds.
        assert_eq!(owner_of(&[me.clone()], dir, key, false, Some(&me.slug)), Owner::Free);
        // 4. the SELF-keyed no-op promotion (a project that already IS this key and
        //    merely never recorded its path) is not a collision.
        let selfkeyed = at(key, "/Users/me/local/epsilon", "Epsilon");
        assert_eq!(
            owner_of(&[selfkeyed], dir, key, false, Some(key)),
            Owner::Free
        );
    }

    #[test]
    fn new_project_promotes_the_idea_instead_of_minting_a_duplicate() {
        // finding 2: "＋ new project" named after an existing idea. The idea has
        // slug `idea:omega` and path None, so it matched NEITHER owner check — a
        // SECOND row was created over the same directory, forever, with the notes
        // stranded on the row he had to abandon (and the idea left permanently
        // un-spawnable, since ensure_project_dir would then find the sibling).
        let projects = vec![idea("Omega")];
        let dir = std::path::Path::new("/Users/me/local/omega");
        let key = "path:/Users/me/local/omega";
        // nothing OWNS the dir…
        assert_eq!(owner_of(&projects, dir, key, false, None), Owner::Free);
        // …but the path-less idea IS this project → promote it, don't fork.
        assert_eq!(pathless_owner(&projects, "Omega").as_deref(), Some("idea:omega"));
        // case/punctuation fold the same way the directory does.
        assert_eq!(pathless_owner(&projects, "  omega  ").as_deref(), Some("idea:omega"));

        // matched on the DIR, not on idea_key: dir_name_for caps at 64 chars, so
        // two long names share a directory WITHOUT sharing an idea key — keying
        // the guard on idea_key(name) would still have forked a duplicate row.
        let long_a = "a".repeat(70);
        let long_b = format!("{}bbbb", "a".repeat(66));
        assert_ne!(
            orchestrator_core::registry::idea_key(&long_a),
            orchestrator_core::registry::idea_key(&long_b)
        );
        assert_eq!(dir_name_for(&long_a), dir_name_for(&long_b));
        assert_eq!(
            pathless_owner(&[idea(&long_a)], &long_b).as_deref(),
            Some(orchestrator_core::registry::idea_key(&long_a).as_str())
        );

        // a project that already OWNS a directory is never a promote candidate.
        assert_eq!(pathless_owner(&[at(key, "/Users/me/local/omega", "Omega")], "Omega"), None);
    }

    #[test]
    fn a_first_run_spawn_cannot_mint_the_sentinel_s_folder() {
        // THE BUG: with an EMPTY portfolio `project()` returns the empty-portfolio
        // sentinel, and ⌘T ran the ordinary promotion on it. The sentinel's name is
        // a perfectly legal directory name, so a brand-new user's very first
        // keystroke created <projects_root>/no-projects-yet on disk, a store
        // project called "No projects yet", and a session under the key "welcome".
        let root = std::path::Path::new("/Users/me/local");
        let sentinel = crate::empty_project();
        assert_eq!(
            dir_name_for(&sentinel.name),
            "no-projects-yet",
            "the sentinel's name IS creatable — which is exactly why it must never be used"
        );
        assert!(is_no_project_sentinel(sentinel));
        assert_eq!(
            promotion_dir_for(sentinel, root),
            None,
            "the sentinel must yield NO directory — this is the mkdir target ensure_project_dir uses"
        );

        // ...and the guard is the SENTINEL, not "path-less": a real idea project
        // still promotes onto its own folder on first spawn (the #29 contract),
        // even one the user happened to name exactly like the sentinel.
        let twin = idea("No projects yet");
        assert!(!is_no_project_sentinel(&twin), "identity, not the name");
        assert_eq!(
            promotion_dir_for(&twin, root),
            Some(root.join("no-projects-yet"))
        );
        assert_eq!(
            promotion_dir_for(&idea("Omega"), root),
            Some(root.join("omega"))
        );
    }

    /// THE RACE the empty-portfolio guard opened: `projects` is EMPTY at boot
    /// (boot.rs seeds `seed_projects()`, a `Vec::new()`) and is only filled when
    /// the background scan lands in `adopt_projects` — seconds later on a big
    /// portfolio. So "project() is the sentinel" is ALSO true, at every launch,
    /// for a user who has 30 projects. Keyed on the sentinel alone, the guard
    /// refused his ⌘T for that whole window and answered with "ADD A PROJECT".
    #[test]
    fn a_full_portfolio_is_never_told_it_has_no_projects_mid_scan() {
        assert_eq!(no_project_reason_for(true, false), Some(STILL_SCANNING));
        assert_eq!(no_project_reason_for(true, true), Some(NO_PROJECT_YET));
        // a real project is never refused, scanned or not (an optimistic insert
        // from ＋New is selectable before any scan confirms it).
        assert_eq!(no_project_reason_for(false, false), None);
        assert_eq!(no_project_reason_for(false, true), None);
        // the two must stay DIFFERENT sentences: adopt_projects retracts the
        // scanning one by value, and "add a project" would be retracted wrongly.
        assert_ne!(STILL_SCANNING, NO_PROJECT_YET);
    }

    /// `no_project_reason_for` can only tell the two states apart if its caller
    /// hands it the SCAN FLAG. The behaviour itself needs a gpui `Context`, so
    /// this greps the source (the features.rs canary idiom): every needle is
    /// split across a concat! because the file being read is this one.
    #[test]
    fn spawn_cwd_reads_the_scan_flag_before_refusing() {
        let src = include_str!("projdir.rs");
        let body = src.split("mod tests").next().unwrap_or(src);
        assert!(
            body.contains(concat!("if self.scan", "ned {")),
            "spawn_cwd must branch on the scan flag, not on the sentinel alone"
        );
        assert!(
            body.contains(concat!("rail_new_menu_", "open = true")),
            "a genuinely empty portfolio is still answered with the ＋New menu"
        );
        assert!(
            body.contains(concat!("term_error = Some(STILL_", "SCANNING.to_string())")),
            "the mid-scan answer must be a visible WAIT, never the ＋New menu"
        );
    }

    #[test]
    fn dir_name_is_a_safe_single_component() {
        assert_eq!(dir_name_for("Cinematic Check-in"), "cinematic-check-in");
        assert_eq!(dir_name_for("My  Cool   App"), "my-cool-app");
        // traversal + absolute paths are impossible: separators and dots are
        // stripped by the slug, not "sanitized" afterwards.
        assert_eq!(dir_name_for("../../etc/passwd"), "etcpasswd");
        assert_eq!(dir_name_for("/tmp/evil"), "tmpevil");
        assert_eq!(dir_name_for(".."), "untitled");
        assert_eq!(dir_name_for("~/.ssh"), "ssh");
        assert_eq!(dir_name_for("🚀🔥"), "untitled");
        assert_eq!(dir_name_for(""), "untitled");
        for n in ["../../etc/passwd", "/tmp/evil", "..", "~/.ssh", "a/b"] {
            let d = dir_name_for(n);
            assert!(!d.contains('/') && !d.contains("..") && !d.starts_with('-'), "{n} → {d}");
            assert_eq!(std::path::Path::new(&d).components().count(), 1, "{n} → {d}");
        }
        // long names are capped to one sane component
        assert_eq!(dir_name_for(&"x".repeat(300)).len(), 64);
    }

    #[test]
    fn dir_name_matches_the_idea_slug() {
        // the promotion path derives the dir from the SAME slug the idea key
        // uses, so an idea and its promoted project name the same folder.
        let key = orchestrator_core::registry::idea_key("Kod Map");
        assert_eq!(format!("idea:{}", dir_name_for("Kod Map")), key);
    }

    #[test]
    fn open_folder_never_forks_a_second_row() {
        // "Open folder…" (#34) on an ARBITRARY existing folder. The whole point:
        // the row the user opens shares ONE key with the rows the folder's own
        // sessions produce, so no folder is ever a project twice.
        let dir = std::path::Path::new("/Users/me/work/foo");
        let key = "path:/Users/me/work/foo";

        // FREE — a brand-new folder → Create, keyed canonically, named by its REAL
        // basename (verbatim, NOT slugified).
        assert_eq!(
            open_folder_action(&[], dir, key, false, None),
            OpenFolderAction::Create { key: key.into(), name: "foo".into() },
        );

        // SCANNED by KEY — the folder already IS a rail project under this key →
        // Select it, never a second row.
        let same = at(key, "/Users/me/work/foo", "Foo");
        assert_eq!(
            open_folder_action(&[same], dir, key, true, Some("Foo")),
            OpenFolderAction::Select(key.into()),
        );

        // SCANNED by PATH — a GIT-keyed row owns this very folder under a DIFFERENT
        // key → Select that git-keyed slug (the folder is already open), no fork.
        let repo = at("github:acme/foo", "/Users/me/work/foo", "foo");
        assert_eq!(
            open_folder_action(&[repo], dir, key, false, None),
            OpenFolderAction::Select("github:acme/foo".into()),
        );

        // FORGOTTEN with a stored name — the store row + map survived a scan that
        // no longer sees it. The folder genuinely resolves to this key, so it IS
        // that project → RE-ATTACH under its OWN stored name (never the basename,
        // which would rename the survivor via ensure_project).
        assert_eq!(
            open_folder_action(&[], dir, key, true, Some("Foo the Original")),
            OpenFolderAction::Reattach { key: key.into(), name: "Foo the Original".into() },
        );

        // FORGOTTEN but the name can't be read — refuse rather than invent one.
        assert_eq!(
            open_folder_action(&[], dir, key, true, None),
            OpenFolderAction::Refuse(
                "/Users/me/work/foo already belongs to another project".into()
            ),
        );
    }

    #[test]
    fn open_folder_name_is_the_real_basename_not_a_slug() {
        // an opened folder already HAS a name on disk — echo it verbatim. This is
        // the opposite of "＋ new project", which mints a slug FROM a typed title.
        assert_eq!(name_for_dir(std::path::Path::new("/a/b/Foo Bar")), "Foo Bar");
        assert_eq!(name_for_dir(std::path::Path::new("/a/b/epsilon")), "epsilon");
        assert_eq!(name_for_dir(std::path::Path::new("/a/b/UPPER.dots")), "UPPER.dots");
        // total: a trailing-slash path still yields the basename, and "/" the path.
        assert_eq!(name_for_dir(std::path::Path::new("/a/b/foo/")), "foo");
        assert_eq!(name_for_dir(std::path::Path::new("/")), "/");
    }
}
