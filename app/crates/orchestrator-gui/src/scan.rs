use gpui::*;
use crate::*;


impl Orchestrator {

    /// Kick the real registry scan on a background thread (it's fs + git I/O —
    /// must not block the UI). The poll task in render swaps
    /// the result in when ready.
    pub(crate) fn start_scan(&mut self, cx: &mut Context<Self>) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        let slot = self.scan_result.clone();
        // store-backed rows ride the same scan: path-less IDEAS as
        // ScanSource::idea (→ RowOrigin::Store rows, sorted after path rows), and
        // projects that OWN A DIRECTORY (#29) as an Explicit source AT that dir —
        // which keys exactly like their own sessions do, so they fold into one row
        // instead of duplicating.
        let rows: Vec<orchestrator_core::StoreProject> = self
            .store
            .lock()
            .ok()
            .and_then(|s| s.store_projects().ok())
            .unwrap_or_default()
            .into_iter()
            .map(|(key, name, path)| orchestrator_core::StoreProject {
                key,
                name,
                path: path.map(std::path::PathBuf::from),
            })
            .collect();
        std::thread::spawn(move || {
            let projects = orchestrator_core::live_projects_with_store(&rows);
            *slot.lock().unwrap() = Some(projects);
        });
        // poll for the result, swap it in, then stop.
        let slot = self.scan_result.clone();
        cx.spawn(async move |this, cx| loop {
            Timer::after(std::time::Duration::from_millis(150)).await;
            let ready = slot.lock().ok().and_then(|mut g| g.take());
            let done = this
                .update(cx, |this, cx| {
                    if let Some(projects) = ready {
                        this.adopt_projects(projects);
                        this.scanning = false;
                        // now that paths are known, decay stale done-parts (◌).
                        this.start_reconcile(cx);
                        cx.notify();
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(true);
            if done {
                break;
            }
        })
        .detach();
    }

    /// Reconcile asserted-`done` statuses against the code they anchor, so a part
    /// whose files changed since you marked it done decays to ◌ stale (the map
    /// stops drifting to fiction). Runs off the UI thread (it stats the fs),
    /// locking the store briefly per project; repaints when finished.
    fn start_reconcile(&mut self, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let targets: Vec<(String, std::path::PathBuf)> = self
            .projects
            .iter()
            .filter_map(|p| p.path.clone().map(|path| (p.slug.clone(), path)))
            .collect();
        if targets.is_empty() {
            return;
        }
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_done = done.clone();
        std::thread::spawn(move || {
            for (slug, root) in targets {
                // lock per project so the UI thread can interleave its renders.
                if let Ok(s) = store.lock() {
                    let _ = s.reconcile_staleness(&slug, &root);
                }
            }
            worker_done.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        cx.spawn(async move |this, cx| loop {
            Timer::after(std::time::Duration::from_millis(200)).await;
            if done.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = this.update(cx, |_this, cx| cx.notify());
                break;
            }
        })
        .detach();
    }

    /// Replace the project list with a fresh scan, keeping the user's selection
    /// stable by slug (the canonical key — stable across rescans). On a miss
    /// (the selected project vanished), fall back to the Standup rather than
    /// landing the user on an unrelated project.
    fn adopt_projects(&mut self, projects: Vec<Project>) {
        // a spawn refused DURING this scan ("still finding your projects") has just
        // stopped being true — retract it, or the banner outlives its reason and
        // greets the user on a workspace that is now perfectly spawnable.
        if self.term_error.as_deref() == Some(projdir::STILL_SCANNING) {
            self.term_error = None;
        }
        // ideas created while a scan was in flight aren't in its snapshot —
        // carry them over instead of letting the wholesale replace eat them
        // (review 1b); the NEXT scan re-derives them from the store.

        if projects.is_empty() {
            self.scanned = true;
            return;
        }
        let sel_slug = self
            .scanned
            .then(|| self.projects.get(self.selected).map(|p| p.slug.clone()))
            .flatten();
        let mut incoming = projects;
        // a project created while a scan was in flight isn't in its snapshot —
        // carry it over instead of letting the wholesale replace eat it (review
        // 1b); the NEXT scan re-derives it from the store. Both store-backed
        // kinds qualify: a path-less idea AND a project that owns a directory
        // (#29) — keying the guard on `idea:` alone would have made a brand-new
        // project vanish from the rail seconds after he named it.
        let store_keys: std::collections::HashSet<String> = self
            .store
            .lock()
            .ok()
            .and_then(|s| s.store_projects().ok())
            .unwrap_or_default()
            .into_iter()
            .map(|(k, _, _)| k)
            .collect();
        for p in self
            .projects
            .iter()
            .filter(|p| p.slug.starts_with("idea:") || store_keys.contains(&p.slug))
        {
            if !incoming.iter().any(|q| q.slug == p.slug) {
                incoming.push(p.clone());
            }
        }
        // the registry sorts by recency, so a rescan would undo a manual rail
        // order (#28) — replay it here, the ONE place the vec is replaced.
        rail_order::apply_order(&mut incoming, &self.project_order);
        self.projects = incoming;
        self.scanned = true;
        // BEFORE ensure_projects_in_store (which would happily insert a fresh
        // EMPTY row under the new key and leave the project's map behind).
        self.rekey_moved_projects();
        self.ensure_projects_in_store();
        match sel_slug.and_then(|slug| self.projects.iter().position(|p| p.slug == slug)) {
            Some(i) => self.selected = i,
            None => {
                self.selected = 0;
                if self.screen == Screen::Workspace {
                    self.screen = Screen::Standup;
                }
            }
        }
        // ORCH_DEMO=flow → jump to the orchestrator project's Workspace (the
        // Flow-map gate) once the real scan has surfaced it.
        if std::env::var("ORCH_DEMO").as_deref() == Ok("flow") {
            if let Some(i) = self.projects.iter().position(|p| p.name == "orchestrator") {
                self.selected = i;
                self.screen = Screen::Workspace;
            }
        }
        // ORCH_DEMO=recover → open beta's Workspace + drawer (no session) so the
        // recoverable-session import list shows.
        if std::env::var("ORCH_DEMO").as_deref() == Ok("recover") {
            if let Some(i) = self.projects.iter().position(|p| p.name == "beta") {
                self.selected = i;
                self.screen = Screen::Workspace;
            }
        }
        // ORCH_DEMO=import → a project's Workspace in the per-project Recover view.
        if std::env::var("ORCH_DEMO").as_deref() == Ok("import") {
            self.screen = Screen::Workspace;
            self.mode = Mode::Recover;
        }
        // ORCH_DEMO=sessions → a project's Workspace with the terminal EXPANDED
        // to the full Sessions workspace (#9).
        if std::env::var("ORCH_DEMO").as_deref() == Ok("sessions") {
            if let Some(i) = self
                .projects
                .iter()
                .position(|p| p.name == "beta")
                .or(Some(0))
            {
                self.selected = i;
                self.screen = Screen::Workspace;
                self.mode = Mode::Agent;
            }
        }
    }

    /// A project whose DIRECTORY is now keyed differently keeps its data (#5).
    ///
    /// A project born `path:/Users/me/local/epsilon` re-keys itself the
    /// moment its agent runs `git init` + `gh repo create --push`: the next scan
    /// reads LIVE git facts, `key_of` sees a remote, and the row becomes
    /// `github:acme/epsilon`. Nothing moved the store, so every slug-keyed
    /// read returned nothing and the project REOPENED EMPTY — map, memory, notes
    /// and journal all still filed under the dead `path:` key.
    ///
    /// So: for every store project that owns a directory, find the scanned row
    /// that owns that SAME directory; if its key differs, move the store (and the
    /// RAM caches, and any live session) onto it.
    ///
    /// NEVER FUSES — the same rule as the promotion guard: if the new key already
    /// has a project row, leave everything exactly as it is. Merging two projects
    /// has no undo, and a wrong re-key is unrecoverable; a stale key is not.
    fn rekey_moved_projects(&mut self) {
        let owned: Vec<(String, std::path::PathBuf)> = self
            .store
            .lock()
            .ok()
            .and_then(|s| s.store_projects().ok())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(k, _, p)| p.map(|p| (k, std::path::PathBuf::from(p))))
            .collect();
        let moves: Vec<(String, String, std::path::PathBuf)> = owned
            .into_iter()
            .filter_map(|(old, dir)| {
                let new = self
                    .projects
                    .iter()
                    .find(|p| p.path.as_deref() == Some(dir.as_path()))
                    .map(|p| p.slug.clone())?;
                (new != old).then_some((old, new, dir))
            })
            .collect();
        for (old, new, dir) in moves {
            {
                let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
                if store.project_exists(&new) {
                    continue; // never merge into an existing identity
                }
                if store.rename_project_key(&old, &new).is_err() {
                    continue;
                }
                let _ = store.set_project_path(&new, &dir.to_string_lossy());
            }
            self.rekey_in_memory(&old, &new);
        }
    }

    /// Make sure the store has a row for every scanned project (UPSERT; never
    /// touches the tree).
    pub(crate) fn ensure_projects_in_store(&self) {
        if let Ok(store) = self.store.lock() {
            for p in &self.projects {
                let _ = store.ensure_project(&p.slug, &p.name);
            }
        }
    }

    /// Mark a project as "start blank" so the seed CTA stops nagging (the user
    /// will author parts directly).
    pub(crate) fn start_blank(&mut self, cx: &mut Context<Self>) {
        let slug = self.project().slug.clone();
        if let Ok(store) = self.store.lock() {
            let _ = store.set_seed_state(&slug, orchestrator_store::SeedState::Blank);
        }
        cx.notify();
    }

}
