//! palette — the ⌘K recall / quick-add / move-to overlay (docs/010 §3b, #10;
//! docs/019 PALETTE): one keystroke from anywhere. Recall searches the three
//! memory layers (map nodes, the decision log, session summaries) and turns ↵
//! into navigation; QuickAdd turns the query into a new node's NAME, applied
//! INSTANTLY on the human lane (docs/019 commitment 1 — ⌘Z is the safety, no
//! cards) — with no parent baked in, the capture files to the idea tray
//! (commitment 5: deciding where a thought lives at capture time is exactly
//! the overhead the map absorbs); MoveTo is the long-distance reparent —
//! every legal destination as a fuzzy-filtered breadcrumb path (⌥-drag is the
//! short-distance one).
//!
//! main.rs-agnostic by construction (the outlinepane pattern): this module
//! owns the pure state machine + the overlay render; the integrator owns the
//! keystream (chars via the IME replace_text_in_range branch, nav/edit keys
//! in on_key_down — the search-bar input pattern), re-runs store.search_all
//! on each edit (synchronous — the store rebuilds its FTS per call, sub-ms at
//! this scale, no worker), and maps each `PaletteEvent` to a real mutation.

use std::rc::Rc;

use gpui::prelude::FluentBuilder;
use gpui::*;
use orchestrator_store::store::SearchHit;
use orchestrator_store::{build_tree, Part, PartId, TreeNode};

// Focused Dark tokens used by the palette render (values match main.rs).
const PANEL: u32 = 0x141820;
const CARD: u32 = 0x191D27;
const CARD2: u32 = 0x1D222C;
const HAIR: u32 = 0x2B303B;
const HAIR_SOFT: u32 = 0x23272F;
const TEXT: u32 = 0xD3D8E1;
const TEXT_STRONG: u32 = 0xF2F5FA;
const MUTED: u32 = 0x9AA3B1;
const MUTED2: u32 = 0x757E8A;
const ACCENT: u32 = 0x7EE2C0;
const AMBER: u32 = 0xE6C07A;
const GREEN: u32 = 0x5BB99B;
/// the SESSION tag — the one blue in Focused Dark: a session is neither
/// product state (green) nor a commitment (amber), so it gets its own hue.
const SESSION_BLUE: u32 = 0x7FA9D9;

/// Display cap: the panel shows at most this many rows, so arrow-key
/// selection can never land on an off-screen hit (no list scrolling to keep
/// in sync). `adopt_hits` rank-truncates to this BEFORE grouping.
pub const ROWS_CAP: usize = 12;

// ---- state (pure; driven by main.rs's keystream) ----

/// Recall = search + open (plus the selection's verb rows). QuickAdd = the
/// query IS the new node's name; hits stay empty (the integrator skips
/// search_all in this mode); `parent: None` = the idea tray (docs/019
/// commitment 5 — capture without filing). MoveTo = the query filters
/// destination breadcrumbs for `moving` (docs/019 PALETTE).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaletteMode {
    #[default]
    Recall,
    QuickAdd {
        parent: Option<PartId>,
    },
    MoveTo {
        moving: PartId,
    },
}

/// What ↵ / ⌘↵ / a row click should DO — the integrator maps these onto the
/// Orchestrator (navigation, or an instant human-lane accept_diff_from).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteEvent {
    /// a node hit — focus it on that project's map.
    FocusNode { project_key: String, part: PartId },
    /// a decision/note hit — open the owning node (its log is the outline).
    OpenLog { project_key: String, part: PartId },
    /// a summary hit — open that session (live, or Recover if it ended).
    OpenSession {
        project_key: String,
        cli_session_id: String,
    },
    /// quick-add commit — an INSTANT Add under the baked-in parent; None =
    /// the idea tray (docs/019 commitment 5). ⌘↵'s keep-open lives in the
    /// integrator, not here — the op is the same either way.
    AddNode {
        parent: Option<PartId>,
        name: String,
    },
    /// recall-mode ⌘↵ — an Add under whatever the integrator deems "here"
    /// (the focused map node; the palette can't know it and shouldn't).
    AddHere { name: String },
    /// move-to commit — ONE reparent of `id` under `dest` (None = root),
    /// the same tree::reparent_op an ⌥-drag drop applies.
    MoveNode { id: PartId, dest: Option<PartId> },
    /// a selection verb row (docs/019 PALETTE) — the integrator routes it to
    /// the SAME handler the context menu uses, on its focused node (the
    /// palette owns keys while open, so the selection can't drift mid-verb).
    Verb(NodeVerb),
}

/// The four context-menu verbs that double as palette rows on the selection
/// (docs/019 PALETTE: rename/delete/status/kind — cheap discoverability).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeVerb {
    Rename,
    Delete,
    CycleStatus,
    CycleKind,
}

/// One rendered verb row — label baked from the node so the row SHOWS what
/// ↵ will do ("status: todo → done"), never a bare imperative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerbRow {
    pub verb: NodeVerb,
    pub label: String,
}

/// One Move-to destination: `dest: None` = the "▸ root" escape hatch; `path`
/// is the full breadcrumb ("Hosted sessions ▸ Terminal host"); `leaf` is the
/// node's own name, carried EXPLICITLY (review: deriving it by splitting the
/// path on " ▸ " mangled any node whose name literally contains that separator).
#[derive(Debug, Clone, PartialEq)]
pub struct MoveTarget {
    pub dest: Option<PartId>,
    pub path: String,
    pub leaf: String,
}

#[derive(Default)]
pub struct PaletteState {
    pub open: bool,
    pub query: String,
    /// grouped display order (nodes → decisions → sessions), ≤ ROWS_CAP.
    pub hits: Vec<SearchHit>,
    /// how many hits/targets existed BEFORE the display cap (the "+N more" row).
    pub total: usize,
    /// the flat selection index — in Recall it walks verbs THEN hits (the
    /// draw order); in MoveTo it walks `targets`; QuickAdd has no rows.
    pub sel: usize,
    pub mode: PaletteMode,
    /// Recall only: the selection's verb rows, drawn ABOVE the hits.
    pub verbs: Vec<VerbRow>,
    /// MoveTo only: the filtered destination list, ≤ ROWS_CAP.
    pub targets: Vec<MoveTarget>,
}

impl PaletteState {
    pub fn open_recall(&mut self) {
        self.reset();
        self.open = true;
    }

