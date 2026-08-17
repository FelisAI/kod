use gpui::*;
use crate::*;

/// Layer a profile's account isolation + env overrides onto a spawn/resume spec.
/// Account isolation is the CLI's config-home env var — claude reads
/// `CLAUDE_CONFIG_DIR`, codex reads `CODEX_HOME` — so a session runs entirely
/// under that profile's account; the profile's own `env` map layers on top.
/// Everything is PUSHED onto `spec.env` (never replaces), so the stage/effort
/// env already staged survives. `kind` decides WHICH isolation var, so a claude
/// resume leg passes Claude and a codex leg passes Codex — a session always
/// resumes under the same account it ran under. Shells (`_`) get no isolation
/// var but still inherit the profile's plain env.
pub(crate) fn apply_profile(spec: &mut SpawnSpec, kind: CliKind, profile: &ProfileRow) {
    if let Some(dir) = profile.config_dir.as_deref().filter(|d| !d.is_empty()) {
        let var = match kind {
            CliKind::Claude => Some("CLAUDE_CONFIG_DIR"),
            CliKind::Codex => Some("CODEX_HOME"),
            _ => None,
        };
        if let Some(var) = var {
            spec.env.push((var.to_string(), dir.to_string()));
        }
    }
    for (k, v) in &profile.env {
        spec.env.push((k.clone(), v.clone()));
    }
}

/// Layer a profile's MODEL + extra args onto a spawn spec's ARGV (#58). Both
/// fields were stored and editable in Settings but never read at spawn, so a
/// user who picked a model on a profile silently got the ambient account
/// default. Empty/whitespace model = "account default": omit the flag rather
/// than pass `--model ""`, which the CLI rejects (same rule the background
/// prompt lanes already follow, extract.rs §326).
///
/// The claude leg ALSO pushes `ANTHROPIC_MODEL`: claude resolves `--model`
/// ABOVE the env var (measured, claude 2.1.220), so the two can never disagree
/// — same value either way — and the env twin keeps the model true for anything
/// downstream that reads it. The flag itself now reaches the process on every
/// leg: `Host::spawn_claude`/`resume_claude` splice caller args into the argv
/// they mint instead of assigning over them (host.rs `claude_args`).
pub(crate) fn apply_profile_argv(spec: &mut SpawnSpec, kind: CliKind, profile: &ProfileRow) {
    if let Some(model) = profile
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        match kind {
            CliKind::Claude => {
                spec.args.push("--model".into());
                spec.args.push(model.to_string());
                spec.env
                    .push(("ANTHROPIC_MODEL".to_string(), model.to_string()));
            }
            CliKind::Codex => {
                spec.args.push("-m".into());
                spec.args.push(model.to_string());
            }
            // a shell has no model concept — the field is inert for it by design.
            _ => {}
        }
    }
    // extra_args ride LAST so they're the user's final word: the escape hatch
    // for any flag the profile's typed fields don't model.
    spec.args.extend(profile.extra_args.iter().cloned());
}

/// The RESUME twin of `apply_profile_argv`. A session must not silently change
/// model between birth and resume — that asymmetry is the whole reason both
/// exist — so the claude leg is IDENTICAL. The codex leg deliberately omits
/// `-m`: `Host::resume_codex` records the one measurement this codebase has on
/// the subject ("a forced -m 400s on a ChatGPT plan"), and a codex rollout is
/// resumed in place with the model it was born under, so re-asserting it buys
/// nothing and risks a 400 at the first turn. extra_args still ride — they're
/// the user's own escape hatch, and the model is the only measured hazard.
pub(crate) fn apply_profile_resume_argv(
    spec: &mut SpawnSpec,
    kind: CliKind,
    profile: &ProfileRow,
) {
    if kind == CliKind::Codex {
        spec.args.extend(profile.extra_args.iter().cloned());
        return;
    }
    apply_profile_argv(spec, kind, profile);
}

/// The app-setting key naming the default profile for a CLI (#56). Shells have
/// no account concept, so they get no key and always spawn ambient. These exact
/// strings are the contract with the Settings UI that writes them.
pub(crate) fn default_profile_key(kind: CliKind) -> Option<&'static str> {
    match kind {
        CliKind::Claude => Some("default_profile_claude"),
        CliKind::Codex => Some("default_profile_codex"),
        _ => None,
    }
}

/// Keep the per-CLI default-profile keys honest after a profile write. Both keys
/// name a profile by ID, so a row that is DELETED (`kind = None`) or re-labelled
/// to the other CLI (`kind = Some(new_kind)`) can leave a key pointing at a
/// profile that no longer belongs in it. Spawn folds such a key to ambient and
/// the Settings picker shows "(ambient account)", so nothing MISBEHAVES — but a
/// stored default that every reader ignores is a lie, and the next feature to
/// read the key without both guards inherits the bug. Clears every key naming
/// `id` except the one this row now belongs in.
pub(crate) fn reconcile_default_profile_keys(store: &Store, id: i64, kind: Option<&str>) {
    for k in [CliKind::Claude, CliKind::Codex] {
        let Some(key) = default_profile_key(k) else {
            continue;
        };
        if kind == Some(k.label()) {
            continue; // the row's own key — a default it still satisfies
        }
        if store.get_setting(key).and_then(|v| v.trim().parse::<i64>().ok()) == Some(id) {
            let _ = store.set_setting(key, "");
        }
    }
}

