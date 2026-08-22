//! orchestrator-gui — the native GPUI app (Focused Dark).
//! Two pillars: the STANDUP (reactive cross-project home) and the WORKSPACE
//! (Map + Outline split). Live projects come from a scan of ~/.claude/projects.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::*;
use orchestrator_core::Project;

use orchestrator_host::input::KeyInput;
use orchestrator_host::pty::SpawnSpec;
use orchestrator_host::session::CliKind;
use orchestrator_host::{
    DecisionView, PendingDecision, Phase, SessionBackend, SessionId, SessionInfo,
};
use orchestrator_store::{
    build_tree, DiffOp, HostedSessionRow, Lifecycle, MemoryBackend, MemoryDocument,
    Part as DesignPart, PartId, PartRef, ProfileRow, RetrievalIntent, RetrievalQuery, Store,
    SummaryRow, TreeNode,
};
use std::rc::Rc;
use std::sync::atomic::AtomicU64;

mod agentic;
mod background;
mod boot;
mod changeset;
mod changeset_review;
mod cmdbar;
mod cockpit;
mod cockpit_strip;
mod command_bar;
mod context_menu;
mod extract;
mod features;
mod ime;
mod kickoff;
#[cfg(target_os = "macos")]
mod macnotify;
mod mapview;
mod outline_edit;
mod outlinepane;
mod pastedrop;
mod palette;
mod palette_ops;
mod projdir;
mod rail_order;
mod ratelimit;
mod recover;
mod render_agent;
mod render_map;
mod render_sidebar;
mod render_standup;
mod render_workspace;
mod returnchannel;
mod scan;
mod search;
mod session_discovery;
mod settings;
mod spawn;
mod standup_plan;
mod summaries;
mod termgeom;
mod termview;
mod textedit;
mod theme;
mod timefmt;
mod triage;
mod winchrome;

use changeset::{changeset_kept, op_asserts_done, op_with_name_edit, plan_changeset_accept};
use features::MAP_ENABLED;
use session_discovery::{fresh_session_discovery_next, restore_row_kind, FreshDiscovery};
use spawn::ProfilePick;
use winchrome::{clamp_sidebar_w, SidebarDragAnchor};

// The render modules reach shared vocabulary through `use crate::*`, and
// `extract`/`termview` name a few of these as `crate::…` outright. Re-exporting
// here keeps the crate root the ONE place those names live, so splitting a god
// file stayed a file boundary and never became an API change.
pub(crate) use ratelimit::is_rate_limited;
pub(crate) use termgeom::*;
pub(crate) use theme::*;

// Keyboard actions (bound in boot::run(); dispatched on the root in render(),
// except ToggleSettings — that one is GLOBAL, see boot).
actions!(
    orchestrator,
    [
        ToggleTerminal,
        StageTerminal,
        NewClaude,
        NewCodex,
        NewShell,
        ToggleSettings,
        /// ⌘W. Bound app-wide but handled ONLY on the Settings window's root
        /// (#54), so it closes that window and stays inert everywhere else —
        /// the main window has no close-without-quitting semantics.
        CloseSettings,
        FindInTerminal,
        FindNext,
        FindPrev,
        TogglePalette
    ]
);

/// Which inline single-line editor the root key router feeds (#10).
#[derive(Clone, Copy, PartialEq, Eq)]
enum InlineTarget {
    RailIdea,
    Outline,
    /// a free-text field of the open profile draft (Phase 5, #61, #62) — gated
    /// by `profile_draft.editing`, which also names WHICH field the buffer is.
    ProfileField,
    /// a single string-valued setting with no other UI (Phase 4: the two
    /// prompt-model keys). Reusable — the concrete key rides `setting_edit.key`.
    SettingText,
}

/// Which free-text field of the profile draft the inline faux-input is attached
/// to (#61, #62). One gate for three fields, rather than a `_editing` bool per
/// field: exactly one of them can own the Settings window's keystream at a time,
/// and a set of bools makes "two at once" representable when it never is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DraftSlot {
    Label,
    /// the Custom… model id (#62) — an id we ship no preset for, typed by hand.
    Model,
    /// free-text argv appended to every spawn under this profile (#61).
    ExtraArgs,
}

/// An in-progress add/edit of a per-CLI account ("profile", Phase 5). `editing_id`
/// = Some when editing an existing row (Save → update_profile), None for a new
/// profile (Save → create_profile). `editing` gates the faux-input keystream
/// onto one of the three free-text fields (which double as its buffer), exactly
/// like `rail_new` gates RailIdea. `env`/`color` are still NOT edited here
/// (advanced): Save preserves the existing row's values rather than wiping them.
#[derive(Clone)]
pub(crate) struct ProfileDraft {
    editing_id: Option<i64>,
    label: String,
    cli_kind: CliKind,
    config_dir: Option<String>,
    /// the model id a spawn adopts; "" = the account's own default, which is
    /// exactly what a NULL `model` column means. A String rather than an
    /// Option<String> because the Custom… faux-input types straight into it.
    model: String,
    /// argv appended to every spawn under this profile (#61), as the user typed
    /// it. Split into real argv by `parse_extra_args` on Save — the stored shape
    /// is a JSON array, and a shell-ish string is what a human can actually edit.
    extra_args: String,
    editing: Option<DraftSlot>,
}

/// An in-progress edit of one string-valued store setting (Phase 4) — the two
/// prompt-model keys are the only settings with no dedicated control. ONE
/// reusable faux-input target parameterized by `key`; Save writes
/// `store.set_setting(key, buf)`. `slot` is the inline-input's slot label.
pub(crate) struct SettingEdit {
    key: &'static str,
    slot: &'static str,
    buf: String,
}

/// What the rail's "+" menu creates. `Project` and `Idea` differ ONLY in whether
/// the project gets a directory now: a Project is `path:`-keyed from birth (its
/// folder exists on disk), an Idea is path-less until its first spawn — which
/// materializes a folder and promotes it to the same `path:` key. Both are typed
/// into the rail's inline name field (#29, #10).
///
/// `OpenFolder` (#34) is the odd one out: it takes NO typed name — it opens the
/// native folder picker directly and registers the CHOSEN folder in place. It
/// therefore never populates `rail_new`, so the inline-field code paths
/// (`commit_rail_new`, the rail hint) never see it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RailNewKind {
    Project,
    Idea,
    OpenFolder,
}

/// Settings is deliberately NOT a Screen (#54): it is its own OS window, so it
/// can't take the `main` slot away from the workspace and doesn't need a
/// "where do I go back to" field. What's left is the two real surfaces.
#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Standup,
    Workspace,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// the focused agent's stage — its curated stream / raw terminal (Agent Rail).
    /// The terminal is no longer a peek/expand drawer; it's a peer mode of the
    /// stage alongside the context views (#9 slice 2).
    Agent,
    /// Map+Outline: the product map with the focused area's outline beside it
    /// (the outline is a collapsible STATE of this one mode — docs/011 §A;
    /// Mode::Flow died with it).
    MapOutline,
    /// the per-project Recover view — rich recoverable cards (goal · summary ·
    /// last state · Resume), opened from the control bar's ⟲ button. Replaces the
    /// global Recover screen (#9 / task #6).
    Recover,
}

/// The workspace stage a project opens on. The Map is the user's default; the
/// OSS build (map compiled out — features.rs) has no map stage, so projects land
/// on the Agent stage instead — which matches the #9 "sessions as the main
/// workspace" reframe and has a clean "Pick an agent" empty state. Every
/// default-landing site routes through here so none can target a hidden stage.
pub(crate) fn default_workspace_mode() -> Mode {
    if MAP_ENABLED {
        Mode::MapOutline
    } else {
        Mode::Agent
    }
}

/// Within the Sessions view, a session shows either our curated STREAM (status,
/// activity, decision cards) or the raw TERMINAL (the real PTY) — the latter an
/// additional drill-in, never the only thing.

/// On-demand session-summary state (the Recover screen), keyed by session id.
#[derive(Clone)]
enum SummaryState {
    Running,
    Done(String),
    Failed(String),
}

