//! mapview — the Workspace's LEFT brain-map CANVAS (docs/010 §3b, docs/011 §B):
//! the TWO-GENERATION canvas. A canvas rooted at R renders children(R) as
//! GEN-0 full cards (column-packed, draggable, pins persisted) and
//! grandchildren as GEN-1 SATELLITES — compact 128px cards auto-packed in
//! rows below their parent, deriving from the parent's FINAL rendered
//! position each frame (so a mid-drag preview carries them). Depth ≥2 is
//! hidden behind a '▸N' badge; `attention_rollup` bubbles hidden needs-you /
//! live / proposal attention onto the visible ancestor so collapse never lies.
//!
//! Positions are NORMALIZED 0..1 (`part.map_x`/`map_y`) — REINTERPRETED as
//! 'position on my parent's canvas' (frame-per-level): every part is gen-0 on
//! exactly one canvas, so satellite pins are IGNORED here and apply one drill
//! down. `None` = auto-layout. All pixel mapping happens in `layout_nodes`.
//!
//! main.rs-agnostic by construction (the outlinepane pattern): every mutation
//! is a caller-supplied closure (`MapHandlers`), and the drag STATE lives in
//! the Orchestrator (`MapDrag` field) — this module owns only the math.
//!
//! Drag protocol (the scrollbar-anchor pattern, main.rs 3b): mouse-down on a
//! gen-0 card arms `MapDrag` (grab point + ORIGINAL normalized pos); every
//! move recomputes the absolute `drag_update(..)` — never incremental deltas —
//! and the integrator feeds `drag.cur` back through `layout_nodes`'
//! `persisted` closure so the card (and its satellites) follow the cursor; ON
//! MOUSE-UP THE INTEGRATOR CALLS `store.set_part_pos(id, cur.0, cur.1)` and
//! clears the field. Satellites never arm a drag (no pins at this level) —
//! the integrator's `node_down` checks `NodePos.gen`.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::*;
use orchestrator_store::tree::countable_ratio;
use orchestrator_store::{Lifecycle, Part, PartId, TreeNode};

// Focused Dark tokens used by the map render (values match main.rs).
const CARD: u32 = 0x191D27;
const CARD2: u32 = 0x1D222C;
const HAIR: u32 = 0x2B303B;
const HAIR_SOFT: u32 = 0x23272F;
const TEXT_STRONG: u32 = 0xF2F5FA;
const MUTED: u32 = 0x9AA3B1;
const MUTED2: u32 = 0x757E8A;
const ACCENT: u32 = 0x7EE2C0;
const AMBER: u32 = 0xE6C07A;
const GREEN: u32 = 0x5BB99B;
/// amber-tinted hairline (building borders / active edges — matches
/// main.rs's render_seed_proposal border).
const AMBER_HAIR: u32 = 0x4A4636;
/// the NEEDS-YOU tag's dark amber fill.
const AMBER_INK: u32 = 0x2A2417;
/// ▶ dispatch at rest: MUTED at ~40% alpha — always present, never shouting
/// (docs/011 §C); hover swaps to full ACCENT.
const DISPATCH_INK: u32 = 0x9AA3B166;

/// Card widths (the mock's 124–182px range): ideas are visually smaller.
const NODE_W: f32 = 168.0;
const IDEA_W: f32 = 140.0;
/// A fresh leaf TASK card's extent — `node_size` of a node with no detail and
/// no children. Dbl-click-create (docs/019 CANVAS) pins the click point
/// BEFORE the node exists, so the pin math needs the extent up front.
pub const DEFAULT_CARD: (f32, f32) = (NODE_W, 51.0);
/// Gen-1 satellite card width (docs/011 §B: 128px compact cards).
pub const SAT_W: f32 = 128.0;
const SAT_H: f32 = 26.0;
const SAT_GAP: f32 = 8.0;
/// Satellites hang slightly indented under their parent's left edge.
const SAT_INDENT: f32 = 12.0;
/// Satellites pack row-major, this many per row.
const SAT_COLS: usize = 2;
/// Vertical breathing room between a gen-0 card and its satellite block.
// generous: the card may grow a live agent-chip row (~20px) that the pure
// node_size estimate can't see — satellites must never cover it.
const SAT_TOP_GAP: f32 = 30.0;

// ---- (1) pure layout: normalized slots → pixel rects ----

/// A laid-out node: pixel rect within the canvas (top-left origin).
/// `gen`: 0 = full card (child of the canvas root, draggable/pinnable),
/// 1 = satellite (grandchild — derives from its parent, never pinned here).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NodePos {
    pub id: PartId,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// footprint extents (card + satellite block) — pins and drags normalize
    /// against THESE, not `w`/`h`, so a flush-pinned parent's satellites stay
    /// inside the canvas at every pass height (review: the two-pass sizing
    /// never converged for pinned parents otherwise, clipping satellites AND
    /// their attention markers). Satellites: fp == card extent.
    pub fp_w: f32,
    pub fp_h: f32,
    pub gen: u8,
}

/// Estimated gen-0 card size — used for connector attach points, drag
/// clamping and the overlap guarantee. A slight under-estimate is invisible:
/// cards paint OVER the connector layer, so an edge that starts a few px
/// inside a taller card is hidden by the card itself.
pub fn node_size(node: &TreeNode) -> (f32, f32) {
    let w = if node.part.lifecycle == Lifecycle::Idea {
        IDEA_W
    } else {
        NODE_W
    };
    let mut h = 51.0; // padding + name row + the always-rendered meta row
    if !node.part.detail.is_empty() {
        h += 15.0;
    }
    let (_, total) = countable_ratio(node);
    if total > 0 && !node.children.is_empty() {
        h += 12.0; // mini progress bar
    }
    (w, h)
}

/// Normalized 0..1 → pixels: 0.0 = flush left/top, 1.0 = flush right/bottom
/// with the card still FULLY visible (the extent is subtracted from the span).
pub fn norm_to_px(n: f64, span: f32, extent: f32) -> f32 {
    let range = (span - extent).max(0.0);
    (n.clamp(0.0, 1.0) as f32 * range).round()
}

/// Pixels → normalized 0..1 (inverse of `norm_to_px`; degenerate spans where
/// the card is wider than the canvas pin to 0 instead of dividing by zero).
pub fn px_to_norm(p: f32, span: f32, extent: f32) -> f64 {
    let range = (span - extent).max(1.0);
    ((p / range) as f64).clamp(0.0, 1.0)
}

/// A laid-out node's normalized position — the drag anchor (`MapDrag.orig`)
/// and what `store.set_part_pos` ultimately persists.
pub fn norm_of(pos: &NodePos, canvas_w: f32, canvas_h: f32) -> (f64, f64) {
    (
        px_to_norm(pos.x, canvas_w, pos.fp_w),
        px_to_norm(pos.y, canvas_h, pos.fp_h),
    )
}

const GAP: f32 = 14.0;

/// Satellite `i`'s offset RELATIVE to its parent card's top-left (row-major
/// grid below the card). Pure so a drag preview and the packer agree.
fn satellite_offset(i: usize, parent_h: f32) -> (f32, f32) {
    let row = (i / SAT_COLS) as f32;
    let col = (i % SAT_COLS) as f32;
    (
        SAT_INDENT + col * (SAT_W + SAT_GAP),
        parent_h + SAT_TOP_GAP + row * (SAT_H + SAT_GAP),
    )
}

/// The column footprint a gen-0 card claims: its own card PLUS its satellite
/// block. Neighboring columns and bands pack against THIS, so no default
/// position can overlap another parent's satellites.
fn footprint(node: &TreeNode) -> (f32, f32) {
    let (w, h) = node_size(node);
    let n = node.children.len();
    if n == 0 {
        return (w, h);
    }
    let cols = n.min(SAT_COLS) as f32;
    let rows = n.div_ceil(SAT_COLS) as f32;
    let sat_w = SAT_INDENT + cols * SAT_W + (cols - 1.0) * SAT_GAP;
    let sat_h = h + SAT_TOP_GAP + rows * SAT_H + (rows - 1.0) * SAT_GAP;
    (w.max(sat_w), sat_h)
}

/// Lay out TWO GENERATIONS (docs/011 §B): `tree` = children of the canvas
/// root. Each gets a gen-0 column (footprint-packed left→right, wrapping into
/// bands); its children become gen-1 satellites offset from the parent's
/// FINAL rendered position — persisted pin or live drag included — so
/// satellites follow a mid-drag preview. Depth ≥2 emits NO NodePos (hidden
/// behind the '▸N' badge). Satellite pins are ignored here (frame-per-level:
/// their map_x/map_y belongs to the canvas one drill down). Output stays DFS
/// pre-order over the visible generations. Content may flow BELOW canvas_h —
/// the caller sizes the container to `content_height` and re-lays-out with
/// it, so the norm frame matches.
pub fn layout_nodes(
    tree: &[TreeNode],
    persisted: impl Fn(PartId) -> Option<(f64, f64)>,
    canvas_w: f32,
    canvas_h: f32,
) -> Vec<NodePos> {
    let mut out = Vec::new();
    let mut cur_x = GAP;
    let mut band_y = GAP;
    let mut band_bottom = GAP;
    for n in tree {
        let (fw, fh) = footprint(n);
        if cur_x > GAP && cur_x + fw + GAP > canvas_w {
            cur_x = GAP;
            band_y = band_bottom + GAP * 1.5;
        }
        let (w, h) = node_size(n);
        let mut pos = NodePos {
            id: n.part.id,
            x: cur_x,
            y: band_y,
            w,
            h,
            fp_w: fw,
            fp_h: fh,
            gen: 0,
        };
        // persisted pin (or the live drag layered upstream) wins; satellites
        // below derive from this FINAL position, so they chase the drag.
        // The pin normalizes against the FOOTPRINT (fw/fh): flush-bottom means
        // the whole block — card AND satellites — is flush, never clipped.
        if let Some((nx, ny)) = persisted(n.part.id) {
            pos.x = norm_to_px(nx.clamp(0.0, 1.0), canvas_w, fw);
            pos.y = norm_to_px(ny.clamp(0.0, 1.0), canvas_h, fh);
        }
        out.push(pos);
        for (i, c) in n.children.iter().enumerate() {
            let (ox, oy) = satellite_offset(i, h);
            out.push(NodePos {
                id: c.part.id,
                x: pos.x + ox,
                y: pos.y + oy,
                w: SAT_W,
                h: SAT_H,
                fp_w: SAT_W,
                fp_h: SAT_H,
                gen: 1,
            });
        }
        band_bottom = band_bottom.max(band_y + fh);
        cur_x += fw + GAP * 1.5;
    }
    out
}

