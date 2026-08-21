//! Process startup: embedded assets, the store handle, the keymap + macOS menu,
//! and the two OS windows (main + Settings).
//!
//! This is the wiring that runs exactly ONCE, before any surface exists — it is
//! not app logic and nothing renders it, so it lives away from the view. `main()`
//! stays in main.rs (Rust requires the binary's entry point at the crate root)
//! and does nothing but call `run()`.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use gpui::*;
use orchestrator_store::Store;

use crate::winchrome::{
    clamp_sidebar_w, parse_win_bounds, WinBoundsWatch, WinSample, MAIN_WIN_BOUNDS_KEY,
    MAIN_WIN_MIN, SETTINGS_WIN_BOUNDS_KEY, SETTINGS_WIN_MIN, SIDEBAR_W_DEFAULT,
};
use crate::*;

/// Embedded assets (the Kod mark) — gpui resolves `svg().path(...)` here.
pub(crate) struct Assets;
impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        Ok(match path {
            "logo/kod.svg" => Some(std::borrow::Cow::Borrowed(include_bytes!(
                "../assets/logo/kod.svg"
            ))),
            _ => None,
        })
    }
    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

/// Open (or create) the DESIGN-tree store at the app-support path.
fn open_store() -> Arc<std::sync::Mutex<Store>> {
    let dir = std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join("Library/Application Support/orchestrator"))
        .unwrap_or_else(|_| std::env::temp_dir().join("orchestrator"));
    let _ = std::fs::create_dir_all(&dir);
    // The store outgrew its first job (the design tree) — it now holds projects,
    // restore rows, overrides, and the activity log. One-time rename to a general
    // name; only if the new file doesn't exist yet (no-op once migrated, no data
    // loss). Move the WAL sidecars too so no committed pages are orphaned.
    let path = dir.join("store.db");
    let legacy = dir.join("design.db");
    if !path.exists() && legacy.exists() {
        let _ = std::fs::rename(&legacy, &path);
        for sfx in ["-wal", "-shm"] {
            let from = dir.join(format!("design.db{sfx}"));
            if from.exists() {
                let _ = std::fs::rename(&from, dir.join(format!("store.db{sfx}")));
            }
        }
    }
    let store =
        Store::open(&path).unwrap_or_else(|_| Store::open_in_memory().expect("in-memory store"));
    Arc::new(std::sync::Mutex::new(store))
}

fn setting_flag(v: Option<String>) -> bool {
    matches!(v.as_deref(), Some("1" | "true" | "yes" | "on"))
}