    /// N on the map (docs/019 PALETTE): the query is the new node's NAME.
    /// `parent: None` = no meaningful selection → the idea tray.
    pub fn open_quick_add(&mut self, parent: Option<PartId>) {
        self.reset();
        self.open = true;
        self.mode = PaletteMode::QuickAdd { parent };
    }

    /// M / menu Move to… (docs/019 PALETTE): the long-distance reparent.
    pub fn open_move_to(&mut self, moving: PartId) {
        self.reset();
        self.open = true;
        self.mode = PaletteMode::MoveTo { moving };
    }

    pub fn close(&mut self) {
        self.reset();
    }

    fn reset(&mut self) {
        self.open = false;
        self.query.clear();
        self.hits.clear();
        self.total = 0;
        self.sel = 0;
        self.mode = PaletteMode::Recall;
        self.verbs.clear();
        self.targets.clear();
    }

    /// IME-committed text (may be a whole CJK candidate, not one keystroke).
    /// Any edit restarts selection at the top — the old index pointed into a
    /// result set that no longer exists.
    pub fn push_str(&mut self, text: &str) {
        self.query.push_str(text);
        self.sel = 0;
    }

    /// One character, multibyte-safe (String::pop is char-wise, not byte-wise).
    pub fn backspace(&mut self) {
        self.query.pop();
        self.sel = 0;
    }

    /// Adopt a fresh result set: keep the TOP `ROWS_CAP` by FTS rank (so the
    /// cap never hides a better cross-kind hit), then group by kind — the
    /// render is grouped, and the flat selection index must walk the same
    /// order the rows are drawn in. Stable sort: rank survives within a group.
    pub fn adopt_hits(&mut self, mut hits: Vec<SearchHit>) {
        self.total = hits.len();
        hits.truncate(ROWS_CAP);
        hits.sort_by_key(|h| group_order(&h.kind));
        self.hits = hits;
        // default ↵ targets the first HIT, never a verb (review: a query that
        // fuzzy-matched only 'delete' put the destructive Delete verb at sel=0,
        // so ↵ silently deleted the focused subtree while the footer said
        // "↵ open"). Verbs sit ABOVE the hits — reach them with ↑. With no
        // hits, sel points past the last row so ↵ is a safe no-op: a verb is
        // only ever fired by a deliberate arrow/click, never by default.
        self.sel = self.verbs.len();
    }

    /// Adopt the selection's verb rows (Recall). Resets selection like any
    /// other row-set change — the old index pointed at rows that moved.
    pub fn adopt_verbs(&mut self, verbs: Vec<VerbRow>) {
        self.verbs = verbs;
        self.sel = 0;
    }

    /// Adopt a fresh (already rank-filtered) destination list, capped like
    /// hits so arrow-keys can never select an off-screen row.
    pub fn adopt_targets(&mut self, mut targets: Vec<MoveTarget>) {
        self.total = targets.len();
        targets.truncate(ROWS_CAP);
        self.targets = targets;
        self.sel = 0;
    }

    /// How many selectable rows the CURRENT mode draws — the one domain
    /// next/prev/confirm all index into.
    fn row_count(&self) -> usize {
        match self.mode {
            PaletteMode::Recall => self.verbs.len() + self.hits.len(),
            PaletteMode::MoveTo { .. } => self.targets.len(),
            PaletteMode::QuickAdd { .. } => 0,
        }
    }

    /// Next row, wrapping at the end. Empty list: no-op, never a panic.
    pub fn next(&mut self) {
        let n = self.row_count();
        if n > 0 {
            self.sel = (self.sel + 1) % n;
        }
    }

    /// Previous row, wrapping at the start.
    pub fn prev(&mut self) {
        let n = self.row_count();
        if n > 0 {
            self.sel = (self.sel + n - 1) % n;
        }
    }

    /// ↵ (`cmd_held` = ⌘↵) → the event the integrator should act on. Pure:
    /// quick-add commits the trimmed query as the name; move-to commits the
    /// selected destination; recall ⌘↵ adds "here"; plain recall ↵ fires the
    /// selected verb or opens the selected hit. None = nothing to do (empty
    /// name / no selection / an unparseable hit) — the palette stays open.
    pub fn confirm(&self, cmd_held: bool) -> Option<PaletteEvent> {
        let name = self.query.trim();
        match self.mode {
            PaletteMode::QuickAdd { parent } => (!name.is_empty()).then(|| PaletteEvent::AddNode {
                parent,
                name: name.to_string(),
            }),
            PaletteMode::MoveTo { moving } => {
                self.targets.get(self.sel).map(|t| PaletteEvent::MoveNode {
                    id: moving,
                    dest: t.dest,
                })
            }
            PaletteMode::Recall if cmd_held => (!name.is_empty()).then(|| PaletteEvent::AddHere {
                name: name.to_string(),
            }),
            PaletteMode::Recall => {
                // the flat index walks verbs first — the draw order.
                if self.sel < self.verbs.len() {
                    return Some(PaletteEvent::Verb(self.verbs[self.sel].verb));
                }
                self.hits
                    .get(self.sel - self.verbs.len())
                    .and_then(hit_event)
            }
        }
    }
}

// ---- pure hit helpers (unit-tested; RULE ZERO) ----

/// Display group order: nodes (the map itself) → decisions (the log) →
/// sessions (the past). Unknown kinds sink to the bottom, never vanish.
pub fn group_order(kind: &str) -> u8 {
    match kind {
        "node" => 0,
        "note" => 1,
        "summary" => 2,
        _ => 3,
    }
}

/// The row's kind tag (docs/010 §3b): NODE green / DECISION amber / SESSION
/// blue. Unknown kinds degrade to a muted "?" rather than lying.
pub fn kind_tag(kind: &str) -> (&'static str, u32) {
    match kind {
        "node" => ("NODE", GREEN),
        "note" => ("DECISION", AMBER),
        "summary" => ("SESSION", SESSION_BLUE),
        _ => ("?", MUTED2),
    }
}

/// A hit's ↵ event. Node/note hits carry the part id as TEXT in `ref_id`
/// (store contract) — an unparseable id yields None instead of a bogus jump.
pub fn hit_event(hit: &SearchHit) -> Option<PaletteEvent> {
    match hit.kind.as_str() {
        "node" => hit.ref_id.parse().ok().map(|part| PaletteEvent::FocusNode {
            project_key: hit.project_key.clone(),
            part,
        }),
        "note" => hit.ref_id.parse().ok().map(|part| PaletteEvent::OpenLog {
            project_key: hit.project_key.clone(),
            part,
        }),
        "summary" => Some(PaletteEvent::OpenSession {
            project_key: hit.project_key.clone(),
            cli_session_id: hit.ref_id.clone(),
        }),
        _ => None,
    }
}