/// The pixel height the laid-out content needs (for content-sized canvases).
/// Positions include satellites, so a tall satellite block grows the canvas.
pub fn content_height(positions: &[NodePos]) -> f32 {
    positions.iter().map(|p| p.y + p.h).fold(0.0, f32::max) + GAP
}

// ---- (4) drag math (state lives in the Orchestrator) ----

/// A live node drag: grab point + the node's ORIGINAL normalized position,
/// captured at mouse-down. `cur` is the live position the map renders while
/// dragging (via `layout_nodes`' `persisted` closure). On mouse-up the
/// integrator persists `cur` with `store.set_part_pos` and clears the field.
#[derive(Clone, Copy)]
pub struct MapDrag {
    pub id: PartId,
    pub grab: Point<Pixels>,
    pub orig: (f64, f64),
    pub node_w: f32,
    pub node_h: f32,
    pub cur: (f64, f64),
    /// ⌥ held (docs/019 CANVAS): this drag is a REPARENT — the drop lands ONE
    /// Move, never a pin. Tracked live per mouse-move, so pressing/releasing
    /// Option mid-drag converts the gesture either way.
    pub alt: bool,
    /// the reparent drop target under the cursor (accent ring); None over
    /// empty canvas = drop moves to the current drill-frame root.
    pub target: Option<PartId>,
}

/// `id` plus every descendant, from flat (id, parent) pairs — the ⌥-drag DENY
/// set: the dragged card's satellites chase the drag preview, so without this
/// they'd always be the topmost hit under the cursor — and dropping a node
/// into its own subtree would cycle the tree besides. Fixpoint over the flat
/// pairs: the tree is tens of rows, O(n·depth) is noise.
pub fn subtree_ids(pairs: &[(PartId, Option<PartId>)], id: PartId) -> HashSet<PartId> {
    let mut out: HashSet<PartId> = HashSet::from([id]);
    loop {
        let before = out.len();
        for (c, p) in pairs {
            if p.is_some_and(|p| out.contains(&p)) {
                out.insert(*c);
            }
        }
        if out.len() == before {
            return out;
        }
    }
}

/// The reparent drop target (docs/019 ⌥-drag): the TOPMOST laid-out node
/// under `pt` (canvas-local px). `positions` is paint order (layout_nodes'
/// DFS pre-order == map_canvas z-order), so the LAST hit is what the user
/// sees under the cursor; the deny set (dragged subtree) never matches.
pub fn drop_target_at(
    positions: &[NodePos],
    deny: &HashSet<PartId>,
    pt: (f32, f32),
) -> Option<PartId> {
    positions
        .iter()
        .rev()
        .find(|p| {
            !deny.contains(&p.id)
                && pt.0 >= p.x
                && pt.0 <= p.x + p.w
                && pt.1 >= p.y
                && pt.1 <= p.y + p.h
        })
        .map(|p| p.id)
}

/// Absolute drag step: `orig + delta_px/usable_range`, clamped to 0..1 so a
/// card can never be stranded outside the canvas. Degenerate canvases (card
/// wider than the viewport) divide by 1px instead of 0 — no NaN, just a pin.
pub fn drag_to(
    orig: (f64, f64),
    delta: (f32, f32),
    canvas_w: f32,
    canvas_h: f32,
    node_w: f32,
    node_h: f32,
) -> (f64, f64) {
    let nx = orig.0 + (delta.0 / (canvas_w - node_w).max(1.0)) as f64;
    let ny = orig.1 + (delta.1 / (canvas_h - node_h).max(1.0)) as f64;
    (nx.clamp(0.0, 1.0), ny.clamp(0.0, 1.0))
}

/// One mouse-move: recompute the drag's live position from the ANCHOR and the
/// current cursor (never incremental — a missed frame can't skew the card).
pub fn drag_update(
    drag: &MapDrag,
    cursor: Point<Pixels>,
    canvas_w: f32,
    canvas_h: f32,
) -> (f64, f64) {
    let delta = (
        f32::from(cursor.x) - f32::from(drag.grab.x),
        f32::from(cursor.y) - f32::from(drag.grab.y),
    );
    drag_to(
        drag.orig,
        delta,
        canvas_w,
        canvas_h,
        drag.node_w,
        drag.node_h,
    )
}

// ---- (2) node card: pure styling fns + the element builders ----

/// Status glyph + tint, map dialect (the mock's ✓◔☐◌ — checkbox language for
/// a spatial card, vs the outline's ●◐○·). Honest uncertainty (docs/016):
/// a stale assertion decays to a hollow amber mark no matter what it claims.
pub fn map_glyph(lc: Lifecycle, stale: bool) -> (&'static str, u32) {
    if stale {
        return ("◌", AMBER); // unverified / stale — confidence lowered
    }
    match lc {
        Lifecycle::Done => ("✓", GREEN),
        Lifecycle::Building => ("◔", AMBER),
        Lifecycle::Todo => ("☐", MUTED),
        Lifecycle::Idea => ("◌", MUTED2),
    }
}

/// Card border: the selected ring wins; building glows amber-ish; everything
/// else stays hairline-quiet. (Ideas add DASHED on top of this, in the card.)
pub fn card_border(lc: Lifecycle, selected: bool) -> u32 {
    if selected {
        return ACCENT;
    }
    match lc {
        Lifecycle::Building => AMBER_HAIR,
        _ => HAIR,
    }
}

/// Whole-card opacity: done recedes, todo calms, ideas ghost; building — the
/// live frontier — is the only state at full presence.
pub fn card_opacity(lc: Lifecycle) -> f32 {
    match lc {
        Lifecycle::Done => 0.72,
        Lifecycle::Todo => 0.85,
        Lifecycle::Idea => 0.75,
        Lifecycle::Building => 1.0,
    }
}

/// Mini progress-bar fill color, keyed to the node's own state.
pub fn bar_color(lc: Lifecycle) -> u32 {
    match lc {
        Lifecycle::Building => AMBER,
        Lifecycle::Done => GREEN,
        _ => MUTED2,
    }
}

/// The one-line meta under the name: the detail (clipped to a card line), or
/// the lifecycle word when there's nothing else to say (the mock's "idea").
pub fn meta_text(p: &Part) -> String {
    if p.detail.is_empty() {
        p.lifecycle.as_str().to_string()
    } else {
        crate::termview::trim(&p.detail, 40)
    }
}

/// The '▸N' badge a satellite wears when its own subtree hides behind it
/// (docs/011 §B) — N = direct children one drill away.
pub fn sat_badge(node: &TreeNode) -> Option<String> {
    (!node.children.is_empty()).then(|| format!("▸{}", node.children.len()))
}

/// A live session chip on a node card (docs/011 §D + docs/019 chips). PASSED
/// IN, never computed here — joining live sessions against the linkage +
/// epistemic tier is the integrator's knowledge. `extra` = how many MORE live
/// sessions sit on this node beyond the one shown.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentChip {
    pub cli_id: String,
    pub label: String,
    pub busy: bool,
    pub extra: usize,
    /// docs/019 chips: `false` = a solid DISPATCH/declared chip (declared
    /// intent); `true` = a hollow-dotted OBSERVED-touch chip (the session is
    /// only observed editing here, above threshold — never intent).
    pub hollow: bool,
    /// docs/019 commitment 4: the daemon heartbeat is stale — this chip washes
    /// grey ("live link lost") instead of painting a lying alive/pulsing dot.
    pub washed: bool,
}

impl AgentChip {
    /// A solid dispatch/declared chip (the common case).
    pub fn dispatch(cli_id: String, label: String, busy: bool) -> AgentChip {
        AgentChip {
            cli_id,
            label,
            busy,
            extra: 0,
            hollow: false,
            washed: false,
        }
    }
}

/// Chip dot (color, pulses): a washed (heartbeat-lost) dot is grey + still;
/// busy pulses amber — burning tokens; idle holds steady green — your turn
/// (docs/011 §D semantics + docs/019 commitment 4).
pub fn chip_dot(busy: bool) -> (u32, bool) {
    if busy {
        (AMBER, true)
    } else {
        (GREEN, false)
    }
}

/// The derived-building badge text (docs/019 commitment 2): "◔ building" while
/// an alive dispatch/declared link runs, else "◔ built Nh/Nd ago" for a link
/// whose own last activity is <48h. Purely a function of the age seconds.
pub fn building_badge(age_secs: u64) -> String {
    if age_secs < 90 {
        "◔ building".to_string()
    } else if age_secs < 3600 {
        format!("◔ built {}m ago", age_secs / 60)
    } else if age_secs < 48 * 3600 {
        format!("◔ built {}h ago", age_secs / 3600)
    } else {
        format!("◔ built {}d ago", age_secs / 86400)
    }
}

/// Chip text: the label, plus " ·N live" when more sessions sit on the node.
pub fn chip_text(chip: &AgentChip) -> String {
    if chip.extra > 0 {
        format!("{} ·{} live", chip.label, chip.extra)
    } else {
        chip.label.clone()
    }
}

// ---- roll-up: collapse must NEVER hide attention (docs/011 §B) ----