/// The whole of startup. Called by `main()` and nothing else.
pub(crate) fn run() {
    Application::new().with_assets(Assets).run(|cx: &mut App| {
        // Global keyboard map (docs/013 §4). ⌃` folds the terminal, ⇧⌃`
        // expands the stage, ⌘T/⇧⌘T/⌥⌘T spawn claude/codex/shell.
        //
        // Every binding stays CONTEXT-FREE, including inside the Settings window
        // where most of them have no handler (#62.5, decided rather than
        // inherited). gpui matches a binding against the focused window's
        // dispatch path and then looks for a handler on it: with no handler the
        // action is simply dropped, so ⌘T/⌘K/⌘F/⌘G/⌃` in Settings already do
        // nothing — which is the behaviour we want, since none of them name
        // anything the Settings window contains. Scoping them out with a
        // `KeyContext` would produce the identical user-visible result while
        // adding a stringly-typed contract between every binding and the main
        // window's root element: get the string wrong (or rename the context)
        // and the shortcuts go dead app-wide, silently. Absence of a handler
        // can't rot that way, so absence is the scope.
        cx.bind_keys([
            KeyBinding::new("ctrl-`", ToggleTerminal, None),
            KeyBinding::new("shift-ctrl-`", StageTerminal, None),
            KeyBinding::new("cmd-t", NewClaude, None),
            KeyBinding::new("cmd-shift-t", NewCodex, None),
            KeyBinding::new("cmd-alt-t", NewShell, None),
            KeyBinding::new("cmd-,", ToggleSettings, None),
            // ⌘W is bound app-wide but ONLY the Settings window's root handles it
            // (#54). Elsewhere it matches, finds no handler, propagates, and does
            // exactly what it does today: nothing. There is no close-without-quit
            // for the main window, so giving it one here would be a lie.
            KeyBinding::new("cmd-w", CloseSettings, None),
            KeyBinding::new("cmd-f", FindInTerminal, None),
            KeyBinding::new("cmd-k", TogglePalette, None),
            KeyBinding::new("cmd-g", FindNext, None),
            KeyBinding::new("cmd-shift-g", FindPrev, None),
        ]);
        // Native macOS menu bar: an app menu with Settings (⌘,) + Close Settings
        // (⌘W). Both dispatch actions — ToggleSettings to the GLOBAL listener
        // registered below (never to the Orchestrator root), CloseSettings to the
        // Settings window's own root, which is why macOS grays it out whenever
        // that window isn't the active one.
        cx.set_menus(vec![Menu {
            name: "kod".into(),
            items: vec![
                MenuItem::action("Settings…", ToggleSettings),
                MenuItem::action("Close Settings", CloseSettings),
            ],
        }]);
        // The store opens BEFORE the window so the saved geometry can seed
        // WindowOptions (#52). The same handle moves into the view below, so
        // this is still exactly one store per process.
        let store = open_store();
        let displays: Vec<Bounds<Pixels>> = cx.displays().iter().map(|d| d.bounds()).collect();
        let saved_bounds = store
            .lock()
            .ok()
            .and_then(|s| s.get_setting(MAIN_WIN_BOUNDS_KEY))
            .and_then(|v| parse_win_bounds(&v, &displays, MAIN_WIN_MIN));
        // ORCH_DEMO pins the window at a known origin so a region screenshot can
        // crop to it without accessibility access (verification only) — it
        // deliberately outranks any restored geometry.
        let bounds = if std::env::var("ORCH_DEMO").is_ok() {
            Bounds {
                origin: point(px(60.), px(60.)),
                size: gpui::size(px(1240.), px(820.)),
            }
        } else if let Some(b) = saved_bounds {
            b
        } else {
            Bounds::centered(None, gpui::size(px(1240.), px(820.)), cx)
        };
        let main_window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("kod".into()),
                    ..Default::default()
                }),
                // The SAME const the restored geometry is validated against, for
                // the same reason Settings does it (#62.1): without an OS floor
                // the user can drag the window below the size `parse_win_bounds`
                // accepts, and every launch after that silently re-centers at
                // 1240x820 with the chosen size thrown away and nothing on
                // screen saying why.
                window_min_size: Some(gpui::size(px(MAIN_WIN_MIN.0), px(MAIN_WIN_MIN.1))),
                ..Default::default()
            },
            |window, cx| {
                let win_handle = window.window_handle();
                cx.new(|cx| {
                    // Attach to the session daemon (spawning it if needed) so
                    // sessions survive GUI restarts; falls back to in-process on
                    // ORCH_NO_DAEMON or any daemon failure (docs/018 §9, §13). The
                    // mode is shown in the UI so the user knows it's safe.
                    let (host, host_mode) = orchestrator_daemon::connect_or_spawn();
                    termview::drive_repaints(host.clone(), cx);
                    // Detect a NEW "needs you" every ~500ms → raise a toast + a macOS
                    // notification when unfocused (#4 slice 2). Own loop (not the
                    // generic drive_repaints) so it can touch Orchestrator state.
                    // Focus is read FRESH from the window each tick (not a render-
                    // synced cache, which goes stale when the window is minimized /
                    // not repainting) so the "unfocused" notification gate is reliable.
                    // ORCH_NOTIFY_TEST=1 → fire ONE notification a few seconds after
                    // boot. The notify path is otherwise only reachable by getting a
                    // real agent to ask a real permission question, which makes the
                    // interesting part — whether macOS attributes the banner to Kod
                    // or to "Script Editor" — almost untestable by hand. Dev-only,
                    // opt-in, fires once.
                    if std::env::var("ORCH_NOTIFY_TEST").is_ok() {
                        crate::macnotify::log_authorization_state();
                        cx.spawn(async move |_, _| {
                            // 15s, not 3s, and it announces itself. macOS does NOT
                            // show a banner for the FRONTMOST app unless the app
                            // installs a UNUserNotificationCenterDelegate returning
                            // .banner from willPresent — and this app installs none.
                            // Firing 3s after launch tested the one condition
                            // guaranteed to show nothing, while the REAL needs-you
                            // path only fires when Kod is not active (the `active`
                            // gate on tick_needs). The delay is so a tester can click
                            // away first and exercise the condition that ships.
                            eprintln!(
                                "[kod] notify test armed: SWITCH TO ANOTHER APP now — \
                                 firing in 15s (banners are suppressed while Kod is frontmost)"
                            );
                            Timer::after(std::time::Duration::from_secs(15)).await;
                            eprintln!("[kod] notify test: posting now");
                            crate::notify::needs_you(
                                "atlas · claude",
                                "Run `cargo test --workspace`? (this is a test notification)",
                            );
                        })
                        .detach();
                    }
                    cx.spawn(async move |this, cx| loop {
                        Timer::after(std::time::Duration::from_millis(500)).await;
                        // Same probe also samples geometry, so window-size
                        // persistence (#52, #62) needs no second timer. Geometry
                        // is per-window: each window has its own key and its own
                        // settle detector, sampled here and nowhere else.
                        let wb = cx
                            .update_window(win_handle, |_, window, _| WinSample::of(window))
                            .ok();
                        // Settings is transient, so its handle is read fresh each
                        // tick rather than captured: a captured one would go
                        // stale the first time the user closed that window, and
                        // every later sample would be of a window that is gone.
                        let swb = this
                            .update(cx, |o: &mut Orchestrator, _| o.settings_window)
                            .ok()
                            .flatten()
                            .and_then(|h| {
                                cx.update_window(h.into(), |_, window, _| WinSample::of(window))
                                    .ok()
                            });
                        // "Is Kod frontmost?" must be asked of the APP, not of the
                        // main window (#54): with Settings in its own window the
                        // main handle reports inactive while the user is looking
                        // straight at Kod, and tick_needs would raise a macOS
                        // notification for a session already on screen.
                        let active =
                            cx.update(|cx| cx.active_window().is_some()).unwrap_or(false);
                        if this
                            .update(cx, |this: &mut Orchestrator, cx| {
                                this.tick_window_bounds(wb);
                                this.tick_settings_window_bounds(swb);
                                this.tick_needs(active, cx)
                            })
                            .is_err()
                        {
                            // The Orchestrator entity is GONE — the main window was
                            // closed, dropping the root view (every other handle to
                            // it is weak). This tick loop is the badge's only
                            // writer, and the process does NOT exit with the window:
                            // gpui 0.2.2's mac platform never sets
                            // applicationShouldTerminateAfterLastWindowClosed and
                            // nothing here calls cx.quit(), so Kod keeps its Dock
                            // tile. Without this clear the badge freezes on its last
                            // count and points at asks that a windowless app cannot
                            // even show, for the rest of the login session.
                            #[cfg(target_os = "macos")]
                            crate::macnotify::set_dock_badge(0);
                            break;
                        }
                    })
                    .detach();
                    // `store` was opened above the window so its saved geometry
                    // could seed WindowOptions (#52); it moves into the view here.
                    // (no summary-revival hook: dead jobs no longer blacklist a
                    // session. A death now buys an escalating COOL-OFF that
                    // expires from the death's own timestamp — so recovery needs
                    // no startup sweep, and no longer depends on the failure
                    // matching one hardcoded legacy error string, which is what
                    // the deleted `revive_obsolete_summary_failures` did.)
                    // start with the instant seed; the real registry scan runs
                    // on a background thread and swaps in (~2.6s of fs/git).
                    // load manual-attach overrides before the store is moved in.
                    let overrides = store
                        .lock()
                        .ok()
                        .and_then(|s| s.overrides_map().ok())
                        .unwrap_or_default();
                    let claude_effort = store
                        .lock()
                        .ok()
                        .and_then(|s| s.get_setting("claude_effort"))
                        .unwrap_or_default();
                    // Rail width (#52) — clamped on read so a stale or hand-edited
                    // value can never restore an unusable (or invisible) rail.
                    let sidebar_w = store
                        .lock()
                        .ok()
                        .and_then(|s| s.get_setting("sidebar_w"))
                        .and_then(|v| v.trim().parse::<f32>().ok())
                        .map(clamp_sidebar_w)
                        .unwrap_or(SIDEBAR_W_DEFAULT);
                    let prompt_provider_setting = store
                        .lock()
                        .ok()
                        .and_then(|s| s.get_setting("prompt_provider"));
                    let prompt_plumbing_model = store
                        .lock()
                        .ok()
                        .and_then(|s| s.get_setting("prompt_plumbing_model"));
                    let prompt_structural_model = store
                        .lock()
                        .ok()
                        .and_then(|s| s.get_setting("prompt_structural_model"));
                    let prompt_config = extract::PromptConfig::from_settings(
                        prompt_provider_setting.as_deref(),
                        prompt_plumbing_model.as_deref(),
                        prompt_structural_model.as_deref(),
                    );
                    let prompt_provider = prompt_config.provider;
                    extract::set_prompt_config(prompt_config);
                    // summaries are OPT-IN (default off) and demo runs force off:
                    // a plain dev launch must never spend plan quota (critique #16).
                    let standup_seen0: u64 = store
                        .lock()
                        .ok()
                        .and_then(|s| s.get_setting("standup_seen_ms"))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    let summaries_on = store
                        .lock()
                        .ok()
                        .and_then(|s| s.get_setting("summaries_on"))
                        .as_deref()
                        != Some("0")
                        && std::env::var("ORCH_DEMO").is_err();
                    // needs-you toast lifetime (#2): unset / unparseable → 0 =
                    // permanent (a toast waits until ✕ or "Open in terminal ▸").
                    let toast_secs: u64 = store
                        .lock()
                        .ok()
                        .and_then(|s| s.get_setting("toast_secs"))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    // the manual rail order (#28): a JSON array of slugs (they're
                    // free-form paths, so never a delimiter-joined string). Absent
                    // or unparseable → empty = the registry's own order.
                    let project_order: Vec<String> = store
                        .lock()
                        .ok()
                        .and_then(|s| s.get_setting("project_order"))
                        .and_then(|v| serde_json::from_str(&v).ok())
                        .unwrap_or_default();
                    // auto-continue-on-limit-reset (docs/019): absent → OFF. The
                    // daemon is storage-free, so re-push the persisted choice once
                    // on attach — a fresh (or retired-then-respawned) daemon starts
                    // with the flag false until the GUI tells it otherwise.
                    let auto_continue = setting_flag(
                        store.lock().ok().and_then(|s| s.get_setting("auto_continue")),
                    );
                    let ac_fire_on_reset = setting_flag(
                        store.lock().ok().and_then(|s| s.get_setting("ac_fire_on_reset")),
                    );
                    host.set_auto_continue(auto_continue, ac_fire_on_reset);
                    // the PROJECTS FOLDER (#29): where "＋ new project" creates a
                    // project's own directory, and the root the scan/fold trust.
                    // Unset → ~/local (what the scan always hardcoded). Mirrored
                    // into core, which reads it from the pure/live layers that
                    // can't reach the store. Never created here: a boot must not
                    // silently mkdir in the user's home — the first project
                    // (or Settings) does it, visibly.
                    let projects_root: std::path::PathBuf = store
                        .lock()
                        .ok()
                        .and_then(|s| s.get_setting("projects_root"))
                        .map(std::path::PathBuf::from)
                        .filter(|p| p.is_absolute())
                        .unwrap_or_else(orchestrator_core::default_projects_root);
                    orchestrator_core::set_projects_root(projects_root.clone());
                    let mut o = Orchestrator {
                        screen: Screen::Standup,
                        selected: 0,
                        mode: default_workspace_mode(),
                        host,
                        host_mode,
                        term_focus: cx.focus_handle(),
                        root_focus: cx.focus_handle(),
                        active_session: std::collections::HashMap::new(),
                        sess_unreviewed: std::collections::HashSet::new(),
                        standup_updates_all: false,
                        standup_live_open: false,
                        standup_block_open: std::collections::HashSet::new(),
                        standup_earlier_open: false,
                        infos_cache: std::collections::HashMap::new(),
                        map_drag: None,
                        map_drop_deny: std::collections::HashSet::new(),
                        map_menu: None,
                        canvas_create_pin: None,
                        outline_edit: outlinepane::EditState::default(),
                        review: None,
                        outline_open_cache: std::collections::HashMap::new(),
                        outline_link_open: false,
                        dispatch_memo: std::cell::RefCell::new((
                            u64::MAX,
                            String::new(),
                            std::collections::HashMap::new(),
                        )),
                        map_root_cache: std::collections::HashMap::new(),
                        outline_open_pending: None,
                        standup_divider_ms: standup_seen0,
                        prev_screen: Screen::Standup,
                        settings_window: None,
                        settings_focus: None,
                        profile_draft: None,
                        setting_edit: None,
                        inline_caret: textedit::Caret::default(),
                        standup_expanded: std::collections::HashSet::new(),
                        standup_updates: std::cell::RefCell::new((u64::MAX, Vec::new())),
                        proj_updates: std::cell::RefCell::new((u64::MAX, std::collections::HashMap::new())),
                        proj_seen: std::cell::RefCell::new(std::collections::HashMap::new()),
                        breakdown_inflight: Arc::new(std::sync::Mutex::new(
                            std::collections::HashSet::new(),
                        )),
                        breakdown_err: Arc::new(std::sync::Mutex::new(None)),
                        palette: palette::PaletteState::default(),
                        palette_focus: cx.focus_handle(),
                        move_menu_open: false,
                        rail_new: None,
                        rail_new_kind: RailNewKind::Project,
                        rail_new_err: None,
                        rail_new_menu_open: false,
                        projects_root,
                        projects_root_err: None,
                        search: search::SearchState::default(),
                        search_rx: Arc::new(std::sync::Mutex::new(None)),
                        search_inflight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                        sess_summaries: std::collections::HashMap::new(),
                        summary_fresh: std::collections::HashSet::new(),
                        latest_turn_at: std::collections::HashMap::new(),
                        summaries_on,
                        toast_secs,
                        auto_continue,
                        ac_fire_on_reset,
                        sum_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                        sum_cooldown_until: Arc::new(AtomicU64::new(0)),
                        sum_job_times: Vec::new(),
                        tap_events_seen: 0,
                        tap_rows_written: 0,
                        last_beat_ms: 0,
                        prev_alive_cli: std::collections::HashSet::new(),
                        selection: None,
                        drag_anchor: None,
                        selection_session: None,
                        drag_from_px: None,
                        drag_moved: false,
                        scrollbar_drag: None,
                        sidebar_w,
                        sidebar_drag: None,
                        win_bounds: WinBoundsWatch::new(MAIN_WIN_BOUNDS_KEY),
                        settings_win_bounds: WinBoundsWatch::new(SETTINGS_WIN_BOUNDS_KEY),
                        ime_preedit: String::new(),
                        term_error: None,
                        last_resize: None,
                        projects: orchestrator_core::seed_projects(),
                        project_order,
                        scan_result: Arc::new(std::sync::Mutex::new(None)),
                        scanning: false,
                        scanned: false,
                        store,
                        focused_part: None,
                        extracting: None,
                        extract_slot: Arc::new(std::sync::Mutex::new(None)),
                        agentic: None,
                        agentic_slot: Arc::new(std::sync::Mutex::new(None)),
                        map_intel_times: Vec::new(),
                        cmd: CmdBar::default(),
                        cmd_focus: cx.focus_handle(),
                        backfilled: std::collections::HashSet::new(),
                        recoverable: Arc::new(std::sync::Mutex::new(Vec::new())),
                        overrides,
                        attach_picker: None,
                        recover_all: false,
                        claude_effort,
                        prompt_provider,
                        restore_offer: Vec::new(),
                        restore_dismissed: false,
                        restore_expanded: false,
                        spawn_menu_open: false,
                        summaries: std::collections::HashMap::new(),
                        summary_sink: Arc::new(std::sync::Mutex::new(Vec::new())),
                        seen_needs: std::collections::HashSet::new(),
                        active_toast: None,
                        last_persisted_seq: std::collections::HashMap::new(),
                        suggest_open: false,
                        triage_active: false,
                        triage_cursor: None,
                        triage_done_armed: false,
                    };
                    o.ensure_projects_in_store();
                    o.load_restore_offer();
                    o.start_scan(cx);
                    o.load_recoverable(cx);
                    // Dev affordance: ORCH_DEMO=term opens a workspace with a
                    // live shell (terminal verification); ORCH_DEMO=1 just pins
                    // the window (pinned above) and stays on the Standup.
                    if std::env::var("ORCH_DEMO").as_deref() == Ok("term") {
                        // B9: guard the empty portfolio (fresh OSS user / async
                        // scan) — unwrap_or(0) + o.projects[0] would panic on empty.
                        if let Some(i) = o.projects.iter().position(|p| p.path.is_some()) {
                            o.selected = i;
                            o.screen = Screen::Workspace;
                            o.mode = Mode::Agent;
                            let slug = o.projects[i].slug.clone();
                            let cwd = o.projects[i]
                                .path
                                .clone()
                                .unwrap_or_else(|| std::env::temp_dir());
                            if let Ok(id) = o.host.spawn_shell(slug.clone(), cwd) {
                                o.active_session.insert(slug, id);
                            }
                        }
                    }
                    // ORCH_DEMO=flow opens the orchestrator project's Workspace
                    // (the Flow-map gate) on the seed pipeline.
                    if std::env::var("ORCH_DEMO").as_deref() == Ok("flow") {
                        if let Some(i) = o.projects.iter().position(|p| {
                            p.slug.contains("orchestrator") || p.name == "orchestrator"
                        }) {
                            o.selected = i;
                            o.screen = Screen::Workspace;
                        }
                    }
                    o
                })
            },
        )
        .expect("open_window");
        // ⌘, and the app menu's "Settings…" are registered GLOBALLY, not on the
        // Orchestrator root (#54), for two reasons. (1) macOS grays a menu item
        // whose action is unreachable from the FOCUSED dispatch tree, and gpui
        // checks only the ACTIVE window — with Settings in its own window the
        // active tree is often not the Orchestrator's. A global registration
        // short-circuits that check and keeps the item enabled unconditionally.
        // (2) ⌘, must behave identically from either window.
        // The handle is WEAK on purpose: a global listener lives for the whole
        // process, so a strong one would pin the Orchestrator — host, store, every
        // session cache — alive after the main window closed.
        let orch = main_window
            .entity(cx)
            .expect("root view")
            .downgrade();
        // ORCH_DEMO=settings opens the Settings window at boot. It is the one
        // surface with no other headless route in: it lives in its own OS window
        // opened by ⌘, or a menu item, so screenshotting or eyeballing it
        // otherwise needs a human to press a key. Deferred so it opens after the
        // main window has finished taking its slot, exactly like the ⌘, path.
        // `settings` alone, or `settings:<section>` to land on a specific pane —
        // the pane is picked in SettingsWindow::new, so match the PREFIX here.
        if std::env::var("ORCH_DEMO")
            .is_ok_and(|v| v == "settings" || v.starts_with("settings:"))
        {
            let boot_orch = orch.clone();
            cx.defer(move |cx| toggle_settings(&boot_orch, cx));
        }
        cx.on_action::<ToggleSettings>(move |_, cx| {
            let orch = orch.clone();
            // Belt-and-braces against the re-entrancy footgun: a KEY-driven action
            // dispatches SYNCHRONOUSLY while the dispatching window is taken OUT of
            // the app's window map, so a liveness probe run inline would report a
            // live window as closed and open a duplicate. Deferring puts the window
            // back in its slot first. (The Settings window also swallows ⌘, on its
            // own root, so this handler normally doesn't run there at all.)
            cx.defer(move |cx| toggle_settings(&orch, cx));
        });
        cx.activate(true);
    });
}