struct Orchestrator {
    projects: Vec<Project>,
    /// the user's manual rail order (#28): slugs, top-first. Persisted as
    /// JSON under `project_order` and replayed over every rescan, which would
    /// otherwise reshuffle the rail by recency. A slug that isn't here (a fresh
    /// project) sorts to the tail. Empty = the registry's own order.
    project_order: Vec<String>,
    screen: Screen,
    selected: usize,
    mode: Mode,
    // --- terminal host (docs/013) ---
    /// the session backend — `SessionHost` in-process today (`Local`); swappable
    /// for a `RemoteHost` daemon client behind the same surface (docs/018).
    host: Arc<dyn SessionBackend>,
    /// daemon-attached vs in-process — shown so the user knows whether
    /// sessions survive a restart (dogfooding feedback).
    host_mode: orchestrator_daemon::HostMode,
    term_focus: FocusHandle,
    /// a focused root when the terminal isn't — the ROOT KEY ROUTER hangs off this
    /// node, and gpui only dispatches keys along the focused node's path, so
    /// nothing outside the terminal would get a keystroke without it. (It used to
    /// also keep the macOS Settings menu item enabled; that job now belongs to the
    /// GLOBAL ToggleSettings listener registered in `boot::run` — see #54.)
    root_focus: FocusHandle,
    /// the active session per project slug (which chip is selected).
    active_session: std::collections::HashMap<String, SessionId>,
    /// The ▲ WHAT HAPPENED tier has been expanded past its caps. Deliberately
    /// NOT persisted: "show me everything" is
    /// an answer to this morning's question, not a preference, and a standup
    /// that opens fully expanded every day is the flat list this replaced.
    standup_updates_all: bool,
    /// The ● LIVE tier is expanded to its per-session rows. Also transient:
    /// the collapsed strip is the point, and a persisted 'expanded' would
    /// restore the wall of rows every morning.
    standup_live_open: bool,
    /// Project blocks in ▲ WHAT HAPPENED whose "+N more" has been opened.
    /// Transient: an expanded block is an answer to one question, not a setting.
    standup_block_open: std::collections::HashSet<String>,
    /// The EARLIER group in ▲ WHAT HAPPENED is expanded. Collapsed by default
    /// whenever there IS something new — already-read projects are context, and
    /// context does not get to push the news off the screen.
    standup_earlier_open: bool,
    /// Bumped per pasted image so two pastes in the same millisecond cannot
    /// land on the same temp filename and silently overwrite each other.
    paste_seq: u64,
    /// Sessions that finished a turn you have not opened since (#13). A LEDGER,
    /// not a phase: phase is whatever the agent is doing now, this is whether
    /// YOU have caught up with it. Set in `persist_events` on a TurnEnd whose
    /// session you were not watching; cleared by `select_session`.
    sess_unreviewed: std::collections::HashSet<SessionId>,
    /// per-frame snapshot of every project's live sessions — populated ONCE at the
    /// top of render() so the sidebar/header/stage don't each re-lock the host
    /// 13-24×/frame (review fix). Read via cached_infos(); ~16ms stale in handlers.
    infos_cache: std::collections::HashMap<String, Vec<SessionInfo>>,
    /// live brain-map node drag (#10): anchor + live normalized pos; cleared on
    /// mouse-up, which is when store.set_part_pos persists it.
    map_drag: Option<mapview::MapDrag>,
    /// the live drag's ⌥-reparent DENY set (docs/019): the dragged node + its
    /// whole subtree — its satellites chase the preview, so they'd otherwise
    /// be the topmost hit, and dropping into them would cycle the tree.
    /// Sibling of map_drag (not a MapDrag field) to keep MapDrag Copy.
    map_drop_deny: std::collections::HashSet<PartId>,
    /// the canvas right-click context menu (docs/019 CANVAS): one Option =
    /// one menu, like the one-inline-edit contract.
    map_menu: Option<MapMenu>,
    /// dbl-click-create's pending pin (normalized, docs/019 CANVAS): captured
    /// at the double-click, applied by commit_outline_edit AFTER the Add
    /// lands (the node doesn't exist while the name is being typed).
    canvas_create_pin: Option<(f64, f64)>,
    /// the ONE inline outline edit (add part/decision/note, rename) — one
    /// Option = one edit at a time by construction (#10).
    outline_edit: outlinepane::EditState,
    /// the open changeset under review + its toggle-off / name-edit overlay
    /// (docs/019 slice 1c). None until the user faces a changeset; reset
    /// when a different changeset becomes the open one.
    review: Option<ChangesetReview>,
    /// per-project outline collapse (docs/011 §A): cache over the persisted
    /// `map_outline_open:<slug>` setting; absent = open.
    outline_open_cache: std::collections::HashMap<String, bool>,
    /// the focus card's "link a session ▸" expander (render-only pane state).
    outline_link_open: bool,
    /// (write_gen, slug) → dispatch map memo: the chip join runs per frame and
    /// the pulse animation repaints continuously while a session works.
    dispatch_memo: std::cell::RefCell<(u64, String, std::collections::HashMap<String, i64>)>,
    /// per-project drill root (docs/011 §B): the node whose children are the
    /// canvas' gen-0. Cache over the persisted `map_root:<slug>` setting.
    map_root_cache: std::collections::HashMap<String, Option<PartId>>,
    /// (write_gen → rows) memo for the Standup MAP UPDATES tier — the naive
    /// version ran load_tree PER ROW PER FRAME on the morning screen (review).
    #[allow(clippy::type_complexity)]
    standup_updates: std::cell::RefCell<(
        u64,
        Vec<(
            String,
            String,
            DiffOp,
            Option<String>,
            Option<PartId>,
            String,
            String,
        )>,
    )>,
    /// (write_gen → rows) memo for the Standup PORTFOLIO tier (docs/019 slice 4):
    /// each project's plain-words rollup line. Deterministic + cheap, but keyed
    /// by write_gen so gap_findings/next_up don't re-run every morning frame.
    /// Rows: (slug, name, rollup line).
    /// (write_gen → slug→newest-timeline-ms) memo: each project's last update,
    /// diffed against `proj_seen` to show an UNREAD cue on rail project titles
    /// (#50). Rebuilt only on a store write (mirrors `standup_updates`).
    proj_updates: std::cell::RefCell<(u64, std::collections::HashMap<String, u64>)>,
    /// slug → wall-ms the user last OPENED the project (persisted as the setting
    /// `proj_seen_ms:{slug}`; lazily read + cached here, stamped in select_project).
    proj_seen: std::cell::RefCell<std::collections::HashMap<String, u64>>,
    /// the Standup seen-ledger (docs/012 §3): divider ts for THIS visit
    /// (loaded on enter, stamped to the store on leave via prev_screen).
    standup_divider_ms: u64,
    prev_screen: Screen,
    /// the open Settings window (#54), if any. `WindowHandle` is Copy and keeps
    /// the window alive exactly zero — it is an id, so a stale one is harmless:
    /// `toggle_settings` probes it with `update(..).is_ok()` and falls through to
    /// opening a fresh window when the user has closed this one.
    settings_window: Option<WindowHandle<settings::SettingsWindow>>,
    /// the Settings window's own keyboard sink, mirrored here for the duration of
    /// that window. The shared settings listeners (Edit, ＋ Add profile) are
    /// ORCHESTRATOR listeners, so the `window` they receive is whichever window
    /// dispatched them — focusing `root_focus` from the Settings window would
    /// point its focus at a node that only exists in the main window's tree.
    settings_focus: Option<FocusHandle>,
    /// an in-progress add/edit of a profile (Phase 5). Some = the draft editor is
    /// open; `editing` inside it gates the Settings keystream onto one field.
    /// Lives here (not on the Settings view) because the shared `route_inline_key`
    /// mutates it; `close_settings_state` drops it when that window goes away.
    profile_draft: Option<ProfileDraft>,
    /// an in-progress edit of one string-valued setting (Phase 4: a prompt-model
    /// key). Some = its faux-input owns the Settings window's keystream.
    setting_edit: Option<SettingEdit>,
    /// The caret for whichever inline field is currently live. ONE, not one
    /// per field: `route_inline_key`'s if/else chain guarantees at most one
    /// editor is open at a time, so a second caret could only ever be stale.
    /// Re-seeded (to end-of-text) every time an editor opens or changes slot.
    inline_caret: textedit::Caret,
    /// expanded timeline rows (key = event ts_ms ^ kind ordinal).
    standup_expanded: std::collections::HashSet<u64>,
    /// deferred outline-open (slug, at_ms): opening INSTANTLY on click-1 of a
    /// double-click reflowed the canvas 980→560 and moved the card before
    /// click-2 landed (review HIGH). A click schedules the open; a completing
    /// drill cancels it; the tick applies it after the double-click window.
    outline_open_pending: Option<(String, u64)>,
    /// parts with a '◇ break down' proposal in flight (worker threads share it).
    breakdown_inflight: Arc<std::sync::Mutex<std::collections::HashSet<PartId>>>,
    /// worker-side breakdown failures, drained into term_error on the tick.
    breakdown_err: Arc<std::sync::Mutex<Option<String>>>,
    /// ⌘K recall/quick-add palette + its focus (chars ride on_key_down).
    palette: palette::PaletteState,
    palette_focus: FocusHandle,
    /// the session move-to-project picker is open (stage subhead ⇄, #10).
    move_menu_open: bool,
    /// the rail's inline "name…" input (Some = typing), and WHICH of the two
    /// rows opened it (#29): a PROJECT gets its own directory under
    /// `projects_root` and is `path:`-keyed from birth; an IDEA stays path-less
    /// until its first spawn, which promotes it.
    rail_new: Option<String>,
    rail_new_kind: RailNewKind,
    /// why the last rail creation failed (mkdir denied, name taken) — rendered
    /// under the ＋ rows, where the user is looking. Cleared on the next try.
    rail_new_err: Option<String>,
    /// the rail's ＋New menu is open (#67). The three creations it offers are
    /// genuinely different and none can be dropped, but a one-word rail row
    /// explained none of them and the rail is far too narrow to explain — so the
    /// choice lives in a menu, which has room for a sentence each.
    rail_new_menu_open: bool,
    /// where a new project's own directory is created (Settings → Projects
    /// folder; `projects_root`, default `~/local`). Mirrored into core, which
    /// scans/folds against it but can't read the store.
    projects_root: std::path::PathBuf,
    /// why the last projects-folder pick was REFUSED (e.g. `$HOME` itself, which
    /// would put every future project directly in the home folder — the exact
    /// thing #29 exists to prevent). Rendered under the picker.
    projects_root_err: Option<String>,
    /// ⌘F terminal search state + the worker handoff (results land off-thread).
    search: search::SearchState,
    search_rx:
        Arc<std::sync::Mutex<Option<(u64, Vec<orchestrator_host::emulator::SearchMatch>, u32)>>>,
    search_inflight: Arc<std::sync::atomic::AtomicBool>,
    /// #16 LLM summaries — latest per session (tick-refreshed from the store).
    sess_summaries: std::collections::HashMap<String, SummaryRow>,
    /// sessions whose latest summary COVERS their newest content (freshness
    /// gate: a stale summary must never outrank the raw line — critique #16).
    summary_fresh: std::collections::HashSet<String>,
    /// newest persisted event per session — the durable freshness anchor.
    latest_turn_at: std::collections::HashMap<String, u64>,
    /// summarizer state: opt-in flag (Settings, default OFF; ORCH_DEMO forces
    /// off), single-lane running flag, error cooldown (worker-set on rate
    /// limits), hourly budget window, and per-session attempt debounce.
    summaries_on: bool,
    /// needs-you toast lifetime in seconds; `0` = permanent (never auto-expires;
    /// only ✕ or "Open in terminal ▸" dismiss it). Persisted as toast_secs; read by
    /// tick_needs (#22).
    toast_secs: u64,
    /// global auto-continue-on-limit-reset flag (docs/019). Persisted as
    /// `auto_continue`; pushed to the storage-free daemon on change + on attach.
    /// Default OFF — it resumes a blocked session unattended.
    auto_continue: bool,
    /// config (default OFF): when auto_continue is on, FIRE on the resolved reset
    /// INSTANT (the working behavior) rather than the never-reached cleared-banner
    /// edge (audit B2 / task #31).
    ac_fire_on_reset: bool,
    sum_running: Arc<std::sync::atomic::AtomicBool>,
    sum_cooldown_until: Arc<AtomicU64>,
    sum_job_times: Vec<u64>,
    /// docs/019 slice 3 tap health (commitment 4): cumulative tool-use events
    /// observed vs weighted touch rows written — a silently-broken parser reads
    /// as "N tool events, 0 rows" (the format-drift alarm rides the truth meter).
    tap_events_seen: u64,
    tap_rows_written: u64,
    /// docs/019 slice 3 heartbeat gate (commitment 4): wall-clock ms of the last
    /// time the daemon was observed live. Stale → the ARE layer washes grey with
    /// a "live link lost" line (an honest empty beats a lying alive chip).
    last_beat_ms: u64,
    /// cli ids seen ALIVE last frame — the clean-exit observer (review #12):
    /// a session we watch transition alive→dead was exited on purpose, so its
    /// store row closes and the crash banner stays honest. A session that
    /// VANISHES (daemon died/retired with it) is a crash — row stays open.
    prev_alive_cli: std::collections::HashSet<String>,
    /// Terminal drag-selection (#9 3b): CHAR-granular ((row,col) anchor, (row,col)
    /// head) in ABSOLUTE grid coords; viewport-relative, so it's cleared on session/
    /// view switch, scroll, Esc, or any forwarded keystroke.
    selection: Option<((usize, usize), (usize, usize))>,
    drag_anchor: Option<(usize, usize)>,
    selection_session: Option<SessionId>,
    /// Click-vs-drag for link-open: the pixel where the press began + whether the
    /// pointer has since travelled past a small slop. A plain click OPENS a link
    /// only when the pointer never moved — cell-equality alone misreads an
    /// out-and-back drag, or a straight-down drag off the last row (the cell clamps
    /// while pixels keep moving), as a click. Both reset on each press.
    drag_from_px: Option<Point<Pixels>>,
    drag_moved: bool,
    /// live scrollbar thumb-drag (#9 3b step 5); cleared on mouse-up.
    scrollbar_drag: Option<ScrollDragAnchor>,
    /// Sidebar rail width in px (#52). User-resizable via the right-edge gutter
    /// and persisted as the `sidebar_w` setting. This is also the ONLY source of
    /// the rail's width for the PTY column computation — `apply_pty_geometry`
    /// subtracts it, so the terminal reflows as the rail moves.
    sidebar_w: f32,
    /// live sidebar resize-gutter drag; cleared (and persisted) on mouse-up.
    sidebar_drag: Option<SidebarDragAnchor>,
    /// Window-geometry settle detectors (#52, #62) — ONE PER WINDOW, each owning
    /// its own store key. A single shared detector would let a sample from one
    /// window cancel the other's pending sample, and a geometry that never
    /// settles is a geometry that never persists.
    win_bounds: winchrome::WinBoundsWatch,
    settings_win_bounds: winchrome::WinBoundsWatch,
    /// in-progress IME composition (preedit) for the focused terminal (#9 3c).
    /// Empty when not composing; the input-handler UTF-16 ranges index into this.
    ime_preedit: String,
    /// last spawn error (e.g. claude not on PATH), surfaced in the drawer.
    term_error: Option<String>,
    /// last geometry applied to the active session, so resize fires only on
    /// actual change (window resize / stage toggle) — never every frame.
    last_resize: Option<(u64, u16, u16)>,
    /// the real registry scan (M4) runs on a background thread; the result
    /// lands here and the poll task swaps it into `projects`.
    scan_result: Arc<std::sync::Mutex<Option<Vec<Project>>>>,
    scanning: bool,
    /// true once the first real registry scan has landed (until then the feed
    /// shows a quiet loading state, not the fabricated seed).
    scanned: bool,
    // --- the real DESIGN tree (M-flowmap, docs/016) ---
    store: Arc<std::sync::Mutex<Store>>,
    /// the focused part in the Workspace outline (by stable id, not index).
    focused_part: Option<PartId>,
    /// the project slug currently running an LLM structure extraction.
    extracting: Option<String>,
    /// background extraction result: (slug, Ok(ops) | Err(message)).
    extract_slot: Arc<std::sync::Mutex<Option<(String, Result<Vec<DiffOp>, String>)>>>,
    /// docs/019 slice 2: an in-flight agentic cartographer run (seed / re-ground
    /// / expand / rework / cmd-bar intent). Some => the "reading docs…" progress
    /// card shows; the user-invoked structural lane runs one at a time.
    agentic: Option<AgenticRun>,
    /// background cartographer result: (slug, Ok(landed changeset) | Err(msg)).
    #[allow(clippy::type_complexity)] // the worker-handoff slot shape (mirrors extract_slot)
    agentic_slot: Arc<std::sync::Mutex<Option<(String, Result<AgenticLanded, String>)>>>,
    /// the structural-lane rate ledger (docs/019 model routing T3): run-start
    /// secs, SEPARATE from the plumbing `sum_job_times` so a burst of summaries
    /// can never starve a user-invoked seed (map_intel_rate_allows / 6·hr).
    map_intel_times: Vec<u64>,
    /// the "talk to evolve" command bar (docs/019 COMMAND BAR): imperatives on
    /// the human lane, design intent on the machine lane. Its own focus like
    /// the palette (chars ride on_key_down while open).
    cmd: CmdBar,
    cmd_focus: FocusHandle,
    /// sessions whose transcript we've already backfilled into the timeline (#9 §4).
    backfilled: std::collections::HashSet<SessionId>,
    /// recoverable sessions discovered on disk (the import/recovery feature),
    /// loaded once on a background thread.
    recoverable: Arc<std::sync::Mutex<Vec<orchestrator_core::scan::RecoverableSession>>>,
    /// manual project-attach overrides (cli_session_id → project slug), loaded
    /// from the store at construction; consulted by `resolve_home`.
    overrides: std::collections::HashMap<String, String>,
    /// the recoverable session whose attach-picker is open (by id), if any.
    attach_picker: Option<String>,
    /// the Recover view is showing ALL sessions (not just this project's), so a
    /// mis-homed/orphan session can be found and filed under the right project.
    recover_all: bool,
    /// default effort for every claude this app starts or resumes ("" = off,
    /// low/…/xhigh, or "ultracode"). Persisted in the store; carried on
    /// SpawnSpec.effort and applied host-side in the session --settings file.
    claude_effort: String,
    /// provider for isolated background prompt calls (summaries, memory extraction,
    /// map proposals, seed/re-ground/expand/rework). Persisted in app_settings.
    prompt_provider: extract::PromptProvider,
    /// sessions left alive by the prior process (crash/left-open) — the
    /// restore-on-launch offer, read ONCE at construction (then cleared).
    restore_offer: Vec<orchestrator_store::HostedSessionRow>,
    restore_dismissed: bool,
    restore_expanded: bool,
    /// the control-bar "+" new-session dropdown (claude/codex/shell) open state.
    spawn_menu_open: bool,
    /// on-demand session summaries (Recover screen), keyed by session id.
    summaries: std::collections::HashMap<String, SummaryState>,
    /// background sink the summary threads write into; drained by the poll.
    summary_sink: Arc<std::sync::Mutex<Vec<(String, Result<String, String>)>>>,
    /// "needs you" sessions already surfaced, so the toast + macOS notification
    /// fire ONCE per new request (pruned to currently-waiting, so a re-ask after
    /// resolve fires again). #4 slice 2.
    seen_needs: std::collections::HashSet<SessionId>,
    /// the active needs-you toast overlay (auto-expires; replaced by a newer one).
    active_toast: Option<ToastData>,
    /// per-session max event seq already persisted to the store this run, so the
    /// digest writer only inserts NEW TurnEnd events (durable Today digest).
    last_persisted_seq: std::collections::HashMap<SessionId, u64>,
    /// docs/019 slice 4 (SHOULD): the collapsed "suggestions" gap drawer is
    /// expanded. Transient (per-session), reset on project switch — a drawer is
    /// a glance affordance, not a persisted preference.
    suggest_open: bool,
    /// docs/019 slice 4 (T7): the triage sweep is active on the Map — a keyboard
    /// mode walking nodes j/k, single-key status stamps written as human:triage.
    triage_active: bool,
    /// the triage cursor (by stable id) — the node the next t/i/x stamps.
    triage_cursor: Option<PartId>,
    /// DONE is the assertion that lies expensively (docs/019 T7), so it costs one
    /// EXTRA deliberate keystroke: the first `x` arms this, a second confirms.
    /// Any move (j/k) or other stamp (t/i) disarms it.
    triage_done_armed: bool,
}