/// One-line snippet: collapse all whitespace runs (notes and summaries carry
/// newlines), truncate at `max_chars` CHARS (multibyte-safe) with an ellipsis.
pub fn snippet(s: &str, max_chars: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= max_chars {
        one
    } else {
        let cut: String = one.chars().take(max_chars).collect();
        format!("{}…", cut.trim_end())
    }
}

/// A row's (primary, secondary) text. Per store contract: node = name/detail;
/// note = kind/text — the TEXT is what the user recalls, the kind is already
/// the tag, so the title is dropped; summary = goal/(headline + next step).
pub fn row_text(hit: &SearchHit) -> (String, String) {
    match hit.kind.as_str() {
        "note" => (snippet(&hit.body, 60), String::new()),
        _ => (snippet(&hit.title, 60), snippet(&hit.body, 72)),
    }
}

/// The "+N more" row under a capped list — None while everything fits.
pub fn more_note(total: usize, shown: usize) -> Option<String> {
    (total > shown).then(|| format!("+{} more — keep typing", total - shown))
}

// ---- move-to + verb helpers (pure; docs/019 PALETTE) ----

/// Case-insensitive subsequence match ("fuzzy"): every query char appears in
/// `hay` in order. Returns the index (in lowercased-char space) of the FIRST
/// matched char — earlier is a better match — or None when the query doesn't
/// fit. An empty/whitespace query matches everything at 0 (no filter).
pub fn fuzzy_pos(hay: &str, query: &str) -> Option<usize> {
    let q: Vec<char> = query.trim().to_lowercase().chars().collect();
    if q.is_empty() {
        return Some(0);
    }
    let mut qi = 0;
    let mut first = 0;
    for (i, c) in hay.to_lowercase().chars().enumerate() {
        if c == q[qi] {
            if qi == 0 {
                first = i;
            }
            qi += 1;
            if qi == q.len() {
                return Some(first);
            }
        }
    }
    None
}

/// Every legal destination for moving `moving`, in tree (display) order, as
/// breadcrumb paths. Excluded: the moving node's own subtree (landing inside
/// it detaches it — the same cycle guard as reparent_op), and its CURRENT
/// parent (a "move" to where it already lives says nothing). "▸ root" leads —
/// the always-visible way out of a bad nesting — unless it's already a root.
pub fn move_targets(parts: &[Part], moving: PartId) -> Vec<MoveTarget> {
    fn deny_subtree(parts: &[Part], id: PartId, deny: &mut std::collections::HashSet<PartId>) {
        deny.insert(id);
        for c in parts.iter().filter(|p| p.parent_id == Some(id)) {
            deny_subtree(parts, c.id, deny);
        }
    }
    fn walk(
        nodes: &[TreeNode],
        trail: &str,
        deny: &std::collections::HashSet<PartId>,
        skip: Option<PartId>,
        out: &mut Vec<MoveTarget>,
    ) {
        for n in nodes {
            if deny.contains(&n.part.id) {
                continue; // the whole subtree is inside the moving node
            }
            let path = if trail.is_empty() {
                n.part.name.clone()
            } else {
                format!("{trail} ▸ {}", n.part.name)
            };
            if Some(n.part.id) != skip {
                out.push(MoveTarget {
                    dest: Some(n.part.id),
                    path: path.clone(),
                    leaf: n.part.name.clone(),
                });
            }
            // the current parent is skipped as a ROW, but its children are
            // real destinations — keep descending.
            walk(&n.children, &path, deny, skip, out);
        }
    }
    let cur_parent = parts
        .iter()
        .find(|p| p.id == moving)
        .and_then(|p| p.parent_id);
    let mut deny = std::collections::HashSet::new();
    deny_subtree(parts, moving, &mut deny);
    let mut out = Vec::new();
    if cur_parent.is_some() {
        out.push(MoveTarget {
            dest: None,
            path: "▸ root".into(),
            leaf: "▸ root".into(),
        });
    }
    walk(&build_tree(parts), "", &deny, cur_parent, &mut out);
    out
}

/// Fuzzy-filter + rank destinations: leaf-name matches beat path-only matches
/// (typing a node's name should find THAT node before every path that merely
/// threads through it), then earlier match position, then tree order (the
/// enumerate index — sort_by_key is stable but the key makes it explicit).
pub fn filter_targets(targets: &[MoveTarget], query: &str) -> Vec<MoveTarget> {
    let mut ranked: Vec<(u8, usize, usize, &MoveTarget)> = targets
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            // rank the node's own name first, then the full path (review:
            // use the carried leaf, never a path split that a " ▸ " name breaks).
            match (fuzzy_pos(&t.leaf, query), fuzzy_pos(&t.path, query)) {
                (Some(p), _) => Some((0u8, p, i, t)),
                (None, Some(p)) => Some((1, p, i, t)),
                (None, None) => None,
            }
        })
        .collect();
    ranked.sort_by_key(|(tier, pos, i, _)| (*tier, *pos, *i));
    ranked.into_iter().map(|(_, _, _, t)| t.clone()).collect()
}

/// The selection's verb rows (docs/019 PALETTE), query-filtered. Filtering
/// matches the verb KEYWORD only (rename/delete/status/kind) — the label
/// carries the node's name, and matching on it would shadow real search hits
/// behind verb rows whenever the query resembles the selection. Labels show
/// the OUTCOME ("status: todo → done"), reusing the canonical cycles.
pub fn verb_rows(node: &Part, query: &str) -> Vec<VerbRow> {
    let name = snippet(&node.name, 24);
    let rows = [
        (NodeVerb::Rename, "rename", format!("rename “{name}”")),
        (
            NodeVerb::CycleStatus,
            "status",
            format!(
                "status: {} → {}",
                node.lifecycle.as_str(),
                node.lifecycle.cycle().as_str()
            ),
        ),
        (
            NodeVerb::CycleKind,
            "kind",
            format!(
                "kind: {} → {}",
                node.kind.as_str(),
                node.kind.cycle().as_str()
            ),
        ),
        (
            NodeVerb::Delete,
            "delete",
            format!("delete “{name}” — subtree, ⌘Z undoes"),
        ),
    ];
    rows.into_iter()
        .filter(|(_, key, _)| fuzzy_pos(key, query).is_some())
        .map(|(verb, _, label)| VerbRow { verb, label })
        .collect()
}