/// Attention hiding behind a card: counts over descendants NOT visible from
/// it. `hidden_live` counts NODES carrying a live chip (render only needs
/// presence); `hidden_props` counts pending-proposal target nodes.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rollup {
    pub hidden_needs: usize,
    pub hidden_live: usize,
    pub hidden_props: usize,
}

/// SATELLITE semantics: EVERY strict descendant hides behind the '▸N' badge,
/// so the whole subtree below `node` counts.
pub fn attention_rollup(
    node: &TreeNode,
    needs: &HashSet<PartId>,
    agents: &HashMap<PartId, AgentChip>,
    prop_targets: &HashSet<PartId>,
) -> Rollup {
    fn walk(
        n: &TreeNode,
        needs: &HashSet<PartId>,
        agents: &HashMap<PartId, AgentChip>,
        props: &HashSet<PartId>,
        r: &mut Rollup,
    ) {
        let id = n.part.id;
        if needs.contains(&id) {
            r.hidden_needs += 1;
        }
        if agents.contains_key(&id) {
            r.hidden_live += 1;
        }
        if props.contains(&id) {
            r.hidden_props += 1;
        }
        for c in &n.children {
            walk(c, needs, agents, props, r);
        }
    }
    let mut r = Rollup::default();
    for c in &node.children {
        walk(c, needs, agents, prop_targets, &mut r);
    }
    r
}

/// GEN-0 semantics: direct children are VISIBLE satellites (each wearing its
/// own markers), so only depth ≥2 below this card is hidden — the union of
/// the satellites' own roll-ups.
pub fn attention_rollup_gen0(
    node: &TreeNode,
    needs: &HashSet<PartId>,
    agents: &HashMap<PartId, AgentChip>,
    prop_targets: &HashSet<PartId>,
) -> Rollup {
    let mut r = Rollup::default();
    for c in &node.children {
        let s = attention_rollup(c, needs, agents, prop_targets);
        r.hidden_needs += s.hidden_needs;
        r.hidden_live += s.hidden_live;
        r.hidden_props += s.hidden_props;
    }
    r
}

// caller-supplied callbacks (what cx.listener would otherwise close over) —
// the map never touches the Orchestrator or the store itself.
pub type NodeDownCb = Rc<dyn Fn(PartId, Point<Pixels>, &mut Window, &mut App)>;
pub type CycleCb = Rc<dyn Fn(PartId, Lifecycle, &mut Window, &mut App)>;
/// (cursor in WINDOW px for the anchor-delta drag math, cursor in CANVAS px
/// for drop-target hit-testing, ⌥ held) — the canvas converts coords itself
/// so the integrator never needs the container's window origin.
pub type MoveCb = Rc<dyn Fn(Point<Pixels>, (f32, f32), bool, &mut Window, &mut App)>;
pub type Cb = Rc<dyn Fn(&mut Window, &mut App)>;
pub type DispatchCb = Rc<dyn Fn(PartId, bool, &mut Window, &mut App)>;
pub type OpenAgentCb = Rc<dyn Fn(String, &mut Window, &mut App)>;
pub type DrillCb = Rc<dyn Fn(PartId, &mut Window, &mut App)>;
/// dbl-click on empty canvas: the click point in CANVAS-local px.
pub type BgCreateCb = Rc<dyn Fn((f32, f32), &mut Window, &mut App)>;

#[derive(Clone)]
pub struct MapHandlers {
    /// mouse-down on a card body: focus it AND (gen-0 only) arm the drag —
    /// the integrator builds `MapDrag` from the event position + `norm_of`
    /// the card's pos, and must SKIP arming when `NodePos.gen == 1`
    /// (satellites carry no pins at this level).
    pub node_down: NodeDownCb,
    /// click the status glyph → cycle. Passes the CURRENT lifecycle; the
    /// integrator advances it (main.rs already owns `next_lifecycle`).
    pub cycle_status: CycleCb,
    /// container-level mouse-move: `drag_update` + notify when a drag is live
    /// (cheap no-op otherwise — the scrollbar-drag precedent).
    pub drag_move: MoveCb,
    /// container-level mouse-up (in OR out of bounds): persist + clear.
    pub drag_up: Cb,
    /// ▶ in a card header → spawn a session onto this node (docs/011 §C).
    /// The bool is the click's alt/option: dispatch AND jump to Agent stage.
    pub dispatch: DispatchCb,
    /// click a live-agent chip → Agent stage on that session (its cli id).
    pub open_agent: OpenAgentCb,
    /// re-root the canvas on this node (docs/011 §B drill-in): dbl-click on a
    /// card with children, or its '▸N' badge. State (map_root) lives in main.
    pub drill: DrillCb,
    /// right-click ANY node, both generations (docs/019 CANVAS): open the
    /// context menu at the cursor. WINDOW coords — the menu is a window-space
    /// overlay in main.rs, not a canvas child (it must never clip against the
    /// canvas scroll container).
    pub context_menu: NodeDownCb,
    /// dbl-click a gen-0 card's TITLE (docs/019 CANVAS): inline rename in
    /// situ — main.rs opens the canvas-side editor (`CanvasInput` overlay).
    pub rename: DrillCb,
    /// dbl-click empty canvas (docs/019 CANVAS): create a node THERE — child
    /// of the current drill-frame root, pinned at the point, named inline.
    pub bg_create: BgCreateCb,
}

/// The roll-up markers BOTH card kinds wear (docs/011 §B: collapse must never
/// hide attention): hidden needs-you → amber corner count, hidden live →
/// pulsing edge dot, hidden proposals → tiny ▲N.
fn rollup_markers(rollup: &Rollup, id: PartId) -> Vec<AnyElement> {
    let mut out = Vec::new();
    if rollup.hidden_needs > 0 {
        out.push(
            div()
                .absolute()
                .top(px(-6.))
                .left(px(-6.))
                .px(px(4.))
                .rounded(px(8.))
                .bg(rgb(AMBER_INK))
                .border_1()
                .border_color(rgb(AMBER_HAIR))
                .text_size(px(8.5))
                .font_family("Menlo")
                .text_color(rgb(AMBER))
                .child(SharedString::from(rollup.hidden_needs.to_string()))
                .into_any_element(),
        );
    }
    if rollup.hidden_live > 0 {
        let dot = div()
            .w(px(6.))
            .h(px(6.))
            .rounded(px(3.))
            .bg(rgb(GREEN))
            .with_animation(
                ("map-roll-live", id as u64),
                Animation::new(Duration::from_millis(1600))
                    .repeat()
                    .with_easing(pulsating_between(0.35, 0.95)),
                |d, delta| d.opacity(delta),
            );
        out.push(
            div()
                .absolute()
                .right(px(-3.))
                .top(px(9.))
                .child(dot)
                .into_any_element(),
        );
    }
    if rollup.hidden_props > 0 {
        out.push(
            div()
                .absolute()
                .bottom(px(-6.))
                .right(px(6.))
                .text_size(px(8.5))
                .font_family("Menlo")
                .text_color(rgb(AMBER))
                .child(SharedString::from(format!("▲{}", rollup.hidden_props)))
                .into_any_element(),
        );
    }
    out
}