/// Which account a NEW session runs under, decided at the CALL SITE. Three
/// answers — and the `Option<i64>` this replaced could only spell two of them:
/// its `None` meant "no explicit pick", so the moment a default profile existed
/// (#56) every claude/codex spawn adopted it and an AMBIENT session became
/// inexpressible. The user's own login was then reachable only by going to
/// Settings and clearing the default, i.e. a mode switch, not a choice (#62.3).
/// A third variant is the fix rather than a bool: `Ambient` must never fall
/// through to the default lane, and the type is what guarantees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProfilePick {
    /// No pick made here (⌘T, ▶ work on this, the plain "+" menu rows) — this
    /// CLI's default-profile setting decides.
    Default,
    /// Deliberately NO profile: whatever account the CLI is already logged into,
    /// with no config-dir isolation and none of a profile's env/model/args.
    Ambient,
    /// This exact profile row (the "+" menu's ACCOUNTS rows).
    Profile(i64),
}

/// Resolve a pick to the account a spawn runs under. A `Profile` pick (the
/// spawn-menu picker) is taken as-is, `Ambient` is nothing at all, and `Default`
/// lets this CLI's default-profile setting decide — resolved HERE so every
/// caller that makes no pick inherits defaults without changing its call.
/// Resolved to the ROW, never a bare id: a deleted profile (stale default *or*
/// stale explicit pick) folds to ambient instead of spawning under a dangling
/// profile_id that resume and codex birth-discovery would later chase into an
/// account that no longer exists. Absent/blank/garbage setting is ambient too —
/// a default account is a convenience and must never fail the spawn.
///
/// The DEFAULT lane also requires the row's `cli_kind` to match the key it came
/// out of. Editing a profile's CLI re-labels an existing account folder, and the
/// default keys are written by id: without this check `default_profile_claude`
/// could name a now-codex profile and push its `CODEX_HOME` folder at claude as
/// `CLAUDE_CONFIG_DIR` — an unauthenticated login the user never chose, while
/// Settings (which DOES filter by kind) showed "(ambient account)". Settings
/// clears the key on such an edit; this is the read-side half, so a store that
/// already went stale heals instead of spawning wrong. The EXPLICIT lane stays
/// unfiltered on purpose: the spawn menu derives the kind FROM the row it
/// offers, so it can't mismatch, and a shell legitimately borrows any profile
/// for its plain env.
pub(crate) fn resolve_spawn_profile(
    store: &Store,
    kind: CliKind,
    pick: ProfilePick,
) -> Option<ProfileRow> {
    match pick {
        ProfilePick::Ambient => None,
        ProfilePick::Profile(id) => store.profile(id),
        ProfilePick::Default => {
            let raw = store.get_setting(default_profile_key(kind)?)?;
            store
                .profile(raw.trim().parse::<i64>().ok()?)
                .filter(|p| p.cli_kind == kind.label())
        }
    }
}


impl Orchestrator {

    /// Stamp the stage geometry + the default claude effort (Settings) on a
    /// spawn spec. EVERY spec the GUI hands the host goes through here — spawn,
    /// resume, restore — so a new path can't silently miss the effort (the bug
    /// class that hit resumed sessions). The host applies `effort` only on its
    /// claude paths; it's inert for shell/codex.
    pub(crate) fn stage_spec(&self, mut spec: SpawnSpec) -> SpawnSpec {
        spec.rows = STAGE_ROWS;
        spec.cols = TERM_COLS;
        spec.effort = self.claude_effort.clone();
        spec
    }