// ---- caller-supplied callbacks (what cx.listener would otherwise close over) ----

pub type IxCb = Rc<dyn Fn(usize, &mut Window, &mut App)>;
pub type Cb = Rc<dyn Fn(&mut Window, &mut App)>;

/// Mouse paths only — the keyboard (↵/⌘↵/↑↓/esc/chars) stays in main.rs's
/// on_key_down + IME branch, where the one keystream already lives.
#[derive(Clone)]
pub struct PaletteHandlers {
    /// click any row (verb/hit/target) → the integrator selects it by flat
    /// index and fires its event (sel := ix, then act on `confirm(false)`).
    pub activate_hit: IxCb,
    /// scrim click — the same close the integrator's Esc branch calls.
    pub close: Cb,
}

// ---- copy that must not promise a refused keystroke ----

/// The footer's key legend. PURE and separate from the element so the one thing
/// that can be WRONG about it — advertising ⌘↵ where `capture_refusal` would
/// refuse — is a test rather than a screenshot.
///
/// Only Recall varies: QuickAdd and MoveTo are opened from map-only UI, so they
/// cannot be on screen in a build where capture is off.
pub fn footer_hint(mode: PaletteMode, can_capture: bool) -> &'static str {
    match mode {
        PaletteMode::QuickAdd { parent: Some(_) } => "↵ add & select · ⌘↵ keep capturing · esc",
        PaletteMode::QuickAdd { parent: None } => "↵ capture to ideas · ⌘↵ keep capturing · esc",
        PaletteMode::MoveTo { .. } => "↵ move here · ↑↓ pick · esc",
        PaletteMode::Recall if can_capture => "↵ open · ⌘↵ add here · ↑↓ move · esc",
        // ⌘K itself stays alive with the map compiled out (session recall is
        // ungated), so this is the hint the default OSS build actually shows.
        PaletteMode::Recall => "↵ open · ↑↓ move · esc",
    }
}

/// Recall's empty-result line. Same rule as `footer_hint`: it offered ⌘↵ as the
/// way out of an empty search, which in the shipping build is a red banner.
pub fn no_hits_line(q: &str, can_capture: bool) -> String {
    if can_capture {
        format!("no matches — ⌘↵ adds “{q}” as a new node")
    } else {
        format!("no matches for “{q}”")
    }
}

// ---- element builder ----