/// One gen-0 spatial node card, absolutely positioned at its laid-out rect.
/// `needs_you`, `agent` and `rollup` are PASSED IN, never computed here —
/// mapping live sessions to parts is the integrator's knowledge. `drop_hint`
/// = a live ⌥-drag hovers this card (docs/019: release reparents under it).
#[allow(clippy::too_many_arguments)]
pub fn node_card(
    node: &TreeNode,
    pos: &NodePos,
    selected: bool,
    needs_you: bool,
    drop_hint: bool,
    agent: Option<AgentChip>,
    building: Option<u64>,
    warm: Option<u64>,
    rollup: &Rollup,
    h: &MapHandlers,
) -> impl IntoElement {
    let p = &node.part;
    let id = p.id;
    let idea = p.lifecycle == Lifecycle::Idea;
    let (g, gc) = map_glyph(p.lifecycle, p.stale);
    let (done, total) = countable_ratio(node);
    // whole-subtree ratio (unchanged by the two-gen model): the bar speaks
    // for everything below, visible or drilled away.
    let show_bar = total > 0 && !node.children.is_empty();
    let ratio = if total == 0 {
        0.0
    } else {
        done as f32 / total as f32
    };
    let has_children = !node.children.is_empty();
    let down = h.node_down.clone();
    let drill = h.drill.clone();
    let cycle = h.cycle_status.clone();
    let dispatch = h.dispatch.clone();
    let open_agent = h.open_agent.clone();
    let context_menu = h.context_menu.clone();
    let rename = h.rename.clone();
    let lc = p.lifecycle;
    div()
        .id(("map-node", id as u64))
        .absolute()
        .left(px(pos.x))
        .top(px(pos.y))
        .w(px(pos.w))
        .flex()
        .flex_col()
        .px(px(10.))
        .py(px(8.))
        .rounded(px(10.))
        .bg(rgb(CARD))
        .border_1()
        .border_color(rgb(card_border(p.lifecycle, selected)))
        .when(idea, |c| c.border_dashed())
        // the ⌥-drag drop ring beats every other border state — "release to
        // move under me" must be unambiguous while the mouse is down.
        .when(drop_hint, |c| c.border_color(rgb(ACCENT)).bg(rgb(CARD2)))
        .opacity(card_opacity(p.lifecycle))
        .cursor_pointer()
        .hover(|s| s.border_color(rgb(if selected { ACCENT } else { 0x36404A })))
        // right-click = the context menu (docs/019 CANVAS: every verb
        // mouse-reachable). Eaten so no canvas-level handler double-acts.
        .on_mouse_down(
            MouseButton::Right,
            move |e: &MouseDownEvent, window, app| {
                app.stop_propagation();
                context_menu(id, e.position, window, app);
            },
        )
        .on_mouse_down(MouseButton::Left, move |e: &MouseDownEvent, window, app| {
            // eat the mousedown so a future canvas-level pan/select handler
            // (bubble runs topmost-first) never double-handles it.
            app.stop_propagation();
            // dbl-click drills BEFORE any drag arms (docs/011 §B): a re-root
            // gesture must never also pin the card. Click 1's dead-zone
            // already keeps a tight double from pinning; a sloppy >4px double
            // may pin-then-drill — accepted.
            if e.click_count >= 2 && has_children {
                drill(id, window, app);
                return;
            }
            down(id, e.position, window, app);
        })
        // amber NEEDS-YOU tag, floating off the card's top edge (the mock).
        .when(needs_you, |c| {
            c.child(
                div()
                    .absolute()
                    .top(px(-9.))
                    .right(px(9.))
                    .px(px(6.))
                    .py(px(1.))
                    .rounded(px(20.))
                    .bg(rgb(AMBER_INK))
                    .border_1()
                    .border_color(rgb(AMBER_HAIR))
                    .text_size(px(9.))
                    .font_family("Menlo")
                    .text_color(rgb(AMBER))
                    .child("⚠ NEEDS YOU"),
            )
        })
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(7.))
                .child(
                    div()
                        .id(("map-cyc", id as u64))
                        .flex_none()
                        .text_size(px(12.))
                        .text_color(rgb(gc))
                        .hover(|s| s.text_color(rgb(TEXT_STRONG)))
                        .child(g)
                        // the glyph eats its mousedown so a status click never
                        // arms a drag on the card underneath.
                        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, app| {
                            app.stop_propagation()
                        })
                        .on_click(move |_: &ClickEvent, window, app| cycle(id, lc, window, app)),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_grow()
                        .text_size(px(12.5))
                        .font_weight(if idea {
                            FontWeight::MEDIUM
                        } else {
                            FontWeight::SEMIBOLD
                        })
                        .text_color(rgb(if idea { MUTED } else { TEXT_STRONG }))
                        .child(SharedString::from(p.name.clone()))
                        // dbl-click the TITLE = rename in situ (docs/019
                        // CANVAS); the card BODY keeps drill-in (docs/011).
                        // Only the SECOND press is eaten — click 1 must still
                        // bubble to the card (focus + drag arm), and eating
                        // press 2 keeps a title double from drilling.
                        .on_mouse_down(
                            MouseButton::Left,
                            move |e: &MouseDownEvent, window, app| {
                                if e.click_count >= 2 {
                                    app.stop_propagation();
                                    rename(id, window, app);
                                }
                            },
                        ),
                )
                // ▶ dispatch (docs/011 §C): always present at low emphasis,
                // full accent on hover; idea ghosts have nothing to dispatch.
                .when(!idea, |row| {
                    row.child(
                        div()
                            .id(("map-run", id as u64))
                            .flex_none()
                            .text_size(px(10.))
                            .text_color(rgba(DISPATCH_INK))
                            .hover(|s| s.text_color(rgb(ACCENT)))
                            .child("▶")
                            // eats its mousedown so a dispatch click never
                            // arms a drag (the status-glyph pattern).
                            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, app| {
                                app.stop_propagation()
                            })
                            .on_click(move |e: &ClickEvent, window, app| {
                                dispatch(id, e.modifiers().alt, window, app)
                            }),
                    )
                }),
        )
        .child(
            div()
                .mt(px(4.))
                .text_size(px(10.5))
                .text_color(rgb(MUTED))
                .child(SharedString::from(meta_text(p))),
        )
        // derived-building badge (docs/019 commitment 2): computed per frame from
        // an alive/<48h dispatch|declared link — NEVER stored, never from a touch.
        .when_some(building, |c, age| {
            c.child(
                div()
                    .mt(px(4.))
                    .text_size(px(10.))
                    .font_family("Menlo")
                    .text_color(rgb(AMBER))
                    .child(SharedString::from(building_badge(age))),
            )
        })
        // WARM BAND (docs/019 slice 4 resumption): touched within 7d but NOT
        // live — a quiet MUTED "where was I" whisper, visually distinct from the
        // amber building badge (only one of the two ever shows: warm excludes
        // building). "◌" (dashed hollow), never the solid ◔.
        .when_some(warm, |c, age| {
            let label = format!("◌ {}", crate::cockpit::warm_age_label(age));
            c.child(
                div()
                    .mt(px(4.))
                    .text_size(px(9.5))
                    .font_family("Menlo")
                    .text_color(rgb(MUTED2))
                    .child(SharedString::from(label)),
            )
        })
        .when(show_bar, |c| {
            c.child(
                div()
                    .mt(px(7.))
                    .h(px(4.))
                    .rounded(px(4.))
                    .bg(rgb(CARD2))
                    .child(
                        div()
                            .w(gpui::relative(ratio.clamp(0.0, 1.0)))
                            .h(px(4.))
                            .rounded(px(4.))
                            .bg(rgb(bar_color(p.lifecycle))),
                    ),
            )
        })
        // live-agent chip (docs/011 §D): busy pulses amber (burning tokens),
        // idle holds steady green (your turn). Click → that session.
        .when_some(agent, |c, chip| {
            let text = chip_text(&chip);
            // washed (heartbeat lost) → grey + still; hollow (observed touch) →
            // a ring, never a filled/pulsing dot (docs/019 chips + commitment 4).
            let (mut dot_color, mut pulses) = chip_dot(chip.busy);
            if chip.washed {
                dot_color = MUTED2;
                pulses = false;
            }
            let hollow = chip.hollow;
            let cli_id = chip.cli_id;
            let base = div().flex_none().w(px(6.)).h(px(6.)).rounded(px(3.));
            let dot = if hollow {
                base.border_1().border_color(rgb(dot_color))
            } else {
                base.bg(rgb(dot_color))
            };
            let dot = if pulses && !hollow {
                dot.with_animation(
                    ("map-pulse", id as u64),
                    Animation::new(Duration::from_millis(1600))
                        .repeat()
                        .with_easing(pulsating_between(0.35, 0.95)),
                    |dot, delta| dot.opacity(delta),
                )
                .into_any_element()
            } else {
                dot.into_any_element()
            };
            c.child(
                div()
                    .id(("map-chip", id as u64))
                    .mt(px(7.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(5.))
                    .px(px(6.))
                    .py(px(1.))
                    .rounded(px(20.))
                    .bg(rgb(CARD2))
                    .border_1()
                    .border_color(rgb(HAIR_SOFT))
                    // a hollow observed chip wears a DOTTED border (docs/019).
                    .when(hollow, |s| s.border_dashed().border_color(rgb(HAIR)))
                    .hover(|s| s.border_color(rgb(HAIR)))
                    .child(dot)
                    .child(
                        div()
                            .text_size(px(10.))
                            .font_family("Menlo")
                            .text_color(rgb(if chip.washed { MUTED2 } else { MUTED }))
                            .child(SharedString::from(text)),
                    )
                    // the chip eats its mousedown so a jump-to-session click
                    // never arms a drag on the card underneath.
                    .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, app| {
                        app.stop_propagation()
                    })
                    .on_click(move |_: &ClickEvent, window, app| {
                        open_agent(cli_id.clone(), window, app)
                    }),
            )
        })
        .children(rollup_markers(rollup, id))
}

/// A GEN-1 satellite: the compact 128px card packed under its gen-0 parent
/// (docs/011 §B) — status glyph + name + '▸N' when its own subtree hides
/// behind it. No pins at this level (its map_x/map_y is a pin on ITS parent's
/// canvas, one drill down), so a single click is focus-only; dbl-click or the
/// badge drills in. `live` = the node's OWN chip presence, rendered as a bare
/// dot (the full chip belongs to gen-0 real estate).
#[allow(clippy::too_many_arguments)]
pub fn satellite_card(
    node: &TreeNode,
    pos: &NodePos,
    selected: bool,
    needs_you: bool,
    drop_hint: bool,
    live: Option<&AgentChip>,
    building: Option<u64>,
    warm: bool,
    rollup: &Rollup,
    h: &MapHandlers,
) -> impl IntoElement {
    let p = &node.part;
    let id = p.id;
    let (g, gc) = map_glyph(p.lifecycle, p.stale);
    let badge = sat_badge(node);
    let has_children = badge.is_some();
    let down = h.node_down.clone();
    let drill = h.drill.clone();
    let badge_drill = h.drill.clone();
    let context_menu = h.context_menu.clone();
    div()
        .id(("map-sat", id as u64))
        .absolute()
        .left(px(pos.x))
        .top(px(pos.y))
        .w(px(pos.w))
        .h(px(pos.h))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.))
        .px(px(7.))
        .rounded(px(7.))
        .bg(rgb(CARD))
        .border_1()
        .border_color(rgb(if selected {
            ACCENT
        } else if needs_you {
            AMBER_HAIR
        } else {
            HAIR_SOFT
        }))
        .when(p.lifecycle == Lifecycle::Idea, |c| c.border_dashed())
        // ⌥-drag drop ring (docs/019): a satellite is a legal drop point —
        // the dragged card becomes ITS child, one level deeper.
        .when(drop_hint, |c| c.border_color(rgb(ACCENT)).bg(rgb(CARD2)))
        .opacity(card_opacity(p.lifecycle))
        .cursor_pointer()
        .hover(|s| s.border_color(rgb(if selected { ACCENT } else { 0x36404A })))
        // right-click = the same context menu as gen-0 (docs/019 CANVAS:
        // "every node", both generations).
        .on_mouse_down(
            MouseButton::Right,
            move |e: &MouseDownEvent, window, app| {
                app.stop_propagation();
                context_menu(id, e.position, window, app);
            },
        )
        .on_mouse_down(MouseButton::Left, move |e: &MouseDownEvent, window, app| {
            app.stop_propagation();
            if e.click_count >= 2 && has_children {
                drill(id, window, app);
                return;
            }
            // focus only — the integrator must not arm MapDrag for gen-1
            // (NodePos.gen tells it which kind it grabbed).
            down(id, e.position, window, app);
        })
        .child(
            div()
                .flex_none()
                .text_size(px(10.))
                .text_color(rgb(gc))
                .child(g),
        )
        // derived-building glyph (docs/019 commitment 2) — compact ◔ on a
        // satellite (the full "built Nh" badge is gen-0 real estate).
        .when_some(building, |c, _age| {
            c.child(
                div()
                    .flex_none()
                    .text_size(px(9.))
                    .text_color(rgb(AMBER))
                    .child("◔"),
            )
        })
        // WARM BAND (docs/019 slice 4) — compact dashed "◌" resumption trail
        // (muted, distinct from the solid amber ◔; only one ever shows).
        .when(warm && building.is_none(), |c| {
            c.child(
                div()
                    .flex_none()
                    .text_size(px(9.))
                    .text_color(rgb(MUTED2))
                    .child("◌"),
            )
        })
        .child(
            div()
                .min_w_0()
                .flex_grow()
                .text_size(px(10.5))
                .text_color(rgb(MUTED))
                .child(SharedString::from(crate::termview::trim(&p.name, 14))),
        )
        // the node's OWN live presence (it is visible — not part of rollup).
        .when_some(live, |c, chip| {
            let (mut dot_color, mut pulses) = chip_dot(chip.busy);
            if chip.washed {
                dot_color = MUTED2;
                pulses = false;
            }
            let hollow = chip.hollow;
            let base = div().flex_none().w(px(5.)).h(px(5.)).rounded(px(2.5));
            let dot = if hollow {
                base.border_1().border_color(rgb(dot_color))
            } else {
                base.bg(rgb(dot_color))
            };
            c.child(if pulses && !hollow {
                dot.with_animation(
                    ("map-sat-live", id as u64),
                    Animation::new(Duration::from_millis(1600))
                        .repeat()
                        .with_easing(pulsating_between(0.35, 0.95)),
                    |d, delta| d.opacity(delta),
                )
                .into_any_element()
            } else {
                dot.into_any_element()
            })
        })
        .when_some(badge, |c, b| {
            c.child(
                div()
                    .id(("map-sat-badge", id as u64))
                    .flex_none()
                    .px(px(4.))
                    .rounded(px(6.))
                    .bg(rgb(CARD2))
                    .text_size(px(9.))
                    .font_family("Menlo")
                    .text_color(rgb(MUTED2))
                    .hover(|s| s.text_color(rgb(ACCENT)))
                    .child(SharedString::from(b))
                    // the badge eats its mousedown (the glyph pattern) so a
                    // drill click never doubles as focus on the card under it.
                    .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, app| {
                        app.stop_propagation()
                    })
                    .on_click(move |_: &ClickEvent, window, app| badge_drill(id, window, app)),
            )
        })
        .children(rollup_markers(rollup, id))
}