    /// Spawn a CLI in the open project's OWN DIRECTORY — materializing one (and
    /// promoting a path-less idea onto it) if the project doesn't have one yet
    /// (#29). It used to fall back to $HOME: every idea project's session ran in
    /// the user's home folder, with file-write access and no isolation. There
    /// is no fallback now — `spawn_cwd` either yields a real directory or
    /// surfaces why it couldn't, and we don't spawn. Spawn failures (e.g. the CLI
    /// isn't on PATH) are surfaced in the drawer, never swallowed (docs/013 §1).
    pub(crate) fn spawn_session(&mut self, kind: CliKind, pick: ProfilePick, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cwd) = self.spawn_cwd(cx) else {
            return;
        };
        // AFTER ensure: a promoted idea's slug just changed (idea:x → path:…),
        // and the session must be filed under the key its rail row now carries.
        let slug = self.project().slug.clone();
        // the account this session runs under: the caller's pick — a profile, a
        // deliberate ambient, or this CLI's default (#56). Held as the full row
        // so the spec injection, the crash record, and codex's rollout scan all
        // agree.
        let profile = self
            .store
            .lock()
            .ok()
            .and_then(|s| resolve_spawn_profile(&s, kind, pick));
        // the RESOLVED id from here down. A defaulted session MUST be recorded
        // under the account it actually ran on, or its resume leg reopens it
        // ambient and codex's birth-discovery scans the wrong CODEX_HOME — the
        // session would never get a recovery row.
        let profile_id = profile.as_ref().map(|p| p.id);
        let mut spec = self.stage_spec(match kind {
            CliKind::Shell => SpawnSpec::shell(&cwd),
            CliKind::Claude => SpawnSpec::program("claude", &cwd),
            CliKind::Codex => SpawnSpec::program("codex", &cwd),
        });
        // AFTER stage_spec, so the account env reaches the host spawn (the effort
        // + stage env pushed above survive — apply_profile only appends).
        if let Some(p) = &profile {
            apply_profile(&mut spec, kind, p);
            apply_profile_argv(&mut spec, kind, p);
        }
        self.screen = Screen::Workspace;
        // a freshly-spawned agent lands you on its stage (Agent mode, #9 slice 2).
        self.mode = Mode::Agent;
        let cwd_str = cwd.to_string_lossy().into_owned();
        // Each CLI records a crash-recovery row at spawn (claude: the minted
        // --session-id; codex: discovered ≤4s later from its new rollout).
        // Shells never record (no transcript). spawn_claude returns the id, so
        // its arm can't unify with spawn()'s — branch per kind.
        match kind {
            CliKind::Claude => match self.host.spawn_claude(slug.clone(), spec) {
                Ok((id, cli_id)) => {
                    if let Ok(store) = self.store.lock() {
                        let _ = store.record_session(&cli_id, &slug, "claude", &cwd_str, profile_id);
                    }
                    self.term_error = None;
                    self.active_session.insert(slug, id);
                    self.term_focus.focus(window);
                }
                Err(e) => self.term_error = Some(format!("couldn't start {}: {e}", kind.label())),
            },
            CliKind::Codex => {
                // snapshot the cwd's existing codex ids BEFORE spawn so discovery
                // picks the genuinely-new rollout (not a sibling in the same cwd).
                // a profiled codex writes its rollout under the profile's CODEX_HOME,
                // so birth-discovery must scan there (not just ~/.codex) or the new
                // session is never recorded and can never be resumed.
                // read off the row resolved above, not a second store lookup —
                // that one keyed off the CALLER's id, so a defaulted spawn would
                // have scanned ~/.codex while the session wrote under the
                // profile's CODEX_HOME (#56).
                let codex_home = profile
                    .as_ref()
                    .and_then(|p| p.config_dir.clone())
                    .filter(|d| !d.is_empty())
                    .map(std::path::PathBuf::from);
                let pre: std::collections::HashSet<String> =
                    orchestrator_core::scan::codex_ids_for_cwd(&cwd, codex_home.as_deref())
                        .into_iter()
                        .collect();
                match self.host.spawn(slug.clone(), kind, spec) {
                    Ok(id) => {
                        self.term_error = None;
                        self.active_session.insert(slug.clone(), id);
                        self.term_focus.focus(window);
                        self.record_codex_fresh(id, slug, cwd.clone(), pre, profile_id, codex_home);
                    }
                    Err(e) => {
                        self.term_error = Some(format!("couldn't start {}: {e}", kind.label()))
                    }
                }
            }
            CliKind::Shell => match self.host.spawn(slug.clone(), kind, spec) {
                Ok(id) => {
                    self.term_error = None;
                    self.active_session.insert(slug, id);
                    self.term_focus.focus(window);
                }
                Err(e) => self.term_error = Some(format!("couldn't start {}: {e}", kind.label())),
            },
        }
        cx.notify();
    }

    /// ▶ work on this (docs/011 §C): compose the kickoff from the node's memory
    /// (path, detail, verbatim decisions, linked summaries), spawn claude with
    /// the prompt pre-submitted, and link session↔node AT BIRTH via the minted
    /// --session-id. Default stays on the map — the chip appearing on the node
    /// is the confirmation; alt(⌥) jumps to the Agent stage.
    pub(crate) fn dispatch_to_part(
        &mut self,
        part_id: PartId,
        alt: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // same contract as spawn_session (#29): the node's session runs in the
        // project's own directory, created + promoted here if it has none. This
        // path is the more dangerous half of the old $HOME bug — it spawns claude
        // with a PRE-SUBMITTED prompt, so an idea's "▶ work on this" used to put a
        // WORKING agent in the home folder.
        let Some(cwd) = self.spawn_cwd(cx) else {
            return;
        };
        let proj = self.project();
        let slug = proj.slug.clone(); // post-promotion key (see spawn_session)
        let pname = proj.name.clone();
        // the project always has a directory now; it may still be EMPTY (a fresh
        // idea) — that's what `has_repo` has always meant to the kickoff prompt.
        let has_repo = projdir::is_nonempty_dir(&cwd);
        let (parts, notes, memory_context) = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let parts = store.load_tree(&slug).unwrap_or_default();
            let notes = store.notes_for_part(part_id).unwrap_or_default();
            let mut memory_query = String::new();
            if let Some(node) = parts.iter().find(|p| p.id == part_id) {
                memory_query.push_str(&node.name);
                memory_query.push('\n');
                if node.detail_md.trim().is_empty() {
                    memory_query.push_str(&node.detail);
                } else {
                    memory_query.push_str(&node.detail_md);
                }
                memory_query.push('\n');
                for child in parts.iter().filter(|p| p.parent_id == Some(part_id)) {
                    memory_query.push_str(&child.name);
                    memory_query.push('\n');
                }
            }
            for note in &notes {
                if matches!(note.kind.as_str(), "decision" | "note" | "context") {
                    memory_query.push_str(&note.text);
                    memory_query.push('\n');
                }
            }
            // OSS gate (features.rs): skip the memory-graph retrieval when the
            // Memory subsystem is compiled out — the kickoff gets no memory context.
            let memory_context = if crate::features::MEMORY_ENABLED {
                store
                    .retrieve(RetrievalQuery {
                        project_key: slug.clone(),
                        intent: RetrievalIntent::Kickoff,
                        text: memory_query,
                        scope_memory_id: None,
                        since_secs: None,
                        limit: 5,
                    })
                    .map(|retrieval| retrieval.context_md)
                    .unwrap_or_default()
            } else {
                String::new()
            };
            (parts, notes, memory_context)
        };
        let summaries: Vec<(String, String)> = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            store
                .sessions_for_part(part_id)
                .iter()
                .filter_map(|(cli, _, _)| self.sess_summaries.get(cli))
                .map(|s| {
                    let detail: Vec<String> =
                        serde_json::from_str(&s.detail_json).unwrap_or_default();
                    (s.headline.clone(), detail.join(" "))
                })
                .take(4)
                .collect()
        };
        // linked-doc contents (docs/019 T11): parse the markdown links out of the
        // node's detail_md and read the repo files, so dispatch is context-rich.
        // Only with a repo (paths resolve against cwd); per-doc + total capped.
        let linked_docs: Vec<(String, String)> = if has_repo {
            let body = parts
                .iter()
                .find(|p| p.id == part_id)
                .map(|p| p.detail_md.clone())
                .unwrap_or_default();
            returnchannel::parse_doc_links(&body, 6)
                .into_iter()
                .filter_map(|(title, rel)| {
                    // fence path traversal: only read INSIDE the repo (a `..`
                    // escape or absolute path is skipped, never followed).
                    let rel_clean = rel.trim_start_matches("./");
                    if rel_clean.starts_with('/') || rel_clean.split('/').any(|c| c == "..") {
                        return None;
                    }
                    let full = cwd.join(rel_clean);
                    std::fs::read_to_string(&full).ok().map(|mut content| {
                        const PER_DOC_CAP: usize = 3_000;
                        if content.chars().count() > PER_DOC_CAP {
                            content = content.chars().take(PER_DOC_CAP).collect::<String>()
                                + "\n… [doc truncated]";
                        }
                        (title, content)
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        let prompt = kickoff::compose_with_memory(
            &pname,
            &parts,
            part_id,
            &notes,
            &summaries,
            &linked_docs,
            &memory_context,
            has_repo,
        );
        let mut spec = self.stage_spec(SpawnSpec::program("claude", &cwd));
        spec.initial_prompt = prompt;
        // '▶ work on this' is a claude spawn like any other, so it adopts the
        // default claude account (#56). It used to be the ONE spawn path that
        // skipped profile resolution entirely: the user's chosen default was
        // ignored, and `record_session` stamped profile_id = None, so the resume
        // leg reopened it ambient too. There is no explicit pick here — a map
        // node dispatch has no account picker — so the default is the answer.
        let profile = self
            .store
            .lock()
            .ok()
            .and_then(|s| resolve_spawn_profile(&s, CliKind::Claude, ProfilePick::Default));
        if let Some(p) = &profile {
            apply_profile(&mut spec, CliKind::Claude, p);
            apply_profile_argv(&mut spec, CliKind::Claude, p);
        }
        let profile_id = profile.as_ref().map(|p| p.id);
        self.screen = Screen::Workspace;
        match self.host.spawn_claude(slug.clone(), spec) {
            Ok((id, cli_id)) => {
                {
                    // poison-recover like the read paths — a skipped birth
                    // linkage means a live prompted session with no chip, no
                    // recovery row, and a user re-dispatching duplicates.
                    let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = store.record_session(
                        &cli_id,
                        &slug,
                        "claude",
                        &cwd.to_string_lossy(),
                        profile_id,
                    );
                    let _ = store.link_session_part(&cli_id, part_id, &slug, "dispatch");
                    let _ = store.add_note(
                        &slug,
                        part_id,
                        "session",
                        "▶ session started",
                        &format!("sess-{cli_id}"),
                    );
                }
                self.term_error = None;
                self.active_session.insert(slug, id);
                if alt {
                    self.focus_session(&self.project().slug.clone(), id, window, cx);
                } else {
                    self.focused_part = Some(part_id);
                    self.mode = crate::default_workspace_mode();
                }
            }
            Err(e) => self.term_error = Some(format!("couldn't start claude: {e}")),
        }
        cx.notify();
    }

    /// '◇ break down' (docs/011 slice 3): one isolated claude call proposes
    /// 3-7 children for the node as evidence-bearing Add ops, flowing through
    /// the SAME pending/per-op-✓✕ machinery as session proposals — worst case
    /// is a dismissed card, never a corrupted tree. Shares the summarizer's
    /// hourly budget; user-initiated, so it doesn't queue behind the lane.
    pub(crate) fn spawn_breakdown(&mut self, part_id: PartId, cx: &mut Context<Self>) {
        // OSS gate (features.rs): '◇ break down' spends on the shared plumbing
        // lane. Compiled out of the default build; backstop behind the hidden UI.
        if !crate::features::MAP_ENABLED {
            return;
        }
        {
            let mut inflight = self
                .breakdown_inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if inflight.contains(&part_id) {
                return;
            }
            let now_s = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            self.sum_job_times
                .retain(|t| now_s.saturating_sub(*t) < 3600);
            if self.sum_job_times.len() >= 20 {
                self.term_error = Some("map budget spent this hour — try break down later".into());
                cx.notify();
                return;
            }
            inflight.insert(part_id);
        }
        let slug = self.project().slug.clone();
        let (node_line, detail, decisions, children) = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let parts = store.load_tree(&slug).unwrap_or_default();
            let Some(node) = parts.iter().find(|p| p.id == part_id) else {
                self.breakdown_inflight
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&part_id);
                return;
            };
            let node_line = format!("[{}] {} ({})", node.id, node.name, node.lifecycle.as_str());
            let children: Vec<String> = parts
                .iter()
                .filter(|p| p.parent_id == Some(part_id))
                .map(|p| p.name.clone())
                .collect();
            let decisions: Vec<String> = store
                .notes_for_part(part_id)
                .unwrap_or_default()
                .iter()
                .filter(|n| n.kind == "decision")
                .map(|n| n.text.clone())
                .collect();
            (node_line, node.detail.clone(), decisions, children)
        };
        // the budget slot burns only for a call that actually goes out — a
        // vanished node above returned without spending it (review).
        self.sum_job_times.push(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );
        let store = self.store.clone();
        let inflight = self.breakdown_inflight.clone();
        let err_slot = self.breakdown_err.clone();
        std::thread::spawn(move || {
            // panic-safe: the inflight entry clears even if the proposer or
            // the store insert unwinds — else '◇ breaking down…' sticks until
            // restart (review).
            struct Clear(
                Arc<std::sync::Mutex<std::collections::HashSet<PartId>>>,
                PartId,
            );
            impl Drop for Clear {
                fn drop(&mut self) {
                    self.0
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&self.1);
                }
            }
            let _clear = Clear(inflight, part_id);
            match extract::propose_breakdown(&node_line, &detail, &decisions, &children, part_id) {
                Ok(ops) if !ops.is_empty() => {
                    let (o, e): (Vec<DiffOp>, Vec<String>) = ops.into_iter().unzip();
                    let ev: Vec<Option<String>> = e.into_iter().map(Some).collect();
                    let s = store.lock().unwrap_or_else(|p| p.into_inner());
                    let _ = s.add_pending_diff_with_evidence(
                        &slug,
                        &format!("breakdown:{part_id}"),
                        &o,
                        &ev,
                    );
                }
                Ok(_) => {
                    let mut g = err_slot.lock().unwrap_or_else(|p| p.into_inner());
                    let msg = "break down found nothing grounded in this node's notes".to_string();
                    *g = Some(g.take().map(|prev| format!("{prev}; {msg}")).unwrap_or(msg));
                }
                Err(e) => {
                    let mut g = err_slot.lock().unwrap_or_else(|p| p.into_inner());
                    let msg = format!("break down failed: {e}");
                    *g = Some(g.take().map(|prev| format!("{prev}; {msg}")).unwrap_or(msg));
                }
            }
        });
        cx.notify();
    }

    /// Codex mints its own uuidv7 id, so we can't pre-set it. Keep polling while
    /// the hosted process is alive, then do one final check on exit, so a rollout
    /// that flushes after startup still gets a crash-recovery row.
    fn record_codex_fresh(
        &self,
        id: SessionId,
        slug: String,
        cwd: std::path::PathBuf,
        pre: std::collections::HashSet<String>,
        profile_id: Option<i64>,
        codex_home: Option<std::path::PathBuf>,
    ) {
        Self::record_fresh_agent_session(
            id,
            slug,
            "codex",
            cwd,
            pre,
            self.store.clone(),
            self.host.clone(),
            move |cwd, since, exclude| {
                orchestrator_core::scan::newest_codex_id_for_cwd(
                    cwd,
                    since,
                    exclude,
                    codex_home.as_deref(),
                )
            },
            profile_id,
        );
    }

    fn record_fresh_agent_session(
        id: SessionId,
        slug: String,
        kind: &'static str,
        cwd: std::path::PathBuf,
        pre: std::collections::HashSet<String>,
        store: Arc<std::sync::Mutex<Store>>,
        host: Arc<dyn SessionBackend>,
        newest_id_for_cwd: impl Fn(&std::path::Path, u64, &std::collections::HashSet<String>) -> Option<String>
            + Send
            + 'static,
        profile_id: Option<i64>,
    ) {
        let since = orchestrator_core::registry::now_secs();
        std::thread::spawn(move || {
            let mut seen_session = false;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(500));
                let try_record = || -> bool {
                    let Some(cid) = newest_id_for_cwd(&cwd, since, &pre) else {
                        return false;
                    };
                    if let Ok(s) = store.lock() {
                        let _ = s.record_session(&cid, &slug, kind, &cwd.to_string_lossy(), profile_id);
                    }
                    host.set_cli_session_id(id, cid);
                    true
                };
                if try_record() {
                    break;
                }
                let info = host.infos().into_iter().find(|i| i.id == id);
                match fresh_session_discovery_next(
                    info.as_ref().map(|i| (i.alive, i.cli_session_id.is_some())),
                    seen_session,
                ) {
                    FreshDiscovery::Continue { seen } => seen_session = seen,
                    FreshDiscovery::Stop => {
                        let _ = try_record();
                        break;
                    }
                }
            }
        });
    }

    /// Move a live session to another project (#10 dogfooding: e.g. a session
    /// spawned in `teams` that's really building `orchestrator`). Rebinds
    /// host-side, updates the crash-record + durable override so resumes and
    /// summaries follow, and lands you on the session in its new home.
    pub(crate) fn move_session_to(
        &mut self,
        id: SessionId,
        cli_id: Option<String>,
        dest: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.host.rebind(id, dest);
        if let Some(cli) = cli_id {
            if let Ok(store) = self.store.lock() {
                // future resumes + summaries file under the new project.
                let _ = store.set_override(&cli, dest);
                let _ = store.rebind_session(&cli, dest);
            }
            self.overrides.insert(cli, dest.to_string());
        }
        self.focus_session(&dest.to_string(), id, window, cx);
    }

}