/// The canvas context menu (docs/019 CANVAS, ruling 13: every verb
/// mouse-reachable): target node + where it opened (window coords — the
/// overlay is window-space, it must never clip inside the canvas scroller) +
/// which pane is showing. Submenus (kind/status/why) swap the panel body IN
/// PLACE rather than floating a second panel.
#[derive(Clone, Copy, PartialEq)]
struct MapMenu {
    id: PartId,
    at: Point<Pixels>,
    pane: MenuPane,
}

#[derive(Clone, Copy, PartialEq)]
enum MenuPane {
    Root,
    Kind,
    Status,
    Why,
}

/// The changeset the user is currently reviewing (docs/019 slice 1c T9).
/// The changeset ROWS live in the store (pending_diff linked by changeset_id);
/// this is the ephemeral review OVERLAY — which ops the user toggled off and
/// the Add-name edits typed before accepting. Keyed by changeset id: a newer
/// open changeset (or a resolved one) resets it. `off`/`names` index into
/// `flatten_changeset_ops`' stable global order. Accept rebuilds the kept
/// `Vec<DiffOp>` from here and applies it as ONE accept_diff_from (one ⌘Z).
#[derive(Default, Clone)]
struct ChangesetReview {
    id: i64,
    /// global op indices whose keep-state the user FLIPPED from its default
    /// (docs/019 slice 2). Default-kept for a plain op; default-EXCLUDED for a
    /// flagged (unverified) or `done` op — so "Accept all" skips those unless
    /// the user flips them in individually. `changeset_kept` computes the
    /// effective keep from this set + the op's flag/done status.
    off: std::collections::HashSet<usize>,
    /// edited proposed Add names, by global op index (edit-before-accept v1).
    names: std::collections::HashMap<usize, String>,
}