// ---- (3) connectors: pure edge geometry + one canvas() child ----

/// Facing-edge attach points: mostly-vertical pairs connect bottom→top,
/// mostly-horizontal connect side→side — a connector never spears a card.
pub fn edge_endpoints(parent: &NodePos, child: &NodePos) -> ((f32, f32), (f32, f32)) {
    let pc = (parent.x + parent.w / 2.0, parent.y + parent.h / 2.0);
    let cc = (child.x + child.w / 2.0, child.y + child.h / 2.0);
    let (dx, dy) = (cc.0 - pc.0, cc.1 - pc.1);
    if dy.abs() >= dx.abs() {
        if dy >= 0.0 {
            ((pc.0, parent.y + parent.h), (cc.0, child.y))
        } else {
            ((pc.0, parent.y), (cc.0, child.y + child.h))
        }
    } else if dx >= 0.0 {
        ((parent.x + parent.w, pc.1), (child.x, cc.1))
    } else {
        ((parent.x, pc.1), (child.x + child.w, cc.1))
    }
}

/// Cubic control points: pulled halfway along the DOMINANT axis only — the
/// classic S-curve that leaves each card perpendicular to its edge.
pub fn edge_controls(a: (f32, f32), b: (f32, f32)) -> ((f32, f32), (f32, f32)) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    if dy.abs() >= dx.abs() {
        ((a.0, a.1 + dy * 0.5), (b.0, b.1 - dy * 0.5))
    } else {
        ((a.0 + dx * 0.5, a.1), (b.0 - dx * 0.5, b.1))
    }
}

/// Edge (color, dashed, stroke width) by the CHILD's state. Parent→satellite
/// connectors are thinner and dimmer (docs/011 §B) — satellites are context,
/// not the working frontier.
pub fn edge_style(child_lc: Lifecycle, satellite: bool) -> (u32, bool, f32) {
    if satellite {
        return (HAIR_SOFT, child_lc == Lifecycle::Idea, 1.2);
    }
    match child_lc {
        Lifecycle::Building => (AMBER_HAIR, false, 2.0),
        Lifecycle::Idea => (HAIR, true, 1.4),
        _ => (HAIR, false, 1.6),
    }
}

/// Parent→child pairs among the laid-out nodes (an edge is drawn only when
/// BOTH ends are visible — depth ≥2 has no NodePos, so hidden nodes get no
/// edge by construction), tagged with the child's lifecycle for styling.
pub fn edge_list(tree: &[TreeNode], positions: &[NodePos]) -> Vec<(NodePos, NodePos, Lifecycle)> {
    let by_id: HashMap<PartId, NodePos> = positions.iter().map(|p| (p.id, *p)).collect();
    fn walk(
        n: &TreeNode,
        by_id: &HashMap<PartId, NodePos>,
        out: &mut Vec<(NodePos, NodePos, Lifecycle)>,
    ) {
        for c in &n.children {
            if let (Some(pp), Some(cp)) = (by_id.get(&n.part.id), by_id.get(&c.part.id)) {
                out.push((*pp, *cp, c.part.lifecycle));
            }
            walk(c, by_id, out);
        }
    }
    let mut out = Vec::new();
    for n in tree {
        walk(n, &by_id, &mut out);
    }
    out
}