#[cfg(test)]
mod tests {
    // Selective imports (crate-wide pattern; a `use super::*` re-globs `crate::*`
    // and trips the crate recursion limit — see kickoff.rs's mod tests).
    use super::{
        apply_profile, apply_profile_argv, apply_profile_resume_argv, default_profile_key,
        reconcile_default_profile_keys, resolve_spawn_profile, ProfilePick,
    };
    use orchestrator_host::pty::SpawnSpec;
    use orchestrator_host::session::CliKind;
    use orchestrator_store::{ProfileRow, Store};
    use std::collections::HashMap;

    fn profile_with(config_dir: Option<&str>) -> ProfileRow {
        let mut env = HashMap::new();
        env.insert("A".to_string(), "B".to_string());
        ProfileRow {
            id: 1,
            label: "acct".into(),
            cli_kind: "claude".into(),
            config_dir: config_dir.map(|s| s.to_string()),
            model: None,
            extra_args: Vec::new(),
            env,
            color: None,
        }
    }

    fn has(spec: &SpawnSpec, k: &str, v: &str) -> bool {
        spec.env.iter().any(|(ek, ev)| ek == k && ev == v)
    }

    #[test]
    fn apply_profile_claude_sets_config_dir_and_env() {
        let mut spec = SpawnSpec::program("claude", "/tmp");
        apply_profile(&mut spec, CliKind::Claude, &profile_with(Some("/x")));
        assert!(has(&spec, "CLAUDE_CONFIG_DIR", "/x"));
        assert!(has(&spec, "A", "B"));
        // the codex isolation var must NOT leak onto a claude spec.
        assert!(!spec.env.iter().any(|(k, _)| k == "CODEX_HOME"));
    }