/// The whole overlay: a full-window scrim (click = close) with the palette
/// panel top-centered on it (fixed offset — a growing hit list must not make
/// the query line jump). `ctx_name` is the mode's context node display name —
/// QuickAdd's parent / MoveTo's moving node; the integrator resolves it
/// (state only holds ids).
///
/// `can_capture` is `capture_refusal().is_none()` — whether ⌘↵ would actually
/// WRITE. It has to be passed in because the two reasons it can be false live on
/// the Orchestrator (the map feature flag; an empty portfolio), and copy that
/// advertises a key the app then refuses is the same lie the Idea row was gated
/// to remove. When it is false the ⌘↵ half of every hint is simply not said.
pub fn palette_overlay(
    state: &PaletteState,
    ctx_name: Option<&str>,
    can_capture: bool,
    h: &PaletteHandlers,
) -> impl IntoElement {
    let q = state.query.trim().to_string();

    // ── query line: mode chip + mono query + caret (the ⌘F bar's look).
    let (chip, chip_color) = match state.mode {
        PaletteMode::QuickAdd { parent: Some(_) } => (
            SharedString::from(match ctx_name {
                Some(p) => format!("＋ under {}", snippet(p, 24)),
                None => "＋ node".to_string(),
            }),
            ACCENT,
        ),
        // the tray chip is AMBER — an unfiled capture, not product state.
        PaletteMode::QuickAdd { parent: None } => (SharedString::from("＋ ideas"), AMBER),
        PaletteMode::MoveTo { .. } => (
            SharedString::from(match ctx_name {
                Some(n) => format!("move {} ▸", snippet(n, 20)),
                None => "move ▸".to_string(),
            }),
            ACCENT,
        ),
        PaletteMode::Recall => (SharedString::from("⌘K"), MUTED2),
    };
    let placeholder = match state.mode {
        PaletteMode::QuickAdd { parent: Some(_) } => "name the new node…",
        PaletteMode::QuickAdd { parent: None } => "capture a thought — files to ideas…",
        PaletteMode::MoveTo { .. } => "filter destinations…",
        PaletteMode::Recall => "recall — nodes · decisions · sessions…",
    };
    let query_line = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(9.))
        .px(px(14.))
        .py(px(11.))
        .child(
            div()
                .flex_none()
                .text_size(px(10.5))
                .font_family("Menlo")
                .text_color(rgb(chip_color))
                .child(chip),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .flex()
                .flex_row()
                .items_center()
                .font_family("Menlo")
                .text_size(px(13.5))
                .when(state.query.is_empty(), |c| {
                    c.child(div().text_color(rgb(ACCENT)).child("▏"))
                        .child(div().text_color(rgb(MUTED2)).child(placeholder))
                })
                .when(!state.query.is_empty(), |c| {
                    c.child(
                        div()
                            .text_color(rgb(TEXT_STRONG))
                            .child(SharedString::from(state.query.clone())),
                    )
                    .child(div().text_color(rgb(ACCENT)).child("▏"))
                }),
        );

    // one selectable row's chrome (spine + selection/hover), shared by verb,
    // hit, and destination rows so the flat `sel` index always looks the same.
    fn sel_row(ix: usize, is_sel: bool, eid: SharedString, activate: IxCb) -> Stateful<Div> {
        div()
            .id(eid)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(9.))
            .mx(px(6.))
            .px(px(7.))
            .py(px(6.))
            .rounded(px(8.))
            .cursor_pointer()
            .when(is_sel, |s| s.bg(rgb(CARD2)))
            .when(!is_sel, |s| s.hover(|s| s.bg(rgb(CARD))))
            .on_click(move |_: &ClickEvent, window, app| activate(ix, window, app))
            // the selection bar — a colored spine, steadier than a border swap.
            .child(
                div()
                    .flex_none()
                    .w(px(3.))
                    .h(px(22.))
                    .rounded(px(2.))
                    .when(is_sel, |b| b.bg(rgb(ACCENT))),
            )
    }

    // ── body: verb+hit rows (recall) / destination rows (move-to) / the
    // instant-add echo line (quick-add).
    let mut body = div()
        .flex()
        .flex_col()
        .py(px(5.))
        .border_t_1()
        .border_color(rgb(HAIR_SOFT));
    match state.mode {
        PaletteMode::QuickAdd { parent } => {
            if !q.is_empty() {
                // instant human lane (docs/019 commitment 1): no propose card,
                // ⌘Z is the confirmation the old "nothing sticks until ✓" was.
                let line = match parent {
                    Some(_) => format!("↵ adds “{q}” and selects it — ⌘Z undoes"),
                    None => format!("↵ files “{q}” in the ideas tray — no filing decision now"),
                };
                body = body.child(
                    div()
                        .px(px(16.))
                        .py(px(6.))
                        .text_size(px(12.))
                        .text_color(rgb(MUTED))
                        .child(SharedString::from(line)),
                );
            }
        }
        PaletteMode::MoveTo { .. } => {
            if state.targets.is_empty() {
                body = body.child(
                    div()
                        .px(px(16.))
                        .py(px(6.))
                        .text_size(px(12.))
                        .text_color(rgb(MUTED2))
                        .child(SharedString::from(format!("no destination matches “{q}”"))),
                );
            }
            for (ix, t) in state.targets.iter().enumerate() {
                let is_sel = ix == state.sel;
                body = body.child(
                    sel_row(
                        ix,
                        is_sel,
                        format!("pal-dest-{ix}").into(),
                        h.activate_hit.clone(),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(20.))
                            .text_size(px(11.))
                            .font_family("Menlo")
                            .text_color(rgb(MUTED2))
                            .child("→"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_size(px(13.))
                            .text_color(rgb(if is_sel { TEXT_STRONG } else { TEXT }))
                            .child(SharedString::from(t.path.clone())),
                    ),
                );
            }
            if let Some(n) = more_note(state.total, state.targets.len()) {
                body = body.child(
                    div()
                        .px(px(16.))
                        .py(px(4.))
                        .text_size(px(10.5))
                        .text_color(rgb(MUTED2))
                        .child(SharedString::from(n)),
                );
            }
        }
        PaletteMode::Recall => {
            // the selection's verbs lead (docs/019 PALETTE) — same flat index
            // domain as the hits below them, so ↑↓/↵ walk the draw order.
            let vlen = state.verbs.len();
            for (ix, v) in state.verbs.iter().enumerate() {
                let is_sel = ix == state.sel;
                body = body.child(
                    sel_row(
                        ix,
                        is_sel,
                        format!("pal-verb-{ix}").into(),
                        h.activate_hit.clone(),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(56.))
                            .text_size(px(9.5))
                            .font_family("Menlo")
                            .text_color(rgb(ACCENT))
                            .child("VERB"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_size(px(13.))
                            .text_color(rgb(if is_sel { TEXT_STRONG } else { TEXT }))
                            .child(SharedString::from(v.label.clone())),
                    ),
                );
            }
            if state.hits.is_empty() {
                if !q.is_empty() {
                    body = body.child(
                        div()
                            .px(px(16.))
                            .py(px(6.))
                            .text_size(px(12.))
                            .text_color(rgb(MUTED2))
                            .child(SharedString::from(no_hits_line(&q, can_capture))),
                    );
                }
            } else {
                for (ix, hit) in state.hits.iter().enumerate() {
                    let is_sel = ix + vlen == state.sel;
                    let (tag, tc) = kind_tag(&hit.kind);
                    let (primary, secondary) = row_text(hit);
                    body = body.child(
                        sel_row(
                            ix + vlen,
                            is_sel,
                            format!("pal-hit-{ix}").into(),
                            h.activate_hit.clone(),
                        )
                        .child(
                            div()
                                .flex_none()
                                .w(px(56.))
                                .text_size(px(9.5))
                                .font_family("Menlo")
                                .text_color(rgb(tc))
                                .child(tag),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .text_color(rgb(if is_sel { TEXT_STRONG } else { TEXT }))
                                        .child(SharedString::from(primary)),
                                )
                                .when(!secondary.is_empty(), |c| {
                                    c.child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(rgb(MUTED2))
                                            .child(SharedString::from(secondary)),
                                    )
                                }),
                        )
                        // provenance: which project the memory belongs to, right-aligned.
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(10.5))
                                .font_family("Menlo")
                                .text_color(rgb(MUTED2))
                                .child(SharedString::from(hit.project_key.clone())),
                        ),
                    );
                }
                if let Some(n) = more_note(state.total, state.hits.len()) {
                    body = body.child(
                        div()
                            .px(px(16.))
                            .py(px(4.))
                            .text_size(px(10.5))
                            .text_color(rgb(MUTED2))
                            .child(SharedString::from(n)),
                    );
                }
            }
        }
    }

    let footer = div()
        .border_t_1()
        .border_color(rgb(HAIR_SOFT))
        .px(px(14.))
        .py(px(7.))
        .text_size(px(10.))
        .text_color(rgb(MUTED2))
        .child(footer_hint(state.mode, can_capture));

    // the body shows whenever there are rows to pick: MoveTo lists every
    // destination before any typing, Recall shows the selection's verbs on an
    // empty query; QuickAdd only ever shows its echo line.
    let show_body = match state.mode {
        PaletteMode::MoveTo { .. } => true,
        PaletteMode::Recall => !q.is_empty() || !state.verbs.is_empty(),
        PaletteMode::QuickAdd { .. } => !q.is_empty(),
    };
    let panel = div()
        .id("palette-panel")
        .mt(px(110.))
        .w(px(560.))
        .max_w(px(560.))
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(HAIR))
        .rounded(px(14.))
        .overflow_hidden()
        .flex()
        .flex_col()
        // clicks inside the panel must not reach the scrim's close.
        .on_click(move |_: &ClickEvent, _window, app| app.stop_propagation())
        .child(query_line)
        .when(show_body, |c| c.child(body))
        .child(footer);

    let close = h.close.clone();
    div()
        .id("palette-scrim")
        .absolute()
        .size_full()
        .flex()
        .flex_row()
        .justify_center()
        .items_start()
        .bg(rgba(0x05070AB0))
        .on_click(move |_: &ClickEvent, window, app| close(window, app))
        .child(panel)
}