/// docs/019 slice 2: an in-flight agentic cartographer run — enough to render
/// the "reading docs…" progress card (a silent multi-minute call reads as a
/// hang) and to route the result to the right project.
struct AgenticRun {
    slug: String,
    /// what the card says ("Re-grounding from your docs & code…").
    label: String,
    /// the fence (None = whole map) — carried for context, not re-read.
    scope: Option<PartId>,
    started: std::time::Instant,
}

/// The cartographer's landed proposal, handed from the worker thread to the
/// poll that turns it into a carded changeset (never applied directly).
struct AgenticLanded {
    title: String,
    instruction: String,
    scope: Option<PartId>,
    origin_run: String,
    ops: Vec<DiffOp>,
    evidence: Vec<Option<String>>,
    flagged: Vec<bool>,
}

/// What kind of structural run the user invoked (docs/019 intelligence
/// pipeline): a whole-map seed/re-ground, a fenced expand/rework, or a
/// cmd-bar design intent (fenced or whole-map).
enum AgenticKind {
    /// whole map (scope NULL): fresh seed on an empty tree, delta re-ground
    /// otherwise; `intent` = the user's words when routed from the cmd bar.
    Seed { intent: Option<String> },
    /// fenced to `node`: expand (add children) or rework (restructure framing);
    /// `intent` = the one-line box / cmd-bar text.
    Fenced {
        node: PartId,
        rework: bool,
        intent: Option<String>,
    },
}

/// The "talk to evolve" command bar (docs/019 COMMAND BAR + ruling 9). One
/// input; the classifier splits imperatives (human lane) from design intent
/// (machine lane). Opened blank (⌘-typed) or forced to a node (context-menu
/// Expand/Rework). Its own focus while open.
#[derive(Default)]
struct CmdBar {
    open: bool,
    query: String,
    /// Some(node) => opened from the context menu, FORCED to the machine lane
    /// fenced to that node; `rework` picks the expand-vs-rework framing.
    node: Option<PartId>,
    rework: bool,
    /// waiting on the one scope keystroke (docs/019: the marquee must not
    /// dead-end) — (intent text, the selection to fence to). While Some, t/w
    /// answer "this subtree / whole map"; Esc cancels back to the input.
    scope_ask: Option<(String, PartId)>,
}

/// A transient "needs you" toast: enough to render the ping + its inline actions.
/// `slug` (not a project index) identifies the target so Open resolves the live
/// index at click-time — the projects list can reorder after a rescan.
struct ToastData {
    id: SessionId,
    slug: String,
    title: String,
    ask: String,
    /// wall-clock secs after which the toast auto-clears; `expire_at == 0` =
    /// PERMANENT — never auto-expires, only ✕ or "Open in terminal ▸" dismiss it (#2).
    expire_at: u64,
}

/// Best-effort native macOS notification (non-blocking) — fired when a new
/// decision needs you and the app window isn't focused (#4 slice 2). Spawned
/// fire-and-forget like the daemon's detach; never waited, never in tests.
mod notify {
    use std::process::{Command, Stdio};
    pub fn needs_you(agent: &str, ask: &str) {
        // Prefer the native, "Kod"-attributed UNUserNotificationCenter banner when
        // we're running from the signed `.app` bundle; fall back to osascript for a
        // bare `cargo run` (no bundle — the UN path would CRASH there, so
        // `post_needs_you` refuses and returns false).
        #[cfg(target_os = "macos")]
        if crate::macnotify::post_needs_you(agent, ask) {
            return;
        }
        // AppleScript string literals are double-quoted — escape backslash + quote
        // (no shell is involved: osascript -e takes the script as one argv).
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            "display notification \"{}\" with title \"Kod — needs you\" subtitle \"{}\"",
            esc(&ask.chars().take(160).collect::<String>()),
            esc(agent),
        );
        // Run + REAP in a detached thread: a dropped Child is never waited on
        // Unix, so a bare .spawn() would leave a zombie per notification. The
        // thread blocks on the ~50ms osascript, not the UI.
        std::thread::spawn(move || {
            let _ = Command::new("osascript")
                .arg("-e")
                .arg(script)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        });
    }
}

/// A shared, do-nothing project used when the portfolio is EMPTY — a fresh OSS
/// user with no Claude/Codex history and no projects yet. Lets `project()` fall
/// back instead of index-panicking, so every one of its ~45 callers degrades to a
/// blank project rather than crashing the app on the first render frame.
fn empty_project() -> &'static Project {
    static EMPTY: std::sync::OnceLock<Project> = std::sync::OnceLock::new();
    EMPTY.get_or_init(|| Project::idea("welcome", "No projects yet"))
}