/// The connector layer: ONE canvas() painting every edge as a stroked cubic
/// bezier. Painted FIRST (before the cards), so lines run under the nodes.
/// paint_path wants LOGICAL px in WINDOW coords — offset by bounds.origin.
/// `origin_sink` publishes that origin for the frame: mouse events arrive in
/// WINDOW coords, and the ⌥-drag hit-test + dbl-click-create need CANVAS px
/// (the cell_at pattern from main.rs's IME canvas — paint always precedes
/// event dispatch, so the cell is fresh by the time a handler reads it).
fn connectors(
    edges: Vec<(NodePos, NodePos, Lifecycle)>,
    origin_sink: Rc<Cell<(f32, f32)>>,
) -> impl IntoElement {
    canvas(
        |_bounds, _window, _cx| {},
        move |bounds, _prepaint: (), window, _cx| {
            let o = bounds.origin;
            origin_sink.set((f32::from(o.x), f32::from(o.y)));
            for (pp, cp, lc) in &edges {
                let (a, b) = edge_endpoints(pp, cp);
                let (c1, c2) = edge_controls(a, b);
                let (color, dashed, width) = edge_style(*lc, cp.gen == 1);
                let mut pb = PathBuilder::stroke(px(width));
                if dashed {
                    pb = pb.dash_array(&[px(4.), px(4.)]);
                }
                pb.move_to(o + point(px(a.0), px(a.1)));
                pb.cubic_bezier_to(
                    o + point(px(b.0), px(b.1)),   // to (first — gpui's order)
                    o + point(px(c1.0), px(c1.1)), // ctrl1
                    o + point(px(c2.0), px(c2.1)), // ctrl2
                );
                if let Ok(path) = pb.build() {
                    window.paint_path(path, rgb(color));
                }
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// A canvas-side inline input (rename-in-situ / create-at-point, docs/019
/// CANVAS): positioned by the CALLER in canvas-local px. Pure display — the
/// buffer, commit and keystream live in main.rs (the outlinepane
/// inline_input pattern; chars ride the root key router).
pub struct CanvasInput {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub buf: String,
    pub hint: &'static str,
}

/// The floating editor box for `CanvasInput` — painted LAST, over every card.
fn canvas_input_box(inp: &CanvasInput) -> impl IntoElement {
    div()
        .absolute()
        .left(px(inp.x))
        .top(px(inp.y))
        .w(px(inp.w.max(150.0)))
        // the editor eats its mousedowns (the card pattern): a click on it
        // must not fall through to the background — a double would blur THIS
        // edit and start a create-at-point right under it.
        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, app| {
            app.stop_propagation()
        })
        .flex()
        .flex_col()
        .gap(px(3.))
        .px(px(9.))
        .py(px(7.))
        .rounded(px(10.))
        .bg(rgb(CARD2))
        .border_1()
        .border_color(rgb(ACCENT))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .min_w_0()
                .child(if inp.buf.is_empty() {
                    div()
                        .text_size(px(12.5))
                        .text_color(rgb(MUTED2))
                        .child("name…")
                        .into_any_element()
                } else {
                    div()
                        .text_size(px(12.5))
                        .text_color(rgb(TEXT_STRONG))
                        .child(SharedString::from(inp.buf.clone()))
                        .into_any_element()
                })
                .child(
                    div()
                        .flex_none()
                        .w(px(2.))
                        .h(px(13.))
                        .ml(px(1.))
                        .bg(rgb(ACCENT)),
                ),
        )
        .child(
            div()
                .text_size(px(9.5))
                .text_color(rgb(MUTED2))
                .child(inp.hint),
        )
}

/// The whole map: a fixed-size relative container carrying the connector
/// canvas, then gen-0 cards each followed by their satellites (paint order =
/// z-order — satellites paint over their parent's band). Depth ≥2 renders
/// nothing; its attention arrives via the roll-ups. The container owns the
/// drag's move/up so a fast drag can't escape a 168px card; `on_mouse_up_out`
/// catches a release outside the canvas (no sticky drags). `drop_target` =
/// the live ⌥-drag's hit (accent ring); `inline_input` = the one canvas-side
/// editor (rename-in-situ / create-at-point).
#[allow(clippy::too_many_arguments)]
pub fn map_canvas(
    tree: &[TreeNode],
    positions: &[NodePos],
    selected: Option<PartId>,
    needs_you: &HashSet<PartId>,
    agents: &HashMap<PartId, AgentChip>,
    building: &HashMap<PartId, u64>,
    warm: &HashMap<PartId, u64>,
    prop_targets: &HashSet<PartId>,
    drop_target: Option<PartId>,
    inline_input: Option<&CanvasInput>,
    canvas_w: f32,
    canvas_h: f32,
    h: &MapHandlers,
) -> impl IntoElement {
    let by_id: HashMap<PartId, NodePos> = positions.iter().map(|p| (p.id, *p)).collect();
    // the canvas' window origin, written at paint, read by mouse handlers —
    // window→canvas coord conversion without threading container bounds.
    let origin: Rc<Cell<(f32, f32)>> = Rc::new(Cell::new((0.0, 0.0)));
    let drag_move = h.drag_move.clone();
    let drag_up = h.drag_up.clone();
    let drag_up2 = h.drag_up.clone();
    let bg_create = h.bg_create.clone();
    let move_origin = origin.clone();
    let bg_origin = origin.clone();
    let mut root = div()
        .relative()
        .w(px(canvas_w))
        .h(px(canvas_h))
        .overflow_hidden()
        .on_mouse_move(move |e: &MouseMoveEvent, window, app| {
            let o = move_origin.get();
            let canvas_pt = (f32::from(e.position.x) - o.0, f32::from(e.position.y) - o.1);
            drag_move(e.position, canvas_pt, e.modifiers.alt, window, app)
        })
        .on_mouse_up(MouseButton::Left, move |_: &MouseUpEvent, window, app| {
            drag_up(window, app)
        })
        .on_mouse_up_out(MouseButton::Left, move |_: &MouseUpEvent, window, app| {
            drag_up2(window, app)
        })
        // dbl-click EMPTY canvas = create a node there (docs/019 CANVAS).
        // Cards eat their own mousedowns, so only the background lands here.
        .on_mouse_down(MouseButton::Left, move |e: &MouseDownEvent, window, app| {
            if e.click_count >= 2 {
                let o = bg_origin.get();
                bg_create(
                    (f32::from(e.position.x) - o.0, f32::from(e.position.y) - o.1),
                    window,
                    app,
                );
            }
        })
        .child(connectors(edge_list(tree, positions), origin));
    for n in tree {
        let id = n.part.id;
        if let Some(pos) = by_id.get(&id) {
            let rollup = attention_rollup_gen0(n, needs_you, agents, prop_targets);
            root = root.child(
                node_card(
                    n,
                    pos,
                    selected == Some(id),
                    needs_you.contains(&id),
                    drop_target == Some(id),
                    agents.get(&id).cloned(),
                    building.get(&id).copied(),
                    warm.get(&id).copied(),
                    &rollup,
                    h,
                )
                .into_any_element(),
            );
        }
        for c in &n.children {
            let cid = c.part.id;
            if let Some(pos) = by_id.get(&cid) {
                let rollup = attention_rollup(c, needs_you, agents, prop_targets);
                root = root.child(
                    satellite_card(
                        c,
                        pos,
                        selected == Some(cid),
                        needs_you.contains(&cid),
                        drop_target == Some(cid),
                        agents.get(&cid),
                        building.get(&cid).copied(),
                        warm.contains_key(&cid),
                        &rollup,
                        h,
                    )
                    .into_any_element(),
                );
            }
        }
    }
    if let Some(inp) = inline_input {
        root = root.child(canvas_input_box(inp).into_any_element());
    }
    root
}

#[cfg(test)]
mod tests {
    // NOT `use super::*`: the parent's `use gpui::*` glob would leak in and
    // shadow the built-in `#[test]` with gpui's proc-macro of the same name
    // (recursion-limit blowup at expansion time — same fix as outlinepane).
    use std::collections::{HashMap, HashSet};

    use super::{
        attention_rollup, attention_rollup_gen0, bar_color, building_badge, card_border,
        card_opacity, chip_dot, chip_text, content_height, drag_to, drag_update, drop_target_at,
        edge_controls, edge_endpoints, edge_list, edge_style, layout_nodes, map_glyph, meta_text,
        node_size, norm_of, norm_to_px, px_to_norm, sat_badge, subtree_ids, AgentChip, MapDrag,
        NodePos, Rollup, ACCENT, AMBER, AMBER_HAIR, DEFAULT_CARD, GAP, GREEN, HAIR, HAIR_SOFT,
        MUTED, MUTED2, SAT_W,
    };
    use gpui::{point, px};
    use orchestrator_store::{build_tree, Lifecycle, Part, PartId, TreeNode};

    fn part(id: PartId, parent: Option<PartId>, lc: Lifecycle) -> Part {
        Part {
            id,
            parent_id: parent,
            name: format!("p{id}"),
            detail: "meta".into(),
            lifecycle: lc,
            sort_order: id as f64,
            ..Part::default()
        }
    }

    fn overlap(a: &NodePos, b: &NodePos) -> bool {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    }

    /// 5 roots × 4 children × 2 grandchildren — grandchildren must be hidden.
    fn deep_tree() -> Vec<TreeNode> {
        let mut parts: Vec<Part> = Vec::new();
        for a in 0..5i64 {
            let root = (a + 1) * 1000;
            parts.push(part(root, None, Lifecycle::Building));
            for c in 1..=4i64 {
                let child = root + c;
                parts.push(part(child, Some(root), Lifecycle::Todo));
                for g in 1..=2i64 {
                    parts.push(part(child * 10 + g, Some(child), Lifecycle::Todo));
                }
            }
        }
        build_tree(&parts)
    }

    #[test]
    fn two_gen_layout_no_overlaps_incl_satellite_footprints() {
        let tree = deep_tree();
        for canvas_w in [560.0f32, 980.0] {
            let pos = layout_nodes(&tree, |_| None, canvas_w, 620.0);
            // exactly two generations: 5 gen-0 cards + 20 satellites; the 40
            // grandchildren emit NO NodePos (hidden behind '▸N').
            assert_eq!(pos.len(), 25, "at {canvas_w}");
            assert_eq!(pos.iter().filter(|p| p.gen == 0).count(), 5);
            assert_eq!(pos.iter().filter(|p| p.gen == 1).count(), 20);
            assert!(
                pos.iter().all(|p| p.id < 10_000),
                "grandchild laid out at {canvas_w}"
            );
            for s in pos.iter().filter(|p| p.gen == 1) {
                assert_eq!((s.w, s.h), (SAT_W, super::SAT_H));
            }
            // no overlaps ANYWHERE — column/band packing accounts for each
            // gen-0 card's satellite footprint, not just the card itself.
            for i in 0..pos.len() {
                for j in (i + 1)..pos.len() {
                    assert!(
                        !overlap(&pos[i], &pos[j]),
                        "at {canvas_w}: {:?} vs {:?}",
                        pos[i],
                        pos[j]
                    );
                }
            }
            // width respected (content HEIGHT may exceed the canvas — the
            // caller grows the container via content_height()).
            for p in &pos {
                assert!(p.x >= 0.0 && p.x + p.w <= canvas_w, "at {canvas_w}: {p:?}");
            }
        }
    }

    #[test]
    fn satellites_follow_a_dragged_parent() {
        // FLIPPED on purpose (docs/011 §B): the old packing contract kept
        // children in their packed slots so they never chased a dragged
        // parent. In the two-generation model satellites derive from the
        // parent's FINAL rendered position each frame — a persisted pin or a
        // live drag preview carries them. Never resurrect the old assertion.
        let parts = vec![
            part(1, None, Lifecycle::Building),
            part(2, Some(1), Lifecycle::Todo),
            part(3, Some(1), Lifecycle::Todo),
        ];
        let tree = build_tree(&parts);
        let base = layout_nodes(&tree, |_| None, 980.0, 620.0);
        let moved = layout_nodes(&tree, |id| (id == 1).then_some((0.7, 0.6)), 980.0, 620.0);
        let find = |v: &[NodePos], id: PartId| v.iter().find(|p| p.id == id).copied().unwrap();
        let (b1, m1) = (find(&base, 1), find(&moved, 1));
        let (dx, dy) = (m1.x - b1.x, m1.y - b1.y);
        assert!(
            dx > 0.0 && dy > 0.0,
            "parent should actually move ({dx},{dy})"
        );
        for sat in [2i64, 3] {
            let (b, m) = (find(&base, sat), find(&moved, sat));
            assert_eq!((m.x - b.x, m.y - b.y), (dx, dy), "satellite {sat} strayed");
            // and they stay a satellite block BELOW the parent card.
            assert!(m.y >= m1.y + m1.h, "{m:?} not below {m1:?}");
        }
    }

    #[test]
    fn persisted_gen0_override_wins_and_satellite_pins_are_ignored() {
        let parts = vec![
            part(1, None, Lifecycle::Building),
            part(2, Some(1), Lifecycle::Todo),
        ];
        let tree = build_tree(&parts);
        let pos = layout_nodes(&tree, |id| (id == 1).then_some((1.0, 1.0)), 1000.0, 680.0);
        let p1 = pos.iter().find(|p| p.id == 1).unwrap();
        // 1.0 normalized = flush bottom-right for the whole FOOTPRINT: the
        // card AND its satellite block stay fully visible (review slices 2+3:
        // card-extent pins pushed satellites past the canvas clip and the
        // two-pass sizing never converged).
        assert_eq!((p1.x + p1.fp_w, p1.y + p1.fp_h), (1000.0, 680.0));
        let sat = pos.iter().find(|p| p.id == 2).unwrap();
        assert!(
            sat.y + sat.h <= 680.0,
            "satellite must stay inside the canvas: {sat:?}"
        );
        // frame-per-level (docs/011 §B): a satellite's map_x/map_y is a pin
        // on ITS parent's canvas (one drill down) — ignored on THIS canvas.
        let default = layout_nodes(&tree, |_| None, 1000.0, 680.0);
        let pinned_sat = layout_nodes(&tree, |id| (id == 2).then_some((0.9, 0.9)), 1000.0, 680.0);
        assert_eq!(
            default.iter().find(|p| p.id == 2).unwrap(),
            pinned_sat.iter().find(|p| p.id == 2).unwrap(),
        );
    }

    #[test]
    fn sat_badge_counts_direct_children() {
        let parts = vec![
            part(1, None, Lifecycle::Building),
            part(2, Some(1), Lifecycle::Todo),
            part(3, Some(1), Lifecycle::Todo),
            part(4, Some(1), Lifecycle::Idea),
            part(5, Some(2), Lifecycle::Todo),
        ];
        let tree = build_tree(&parts);
        assert_eq!(sat_badge(&tree[0]), Some("▸3".into()));
        assert_eq!(sat_badge(&tree[0].children[0]), Some("▸1".into()));
        // leaves wear no badge — nothing hides behind them.
        assert_eq!(sat_badge(&tree[0].children[1]), None);
    }

    #[test]
    fn attention_rollup_bubbles_from_a_4_deep_tree() {
        // 1 (gen-0) → 2 (satellite) → 3 (depth 2, hidden) → 4 (depth 3,
        // hidden); 5 is a second hidden child of 2.
        let parts = vec![
            part(1, None, Lifecycle::Building),
            part(2, Some(1), Lifecycle::Building),
            part(3, Some(2), Lifecycle::Building),
            part(4, Some(3), Lifecycle::Todo),
            part(5, Some(2), Lifecycle::Todo),
        ];
        let tree = build_tree(&parts);
        let n1 = &tree[0];
        let n2 = &n1.children[0];
        let n3 = &n2.children[0];
        let needs: HashSet<PartId> = [2, 4].into_iter().collect(); // 2 is VISIBLE (satellite)
        let chip = AgentChip {
            cli_id: "s".into(),
            label: "@claude".into(),
            busy: true,
            extra: 0,
            hollow: false,
            washed: false,
        };
        let agents: HashMap<PartId, AgentChip> = [(3i64, chip)].into_iter().collect();
        let props: HashSet<PartId> = [5].into_iter().collect();
        // gen-0 card: satellite 2 is visible (its own needs marker shows on
        // it), so only depth ≥2 rolls up — needs@4, live@3, prop@5.
        assert_eq!(
            attention_rollup_gen0(n1, &needs, &agents, &props),
            Rollup {
                hidden_needs: 1,
                hidden_live: 1,
                hidden_props: 1
            },
        );
        // the satellite's own rollup counts its WHOLE subtree (3, 4, 5) —
        // but never itself (it is the visible card).
        assert_eq!(
            attention_rollup(n2, &needs, &agents, &props),
            Rollup {
                hidden_needs: 1,
                hidden_live: 1,
                hidden_props: 1
            },
        );
        // deeper node: only its own descendants.
        assert_eq!(
            attention_rollup(n3, &needs, &agents, &props),
            Rollup {
                hidden_needs: 1,
                hidden_live: 0,
                hidden_props: 0
            },
        );
        // nothing hidden → nothing claimed.
        assert_eq!(
            attention_rollup(&n3.children[0], &needs, &agents, &props),
            Rollup::default()
        );
    }

    #[test]
    fn content_height_includes_satellites() {
        let mut parts = vec![part(1, None, Lifecycle::Building)];
        for c in 2..=6i64 {
            parts.push(part(c, Some(1), Lifecycle::Todo));
        }
        let tree = build_tree(&parts);
        let pos = layout_nodes(&tree, |_| None, 980.0, 620.0);
        let parent = pos.iter().find(|p| p.id == 1).unwrap();
        assert!(
            content_height(&pos) > parent.y + parent.h + GAP,
            "satellite block ignored"
        );
        // the content bottom IS a satellite (3 rows of them hang below).
        let bottom = pos
            .iter()
            .max_by(|a, b| (a.y + a.h).total_cmp(&(b.y + b.h)))
            .unwrap();
        assert_eq!(bottom.gen, 1);
        assert_eq!(content_height(&pos), bottom.y + bottom.h + GAP);
    }

    #[test]
    fn layout_order_is_stable_dfs_preorder_over_visible_gens() {
        let parts = vec![
            part(1, None, Lifecycle::Building),
            part(2, Some(1), Lifecycle::Todo),
            part(3, Some(2), Lifecycle::Todo), // depth 2 — hidden
            part(4, None, Lifecycle::Idea),
        ];
        let tree = build_tree(&parts);
        let ids: Vec<PartId> = layout_nodes(&tree, |_| None, 900.0, 600.0)
            .iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, vec![1, 2, 4]);
        // same input, same slots — no randomness anywhere.
        assert_eq!(
            layout_nodes(&tree, |_| None, 900.0, 600.0),
            layout_nodes(&tree, |_| None, 900.0, 600.0)
        );
    }

    #[test]
    fn satellites_pack_below_their_parent_within_its_footprint() {
        let parts = vec![
            part(1, None, Lifecycle::Building),
            part(2, Some(1), Lifecycle::Todo),
            part(3, Some(1), Lifecycle::Todo),
        ];
        let tree = build_tree(&parts);
        let pos = layout_nodes(&tree, |_| None, 1000.0, 680.0);
        let parent = pos.iter().find(|p| p.id == 1).unwrap();
        let (fw, _) = super::footprint(&tree[0]);
        for c in pos.iter().filter(|p| p.gen == 1) {
            assert!(
                c.y >= parent.y + parent.h,
                "satellite {c:?} not below {parent:?}"
            );
            assert!(
                c.x >= parent.x && c.x + c.w <= parent.x + fw,
                "satellite {c:?} escapes the column footprint"
            );
        }
    }

    #[test]
    fn norm_px_roundtrip_and_edges() {
        assert_eq!(norm_to_px(0.0, 1000.0, 168.0), 0.0);
        assert_eq!(norm_to_px(1.0, 1000.0, 168.0), 832.0); // flush right, fully visible
        assert_eq!(px_to_norm(416.0, 1000.0, 168.0), 0.5);
        let n = px_to_norm(norm_to_px(0.37, 1000.0, 168.0), 1000.0, 168.0);
        assert!((n - 0.37).abs() < 0.002, "{n}");
        // out-of-range inputs clamp instead of escaping the canvas.
        assert_eq!(norm_to_px(7.0, 1000.0, 168.0), 832.0);
        assert_eq!(px_to_norm(-50.0, 1000.0, 168.0), 0.0);
        // degenerate span (card wider than canvas): pinned, never NaN.
        assert_eq!(norm_to_px(0.8, 100.0, 168.0), 0.0);
        assert_eq!(px_to_norm(10.0, 100.0, 168.0), 1.0);
    }

    #[test]
    fn norm_of_inverts_layout() {
        let pos = NodePos {
            id: 1,
            x: 416.0,
            y: 258.0,
            w: 168.0,
            h: 64.0,
            fp_w: 168.0,
            fp_h: 64.0,
            gen: 0,
        };
        let (nx, ny) = norm_of(&pos, 1000.0, 680.0);
        assert!((nx - 0.5).abs() < 0.002 && (ny - 258.0 / 616.0).abs() < 0.002);
    }

    #[test]
    fn drag_math_absolute_and_clamped() {
        // zero delta = original position, exactly.
        assert_eq!(
            drag_to((0.4, 0.6), (0.0, 0.0), 1000.0, 680.0, 168.0, 64.0),
            (0.4, 0.6)
        );
        // +83.2px over an 832px usable range = +0.1 normalized.
        let (nx, ny) = drag_to((0.4, 0.6), (83.2, 0.0), 1000.0, 680.0, 168.0, 64.0);
        assert!((nx - 0.5).abs() < 1e-3 && (ny - 0.6).abs() < 1e-9);
        // clamped at both rails — a card can't leave the canvas.
        assert_eq!(
            drag_to(
                (0.9, 0.1),
                (10_000.0, -10_000.0),
                1000.0,
                680.0,
                168.0,
                64.0
            ),
            (1.0, 0.0)
        );
        // degenerate canvas: no NaN/inf, just a hard pin.
        let (dx, dy) = drag_to((0.5, 0.5), (50.0, 50.0), 100.0, 40.0, 168.0, 64.0);
        assert!(dx.is_finite() && dy.is_finite());
    }

    #[test]
    fn drag_update_uses_anchor_not_increments() {
        let drag = MapDrag {
            id: 1,
            grab: point(px(100.0), px(100.0)),
            orig: (0.2, 0.2),
            node_w: 168.0,
            node_h: 64.0,
            cur: (0.2, 0.2),
            alt: false,
            target: None,
        };
        // same cursor twice → same result (absolute math; a missed frame or a
        // duplicate event can't compound).
        let a = drag_update(&drag, point(px(183.2), px(161.6)), 1000.0, 680.0);
        let b = drag_update(&drag, point(px(183.2), px(161.6)), 1000.0, 680.0);
        assert_eq!(a, b);
        assert!(
            (a.0 - 0.3).abs() < 1e-3 && (a.1 - 0.3).abs() < 1e-3,
            "{a:?}"
        );
    }

    #[test]
    fn subtree_ids_collects_the_whole_branch() {
        // 1 > 2 > 3, 1 > 4, 5 root — pairs deliberately child-before-parent
        // so the fixpoint (not a single pass) is what's under test.
        let pairs: Vec<(PartId, Option<PartId>)> = vec![
            (3, Some(2)),
            (2, Some(1)),
            (4, Some(1)),
            (1, None),
            (5, None),
        ];
        let deny = subtree_ids(&pairs, 1);
        assert_eq!(deny, [1i64, 2, 3, 4].into_iter().collect());
        assert_eq!(subtree_ids(&pairs, 2), [2i64, 3].into_iter().collect());
        // a leaf denies only itself; unknown ids likewise (calm, no panic).
        assert_eq!(subtree_ids(&pairs, 5), [5i64].into_iter().collect());
        assert_eq!(subtree_ids(&pairs, 99), [99i64].into_iter().collect());
    }

    #[test]
    fn drop_target_topmost_hit_wins_and_deny_masks() {
        let at = |id: PartId, x: f32, y: f32| NodePos {
            id,
            x,
            y,
            w: 100.0,
            h: 40.0,
            fp_w: 100.0,
            fp_h: 40.0,
            gen: 0,
        };
        // paint order: 1, then 2 overlapping it — 2 paints on top.
        let pos = vec![at(1, 0.0, 0.0), at(2, 50.0, 20.0)];
        let none: HashSet<PartId> = HashSet::new();
        assert_eq!(
            drop_target_at(&pos, &none, (60.0, 30.0)),
            Some(2),
            "topmost (last-painted) wins"
        );
        assert_eq!(drop_target_at(&pos, &none, (10.0, 10.0)), Some(1));
        // deny (the dragged subtree) falls through to what's UNDER it.
        let deny: HashSet<PartId> = [2i64].into_iter().collect();
        assert_eq!(drop_target_at(&pos, &deny, (60.0, 30.0)), Some(1));
        // empty canvas → None (the drop reparents to the drill-frame root).
        assert_eq!(drop_target_at(&pos, &none, (500.0, 500.0)), None);
    }

    #[test]
    fn default_card_matches_a_fresh_leaf_task() {
        // dbl-click-create pins with DEFAULT_CARD before the node exists —
        // it must stay in lockstep with node_size or pins land skewed.
        let mut p = part(1, None, Lifecycle::Todo);
        p.detail = String::new();
        let leaf = TreeNode {
            part: p,
            children: vec![],
        };
        assert_eq!(node_size(&leaf), DEFAULT_CARD);
    }

    #[test]
    fn glyphs_and_tints() {
        assert_eq!(map_glyph(Lifecycle::Done, false), ("✓", GREEN));
        assert_eq!(map_glyph(Lifecycle::Building, false), ("◔", AMBER));
        assert_eq!(map_glyph(Lifecycle::Todo, false), ("☐", MUTED));
        assert_eq!(map_glyph(Lifecycle::Idea, false), ("◌", MUTED2));
        // stale wins over any asserted lifecycle (honest uncertainty).
        assert_eq!(map_glyph(Lifecycle::Done, true), ("◌", AMBER));
    }

    #[test]
    fn card_styling_selected_ring_wins() {
        assert_eq!(card_border(Lifecycle::Building, true), ACCENT);
        assert_eq!(card_border(Lifecycle::Building, false), AMBER_HAIR);
        assert_eq!(card_border(Lifecycle::Done, false), HAIR);
        assert_eq!(card_opacity(Lifecycle::Building), 1.0);
        assert!(card_opacity(Lifecycle::Done) < card_opacity(Lifecycle::Todo));
        assert_eq!(bar_color(Lifecycle::Building), AMBER);
        assert_eq!(bar_color(Lifecycle::Done), GREEN);
        assert_eq!(bar_color(Lifecycle::Todo), MUTED2);
    }

    #[test]
    fn chip_text_appends_extra_live_count() {
        let chip = |extra| AgentChip {
            cli_id: "sess-1".into(),
            label: "@claude · 6m".into(),
            busy: true,
            extra,
            hollow: false,
            washed: false,
        };
        assert_eq!(chip_text(&chip(0)), "@claude · 6m");
        assert_eq!(chip_text(&chip(1)), "@claude · 6m ·1 live");
        assert_eq!(chip_text(&chip(3)), "@claude · 6m ·3 live");
    }

    #[test]
    fn building_badge_reads_by_age() {
        assert_eq!(building_badge(0), "◔ building");
        assert_eq!(building_badge(89), "◔ building");
        assert_eq!(building_badge(600), "◔ built 10m ago");
        assert_eq!(building_badge(3 * 3600), "◔ built 3h ago");
        assert_eq!(building_badge(3 * 86400), "◔ built 3d ago");
    }

    #[test]
    fn chip_dot_busy_pulses_amber_idle_holds_green() {
        // pulse = burning tokens; steady green = your turn (docs/011 §D).
        assert_eq!(chip_dot(true), (AMBER, true));
        assert_eq!(chip_dot(false), (GREEN, false));
    }

    #[test]
    fn meta_falls_back_to_lifecycle_word() {
        let mut p = part(1, None, Lifecycle::Idea);
        p.detail = String::new();
        assert_eq!(meta_text(&p), "idea");
        p.detail = "sample detail text".into();
        assert_eq!(meta_text(&p), "sample detail text");
        p.detail = "x".repeat(80);
        assert_eq!(meta_text(&p).chars().count(), 40); // clipped with ellipsis
    }

    #[test]
    fn edge_endpoints_face_each_other() {
        let parent = NodePos {
            id: 1,
            x: 100.0,
            y: 100.0,
            w: 168.0,
            h: 64.0,
            fp_w: 168.0,
            fp_h: 64.0,
            gen: 0,
        };
        let below = NodePos {
            id: 2,
            x: 120.0,
            y: 400.0,
            w: 148.0,
            h: 52.0,
            fp_w: 148.0,
            fp_h: 52.0,
            gen: 0,
        };
        let ((_, ay), (_, by)) = edge_endpoints(&parent, &below);
        assert_eq!((ay, by), (164.0, 400.0)); // parent bottom → child top
        let right = NodePos {
            id: 3,
            x: 600.0,
            y: 110.0,
            w: 148.0,
            h: 52.0,
            fp_w: 148.0,
            fp_h: 52.0,
            gen: 0,
        };
        let ((ax, _), (bx, _)) = edge_endpoints(&parent, &right);
        assert_eq!((ax, bx), (268.0, 600.0)); // parent right → child left
        let above = NodePos {
            id: 4,
            x: 110.0,
            y: 0.0,
            w: 148.0,
            h: 40.0,
            fp_w: 148.0,
            fp_h: 40.0,
            gen: 0,
        };
        let ((_, ay), (_, by)) = edge_endpoints(&parent, &above);
        assert_eq!((ay, by), (100.0, 40.0)); // parent top → child bottom
        let left = NodePos {
            id: 5,
            x: -400.0,
            y: 110.0,
            w: 148.0,
            h: 52.0,
            fp_w: 148.0,
            fp_h: 52.0,
            gen: 0,
        };
        let ((ax, _), (bx, _)) = edge_endpoints(&parent, &left);
        assert_eq!((ax, bx), (100.0, -252.0)); // parent left → child right
    }

    #[test]
    fn edge_controls_bow_along_dominant_axis() {
        // vertical pair: controls keep x, split y (the S-curve).
        assert_eq!(
            edge_controls((10.0, 0.0), (10.0, 100.0)),
            ((10.0, 50.0), (10.0, 50.0))
        );
        // horizontal pair: controls keep y, split x.
        assert_eq!(
            edge_controls((0.0, 20.0), (100.0, 20.0)),
            ((50.0, 20.0), (50.0, 20.0))
        );
    }

    #[test]
    fn edge_styles_by_child_state_and_satellites_thinner() {
        assert_eq!(
            edge_style(Lifecycle::Building, false),
            (AMBER_HAIR, false, 2.0)
        );
        assert_eq!(edge_style(Lifecycle::Idea, false), (HAIR, true, 1.4));
        assert_eq!(edge_style(Lifecycle::Done, false), (HAIR, false, 1.6));
        assert_eq!(edge_style(Lifecycle::Todo, false), (HAIR, false, 1.6));
        // parent→satellite: 1.2px, dimmer, regardless of state (docs/011 §B).
        assert_eq!(
            edge_style(Lifecycle::Building, true),
            (HAIR_SOFT, false, 1.2)
        );
        assert_eq!(edge_style(Lifecycle::Idea, true), (HAIR_SOFT, true, 1.2));
    }

    #[test]
    fn edge_list_never_reaches_hidden_nodes() {
        let parts = vec![
            part(1, None, Lifecycle::Building),
            part(2, Some(1), Lifecycle::Todo),
            part(3, Some(2), Lifecycle::Idea), // depth 2 — hidden, no edge
            part(4, None, Lifecycle::Todo),    // root: no incoming edge
        ];
        let tree = build_tree(&parts);
        let pos = layout_nodes(&tree, |_| None, 1000.0, 680.0);
        let edges = edge_list(&tree, &pos);
        // only the visible gen-0 → satellite pair; nothing to hidden node 3.
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].2, Lifecycle::Todo);
        assert_eq!((edges[0].0.id, edges[0].1.id), (1, 2));
        assert_eq!(edges[0].1.gen, 1);
        // drop the satellite from the laid-out set → its edge disappears too.
        let pruned: Vec<NodePos> = pos.iter().filter(|p| p.id != 2).copied().collect();
        assert_eq!(edge_list(&tree, &pruned).len(), 0);
    }

    #[test]
    fn node_size_tracks_content() {
        let mut lone = part(1, None, Lifecycle::Todo);
        lone.detail = String::new();
        let bare = TreeNode {
            part: lone,
            children: vec![],
        };
        let with_meta = TreeNode {
            part: part(2, None, Lifecycle::Todo),
            children: vec![],
        };
        assert!(node_size(&with_meta).1 > node_size(&bare).1);
        // a countable child adds the progress bar's height.
        let parent = TreeNode {
            part: part(3, None, Lifecycle::Building),
            children: vec![TreeNode {
                part: part(4, Some(3), Lifecycle::Todo),
                children: vec![],
            }],
        };
        assert!(node_size(&parent).1 > node_size(&with_meta).1);
        // ideas render as smaller ghosts.
        let idea = TreeNode {
            part: part(5, None, Lifecycle::Idea),
            children: vec![],
        };
        assert!(node_size(&idea).0 < node_size(&bare).0);
    }
}