#[cfg(test)]
mod tests {
    // NOT `use super::*`: the module glob-imports gpui, and gpui exports its
    // own `test` attribute macro — a glob would shadow the prelude's `#[test]`
    // and recurse at expansion. Import exactly what the tests touch.
    use super::{
        filter_targets, footer_hint, fuzzy_pos, group_order, hit_event, kind_tag, more_note,
        move_targets, no_hits_line, row_text, snippet, verb_rows, MoveTarget, NodeVerb,
        PaletteEvent, PaletteMode, PaletteState, SearchHit, VerbRow, AMBER, GREEN, MUTED2,
        ROWS_CAP, SESSION_BLUE,
    };
    use orchestrator_store::{Kind, Lifecycle, Part, PartId};

    fn hit(kind: &str, ref_id: &str, title: &str, body: &str) -> SearchHit {
        SearchHit {
            kind: kind.into(),
            project_key: "kod".into(),
            ref_id: ref_id.into(),
            title: title.into(),
            body: body.into(),
        }
    }

    fn node(id: PartId, parent: Option<PartId>, name: &str) -> Part {
        Part {
            id,
            parent_id: parent,
            name: name.into(),
            sort_order: id as f64,
            ..Part::default()
        }
    }

    #[test]
    fn edits_reset_selection_and_are_multibyte_safe() {
        let mut s = PaletteState::default();
        s.open_recall();
        s.push_str("文字 map");
        s.sel = 3;
        s.push_str("!"); // an IME commit can arrive mid-navigation
        assert_eq!(s.sel, 0, "any edit restarts selection at the top");
        s.backspace();
        assert_eq!(s.query, "文字 map", "backspace pops one CHAR, not one byte");
        s.sel = 2;
        s.backspace();
        assert_eq!((s.query.as_str(), s.sel), ("文字 ma", 0));
        // backspace on an empty query: no-op, never a panic.
        let mut e = PaletteState::default();
        e.backspace();
        assert_eq!(e.query, "");
    }

    #[test]
    fn nav_wraps_both_ways() {
        let mut s = PaletteState::default();
        s.adopt_hits(vec![
            hit("node", "1", "a", ""),
            hit("node", "2", "b", ""),
            hit("node", "3", "c", ""),
        ]);
        assert_eq!(s.sel, 0);
        s.next();
        s.next();
        assert_eq!(s.sel, 2);
        s.next();
        assert_eq!(s.sel, 0, "next wraps to the first hit");
        s.prev();
        assert_eq!(s.sel, 2, "prev wraps to the last hit");
        // empty hits: nav is a no-op, never a panic.
        let mut e = PaletteState::default();
        e.next();
        e.prev();
        assert_eq!(e.sel, 0);
    }

    #[test]
    fn adopt_rank_truncates_then_groups() {
        // 14 hits by rank: a summary first, a note second, then 12 nodes —
        // rank-truncation keeps the top 12 (summary + note survive the cap;
        // the two WORST nodes drop), then grouping reorders for display.
        let mut all = vec![
            hit("summary", "sess-1", "goal", "headline"),
            hit("note", "4", "decision", "kill-switch stays"),
        ];
        for i in 0..12 {
            all.push(hit("node", &format!("{i}"), &format!("n{i}"), ""));
        }
        let mut s = PaletteState {
            sel: 5,
            ..Default::default()
        };
        s.adopt_hits(all);
        assert_eq!(s.hits.len(), ROWS_CAP);
        assert_eq!(s.total, 14, "total remembers the pre-cap count");
        assert_eq!(s.sel, 0, "adoption resets the selection");
        let kinds: Vec<&str> = s.hits.iter().map(|h| h.kind.as_str()).collect();
        assert_eq!(&kinds[..10], &["node"; 10], "nodes first");
        assert_eq!(&kinds[10..], &["note", "summary"]);
        // stable within a group: FTS rank order survives the group sort.
        let ids: Vec<&str> = s.hits[..10].iter().map(|h| h.ref_id.as_str()).collect();
        assert_eq!(ids, vec!["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"]);
        assert_eq!(
            more_note(s.total, s.hits.len()).as_deref(),
            Some("+2 more — keep typing")
        );
        assert_eq!(more_note(3, 3), None);
    }

    #[test]
    fn open_close_reset_everything() {
        let mut s = PaletteState::default();
        s.open_quick_add(Some(7));
        assert!(s.open);
        assert_eq!(s.mode, PaletteMode::QuickAdd { parent: Some(7) });
        s.push_str("auth");
        s.adopt_hits(vec![hit("node", "1", "a", "")]);
        s.close();
        assert!(!s.open);
        assert_eq!(
            (s.query.as_str(), s.hits.len(), s.total, s.sel),
            ("", 0, 0, 0)
        );
        assert_eq!(
            s.mode,
            PaletteMode::Recall,
            "close forgets the quick-add parent"
        );
        // reopening in recall mode from quick-add leaves no residue either.
        s.open_quick_add(Some(9));
        s.push_str("x");
        s.open_recall();
        assert_eq!((s.mode, s.query.as_str()), (PaletteMode::Recall, ""));
        // move-to residue (targets) clears on close too.
        s.open_move_to(3);
        s.adopt_targets(vec![MoveTarget {
            dest: None,
            path: "▸ root".into(),
            leaf: "▸ root".into(),
        }]);
        s.adopt_verbs(vec![VerbRow {
            verb: NodeVerb::Rename,
            label: "rename".into(),
        }]);
        s.close();
        assert_eq!(
            (s.targets.len(), s.verbs.len(), s.mode),
            (0, 0, PaletteMode::Recall)
        );
    }

    #[test]
    fn confirm_quick_add_trims_and_requires_a_name() {
        let mut s = PaletteState::default();
        s.open_quick_add(Some(7));
        s.push_str("   ");
        assert_eq!(s.confirm(false), None, "whitespace is not a node name");
        s.query.clear();
        s.push_str("  auth flow  ");
        assert_eq!(
            s.confirm(false),
            Some(PaletteEvent::AddNode {
                parent: Some(7),
                name: "auth flow".into()
            })
        );
        // ⌘ emits the SAME event — keep-open is the integrator's concern,
        // the op is identical (docs/019 rapid capture).
        assert_eq!(
            s.confirm(true),
            Some(PaletteEvent::AddNode {
                parent: Some(7),
                name: "auth flow".into()
            })
        );
        // no meaningful selection at open → the idea tray (commitment 5).
        s.open_quick_add(None);
        s.push_str("cinematic check-in");
        assert_eq!(
            s.confirm(true),
            Some(PaletteEvent::AddNode {
                parent: None,
                name: "cinematic check-in".into()
            })
        );
    }