impl Orchestrator {
    fn project(&self) -> &Project {
        self.projects
            .get(self.selected.min(self.projects.len().saturating_sub(1)))
            .unwrap_or_else(|| empty_project())
    }




    /// One source of truth for "needs you" — a hosted session awaiting a real
    /// decision (M3). Drives both the sidebar badge and the Standup header.
    fn needs_you_count(&self) -> usize {
        self.projects
            .iter()
            .filter(|p| self.live_overlay(&p.slug).2 > 0)
            .count()
    }








    /// Land a jump (⌘K, Standup 'map ▸', proposals banner) with the node
    /// actually VISIBLE: drill to its parent so it renders as gen-0, focus it,
    /// and make sure the outline is open (review: jumps left the target hidden
    /// behind a persisted drill root / collapsed outline).
    fn focus_node_on_map(&mut self, slug: &str, part: PartId, cx: &mut Context<Self>) {
        // OSS gate (features.rs): node jumps are a Map affordance. With the map
        // compiled out there is no stage to land on, so every jump caller (⌘K
        // recall, Standup 'map ▸', proposals banner) no-ops instead of blanking
        // the workspace. The affordances themselves are also hidden.
        if !MAP_ENABLED {
            return;
        }
        self.select_project(slug, cx);
        self.mode = Mode::MapOutline;
        self.focused_part = Some(part);
        let parent = self
            .store
            .lock()
            .ok()
            .and_then(|st| st.load_tree(slug).ok())
            .and_then(|parts| {
                parts
                    .iter()
                    .find(|p| p.id == part)
                    .and_then(|p| p.parent_id)
            });
        self.set_map_root(slug, parent);
        if !self.outline_open(slug) {
            self.set_outline_open(slug, true);
        }
    }

    fn map_root_of(&self, slug: &str) -> Option<PartId> {
        if let Some(v) = self.map_root_cache.get(slug) {
            return *v;
        }
        self.store
            .lock()
            .ok()
            .and_then(|st| st.get_setting(&format!("map_root:{slug}")))
            .and_then(|v| v.parse::<i64>().ok())
    }

    fn set_map_root(&mut self, slug: &str, root: Option<PartId>) {
        self.map_root_cache.insert(slug.to_string(), root);
        if let Ok(store) = self.store.lock() {
            let _ = match root {
                Some(id) => store.set_setting(&format!("map_root:{slug}"), &id.to_string()),
                None => store.set_setting(&format!("map_root:{slug}"), ""),
            };
        }
        self.map_drag = None;
    }

    fn outline_open(&self, slug: &str) -> bool {
        if let Some(v) = self.outline_open_cache.get(slug) {
            return *v;
        }
        self.store
            .lock()
            .ok()
            .and_then(|st| st.get_setting(&format!("map_outline_open:{slug}")))
            .map(|v| v != "0")
            .unwrap_or(true)
    }

    fn set_outline_open(&mut self, slug: &str, open: bool) {
        self.outline_open_cache.insert(slug.to_string(), open);
        if let Ok(store) = self.store.lock() {
            let _ = store.set_setting(
                &format!("map_outline_open:{slug}"),
                if open { "1" } else { "0" },
            );
        }
    }






    /// The REAL selection, validated against the live tree. A stale
    /// focused_part (its node deleted) is NOT a meaningful selection —
    /// docs/019 PALETTE: capture with no selection files to the idea tray,
    /// and Move-to/verbs must never operate on a ghost.
    fn live_selection(&self) -> Option<PartId> {
        let id = self.focused_part?;
        let slug = &self.project().slug;
        let alive = self
            .store
            .lock()
            .ok()
            .and_then(|s| s.load_tree(slug).ok())
            .is_some_and(|ps| ps.iter().any(|p| p.id == id));
        alive.then_some(id)
    }



    // ---- the "talk to evolve" command bar (docs/019 COMMAND BAR) ----
    // Imperatives ("rename auth to Identity") apply INSTANTLY on the human lane
    // with a one-line preview; design intent ("reorganize by area") routes to
    // the machine lane as a fenced changeset, asking the one scope keystroke
    // when the intent crosses the selection's subtree.




















    // ---- changeset review (docs/019 slice 1c T9): the MACHINE lane ----
    // A changeset is a named machine PROPOSAL reviewed as a diff-of-the-
    // document. Human edits stay instant/uncarded (slice 1b); this is the only
    // carded lane. Accept applies the KEPT ops (minus toggled-off, plus name
    // edits) as ONE accept_diff_from = one journal event = one ⌘Z.










    // ---- the canvas context menu's verbs (docs/019 CANVAS) ----
    // Each menu row and its bare-key accelerator share one body here, so the
    // two entrances can never drift apart (ruling 13: GUI-first, keys second).










    /// Jump to the project's HOTTEST session: first awaiting-decision, else
    /// first idle (come-drive-me), else any alive. The rail pill routes here so
    /// the thing that pulled your eye is one click, not header→Map→hunt (#12).
    fn focus_hottest(&mut self, slug: &str, window: &mut Window, cx: &mut Context<Self>) {
        use orchestrator_host::Phase;
        let pick = |ph: Phase| {
            self.cached_infos(slug)
                .iter()
                .find(|s| s.alive && s.phase == ph)
                .map(|s| s.id)
        };
        let target = pick(Phase::AwaitingDecision)
            .or_else(|| pick(Phase::Idle))
            .or_else(|| {
                self.cached_infos(slug)
                    .iter()
                    .find(|s| s.alive)
                    .map(|s| s.id)
            });
        if let Some(id) = target {
            self.focus_session(slug, id, window, cx);
        } else {
            self.select_project(slug, cx);
        }
    }






















    // ---- cockpit (docs/019 slice 4) ---------------------------------------