    #[test]
    fn apply_profile_codex_uses_codex_home() {
        let mut spec = SpawnSpec::program("codex", "/tmp");
        apply_profile(&mut spec, CliKind::Codex, &profile_with(Some("/x")));
        assert!(has(&spec, "CODEX_HOME", "/x"));
        assert!(has(&spec, "A", "B"));
        assert!(!spec.env.iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"));
    }

    #[test]
    fn apply_profile_empty_config_dir_sets_no_isolation_var() {
        // an empty (or absent) config_dir must not push an isolation var, but the
        // profile's plain env still layers on.
        let mut spec = SpawnSpec::program("claude", "/tmp");
        apply_profile(&mut spec, CliKind::Claude, &profile_with(Some("")));
        assert!(!spec.env.iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"));
        assert!(has(&spec, "A", "B"));
    }

    // ── #58: profile model + extra_args reach the argv ──────────────────────

    fn profile_argv(model: Option<&str>, extra: &[&str]) -> ProfileRow {
        ProfileRow {
            id: 1,
            label: "acct".into(),
            cli_kind: "claude".into(),
            config_dir: None,
            model: model.map(|s| s.to_string()),
            extra_args: extra.iter().map(|s| s.to_string()).collect(),
            env: HashMap::new(),
            color: None,
        }
    }

    /// The value that follows `flag` in the argv, if the flag is there at all.
    /// Positional (not a contains check) so a model value can never be mistaken
    /// for the flag being applied.
    fn flag_value(spec: &SpawnSpec, flag: &str) -> Option<String> {
        spec.args
            .windows(2)
            .find(|w| w[0] == flag)
            .map(|w| w[1].clone())
    }

    #[test]
    fn apply_profile_argv_claude_sets_model_flag_and_env() {
        let mut spec = SpawnSpec::program("claude", "/tmp");
        apply_profile_argv(&mut spec, CliKind::Claude, &profile_argv(Some("opus"), &[]));
        assert_eq!(flag_value(&spec, "--model").as_deref(), Some("opus"));
        // the env twin: spawn_claude rebuilds argv, so this is the leg that
        // actually reaches the process today (see apply_profile_argv's doc).
        assert!(has(&spec, "ANTHROPIC_MODEL", "opus"));
        assert!(flag_value(&spec, "-m").is_none());
    }

    #[test]
    fn apply_profile_argv_codex_uses_short_flag_and_no_env() {
        let mut spec = SpawnSpec::program("codex", "/tmp");
        apply_profile_argv(
            &mut spec,
            CliKind::Codex,
            &profile_argv(Some("gpt-5-codex"), &[]),
        );
        assert_eq!(flag_value(&spec, "-m").as_deref(), Some("gpt-5-codex"));
        assert!(flag_value(&spec, "--model").is_none());
        // ANTHROPIC_MODEL is a claude-only channel — it must not leak onto codex.
        assert!(!spec.env.iter().any(|(k, _)| k == "ANTHROPIC_MODEL"));
    }

    #[test]
    fn apply_profile_argv_shell_ignores_model() {
        let mut spec = SpawnSpec::shell("/tmp");
        let before = spec.args.clone();
        apply_profile_argv(&mut spec, CliKind::Shell, &profile_argv(Some("opus"), &[]));
        // a shell has no model concept: argv untouched, no stray env var.
        assert_eq!(spec.args, before);
        assert!(!spec.env.iter().any(|(k, _)| k == "ANTHROPIC_MODEL"));
    }

    #[test]
    fn apply_profile_argv_blank_model_adds_no_flag() {
        // "" / whitespace means "account default" — passing `--model ""` would
        // make the CLI reject the call, so the flag must be omitted entirely.
        for blank in [Some(""), Some("   "), None] {
            for kind in [CliKind::Claude, CliKind::Codex] {
                let mut spec = SpawnSpec::program("x", "/tmp");
                apply_profile_argv(&mut spec, kind, &profile_argv(blank, &[]));
                assert!(spec.args.is_empty(), "{blank:?} produced argv");
                assert!(!spec.env.iter().any(|(k, _)| k == "ANTHROPIC_MODEL"));
            }
        }
    }

    #[test]
    fn apply_profile_argv_appends_extra_args_after_the_model_flag() {
        let mut spec = SpawnSpec::program("claude", "/tmp");
        apply_profile_argv(
            &mut spec,
            CliKind::Claude,
            &profile_argv(Some("opus"), &["--verbose", "--foo=bar"]),
        );
        assert_eq!(
            spec.args,
            vec!["--model", "opus", "--verbose", "--foo=bar"],
            "extra_args must ride last — the user's final word"
        );
    }

    #[test]
    fn apply_profile_argv_extra_args_apply_without_a_model() {
        // extra_args are independent of the model field, and reach a shell too.
        let mut spec = SpawnSpec::shell("/tmp");
        apply_profile_argv(&mut spec, CliKind::Shell, &profile_argv(None, &["-x"]));
        assert_eq!(spec.args.last().map(String::as_str), Some("-x"));
    }

    #[test]
    fn resume_argv_keeps_the_claude_model_so_a_session_cant_change_model() {
        // the asymmetry this pair exists to prevent: spawned on one model,
        // resumed on the account default.
        let mut spec = SpawnSpec::program("claude", "/tmp");
        apply_profile_resume_argv(
            &mut spec,
            CliKind::Claude,
            &profile_argv(Some("opus"), &["--verbose"]),
        );
        assert_eq!(flag_value(&spec, "--model").as_deref(), Some("opus"));
        assert!(has(&spec, "ANTHROPIC_MODEL", "opus"));
        assert!(spec.args.iter().any(|a| a == "--verbose"));
    }

    #[test]
    fn resume_argv_omits_the_codex_model_but_keeps_extra_args() {
        // Host::resume_codex's measured guard: a forced -m 400s on a ChatGPT
        // plan, and the rollout already carries the model it was born under.
        let mut spec = SpawnSpec::program("codex", "/tmp");
        apply_profile_resume_argv(
            &mut spec,
            CliKind::Codex,
            &profile_argv(Some("gpt-5-codex"), &["-c", "x=1"]),
        );
        assert!(flag_value(&spec, "-m").is_none());
        assert_eq!(spec.args, vec!["-c", "x=1"]);
    }

    // ── #56: default profile per CLI ────────────────────────────────────────

    /// RULE ZERO: an in-memory store — no tempdir, no file, nothing under the
    /// developer's home, and no CLI is ever spawned by these tests.
    fn store() -> Store {
        Store::open_in_memory().expect("in-memory store")
    }

    fn make_profile(s: &Store, label: &str, cli_kind: &str) -> i64 {
        s.create_profile(label, cli_kind, None, None, &[], &HashMap::new(), None)
            .expect("create profile")
    }

    #[test]
    fn resolve_spawn_profile_uses_the_per_cli_default_when_the_caller_makes_no_pick() {
        let s = store();
        let claude_id = make_profile(&s, "work", "claude");
        let codex_id = make_profile(&s, "personal", "codex");
        s.set_setting("default_profile_claude", &claude_id.to_string())
            .unwrap();
        s.set_setting("default_profile_codex", &codex_id.to_string())
            .unwrap();

        assert_eq!(
            resolve_spawn_profile(&s, CliKind::Claude, ProfilePick::Default).map(|p| p.id),
            Some(claude_id)
        );
        // each key is per-CLI: claude's default must never answer for codex.
        assert_eq!(
            resolve_spawn_profile(&s, CliKind::Codex, ProfilePick::Default).map(|p| p.id),
            Some(codex_id)
        );
    }

    #[test]
    fn resolve_spawn_profile_explicit_pick_beats_the_default() {
        let s = store();
        let default_id = make_profile(&s, "work", "claude");
        let picked_id = make_profile(&s, "personal", "claude");
        s.set_setting("default_profile_claude", &default_id.to_string())
            .unwrap();

        assert_eq!(
            resolve_spawn_profile(&s, CliKind::Claude, ProfilePick::Profile(picked_id))
                .map(|p| p.id),
            Some(picked_id)
        );
    }

    /// Exactly what `spawn_session` does with a pick: resolve it, then layer the
    /// row onto the spec — or leave the spec alone when there is no row. Also
    /// hands back the id `record_session` would stamp, since that is what the
    /// resume leg reopens under. Pure: nothing is spawned.
    fn spec_for(s: &Store, kind: CliKind, pick: ProfilePick) -> (SpawnSpec, Option<i64>) {
        let profile = resolve_spawn_profile(s, kind, pick);
        let mut spec = SpawnSpec::program(kind.label(), "/tmp");
        if let Some(p) = &profile {
            apply_profile(&mut spec, kind, p);
            apply_profile_argv(&mut spec, kind, p);
        }
        (spec, profile.map(|p| p.id))
    }

    #[test]
    fn ambient_pick_spawns_with_no_profile_even_when_a_default_is_set() {
        // #62.3: the deliberate escape hatch. With a default set, `Default` is
        // still the one-click path the "+" menu's plain row takes — but the
        // ambient row has to reach the user's own login, which the old
        // `None`-means-default API could not express at all.
        let s = store();
        let mut env = HashMap::new();
        env.insert("A".to_string(), "B".to_string());
        let id = s
            .create_profile(
                "work",
                "claude",
                Some("/acct/work"),
                Some("opus"),
                &[],
                &env,
                None,
            )
            .expect("create profile");
        s.set_setting("default_profile_claude", &id.to_string())
            .unwrap();

        // DEFAULT: isolation dir + profile env + model, and the RESOLVED id, so
        // the resume leg reopens the session on the same account.
        let (spec, recorded) = spec_for(&s, CliKind::Claude, ProfilePick::Default);
        assert!(has(&spec, "CLAUDE_CONFIG_DIR", "/acct/work"));
        assert!(has(&spec, "A", "B"));
        assert_eq!(flag_value(&spec, "--model").as_deref(), Some("opus"));
        assert_eq!(recorded, Some(id));

        // AMBIENT: none of it is layered — no isolation var, no profile env, no
        // model — and profile_id stays None, so resume and codex birth-discovery
        // stay on the ambient account too.
        let (spec, recorded) = spec_for(&s, CliKind::Claude, ProfilePick::Ambient);
        assert!(!spec
            .env
            .iter()
            .any(|(k, _)| k == "CLAUDE_CONFIG_DIR" || k == "A" || k == "ANTHROPIC_MODEL"));
        assert!(spec.args.is_empty(), "ambient must add no argv");
        assert_eq!(recorded, None);

        // ...and a claude default still never reaches a codex spawn: the kind
        // filter this rework must not regress.
        let (spec, recorded) = spec_for(&s, CliKind::Codex, ProfilePick::Default);
        assert!(!spec.env.iter().any(|(k, _)| k == "CODEX_HOME"));
        assert_eq!(recorded, None);
    }

    #[test]
    fn resolve_spawn_profile_stale_default_falls_back_to_ambient() {
        // the profile the setting names was deleted: spawn ambient rather than
        // hand back a dangling id that resume + codex discovery would chase.
        let s = store();
        let id = make_profile(&s, "work", "claude");
        s.set_setting("default_profile_claude", &id.to_string())
            .unwrap();
        s.delete_profile(id).expect("delete profile");

        assert!(resolve_spawn_profile(&s, CliKind::Claude, ProfilePick::Default).is_none());
        // a stale EXPLICIT pick folds the same way.
        assert!(resolve_spawn_profile(&s, CliKind::Claude, ProfilePick::Profile(id)).is_none());
    }

    #[test]
    fn resolve_spawn_profile_absent_blank_or_garbage_default_is_ambient() {
        let s = store();
        make_profile(&s, "work", "claude");
        // unset
        assert!(resolve_spawn_profile(&s, CliKind::Claude, ProfilePick::Default).is_none());
        for raw in ["", "   ", "not-a-number"] {
            s.set_setting("default_profile_claude", raw).unwrap();
            assert!(
                resolve_spawn_profile(&s, CliKind::Claude, ProfilePick::Default).is_none(),
                "{raw:?} must not resolve"
            );
        }
    }

    #[test]
    fn resolve_spawn_profile_shell_has_no_default_key() {
        // a shell has no account concept, so it can never adopt a default —
        // even with both CLI keys set.
        let s = store();
        let id = make_profile(&s, "work", "claude");
        s.set_setting("default_profile_claude", &id.to_string())
            .unwrap();
        s.set_setting("default_profile_codex", &id.to_string())
            .unwrap();

        assert!(default_profile_key(CliKind::Shell).is_none());
        assert!(resolve_spawn_profile(&s, CliKind::Shell, ProfilePick::Default).is_none());
        // an explicit pick still applies to a shell (it carries plain env).
        assert_eq!(
            resolve_spawn_profile(&s, CliKind::Shell, ProfilePick::Profile(id)).map(|p| p.id),
            Some(id)
        );
    }

    #[test]
    fn resolve_spawn_profile_ignores_a_default_whose_kind_no_longer_matches() {
        // the profile was made the claude default, then EDITED to be a codex
        // profile. Settings filters the picker by kind and shows "(ambient
        // account)"; spawn must agree, or ⌘T runs claude with CLAUDE_CONFIG_DIR
        // pointed at what is now a codex account folder.
        let s = store();
        let id = make_profile(&s, "work", "claude");
        s.set_setting("default_profile_claude", &id.to_string())
            .unwrap();
        s.update_profile(id, "work", "codex", None, None, &[], &HashMap::new(), None)
            .expect("flip kind");

        assert!(resolve_spawn_profile(&s, CliKind::Claude, ProfilePick::Default).is_none());
        // ...and it does NOT silently become the codex default either — only the
        // key it was actually written into can name it.
        assert!(resolve_spawn_profile(&s, CliKind::Codex, ProfilePick::Default).is_none());
        // an EXPLICIT pick is unfiltered on purpose: the spawn menu derives the
        // kind from the row, and a shell borrows any profile for its env.
        assert_eq!(
            resolve_spawn_profile(&s, CliKind::Claude, ProfilePick::Profile(id)).map(|p| p.id),
            Some(id)
        );
    }

    #[test]
    fn reconcile_clears_only_the_keys_the_row_no_longer_belongs_in() {
        let s = store();
        let id = make_profile(&s, "work", "claude");
        s.set_setting("default_profile_claude", &id.to_string())
            .unwrap();
        // a kind flip to codex invalidates the CLAUDE key only.
        reconcile_default_profile_keys(&s, id, Some("codex"));
        assert_eq!(s.get_setting("default_profile_claude").as_deref(), Some(""));

        // a row that stays claude keeps its default.
        s.set_setting("default_profile_claude", &id.to_string())
            .unwrap();
        reconcile_default_profile_keys(&s, id, Some("claude"));
        assert_eq!(
            s.get_setting("default_profile_claude")
                .and_then(|v| v.parse::<i64>().ok()),
            Some(id)
        );

        // a DELETE (kind = None) clears every key naming it, and never touches a
        // key naming somebody else.
        let other = make_profile(&s, "personal", "codex");
        s.set_setting("default_profile_codex", &other.to_string())
            .unwrap();
        reconcile_default_profile_keys(&s, id, None);
        assert_eq!(s.get_setting("default_profile_claude").as_deref(), Some(""));
        assert_eq!(
            s.get_setting("default_profile_codex")
                .and_then(|v| v.parse::<i64>().ok()),
            Some(other)
        );
    }

    #[test]
    fn default_profile_keys_match_the_settings_ui_contract() {
        // these exact strings are the wire between Settings and spawn — a
        // rename here silently disables every default profile.
        assert_eq!(
            default_profile_key(CliKind::Claude),
            Some("default_profile_claude")
        );
        assert_eq!(
            default_profile_key(CliKind::Codex),
            Some("default_profile_codex")
        );
    }
}