    #[test]
    fn confirm_move_to_targets_and_nav_span_the_target_list() {
        let mut s = PaletteState::default();
        s.open_move_to(9);
        assert_eq!(s.confirm(false), None, "no targets adopted yet");
        s.adopt_targets(vec![
            MoveTarget {
                dest: None,
                path: "▸ root".into(),
                leaf: "▸ root".into(),
            },
            MoveTarget {
                dest: Some(3),
                path: "a ▸ b".into(),
                leaf: "b".into(),
            },
        ]);
        assert_eq!(
            s.confirm(false),
            Some(PaletteEvent::MoveNode { id: 9, dest: None })
        );
        s.next();
        assert_eq!(
            s.confirm(false),
            Some(PaletteEvent::MoveNode {
                id: 9,
                dest: Some(3)
            })
        );
        s.next();
        assert_eq!(s.sel, 0, "nav wraps over targets, not hits");
    }

    #[test]
    fn recall_default_opens_first_hit_verbs_reached_by_arrows() {
        let mut s = PaletteState::default();
        s.open_recall();
        s.adopt_verbs(vec![
            VerbRow {
                verb: NodeVerb::Rename,
                label: "rename “x”".into(),
            },
            VerbRow {
                verb: NodeVerb::Delete,
                label: "delete “x”".into(),
            },
        ]);
        s.adopt_hits(vec![hit("node", "7", "Kill switch", "")]);
        // default ↵ opens the top HIT, never a verb (review: a destructive
        // verb must never be the Enter default even when it leads the draw).
        assert_eq!(
            s.confirm(false),
            Some(PaletteEvent::FocusNode {
                project_key: "kod".into(),
                part: 7
            })
        );
        // ↑ from the first hit lands on the last verb (verbs sit above).
        s.prev();
        assert_eq!(s.confirm(false), Some(PaletteEvent::Verb(NodeVerb::Delete)));
        s.prev();
        assert_eq!(s.confirm(false), Some(PaletteEvent::Verb(NodeVerb::Rename)));
        // ⌘↵ still adds "here" regardless of what row is selected.
        s.push_str("kill");
        assert_eq!(
            s.confirm(true),
            Some(PaletteEvent::AddHere {
                name: "kill".into()
            })
        );
    }

    #[test]
    fn recall_verb_only_query_never_deletes_on_default_enter() {
        // the review's exact bug: query fuzzy-matches ONLY the delete verb,
        // there are no hits — ↵ must be a safe no-op, not a silent delete.
        let mut s = PaletteState::default();
        s.open_recall();
        s.adopt_verbs(vec![VerbRow {
            verb: NodeVerb::Delete,
            label: "delete “x”".into(),
        }]);
        s.adopt_hits(vec![]);
        assert_eq!(
            s.confirm(false),
            None,
            "no hit to open → ↵ does nothing, never fires Delete"
        );
        // the verb is still reachable deliberately (↓ wraps onto it).
        s.next();
        assert_eq!(s.confirm(false), Some(PaletteEvent::Verb(NodeVerb::Delete)));
    }

    #[test]
    fn filter_targets_matches_a_name_containing_the_separator() {
        // review: a node literally named "Roadmap ▸ Q3" must be findable by
        // its own name, not mangled by a path split on " ▸ ".
        let targets = vec![
            MoveTarget {
                dest: Some(1),
                path: "Plans ▸ Roadmap ▸ Q3".into(),
                leaf: "Roadmap ▸ Q3".into(),
            },
            MoveTarget {
                dest: Some(2),
                path: "Other".into(),
                leaf: "Other".into(),
            },
        ];
        let out = filter_targets(&targets, "Roadmap ▸ Q3");
        assert_eq!(
            out.first().map(|t| t.dest),
            Some(Some(1)),
            "leaf match ranks the node first"
        );
    }

    #[test]
    fn confirm_recall_opens_selection_and_cmd_adds_here() {
        let mut s = PaletteState::default();
        s.open_recall();
        assert_eq!(s.confirm(false), None, "no hits, nothing to open");
        assert_eq!(s.confirm(true), None, "⌘↵ with an empty query adds nothing");
        s.push_str("kill");
        assert_eq!(
            s.confirm(true),
            Some(PaletteEvent::AddHere {
                name: "kill".into()
            })
        );
        s.adopt_hits(vec![
            hit("node", "7", "Kill switch", "the master off"),
            hit("note", "9", "decision", "kill-switch stays"),
            hit("summary", "sess-a3f2", "goal", "headline"),
        ]);
        assert_eq!(
            s.confirm(false),
            Some(PaletteEvent::FocusNode {
                project_key: "kod".into(),
                part: 7
            })
        );
        s.next();
        assert_eq!(
            s.confirm(false),
            Some(PaletteEvent::OpenLog {
                project_key: "kod".into(),
                part: 9
            })
        );
        s.next();
        assert_eq!(
            s.confirm(false),
            Some(PaletteEvent::OpenSession {
                project_key: "kod".into(),
                cli_session_id: "sess-a3f2".into()
            })
        );
        // a corrupt ref_id must not fabricate a jump target.
        assert_eq!(hit_event(&hit("node", "not-a-number", "x", "")), None);
        assert_eq!(hit_event(&hit("wat", "1", "x", "")), None);
    }

    #[test]
    fn tags_and_group_order_cover_unknown_kinds() {
        assert_eq!(kind_tag("node"), ("NODE", GREEN));
        assert_eq!(kind_tag("note"), ("DECISION", AMBER));
        assert_eq!(kind_tag("summary"), ("SESSION", SESSION_BLUE));
        assert_eq!(kind_tag("wat"), ("?", MUTED2));
        assert!(group_order("node") < group_order("note"));
        assert!(group_order("note") < group_order("summary"));
        assert!(
            group_order("summary") < group_order("wat"),
            "unknown kinds sink, never vanish"
        );
    }