    /// This frame's cached live sessions for a project (see `infos_cache`).
    fn cached_infos(&self, slug: &str) -> &[SessionInfo] {
        self.infos_cache
            .get(slug)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Drop any terminal drag-selection (on session/view switch, scroll, Esc, or
    /// a forwarded keystroke — the selection is viewport-relative, #9 3b).
    fn clear_term_selection(&mut self) {
        self.selection = None;
        self.drag_anchor = None;
        self.selection_session = None;
        self.drag_from_px = None;
        self.drag_moved = false;
        // B4: a scrollbar thumb-drag whose release was missed (the session exited
        // mid-drag, dropping the container's on_mouse_up) must not survive a
        // session/view switch and keep driving scroll on a later plain hover.
        self.scrollbar_drag = None;
    }

    /// The session you are ACTUALLY looking at: the workspace's agent view, on
    /// the selected project, with that session active. Being the active session
    /// of some OTHER project does not count, and neither does sitting on the
    /// Standup screen — which is exactly when a finished turn is worth flagging.
    ///
    /// KNOWN GAP: this cannot tell whether Kod is frontmost (gpui's `is_active`
    /// is not usable from here — see boot.rs), so a turn that lands while you
    /// are parked on that session in a background window is not flagged. The
    /// session is already on screen in that case, so the cost is small.
    fn watched_session(&self) -> Option<SessionId> {
        if !matches!(self.screen, Screen::Workspace) || !matches!(self.mode, Mode::Agent) {
            return None;
        }
        self.active_session_id()
    }

    /// This session finished a turn you have not opened since (#13).
    pub(crate) fn session_unreviewed(&self, id: SessionId) -> bool {
        self.sess_unreviewed.contains(&id)
    }

    fn active_session_id(&self) -> Option<SessionId> {
        let slug = &self.project().slug;
        let live = self.cached_infos(slug);
        // Only LIVE sessions are selectable. A crashed explicit pick returns None
        // (the stage shows "Pick an agent") rather than silently switching your
        // input to a different agent — the input-misrouting bug the review caught.
        match self.active_session.get(slug).copied() {
            Some(id) if live.iter().any(|s| s.id == id && s.alive) => Some(id),
            Some(_) => None,
            None => live.iter().rev().find(|s| s.alive).map(|s| s.id),
        }
    }











}

/// (awaiting, busy) live counts — the ACTIVE-project sort rank (needs-you first,
/// then working). Reads the per-frame infos slice, no host lock.
fn live_rank(infos: &[SessionInfo]) -> (usize, usize) {
    let aw = infos
        .iter()
        .filter(|s| s.alive && s.phase == orchestrator_host::Phase::AwaitingDecision)
        .count();
    let bz = infos
        .iter()
        .filter(|s| s.alive && s.phase == orchestrator_host::Phase::Busy)
        .count();
    (aw, bz)
}

/// Human-readable file size for the Recover rows.
fn human_bytes(b: u64) -> String {
    if b < 1024 {
        format!("{b} B")
    } else if b < 1024 * 1024 {
        format!("{:.0} KB", b as f64 / 1024.0)
    } else {
        format!("{:.1} MB", b as f64 / (1024.0 * 1024.0))
    }
}

/// Short "Mon D" date from epoch secs (UTC, civil-from-days) — for session start.
fn short_date(secs: u64) -> String {
    if secs == 0 {
        return "?".into();
    }
    let z = (secs / 86_400) as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!("{} {}", MON[(m - 1).clamp(0, 11) as usize], d)
}

impl Orchestrator {
    /// Focus a live session from ANY surface (rail agent, Deck row, needs-you
    /// card). Resolves the project index by SLUG at click-time — self.projects can
    /// reorder on a rescan, so a render-time index goes stale (review fix; matches
    /// toast_element). One source of truth for the session-focus mutation.
    fn focus_session(
        &mut self,
        slug: &str,
        id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = self
            .projects
            .iter()
            .position(|p| p.slug == slug)
            .unwrap_or(self.selected);
        self.active_session.insert(slug.to_string(), id);
        // Opening it IS reviewing it (#13).
        self.sess_unreviewed.remove(&id);
        self.screen = Screen::Workspace;
        self.mode = Mode::Agent;
        if self.search.session != Some(id) {
            self.search.close(); // matches belong to another session (critique)
        }
        self.clear_term_selection();
        self.term_focus.focus(window);
        cx.notify();
    }

    /// Select a project's CONTEXT (the Map) — index resolved by slug at click-time.
    fn select_project(&mut self, slug: &str, cx: &mut Context<Self>) {
        self.outline_link_open = false;
        self.outline_open_pending = None;
        // BEFORE `selected` moves: a live Detail edit blur-commits into the
        // project it belongs to (docs/019 save-on-blur); single-line editors
        // still discard — no invisible key sinks across a switch (review 1b).
        self.blur_outline_edit(cx);
        self.selected = self
            .projects
            .iter()
            .position(|p| p.slug == slug)
            .unwrap_or(self.selected);
        self.search.close();
        self.focused_part = None;
        self.rail_new = None;
        self.rail_new_err = None;
        // the suggestions gap-drawer is per-project (see the field's doc); a
        // stale open state must not carry a prior project's drawer into this one.
        self.suggest_open = false;
        // end any triage sweep: its cursor is a node id from the OLD project, and
        // part.id is global — a stray stamp would mark the wrong project's node
        // and misfile its undo (review finding 2).
        self.triage_active = false;
        self.triage_cursor = None;
        self.triage_done_armed = false;
        self.screen = Screen::Workspace;
        self.mode = default_workspace_mode();
        // docs/019 slice 1c: opening a project is where the machine offers to
        // dissolve its own Tech husk — gated once, surfaced as a review card.
        self.maybe_seed_dissolve_tech(slug);
        // #50: opening a project marks its updates READ — stamp the seen time
        // (persisted) + update the in-memory cache so the rail's unread cue clears.
        let seen_now = crate::render_sidebar::wall_now_ms();
        {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let _ = store.set_setting(&format!("proj_seen_ms:{slug}"), &seen_now.to_string());
        }
        self.proj_seen.borrow_mut().insert(slug.to_string(), seen_now);
        cx.notify();
    }

    /// #50: does this project have a timeline update NEWER than the last time the
    /// user opened it? Drives the "unread" font/color cue on rail project titles.
    /// Cheap: the per-project newest-update map is memoized by `write_gen` (rebuilt
    /// only on a store write); `proj_seen` is read from the setting once and cached
    /// (and stamped fresh in `select_project`), so opening a project clears the cue.
    /// Bring the slug → newest-update-ms memo in line with the store's
    /// `write_gen`.
    ///
    /// TAKES THE STORE LOCK — never call it while already holding one. The
    /// standup's portfolio tier learned this the hard way: it computes its rows
    /// inside a `store.lock()` block, so the unread lookups have to happen after
    /// that block closes or the (non-reentrant) mutex deadlocks.
    fn refresh_proj_updates(&self) {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let gen = store.write_gen();
        if self.proj_updates.borrow().0 != gen {
            let mut m = std::collections::HashMap::new();
            for ev in store.timeline(120) {
                let slot = m.entry(ev.project_key.clone()).or_insert(0u64);
                if ev.ts_ms > *slot {
                    *slot = ev.ts_ms;
                }
            }
            *self.proj_updates.borrow_mut() = (gen, m);
        }
    }

    /// Newest timeline-event timestamp for a project; 0 when it has nothing in
    /// the window. Used to sort the standup's project tiers newest-first.
    pub(crate) fn project_update_ms(&self, slug: &str) -> u64 {
        self.refresh_proj_updates();
        self.proj_updates.borrow().1.get(slug).copied().unwrap_or(0)
    }

    /// Last-opened stamp for a project, memoized in `proj_seen`. Only touches
    /// the store on the FIRST lookup per slug — these run once per project per
    /// frame across the rail and the standup's project tiers, so re-reading the
    /// setting every time would mean a store lock per row per frame.
    fn project_seen_ms(&self, slug: &str) -> u64 {
        if let Some(v) = self.proj_seen.borrow().get(slug) {
            return *v;
        }
        let v = self
            .store
            .lock()
            .ok()
            .and_then(|s| s.get_setting(&format!("proj_seen_ms:{slug}")))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        self.proj_seen.borrow_mut().insert(slug.to_string(), v);
        v
    }

    pub(crate) fn project_unread(&self, slug: &str) -> bool {
        let last_update = self.project_update_ms(slug);
        if last_update == 0 {
            return false;
        }
        last_update > self.project_seen_ms(slug)
    }




    /// Live agent overlay for a project from the host (real M2/M3 state):
    /// (hosted, busy, awaiting, top pending summary).
    fn live_overlay(&self, slug: &str) -> (u32, u32, u32, Option<String>) {
        let sessions = self.cached_infos(slug);
        let mut busy = 0u32;
        let mut awaiting = 0u32;
        let mut top = None;
        for s in sessions {
            match s.phase {
                orchestrator_host::Phase::AwaitingDecision => {
                    awaiting += 1;
                    if top.is_none() {
                        top = s.pending.as_ref().map(|p| p.view.summary());
                    }
                }
                orchestrator_host::Phase::Busy => busy += 1,
                _ => {}
            }
        }
        (sessions.len() as u32, busy, awaiting, top)
    }











    /// The top-right needs-you toast overlay — absolute, global across screens.
    /// NOTIFICATION + NAVIGATION only: it tells you a session is waiting and what
    /// it asked, then hands you to the terminal. It never answers — a one-line
    /// summary is not enough to consent to, and the real dialog is one click away.
    /// Lifetime is configurable (Notifications setting); `expire_at == 0` =
    /// permanent until dismissed (#2).
    fn toast_element(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = self.active_toast.as_ref()?;
        let (id, slug) = (t.id, t.slug.clone());
        let actions = div().flex().flex_row().gap(px(8.)).child(
            div()
                .id("toast-term")
                .px(px(13.))
                .py(px(5.))
                .rounded(px(8.))
                .cursor_pointer()
                .bg(rgb(0x23413A))
                .border_1()
                .border_color(rgb(0x346B54))
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(ACCENT))
                .child("Open in terminal ▸")
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    // focus_session also clears any stale terminal drag-selection
                    // and the ⌘F search bar (index resolved by slug), and lands the
                    // user's keystrokes in the real dialog.
                    this.active_toast = None;
                    this.focus_session(&slug, id, window, cx);
                })),
        );
        Some(
            div()
                .absolute()
                .top(px(18.))
                .right(px(18.))
                .w(px(330.))
                .p(px(13.))
                .rounded(px(12.))
                .bg(rgb(0x201A10))
                .border_1()
                .border_color(rgb(0x5A4A2C))
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .text_size(px(10.5))
                                .font_weight(FontWeight::BOLD)
                                .text_color(rgb(AMBER))
                                .child("⚠ NEEDS YOU"),
                        )
                        .child(div().flex_1())
                        .child(
                            div()
                                .id("toast-x")
                                .text_size(px(12.))
                                .text_color(rgb(MUTED2))
                                .cursor_pointer()
                                .hover(|h| h.text_color(rgb(TEXT)))
                                .child("✕")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.active_toast = None;
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    div()
                        .text_size(px(13.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_STRONG))
                        .child(SharedString::from(t.title.clone())),
                )
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(rgb(TEXT))
                        .child(SharedString::from(t.ask.clone())),
                )
                .child(actions)
                .into_any_element(),
        )
    }
















}

/// Ops referencing a Temp part only apply correctly with their whole diff.
fn has_temp_ref(op: &DiffOp) -> bool {
    matches!(
        op,
        DiffOp::Add {
            parent: PartRef::Temp(_),
            ..
        } | DiffOp::Move {
            parent: PartRef::Temp(_),
            ..
        }
    )
}