/// ⌘, / the app menu's Settings item. A second Settings window is never right, so
/// an open one is RAISED rather than duplicated.
fn toggle_settings(orch: &WeakEntity<Orchestrator>, cx: &mut App) {
    let Some(orch) = orch.upgrade() else { return };
    if let Some(h) = orch.read(cx).settings_window {
        // `update` IS the liveness probe: gpui takes the window out of its slot
        // map and errors when there is nothing there, so a closed window is an Err
        // — and slotmap keys carry a version, so a recycled slot can never resolve
        // a stale handle to someone else's window. The typed handle then downcasts
        // the root view as a second guard. (`is_active` is NOT usable here: it
        // returns None both for "closed" and for "currently borrowed", so it would
        // report a live-but-drawing window as gone and open a duplicate.)
        if h.update(cx, |_, window, _| window.activate_window()).is_ok() {
            // raise Kod itself, not just the window: the per-window raise is only
            // makeKeyAndOrderFront:, which does not bring the app to the front.
            cx.activate(true);
            return;
        }
        // the user closed it — fall through and open a fresh one.
    }
    open_settings_window(&orch, cx);
}

/// Open the Settings window (#54): its own OS window with a left nav rail, not a
/// Screen stealing the workspace's `main` slot.
fn open_settings_window(orch: &Entity<Orchestrator>, cx: &mut App) {
    // Settings keeps its OWN geometry (#62), under its own key: it used to
    // reopen centered at 900×640 every time, so any resize was thrown away the
    // moment the window closed. Validated on the way back in against SETTINGS'
    // floor, so a corrupt value — or one saved on a monitor that has since been
    // unplugged — falls back to centered rather than reopening somewhere the
    // user can't click.
    let store = orch.read(cx).store.clone();
    let displays: Vec<Bounds<Pixels>> = cx.displays().iter().map(|d| d.bounds()).collect();
    let saved_bounds = store
        .lock()
        .ok()
        .and_then(|s| s.get_setting(SETTINGS_WIN_BOUNDS_KEY))
        .and_then(|v| parse_win_bounds(&v, &displays, SETTINGS_WIN_MIN));
    // ORCH_DEMO pins the MAIN window at 60,60 so a region screenshot can crop to
    // it without accessibility access. Pin Settings to the SAME origin rather than
    // centering: a centered window would land half-outside that crop, and sharing
    // the origin means the existing crop captures Settings whenever it's the
    // window under test. (Offsetting to the right instead would risk landing off
    // a small display, which is worse than overlapping.) It deliberately outranks
    // any restored geometry — and, since the pin is not the user's own layout,
    // `persist_win_bounds` refuses to save it back.
    let bounds = if std::env::var("ORCH_DEMO").is_ok() {
        Bounds {
            origin: point(px(60.), px(60.)),
            size: gpui::size(px(900.), px(640.)),
        }
    } else if let Some(b) = saved_bounds {
        b
    } else {
        Bounds::centered(None, gpui::size(px(900.), px(640.)), cx)
    };
    let orch_weak = orch.downgrade();
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // `title: Some(..)` also means appears_transparent = false, which is
            // what actually makes macOS DRAW the word "Settings" in the titlebar.
            titlebar: Some(TitlebarOptions {
                title: Some("Settings".into()),
                ..Default::default()
            }),
            // the 198px rail + the 640 body column + its 26px gutters have a real
            // floor; below it the cards reflow into unreadable slivers. The same
            // const validates a RESTORED geometry, so the window can never refuse
            // to reopen at a size it let the user drag to.
            window_min_size: Some(gpui::size(px(SETTINGS_WIN_MIN.0), px(SETTINGS_WIN_MIN.1))),
            // tabbing_identifier stays None so macOS can never merge Settings into
            // the main window as a native tab.
            ..Default::default()
        },
        |window, cx| {
            // this window's OWN keyboard sink. root_focus/term_focus are nodes in
            // the MAIN window's element tree; focusing them here would leave this
            // window's focus pointing at a node its dispatch tree does not contain.
            let focus = cx.focus_handle();
            focus.focus(window);
            let f = focus.clone();
            let _ = orch_weak.update(cx, |o, _| o.settings_focus = Some(f));
            // a mid-edit ⌘W or red-button close leaves setting_edit/profile_draft
            // dangling on the Orchestrator; on_window_should_close is the only hook
            // the USER-driven close path gives us (remove_window() does NOT fire
            // it, which is why every programmatic path calls the same fns itself).
            // Geometry is taken HERE, before the window goes away — the 500ms poll
            // needs a sample to repeat before it persists one, so a resize
            // followed straight by the red button would otherwise be lost (#62).
            let w = orch_weak.clone();
            window.on_window_should_close(cx, move |window, cx| {
                let wb = WinSample::of(window);
                let _ = w.update(cx, |o, cx| {
                    o.persist_settings_window_bounds(Some(wb));
                    o.close_settings_state(cx);
                });
                true
            });
            let o = orch_weak.clone();
            cx.new(|cx| settings::SettingsWindow::new(o, focus, cx))
        },
    );
    match opened {
        Ok(h) => {
            orch.update(cx, |o, _| o.settings_window = Some(h));
            cx.activate(true);
        }
        Err(e) => {
            // a swallowed failure here is a ⌘, that silently does nothing.
            let _ = orch.update(cx, |o, cx| {
                o.term_error = Some(format!("couldn't open Settings: {e}"));
                cx.notify();
            });
        }
    }
}