    #[test]
    fn snippet_collapses_whitespace_and_truncates_by_chars() {
        assert_eq!(snippet("a\n  b\t\tc", 90), "a b c");
        assert_eq!(snippet("abc", 3), "abc", "exactly at the cap: no ellipsis");
        assert_eq!(snippet("abcd", 3), "abc…");
        // char-wise truncation: never slices inside a multibyte boundary.
        assert_eq!(snippet("文字文字文", 3), "文字文…");
        // a trailing space at the cut is trimmed before the ellipsis.
        assert_eq!(snippet("aaaa bbbb", 5), "aaaa…");
    }

    #[test]
    fn fuzzy_is_a_case_insensitive_subsequence_with_first_match_pos() {
        // greedy: the first matched char anchors the position — 't' lands on
        // "Hos-t-ed" (3), not on "Terminal", and the rest still fits after it.
        assert_eq!(
            fuzzy_pos("Hosted sessions ▸ Terminal host", "termhost"),
            Some(3)
        );
        assert_eq!(fuzzy_pos("Terminal host", "termhost"), Some(0));
        assert_eq!(fuzzy_pos("Hosted sessions", "HOSE"), Some(0), "case folds");
        assert_eq!(fuzzy_pos("abc", ""), Some(0), "empty query matches all");
        assert_eq!(
            fuzzy_pos("abc", "   "),
            Some(0),
            "whitespace query = no filter"
        );
        assert_eq!(fuzzy_pos("abc", "acb"), None, "order matters");
        assert_eq!(fuzzy_pos("", "a"), None);
    }

    #[test]
    fn move_targets_exclude_subtree_and_current_parent_and_lead_with_root() {
        // 1 > [2 > 5, 3], 4 (roots 1 and 4)
        let parts = vec![
            node(1, None, "n1"),
            node(2, Some(1), "n2"),
            node(5, Some(2), "n5"),
            node(3, Some(1), "n3"),
            node(4, None, "n4"),
        ];
        // moving 2: its subtree (2,5) can't host it (cycle), parent 1 is a
        // no-move — but 1's OTHER child 3 is a real destination.
        let t = move_targets(&parts, 2);
        let paths: Vec<&str> = t.iter().map(|t| t.path.as_str()).collect();
        assert_eq!(paths, vec!["▸ root", "n1 ▸ n3", "n4"]);
        assert_eq!(t[0].dest, None);
        assert_eq!((t[1].dest, t[2].dest), (Some(3), Some(4)));
        // moving a ROOT: no "▸ root" row (it already lives there).
        let t = move_targets(&parts, 1);
        let paths: Vec<&str> = t.iter().map(|t| t.path.as_str()).collect();
        assert_eq!(paths, vec!["n4"], "1's whole subtree is denied");
    }

    #[test]
    fn filter_targets_ranks_leaf_matches_over_path_matches() {
        let t = |dest: Option<PartId>, path: &str| MoveTarget {
            dest,
            path: path.into(),
            leaf: path.rsplit(" ▸ ").next().unwrap_or(path).into(),
        };
        let all = vec![
            t(None, "▸ root"),
            t(Some(1), "Hosted sessions"),
            t(Some(2), "Hosted sessions ▸ Terminal host"),
        ];
        // "hosted" fits the LEAF "Hosted sessions" (tier 0) but only the PATH
        // of "… ▸ Terminal host" (tier 1) — the named node wins.
        let f = filter_targets(&all, "hosted");
        let paths: Vec<&str> = f.iter().map(|t| t.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["Hosted sessions", "Hosted sessions ▸ Terminal host"]
        );
        // empty query keeps everything in tree order, root row first.
        assert_eq!(filter_targets(&all, "").len(), 3);
        assert_eq!(filter_targets(&all, "")[0].dest, None);
        assert!(filter_targets(&all, "zzz").is_empty());
    }

    #[test]
    fn verb_rows_filter_on_keyword_and_show_the_outcome() {
        let mut n = node(7, None, "auth flow");
        n.lifecycle = Lifecycle::Todo;
        n.kind = Kind::Task;
        let all = verb_rows(&n, "");
        let labels: Vec<&str> = all.iter().map(|v| v.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "rename “auth flow”",
                "status: todo → done",
                "kind: task → idea",
                "delete “auth flow” — subtree, ⌘Z undoes"
            ]
        );
        assert_eq!(all[1].verb, NodeVerb::CycleStatus);
        // keyword filter: the label carries the node NAME, but matching on it
        // would shadow search hits whenever the query resembles the selection.
        let f = verb_rows(&n, "ren");
        assert_eq!((f.len(), f[0].verb), (1, NodeVerb::Rename));
        assert!(
            verb_rows(&n, "auth").is_empty(),
            "node name never matches a verb"
        );
        assert!(verb_rows(&n, "zzz").is_empty());
    }

    #[test]
    fn row_text_leads_with_what_the_user_recalls() {
        // node: the name, detail as the snippet line.
        assert_eq!(
            row_text(&hit("node", "1", "Kill switch", "the master off")),
            ("Kill switch".into(), "the master off".into())
        );
        // note: the TEXT is the memory — the kind is already the amber tag.
        assert_eq!(
            row_text(&hit("note", "9", "decision", "ship  the\nenum")),
            ("ship the enum".into(), String::new())
        );
        // summary: goal first, headline + next step under it.
        assert_eq!(
            row_text(&hit("summary", "s", "wire the map", "did x. next: y")),
            ("wire the map".into(), "did x. next: y".into())
        );
    }

    /// ⌘K survives the map gate (session recall is ungated) but ⌘↵ inside it does
    /// NOT — `capture_refusal` turns it into a red banner. Both of Recall's hints
    /// used to advertise it anyway, which is the same defect as a verb whose
    /// tagline names a surface that isn't compiled in.
    #[test]
    fn recall_never_offers_a_capture_that_would_be_refused() {
        assert_eq!(
            footer_hint(PaletteMode::Recall, true),
            "↵ open · ⌘↵ add here · ↑↓ move · esc"
        );
        let off = footer_hint(PaletteMode::Recall, false);
        assert!(!off.contains('⌘'), "no key the build refuses: {off}");
        assert!(off.contains("↵ open"), "recall still opens hits: {off}");
        assert_eq!(
            no_hits_line("auth refactor", true),
            "no matches — ⌘↵ adds “auth refactor” as a new node"
        );
        // the empty-search line is where the user is MOST likely to take the
        // suggestion, so it is the one that must not make one.
        assert_eq!(no_hits_line("auth refactor", false), "no matches for “auth refactor”");
    }
}