/// Does a diff op target/parent the given node? (outline per-op filter, #10)
fn op_touches(op: &DiffOp, id: PartId) -> bool {
    match op {
        DiffOp::Add { parent, .. } => *parent == PartRef::Id(id),
        DiffOp::SetStatus { id: t, .. }
        | DiffOp::Rename { id: t, .. }
        | DiffOp::Remove { id: t }
        | DiffOp::SetDetail { id: t, .. }
        | DiffOp::SetKind { id: t, .. } => *t == id,
        DiffOp::Move { id: t, parent, .. } => *t == id || *parent == PartRef::Id(id),
    }
}

/// Local calendar day index for a ms timestamp (day-header grouping).
fn local_day(ts_ms: u64) -> i64 {
    ((ts_ms / 1000) as i64 + orchestrator_host::host::local_off_secs()).div_euclid(86400)
}

/// One human line per proposal op for the Standup checkmarks / changeset rows.
fn describe_op(op: &DiffOp, name_of: &dyn Fn(PartId) -> String) -> String {
    match op {
        DiffOp::Add { name, parent, .. } => match parent {
            PartRef::Id(id) => format!("add “{}” under {}", name, name_of(*id)),
            _ => format!("add “{name}”"),
        },
        DiffOp::SetStatus { id, lifecycle, .. } => {
            format!("{} → {}", name_of(*id), lifecycle.as_str())
        }
        DiffOp::Rename { id, name, detail } => {
            if name_of(*id) == *name && !detail.is_empty() {
                format!("update detail of {}", name_of(*id))
            } else {
                format!("rename {} → “{}”", name_of(*id), name)
            }
        }
        DiffOp::Move { id, parent, .. } => match parent {
            PartRef::Id(p) => format!("move {} under {}", name_of(*id), name_of(*p)),
            _ => format!("move {}", name_of(*id)),
        },
        DiffOp::Remove { id } => format!("remove {}", name_of(*id)),
        DiffOp::SetDetail { id, .. } => format!("describe {}", name_of(*id)),
        DiffOp::SetKind { id, kind } => format!("{} becomes {}", name_of(*id), kind.as_str()),
    }
}

/// Glyph-click cycle (docs/019): user-settable states only — `building` is
/// derived from live session links and can never be asserted by hand.
/// Delegates to the one canonical cycle in the store crate.
fn next_lifecycle(lc: Lifecycle) -> Lifecycle {
    lc.cycle()
}

impl Render for Orchestrator {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // snapshot every project's live sessions ONCE per frame, so the sidebar /
        // header / stage read from the cache instead of re-locking the host 13-24×
        // per frame (review fix). Must precede reflow_terminal (it reads it).
        self.drain_search();
        let slugs: Vec<String> = self.projects.iter().map(|p| p.slug.clone()).collect();
        self.infos_cache.clear();
        for slug in slugs {
            let infos = self.host.infos_for(&slug);
            self.infos_cache.insert(slug, infos);
        }
        self.reflow_terminal(window);
        // heal edit UI whose target vanished (⌘Z, a menu delete, an accepted
        // proposal that Removes the node, a project switch): an invisible
        // context menu, canvas overlay, OR outline editor must never keep
        // owning the keyboard (the 1b key-sink rule — review: an open Detail/
        // rename/sibling editor on a deleted node routed every keystroke into
        // a pane that drew no editor, making the map keyboard appear dead).
        {
            let slug = self.project().slug.clone();
            // Keep the LOAD OUTCOME, not just the id set: an `Err` (poisoned
            // lock / failed read) and a successful load of an empty tree both
            // collapse to an empty set, and treating "empty" as "transient
            // miss" meant deleting the LAST node left the inline editor open
            // over a node that no longer existed — stuck, owning the keyboard,
            // with nothing to edit. Only `None` (a real failure) skips healing.
            let live: Option<std::collections::HashSet<PartId>> = self
                .store
                .lock()
                .ok()
                .and_then(|s| s.load_tree(&slug).ok())
                .map(|ps| ps.iter().map(|p| p.id).collect());
            if let Some(live) = live {
                if self.map_menu.is_some_and(|m| !live.contains(&m.id)) {
                    self.map_menu = None;
                }
                // every id-bearing outline slot (rename/detail/add-sibling on
                // an anchor); the target-less slots (AddPart/Decision/Note,
                // CreateCanvas) ride the self-healing effective focus instead.
                let dead = match self.outline_edit.active {
                    Some(outlinepane::EditSlot::RenameChild(id))
                    | Some(outlinepane::EditSlot::RenameFocused(id))
                    | Some(outlinepane::EditSlot::RenameCanvas(id))
                    | Some(outlinepane::EditSlot::Detail(id))
                    | Some(outlinepane::EditSlot::AddSibling(id)) => !live.contains(&id),
                    _ => false,
                };
                if dead {
                    self.outline_edit = outlinepane::EditState::default();
                }
            }
        }
        // backfill the active session's timeline from its transcript once its id
        // is known (#9 §4): codex's whole timeline, or a resumed claude's history.
        if self.screen == Screen::Workspace && self.mode == Mode::Agent {
            let target = self.active_session_id().and_then(|id| {
                if self.backfilled.contains(&id) {
                    return None;
                }
                let slug = self.project().slug.clone();
                self.host
                    .infos_for(&slug)
                    .into_iter()
                    .find(|i| i.id == id)
                    .filter(|i| i.kind != CliKind::Shell)
                    .and_then(|i| i.cli_session_id.clone().map(|cli| (id, i.kind, cli)))
            });
            if let Some((id, kind, cli)) = target {
                self.start_transcript_backfill(id, kind, cli, cx);
            }
        }
        // Keep an element focused when we're NOT in the terminal, because the ROOT
        // KEY ROUTER hangs off `root_focus` and gpui only walks the focused node's
        // dispatch path — with nothing focused, every non-terminal keystroke
        // (Esc, ⌫, the outline grammar, ⌘Z) would land nowhere.
        // an input sink or the terminal owns focus? then leave it. (Keeping the
        // macOS Settings menu item enabled is NOT this reclaim's job any more —
        // that's the global ToggleSettings listener in `boot::run`, #54.)
        let owns_focus_elsewhere = self.palette.open // the palette owns focus while open (review 1b)
            || self.cmd.open // the command bar owns focus while open (docs/019 COMMAND BAR)
            || self.root_focus.is_focused(window)
            || self.palette_focus.is_focused(window)
            || (self.screen == Screen::Workspace && self.mode == Mode::Agent);
        if !owns_focus_elsewhere {
            self.root_focus.focus(window);
        }
        let sidebar = self.render_sidebar(cx);
        let main = match self.screen {
            Screen::Standup => self.render_standup(cx).into_any_element(),
            Screen::Workspace => self.render_workspace(cx).into_any_element(),
        };
        div()
            .track_focus(&self.root_focus)
            .size_full()
            .flex()
            .flex_row()
            .relative()
            .bg(rgb(APP_BG))
            .text_color(rgb(TEXT))
            // ⌃` toggles the Agent stage ⟺ the Map context (no more peek/expand).
            .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                this.screen = Screen::Workspace;
                // ⌃` toggles Agent ⟺ the Map context. With the map compiled out
                // default_workspace_mode() is Agent, so ⌃` simply keeps you on the
                // Agent stage (there is no second stage to toggle to).
                if this.mode == Mode::Agent {
                    this.mode = default_workspace_mode();
                } else {
                    this.mode = Mode::Agent;
                    this.term_focus.focus(window);
                }
                cx.notify();
            }))
            // ⇧⌃` forces focus onto the Agent stage.
            .on_action(cx.listener(|this, _: &StageTerminal, window, cx| {
                this.screen = Screen::Workspace;
                this.mode = Mode::Agent;
                this.term_focus.focus(window);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &NewClaude, window, cx| {
                // a hotkey makes no account pick, so it adopts this CLI's
                // default-profile setting (#56). The deliberate no-profile spawn
                // is a "+" menu row, where it can carry a label (#62.3).
                this.spawn_session(CliKind::Claude, ProfilePick::Default, window, cx)
            }))
            .on_action(cx.listener(|this, _: &NewCodex, window, cx| {
                this.spawn_session(CliKind::Codex, ProfilePick::Default, window, cx)
            }))
            .on_action(cx.listener(|this, _: &NewShell, window, cx| {
                this.spawn_session(CliKind::Shell, ProfilePick::Default, window, cx)
            }))
            // NOTE: ToggleSettings is deliberately NOT handled here (#54). A
            // Bubble-phase handler on this root would consume the action and the
            // GLOBAL listener — which runs last, and is the only thing keeping the
            // macOS menu item enabled while the Settings window is frontmost —
            // would never fire. See `boot::toggle_settings`.
            .on_action(
                cx.listener(|this, _: &FindInTerminal, window, cx| this.open_search(window, cx)),
            )
            .on_action(cx.listener(|this, _: &FindNext, _, cx| {
                if this.search.open {
                    this.search.next();
                    this.jump_to_current();
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &FindPrev, _, cx| {
                if this.search.open {
                    this.search.prev();
                    this.jump_to_current();
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &TogglePalette, window, cx| {
                // ⌘K is the one way to raise the palette WITHOUT a click, so it is
                // the one path the rail menu's click-catcher can't close — and the
                // menu draws above the palette (deferred), so it would hover over
                // it as a dead panel.
                this.rail_new_menu_open = false;
                if this.palette.open {
                    this.close_palette(window);
                } else {
                    this.palette.open_recall();
                    // key sinks close/blur, palette takes focus (one body
                    // with the N/M entrances — see stage_palette).
                    this.stage_palette(window, cx);
                    // seed the selection's verb rows (docs/019 PALETTE) —
                    // they show before any typing.
                    this.rekick_palette();
                }
                cx.notify();
            }))
            // THE root key router — the single dispatch point for every
            // non-terminal keystroke (root_focus is focused on non-terminal
            // screens — the Settings menu fix; no IME v1). Precedence, first
            // consumer wins:
            //   1. open context menu (owns ALL keys: accelerators act, the
            //      rest are swallowed)
            //   2. rail ＋idea inline editor (keys are content)
            //   3. outline/canvas inline editor (keys are content)
            //   4. the OUTLINE grammar (docs/019 slice 1b): Enter/Tab/⌥↑↓/
            //      F2/E/⌘⌫ on the effective focus, plus the PALETTE verbs
            //      N (QuickAdd/capture) and M (Move-to) on the REAL selection
            //      — inside the same fn so editors always outrank verbs
            //   5. ⌘Z undo, 6. ⌫ drill-up, 7. Esc (drag/overlays/outline)
            // The ⌘K palette never reaches here: palette_focus owns the
            // keystream while open (its listener stops propagation), and the
            // grammar's guard refuses while palette.open as a second lock.
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if this.map_menu.is_some() {
                    // the open context menu owns the keyboard: bare-key
                    // accelerators (R/E/N/M/⌘⌫, docs/019 CANVAS) act, all
                    // else is swallowed so nothing leaks to the grammar.
                    this.map_menu_key(ev, window, cx);
                } else if this.rail_new.is_some() {
                    this.route_inline_key(ev, InlineTarget::RailIdea, cx);
                    // NOTE: the ProfileField / SettingText branches used to sit
                    // here behind a `screen == Screen::Settings` guard. They now
                    // live in the Settings WINDOW's own router (#54) — leaving
                    // them here would let a settings editor left open in the other
                    // window capture this window's keystream, which is exactly the
                    // hijack that guard existed to prevent.
                } else if this.outline_edit.active.is_some() {
                    this.route_inline_key(ev, InlineTarget::Outline, cx);
                } else if this.triage_key(ev, cx) {
                    // consumed: the triage sweep (docs/019 T7) owns j/k/t/i/x/q
                    // while active — single-key human:triage stamps.
                } else if !this.rail_new_menu_open && this.outline_grammar_key(ev, window, cx) {
                    // consumed: an outline verb (Enter/Tab/⌥↑↓/F2/E/⌘⌫) fired.
                    // The rail's ＋New menu takes the same lock the spawn menu
                    // takes inside the grammar's own guard: an open menu owns the
                    // keyboard, or a bare N under it fires a QuickAdd instead.
                } else if ev.keystroke.modifiers.secondary()
                    && !ev.keystroke.modifiers.shift
                    && ev.keystroke.key.as_str() == "z"
                    && this.screen == Screen::Workspace
                    && this.mode == Mode::MapOutline
                    && !this.spawn_menu_open
                    && !this.move_menu_open
                    && !this.rail_new_menu_open
                {
                    // ⌘Z (docs/019 slice 1a): undo the last accept group. The
                    // journal stores a computed inverse for every DiffOp and
                    // undo_last replays it reversed — undo IS the confirmation
                    // dialog this map never shows. Only fires on non-terminal
                    // screens (root_focus routing), so terminal ⌘Z is untouched.
                    let slug = this.project().slug.clone();
                    {
                        let mut store = this.store.lock().unwrap_or_else(|e| e.into_inner());
                        let _ = store.undo_last(&slug);
                    }
                    cx.notify();
                } else if ev.keystroke.key.as_str() == "backspace"
                    && !ev.keystroke.modifiers.secondary() // ⌘⌫ is DELETE (grammar above) — never a drill
                    && this.screen == Screen::Workspace
                    && this.mode == Mode::MapOutline
                    && !this.spawn_menu_open
                    && !this.move_menu_open
                    && !this.rail_new_menu_open
                {
                    // ⌫ drills UP one level (docs/011 §B); Esc never drills.
                    let slug = this.project().slug.clone();
                    if let Some(root) = this.map_root_of(&slug) {
                        let parent = this
                            .store
                            .lock()
                            .ok()
                            .and_then(|st| st.load_tree(&slug).ok())
                            .and_then(|parts| {
                                parts
                                    .iter()
                                    .find(|p| p.id == root)
                                    .and_then(|p| p.parent_id)
                            });
                        this.set_map_root(&slug, parent);
                        cx.notify();
                    }
                } else if ev.keystroke.key.as_str() == "escape" {
                    // overlays own Esc first — otherwise it falls through the
                    // scrim and silently collapses the outline BEHIND a modal
                    // (review slice 1); then Esc collapses the outline (§A).
                    if this.map_drag.take().is_some() {
                        // Esc cancels a drag mid-flight (docs/019 ⌥-drag; a
                        // plain drag too) — released with nothing persisted,
                        // the card snaps back to its last real position.
                        this.map_drop_deny.clear();
                        // (Esc-closes-Settings moved with the surface — the
                        // Settings window's own router owns it now, #54.)
                    } else if this.spawn_menu_open {
                        this.spawn_menu_open = false;
                    } else if this.rail_new_menu_open {
                        this.rail_new_menu_open = false;
                    } else if this.move_menu_open {
                        this.move_menu_open = false;
                    } else if this.screen == Screen::Workspace && this.mode == Mode::MapOutline {
                        let slug = this.project().slug.clone();
                        if this.outline_open(&slug) {
                            this.set_outline_open(&slug, false);
                        }
                    }
                    cx.notify();
                }
            }))
            // ── sidebar resize (#52) ─────────────────────────────────────────
            // move/up live on the ROOT rather than on the 6px gutter, so the drag
            // keeps tracking once the cursor outruns the gutter — the same shape
            // as the terminal scrollbar thumb. A move with no left button held
            // means the release was missed, so drop the stale drag instead of
            // letting a plain hover steer the rail.
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _w, cx| {
                let Some(a) = this.sidebar_drag else {
                    return;
                };
                if e.pressed_button != Some(MouseButton::Left) {
                    this.sidebar_drag = None;
                    this.persist_sidebar_w();
                    cx.notify();
                    return;
                }
                let w = clamp_sidebar_w(a.w0 + (f32::from(e.position.x) - a.grab_x));
                if (w - this.sidebar_w).abs() > 0.5 {
                    this.sidebar_w = w;
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _e: &MouseUpEvent, _w, cx| {
                    if this.sidebar_drag.take().is_some() {
                        this.persist_sidebar_w();
                        cx.notify();
                    }
                }),
            )
            .child(sidebar)
            .child(main)
            // The rail's resize gutter: a strip straddling the rail's right
            // border. It is mounted AFTER `main` so it paints — and hit-tests —
            // above the stage edge it overlaps.
            .child(
                div()
                    .id("sidebar-resize")
                    .absolute()
                    .top_0()
                    .left(px(self.sidebar_w - 3.0))
                    .w(px(6.))
                    .h_full()
                    .cursor(CursorStyle::ResizeLeftRight)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, e: &MouseDownEvent, _w, cx| {
                            this.sidebar_drag = Some(SidebarDragAnchor {
                                grab_x: f32::from(e.position.x),
                                w0: this.sidebar_w,
                            });
                            // don't let the root's press handler read this as a
                            // click-away that dismisses an open menu.
                            cx.stop_propagation();
                        }),
                    ),
            )
            // the rail ＋New menu's click-catcher (#67). It lives on the root, not
            // in the rail, because the menu overhangs the stage — and BELOW the
            // toast, so dismissing the menu never costs the toast its click.
            .when_some(self.rail_new_backdrop(cx), |c, b| c.child(b))
            // global needs-you toast — absolute, on top of any screen (#4 slice 2).
            .when_some(self.toast_element(cx), |c, t| c.child(t))
            // canvas context menu (docs/019 CANVAS) — window-space so it never
            // clips inside the map's scroll container.
            .when_some(self.map_menu_layer(window.viewport_size(), cx), |c, m| {
                c.child(m)
            })
            .when(self.palette.open, |c| c.child(self.palette_layer(cx)))
    }
}

/// The binary's entry point has to live at the crate root, so this is all that
/// stayed behind: every line of startup wiring is in `boot`.
fn main() {
    boot::run()
}
