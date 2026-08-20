//! outlinepane — the Workspace's right-hand OUTLINE pane (docs/010 §3b, #10):
//! the focused map node's card, its append-only DECISION LOG (provenanced,
//! newest first), its children as task rows, the ＋ add-row ghosts, and the
//! per-op proposed-update cards (evidence-quoted, ✓/✕ per op — never a bulk
//! silent apply, docs/016).
//!
//! main.rs-agnostic by construction: every mutation is a caller-supplied
//! closure (`OutlineHandlers`), and the ONE inline editor lives in main.rs
//! (the search-bar input pattern) — this module only renders the buffer +
//! caret in whichever slot is active (`EditState`). All slots are single-line
//! except `Detail`, the multi-line detail_md editor (docs/019 slice 1b).

use std::rc::Rc;

use gpui::prelude::FluentBuilder;
use gpui::*;
use orchestrator_core::recap::rel_time;
use orchestrator_store::store::NoteRow;
use orchestrator_store::tree::countable_ratio;
use orchestrator_store::{DiffOp, Kind, Lifecycle, PartId, PartRef, TreeNode};

// Focused Dark tokens used by the outline render (values match main.rs).
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
/// text on an accent-filled button (matches main.rs's seed-accept).
const INK: u32 = 0x0C140F;
/// session-kind chip: termview's terminal accent (0x62A0D8) muted toward the
/// chip grays — sessions whisper like notes, they never shout like decisions.
const SESSION_CYAN: u32 = 0x7AA4CC;
/// the amber-tinted border of a proposal card (matches render_seed_proposal).
const PROPOSAL_HAIR: u32 = 0x4A4636;

/// The log shows the newest few entries inline; past this it collapses behind
/// a "full log · N" row (the log is append-only and unbounded by design).
const LOG_CAP: usize = 8;

// ---- editing state (rendered here, driven by main.rs) ----

/// Which outline slot the ONE inline edit occupies. Slots are mutually
/// exclusive by construction — a single `Option<EditSlot>` cannot represent
/// two open editors (the one-inline-edit-at-a-time contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditSlot {
    /// the "＋ part" ghost — commits as an Add under the focused node.
    AddPart,
    /// the "＋ decision" ghost — commits as add_note(kind="decision").
    AddDecision,
    /// the "＋ note" ghost — commits as add_note(kind="note").
    AddNote,
    /// a child row's ✎ — commits as a Rename of that part.
    RenameChild(PartId),
    /// the focus card's title (F2 / double-click, docs/019 slice 1b) —
    /// commits as a Rename of the FOCUSED part; the card renders the editor
    /// in the title's place. Rename was child-rows-only before 019.
    RenameFocused(PartId),
    /// Enter on the focused node — commits as an Add SIBLING below it
    /// (midpoint sort_order via tree::sibling_slot). The editor renders
    /// directly under the focus card, where the new row will land.
    AddSibling(PartId),
    /// the multi-line detail editor on detail_md (docs/019 slice 1b): rides
    /// the same root keystream, but Enter inserts a newline — ⌘⏎ commits as
    /// SetDetail, Esc cancels, blur commits (prose must survive a stray
    /// click). Plain text for now; markdown render is a later slice.
    Detail(PartId),
    /// dbl-click a CANVAS card's title / menu Rename (docs/019 CANVAS) —
    /// commits as a Rename like RenameFocused, but the editor renders as a
    /// canvas overlay at the node's rect (mapview::CanvasInput), never in
    /// this pane. A separate variant so the pane and the canvas can't both
    /// paint the same live buffer.
    RenameCanvas(PartId),
    /// dbl-click EMPTY canvas (docs/019 CANVAS) — commits as an Add under
    /// the current drill-frame root, pinned at the click point. The pin
    /// rides main.rs's `canvas_create_pin` (f64s here would break this
    /// enum's Eq); the editor is the same canvas overlay.
    CreateCanvas,
    /// edit-before-accept (docs/019 slice 1c T9): the user tweaks a
    /// proposed Add's NAME in the changeset review before accepting. The
    /// usize is the changeset's global op index; the edited name lands in
    /// `review.names` (not the store) and flows into the applied op on accept.
    ChangesetOpName(usize),
    /// "Flag needs-me…" (docs/019 slice 4): typing the one-line blocking
    /// QUESTION is required — the question IS the payload. Commits by writing
    /// the user needs-you flag (question + set-time) on that node; the
    /// editor renders as a top-of-map input bar, never in this pane.
    NeedsYou(PartId),
}

/// The inline editor's state, owned by the Orchestrator (chars arrive via the
/// IME replace_text_in_range branch + backspace/enter/esc in on_key_down).
#[derive(Clone, Default)]
pub struct EditState {
    pub active: Option<EditSlot>,
    pub buf: String,
}

impl EditState {
    fn is(&self, slot: EditSlot) -> bool {
        self.active == Some(slot)
    }
}

/// One linked session of the focused part, pre-shaped by main.rs
/// (SessionInfo ⋈ session_part). Live rows carry a phase word, ended rows a
/// final headline; `touched` marks whisper-grade rows (observed 'touch' and
/// demoted 'trail' dispatches, docs/019) — they whisper here and NEVER become
/// map chips (docs/011 §D).
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub cli_id: String,
    pub label: String,
    pub live: bool,
    pub phase: String,
    pub headline: String,
    pub touched: bool,
}

// ---- caller-supplied callbacks (what cx.listener would otherwise close over) ----

pub type PartCb = Rc<dyn Fn(PartId, &mut Window, &mut App)>;
pub type CycleCb = Rc<dyn Fn(PartId, Lifecycle, &mut Window, &mut App)>;
pub type SlotCb = Rc<dyn Fn(EditSlot, &mut Window, &mut App)>;
pub type OpCb = Rc<dyn Fn(usize, &mut Window, &mut App)>;
pub type Cb = Rc<dyn Fn(&mut Window, &mut App)>;
pub type DispatchCb = Rc<dyn Fn(PartId, bool, &mut Window, &mut App)>;
pub type SessionCb = Rc<dyn Fn(String, &mut Window, &mut App)>;
pub type LinkCb = Rc<dyn Fn(PartId, String, &mut Window, &mut App)>;
pub type KindCb = Rc<dyn Fn(PartId, Kind, &mut Window, &mut App)>;

/// Every mutation the pane can request, as closures — the pane never touches
/// the Orchestrator or the store itself.
#[derive(Clone)]
pub struct OutlineHandlers {
    /// click a child row → focus it on the map.
    pub focus_child: PartCb,
    /// click a status glyph → assert the given (already-advanced) lifecycle.
    pub cycle_status: CycleCb,
    /// click a ＋ ghost or a row's ✎ → main.rs opens its inline editor there.
    pub begin_edit: SlotCb,
    /// per-op ✓ / ✕ on the proposed-update card — the index is the position
    /// in the `ops` slice handed to `proposed_card` (the caller owns the
    /// mapping back to its pending-diff row).
    pub accept_op: OpCb,
    pub dismiss_op: OpCb,
    /// the collapsed "full log · N" row.
    pub open_full_log: Cb,
    /// '▶ work on this' → dispatch_to_part; `alt` = also jump to the Agent
    /// stage (docs/011 §C — the default is stay-on-map, chip = confirmation).
    pub dispatch: DispatchCb,
    /// click a live row in the SESSIONS section → jump to the Agent stage on
    /// that cli session.
    pub open_session: SessionCb,
    /// '◇ break down' on the focus card → propose 3-7 child Adds via one
    /// isolated claude -p call (docs/011 slice 3); main.rs flips
    /// `breaking_down` while the proposal is in flight.
    pub break_down: PartCb,
    /// pick a live session in the link expander → retro-link it to this part
    /// (main.rs closes the expander after linking).
    pub link_session: LinkCb,
    /// the 'link a session ▸' row — flips the expander (state lives in
    /// main.rs, like EditState).
    pub toggle_link: Cb,
    /// click the focus card's kind chip → assert the given (already-cycled)
    /// kind via DiffOp::SetKind — the one-gesture backfill repair (docs/019).
    pub set_kind: KindCb,
}

// ---- pure formatting helpers (unit-tested; RULE ZERO) ----

/// The visual tier of a changeset review row (docs/019 T9: "a diff of the
/// document"). Add = a ghost/green insert, Remove = a struck deletion, Move =
/// an old→new breadcrumb, Change = a described SetStatus/SetKind/SetDetail/
/// Rename. main.rs styles each tier; the classification lives here so it's
/// tested next to its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffRowKind {
    Add,
    Remove,
    Move,
    Change,
}

/// One op → its diff-of-the-document row (docs/019 slice 1c T9). Pure:
/// - `name_of` names any live part (for Remove / Move subject),
/// - `cur_parent` names a part's CURRENT parent (the Move breadcrumb's origin;
///   empty → "top level"),
/// - `describe` renders the Change-class ops (main.rs passes `describe_op`, the
///   one existing op-phrasing helper — reused, never re-derived).
///
/// Add targets resolve their parent from the op itself. A Temp parent (an Add
/// nested under another Add in the SAME changeset) renders "under a new node" —
/// slice 1c's canned dissolve has no such shape; full nested-Add breadcrumbs
/// ride the deferred canvas ghost overlay.
pub fn changeset_row(
    op: &DiffOp,
    name_of: &dyn Fn(PartId) -> String,
    cur_parent: &dyn Fn(PartId) -> String,
    describe: &dyn Fn(&DiffOp) -> String,
) -> (DiffRowKind, String) {
    let parent_label = |r: &PartRef| -> String {
        match r {
            PartRef::Id(p) => name_of(*p),
            PartRef::Root => "top level".to_string(),
            PartRef::Temp(_) => "a new node".to_string(),
        }
    };
    match op {
        DiffOp::Add { name, parent, .. } => (
            DiffRowKind::Add,
            format!("＋ {name}  ·  under {}", parent_label(parent)),
        ),
        DiffOp::Remove { id } => (DiffRowKind::Remove, format!("－ {}", name_of(*id))),
        DiffOp::Move { id, parent, .. } => {
            let from = cur_parent(*id);
            let from = if from.is_empty() {
                "top level".to_string()
            } else {
                from
            };
            (
                DiffRowKind::Move,
                format!("{}:  {}  →  {}", name_of(*id), from, parent_label(parent)),
            )
        }
        _ => (DiffRowKind::Change, describe(op)),
    }
}

/// The provenance by-line of a log entry: `— you · 2m` for user entries,
/// `— session a3f2c9d1 · 3h` for session-sourced ones (source `sess-<cli id>`,
/// shown truncated to 8 chars). Unknown sources pass through verbatim rather
/// than masquerading as the user.
pub fn provenance(source: &str, ts_secs: u64, now_secs: u64) -> String {
    let ago = rel_time(now_secs.saturating_sub(ts_secs));
    let who = if source == "user" {
        "you".to_string()
    } else if let Some(id) = source.strip_prefix("sess-") {
        format!("session {}", id8(id))
    } else {
        source.to_string()
    };
    format!("— {who} · {ago}")
}

/// A cli session id shortened for display — first 8 chars, shorter ids intact.
pub fn id8(cli_id: &str) -> String {
    cli_id.chars().take(8).collect()
}

/// The link expander's header label — the chevron flips with open state.
pub fn link_row_label(open: bool) -> &'static str {
    if open {
        "link a session ▾"
    } else {
        "link a session ▸"
    }
}

/// The collapsed-log row label — `None` while everything fits inline.
pub fn overflow_label(total: usize, cap: usize) -> Option<String> {
    (total > cap).then(|| format!("full log · {total} entries"))
}

/// Kind chip for a log entry: decisions SHOUT (they're commitments), notes and
/// context whisper. Unknown kinds degrade to a note, never to a decision.
pub fn kind_chip(kind: &str) -> (&'static str, u32) {
    match kind {
        "decision" => ("DECIDED", AMBER),
        "context" => ("context", MUTED2),
        "session" => ("SESSION", SESSION_CYAN),
        _ => ("note", MUTED),
    }
}

/// Status glyph + color — honest uncertainty (docs/016): a stale assertion
/// decays to a hollow amber mark no matter what it claims. Mirrors main.rs's
/// `part_glyph` (which takes the whole Part; the integrator may dedupe onto this).
pub fn glyph(lifecycle: Lifecycle, stale: bool) -> (&'static str, u32) {
    if stale {
        return ("◌", AMBER); // unverified / stale — confidence lowered
    }
    match lifecycle {
        Lifecycle::Done => ("●", GREEN),
        Lifecycle::Building => ("◐", AMBER),
        Lifecycle::Todo => ("○", MUTED),
        Lifecycle::Idea => ("·", MUTED2),
    }
}

/// The focus card's state tag — the lifecycle word, `· unverified` when stale.
pub fn state_tag(lifecycle: Lifecycle, stale: bool) -> String {
    let base = lifecycle.as_str();
    if stale {
        format!("{base} · unverified")
    } else {
        base.to_string()
    }
}

/// Click-cycle order (mirrors main.rs): Idea enters the committed loop; Done
/// wraps to Todo, not Idea — demoting finished work to a ghost is never wanted.
/// Delegates to the ONE canonical cycle (docs/019: `building` is derived-only
/// and unreachable by hand; a stale local copy made glyph clicks a no-op).
pub fn next_lifecycle(lc: Lifecycle) -> Lifecycle {
    lc.cycle()
}

/// Kind-chip color (docs/019 slice 1b): areas read structural (accent),
/// tasks neutral, ideas whisper — the same loudness ladder as the lifecycle
/// glyphs, so kind never shouts over status.
pub fn kind_color(kind: Kind) -> u32 {
    match kind {
        Kind::Area => ACCENT,
        Kind::Task => MUTED,
        Kind::Idea => MUTED2,
    }
}

/// The per-aspect progress phrase ("3 of 7 built") — `None` when nothing is
/// committed yet (ideas don't count, and "0 of 0" reads as failure).
pub fn ratio_label(done: usize, total: usize) -> Option<String> {
    (total > 0).then(|| format!("{done} of {total} built"))
}

/// Static status dot for a live session row — the pulsing chip lives on the
/// map; the outline stays calm. Working burns amber, your-turn rests green,
/// needs-you flags ⚠ amber. Unknown phase words stay neutral rather than
/// claim a state they can't back.
pub fn phase_dot(phase: &str) -> (&'static str, u32) {
    match phase {
        "working" => ("●", AMBER),
        "your turn" => ("●", GREEN),
        "needs you" => ("⚠", AMBER),
        _ => ("●", MUTED2),
    }
}

/// SESSIONS section order: live dispatch rows lead, ended rows follow, and
/// 'also touched' rows whisper last regardless of liveness (observed
/// attribution never outranks declared — docs/011 §D). Stable within each
/// group, so the caller's store order survives.
pub fn ordered_sessions(rows: &[SessionRow]) -> Vec<&SessionRow> {
    let mut v: Vec<&SessionRow> = rows.iter().collect();
    v.sort_by_key(|r| (r.touched, !r.live));
    v
}

/// The break-down affordance's label — the in-flight variant renders muted
/// and handler-less (disabled) while the proposal call runs.
pub fn break_down_label(breaking_down: bool) -> &'static str {
    if breaking_down {
        "◇ breaking down…"
    } else {
        "◇ break down"
    }
}

/// Log order: newest first. The store already returns this, but re-assert it
/// here — ties on ts_secs (same-second inserts) break by id (AUTOINCREMENT ⇒
/// higher id is newer).
pub fn newest_first(notes: &[NoteRow]) -> Vec<&NoteRow> {
    let mut v: Vec<&NoteRow> = notes.iter().collect();
    v.sort_by(|a, b| b.ts_secs.cmp(&a.ts_secs).then(b.id.cmp(&a.id)));
    v
}

// ---- element builders ----

fn section_head(label: &'static str) -> impl IntoElement {
    div()
        .mt(px(10.))
        .text_size(px(10.5))
        .text_color(rgb(MUTED2))
        .child(label)
}

/// Thin per-aspect progress track (same geometry as main.rs's `bar`).
fn meter(done: usize, total: usize, width: f32) -> impl IntoElement {
    let ratio = if total == 0 {
        0.0
    } else {
        done as f32 / total as f32
    };
    let w = (width * ratio.clamp(0.0, 1.0)).round();
    div()
        .w(px(width))
        .h(px(3.))
        .rounded(px(3.))
        .bg(rgb(HAIR_SOFT))
        .child(div().w(px(w)).h(px(3.)).rounded(px(3.)).bg(rgb(GREEN)))
}

/// The live single-line editor: slot label, buffer, block caret, key hints.
/// Pure display — main.rs owns the keystream and focus. `pub` so the changeset
/// review card (rendered in main.rs) reuses the exact editor chrome for its
/// edit-before-accept name input (docs/019 slice 1c).
pub fn inline_input(
    slot_label: &'static str,
    buf: &str,
    caret: crate::textedit::Caret,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(7.))
        .px(px(9.))
        .py(px(5.))
        .rounded(px(8.))
        .bg(rgb(CARD2))
        .border_1()
        .border_color(rgb(ACCENT))
        .child(
            div()
                .flex_none()
                .text_size(px(10.5))
                .text_color(rgb(MUTED2))
                .child(slot_label),
        )
        .child({
            // The buffer is drawn as THREE runs — before the selection, the
            // selection, after it — because that is what lets the caret sit
            // where the cursor actually IS. The old single run could only ever
            // put the bar after the whole string, which is why the field was
            // append-only no matter what the key router did.
            let mut c = caret;
            c.clamp(buf);
            let (s, e) = c.range();
            let run = |t: &str| {
                div()
                    .text_size(px(12.5))
                    .text_color(rgb(TEXT_STRONG))
                    .child(SharedString::from(t.to_string()))
            };
            div()
                .flex()
                .flex_row()
                .items_center()
                .min_w_0()
                .child(run(&buf[..s]))
                // A selection REPLACES the caret as the position indicator:
                // drawing both at once reads as a rendering bug.
                .when(s != e, |r| {
                    r.child(
                        div()
                            .rounded(px(2.))
                            .bg(rgba(0x7EE2C038))
                            .child(run(&buf[s..e])),
                    )
                })
                .when(s == e, |r| {
                    r.child(div().flex_none().w(px(2.)).h(px(13.)).bg(rgb(ACCENT)))
                })
                .child(run(&buf[e..]))
        })
        .child(div().flex_1())
        .child(
            div()
                .flex_none()
                .text_size(px(10.))
                .text_color(rgb(MUTED2))
                .child("⏎ save · esc cancel"),
        )
}

/// The multi-line detail editor (docs/019 slice 1b): a textarea-grade buffer
/// on detail_md riding the SAME root keystream as inline_input — main.rs
/// turns plain Enter into a newline for this slot and commits on ⌘⏎ (blur
/// also commits; Esc cancels). Pure display: lines + block caret + key hints.
fn detail_editor(buf: &str) -> impl IntoElement {
    let mut lines: Vec<String> = buf.split('\n').map(str::to_string).collect();
    let last = lines.pop().unwrap_or_default();
    let mut col = div()
        .flex()
        .flex_col()
        .gap(px(1.))
        .px(px(9.))
        .py(px(6.))
        .rounded(px(8.))
        .bg(rgb(CARD2))
        .border_1()
        .border_color(rgb(ACCENT));
    for l in lines {
        // an empty div collapses to 0px — blank lines must keep their row.
        col = col.child(
            div()
                .min_h(px(16.))
                .text_size(px(12.5))
                .text_color(rgb(TEXT_STRONG))
                .child(SharedString::from(if l.is_empty() {
                    " ".to_string()
                } else {
                    l
                })),
        );
    }
    col.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .min_h(px(16.))
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(rgb(TEXT_STRONG))
                    .child(SharedString::from(last)),
            )
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
            .mt(px(4.))
            .text_size(px(10.))
            .text_color(rgb(MUTED2))
            .child("⌘⏎ save · esc cancel"),
    )
}

/// (1) The focus card: ancestry one-liner (when focused below the canvas
/// root), cycleable glyph, name (F2/double-click renames in place), the kind
/// chip (docs/019: one gesture flips a backfill miss), state tag, the full
/// detail_md prose (click or E → the multi-line editor; clips die at gen-0,
/// the outline carries whole prose), and "n of m" via countable_ratio.
pub fn focus_card(
    node: &TreeNode,
    ancestry: &str,
    breaking_down: bool,
    edit: &EditState,
    caret: crate::textedit::Caret,
    h: &OutlineHandlers,
) -> impl IntoElement {
    let p = &node.part;
    let id = p.id;
    let (g, gc) = glyph(p.lifecycle, p.stale);
    let next = next_lifecycle(p.lifecycle);
    let cycle = h.cycle_status.clone();
    let dispatch = h.dispatch.clone();
    let set_kind = h.set_kind.clone();
    let next_kind = p.kind.cycle();
    let (done, total) = countable_ratio(node);
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        // 'Capture ▸ Diarization' — contextualizes focusing a satellite or a
        // drilled node (docs/011 §A, slice 2).
        .when(!ancestry.is_empty(), |c| {
            c.child(
                div()
                    .text_size(px(10.5))
                    .text_color(rgb(MUTED2))
                    .child(SharedString::from(ancestry.to_string())),
            )
        })
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(9.))
                .child(
                    div()
                        .id(SharedString::from(format!("ofoc-cyc-{id}")))
                        .flex_none()
                        .cursor_pointer()
                        .text_size(px(15.))
                        .text_color(rgb(gc))
                        .hover(|s| s.text_color(rgb(TEXT_STRONG)))
                        .child(g)
                        .on_click(move |_: &ClickEvent, window, app| cycle(id, next, window, app)),
                )
                .child(if edit.is(EditSlot::RenameFocused(id)) {
                    // rename-in-place (docs/019 slice 1b): the FOCUSED node was
                    // the one row the ✎ grammar couldn't reach.
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(inline_input("rename", &edit.buf, caret))
                        .into_any_element()
                } else {
                    let begin = h.begin_edit.clone();
                    div()
                        .id(SharedString::from(format!("ofoc-title-{id}")))
                        .text_size(px(16.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_STRONG))
                        .child(SharedString::from(p.name.clone()))
                        // double-click only — a single click on the title must
                        // stay inert (the row isn't a button, it's a name).
                        .on_click(move |ev: &ClickEvent, window, app| {
                            if ev.click_count() >= 2 {
                                begin(EditSlot::RenameFocused(id), window, app)
                            }
                        })
                        .into_any_element()
                })
                .child(
                    // the kind chip (docs/019 commitment 2): area | task | idea,
                    // click = SetKind to the next kind — repair, not a form.
                    div()
                        .id(SharedString::from(format!("ofoc-kind-{id}")))
                        .flex_none()
                        .px(px(6.))
                        .py(px(1.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(rgb(HAIR_SOFT))
                        .cursor_pointer()
                        .text_size(px(10.))
                        .font_family("Menlo")
                        .text_color(rgb(kind_color(p.kind)))
                        .hover(|s| s.text_color(rgb(TEXT_STRONG)).border_color(rgb(HAIR)))
                        .child(p.kind.as_str())
                        .on_click(move |_: &ClickEvent, window, app| {
                            set_kind(id, next_kind, window, app)
                        }),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.5))
                        .text_color(rgb(gc))
                        .child(SharedString::from(state_tag(p.lifecycle, p.stale))),
                ),
        )
        .child(if edit.is(EditSlot::Detail(id)) {
            detail_editor(&edit.buf).into_any_element()
        } else if p.detail_md.trim().is_empty() {
            // the empty-body ghost seeds the editor's shape (docs/019: the
            // system-map What-it-is / How-it-works schema as scaffolding).
            let begin = h.begin_edit.clone();
            div()
                .id(SharedString::from(format!("ofoc-det-{id}")))
                .cursor_pointer()
                .text_size(px(11.5))
                .text_color(rgb(MUTED2))
                .hover(|s| s.text_color(rgb(TEXT)))
                .child("＋ detail — what it is · how it works")
                .on_click(move |_: &ClickEvent, window, app| {
                    begin(EditSlot::Detail(id), window, app)
                })
                .into_any_element()
        } else {
            // the FULL body, line by line (the 40-char clip died in docs/019);
            // click anywhere in the prose to edit it.
            let begin = h.begin_edit.clone();
            let mut prose = div()
                .id(SharedString::from(format!("ofoc-det-{id}")))
                .cursor_pointer()
                .flex()
                .flex_col()
                .gap(px(1.))
                .rounded(px(6.))
                .hover(|s| s.bg(rgb(CARD)))
                .on_click(move |_: &ClickEvent, window, app| {
                    begin(EditSlot::Detail(id), window, app)
                });
            for l in p.detail_md.trim_end().lines() {
                prose = prose.child(
                    div()
                        .min_h(px(16.))
                        .text_size(px(13.))
                        .text_color(rgb(MUTED))
                        .child(SharedString::from(if l.is_empty() {
                            " ".to_string()
                        } else {
                            l.to_string()
                        })),
                );
            }
            prose.into_any_element()
        })
        .when(edit.is(EditSlot::AddSibling(id)), |c| {
            // Enter's new-sibling editor renders under the card — where the
            // row will land in the tree (docs/019 OUTLINE grammar).
            c.child(inline_input("＋ sibling", &edit.buf, caret))
        })
        .when_some(ratio_label(done, total), |c, label| {
            c.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(meter(done, total, 70.))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED2))
                            .child(SharedString::from(label)),
                    ),
            )
        })
        .child(
            div()
                .mt(px(2.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .id(SharedString::from(format!("ofoc-disp-{id}")))
                        .flex_none()
                        .cursor_pointer()
                        .text_size(px(11.5))
                        .text_color(rgb(MUTED))
                        .hover(|s| s.text_color(rgb(ACCENT)))
                        .child("▶ work on this")
                        // stay-on-map default; ⌥ = dispatch AND jump (docs/011 §C)
                        .on_click(move |ev: &ClickEvent, window, app| {
                            dispatch(id, ev.modifiers().alt, window, app)
                        }),
                )
                .child(if breaking_down {
                    // in flight: muted, no handler — never a modal (docs/011 slice 3)
                    div()
                        .flex_none()
                        .text_size(px(11.5))
                        .text_color(rgb(MUTED2))
                        .child(break_down_label(true))
                        .into_any_element()
                } else {
                    let break_down = h.break_down.clone();
                    div()
                        .id(SharedString::from(format!("ofoc-brk-{id}")))
                        .flex_none()
                        .cursor_pointer()
                        .text_size(px(11.5))
                        .text_color(rgb(MUTED))
                        .hover(|s| s.text_color(rgb(ACCENT)))
                        .child(break_down_label(false))
                        .on_click(move |_: &ClickEvent, window, app| break_down(id, window, app))
                        .into_any_element()
                })
                .child(
                    div()
                        .flex_none()
                        .text_size(px(10.))
                        .text_color(rgb(MUTED2))
                        .child("⌥ opens terminal"),
                ),
        )
}

/// The 'link a session ▸' expander (docs/011 slice 1 day 4): retro-link a live
/// session that wasn't dispatched from this node. Render-only — `link_open`
/// lives in main.rs (like EditState) and flips via `toggle_link`; picking a
/// row calls `link_session` and main.rs closes the expander.
pub fn link_row(
    part_id: PartId,
    live_sessions: &[(String, String)],
    link_open: bool,
    h: &OutlineHandlers,
) -> impl IntoElement {
    let toggle = h.toggle_link.clone();
    let mut col = div().flex().flex_col().gap(px(2.)).child(
        div()
            .id("olink-toggle")
            .px(px(6.))
            .py(px(3.))
            .rounded(px(7.))
            .cursor_pointer()
            .text_size(px(11.))
            .text_color(rgb(MUTED2))
            .hover(|s| s.text_color(rgb(TEXT)))
            .child(link_row_label(link_open))
            .on_click(move |_: &ClickEvent, window, app| toggle(window, app)),
    );
    if link_open {
        if live_sessions.is_empty() {
            col = col.child(
                div()
                    .px(px(9.))
                    .py(px(3.))
                    .text_size(px(11.5))
                    .text_color(rgb(MUTED2))
                    .child("no live sessions"),
            );
        }
        for (ix, (cli_id, label)) in live_sessions.iter().enumerate() {
            let link = h.link_session.clone();
            let cid = cli_id.clone();
            col = col.child(
                div()
                    .id(SharedString::from(format!("olink-{ix}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .px(px(9.))
                    .py(px(4.))
                    .rounded(px(7.))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(CARD)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .text_color(rgb(TEXT))
                            .child(SharedString::from(label.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.5))
                            .font_family("Menlo")
                            .text_color(rgb(MUTED2))
                            .child(SharedString::from(id8(&cid))),
                    )
                    .on_click(move |_: &ClickEvent, window, app| {
                        link(part_id, cid.clone(), window, app)
                    }),
            );
        }
    }
    col
}

/// (2) The DECISION LOG: provenanced entries newest first, capped at LOG_CAP
/// with a clickable "full log · N" row past that.
pub fn decision_log(notes: &[NoteRow], now_secs: u64, h: &OutlineHandlers) -> impl IntoElement {
    let ordered = newest_first(notes);
    let mut col = div()
        .flex()
        .flex_col()
        .gap(px(5.))
        .child(section_head("DECISION LOG"));
    if ordered.is_empty() {
        col = col.child(
            div()
                .text_size(px(12.))
                .text_color(rgb(MUTED2))
                .child("Nothing logged yet — decisions and notes land here, newest first."),
        );
    }
    for n in ordered.iter().take(LOG_CAP) {
        let (chip, cc) = kind_chip(&n.kind);
        col = col.child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(8.))
                .px(px(9.))
                .py(px(6.))
                .rounded(px(8.))
                .bg(rgb(CARD))
                .child(
                    div()
                        .flex_none()
                        .mt(px(1.))
                        .text_size(px(9.5))
                        .font_family("Menlo")
                        .text_color(rgb(cc))
                        .child(chip),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(px(12.5))
                                .text_color(rgb(TEXT))
                                .child(SharedString::from(n.text.clone())),
                        )
                        .child(
                            div()
                                .text_size(px(10.5))
                                .font_family("Menlo")
                                .text_color(rgb(MUTED2))
                                .child(SharedString::from(provenance(
                                    &n.source, n.ts_secs, now_secs,
                                ))),
                        ),
                ),
        );
    }
    if let Some(label) = overflow_label(ordered.len(), LOG_CAP) {
        let open = h.open_full_log.clone();
        col = col.child(
            div()
                .id("olog-full")
                .px(px(9.))
                .py(px(4.))
                .rounded(px(7.))
                .cursor_pointer()
                .text_size(px(11.))
                .text_color(rgb(MUTED2))
                .hover(|s| s.text_color(rgb(TEXT)))
                .child(SharedString::from(label))
                .on_click(move |_: &ClickEvent, window, app| open(window, app)),
        );
    }
    col
}

/// (2b) The SESSIONS section (docs/011 slice 3), between the log and the
/// child rows: live dispatch rows first (static dot + phase word, click →
/// Agent stage), ended rows keep their final headline, touch-role rows
/// whisper 'also touched' (no dot, never clickable — observed, not declared).
/// The caller omits this section entirely when `sessions` is empty.
pub fn sessions_section(sessions: &[SessionRow], h: &OutlineHandlers) -> impl IntoElement {
    let mut col = div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(section_head("SESSIONS"));
    for (ix, s) in ordered_sessions(sessions).into_iter().enumerate() {
        if s.touched {
            col = col.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .px(px(9.))
                    .py(px(3.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(11.5))
                            .text_color(rgb(MUTED2))
                            .child(SharedString::from(s.label.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED2))
                            .child("also touched"),
                    ),
            );
        } else if s.live {
            let open = h.open_session.clone();
            let cid = s.cli_id.clone();
            let (dot, dc) = phase_dot(&s.phase);
            col = col.child(
                div()
                    .id(SharedString::from(format!("osess-{ix}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .px(px(9.))
                    .py(px(4.))
                    .rounded(px(7.))
                    .cursor_pointer()
                    .hover(|st| st.bg(rgb(CARD)))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.))
                            .text_color(rgb(dc))
                            .child(dot),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .text_color(rgb(TEXT))
                            .child(SharedString::from(s.label.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.5))
                            .text_color(rgb(MUTED2))
                            .child(SharedString::from(s.phase.clone())),
                    )
                    .on_click(move |_: &ClickEvent, window, app| open(cid.clone(), window, app)),
            );
        } else {
            col = col.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .px(px(9.))
                    .py(px(3.))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.5))
                            .text_color(rgb(MUTED2))
                            .child(SharedString::from(format!("■ {}", s.label))),
                    )
                    .when(!s.headline.is_empty(), |row| {
                        row.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(11.))
                                .text_color(rgb(MUTED2))
                                .child(SharedString::from(s.headline.clone())),
                        )
                    }),
            );
        }
    }
    col
}

/// (3) Children as task rows: cycleable glyph, name + detail (click focuses
/// the child on the map), ✎ opens the inline rename in place. Glyph/✎ clicks
/// stop propagation so they don't also re-focus the row.
pub fn child_rows(
    children: &[TreeNode],
    edit: &EditState,
    caret: crate::textedit::Caret,
    h: &OutlineHandlers,
) -> impl IntoElement {
    let mut col = div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(section_head("PARTS"));
    if children.is_empty() {
        col = col.child(
            div()
                .py(px(4.))
                .text_size(px(12.))
                .text_color(rgb(MUTED2))
                .child("No sub-parts yet — ＋ part below."),
        );
    }
    for ch in children {
        let p = &ch.part;
        let id = p.id;
        if edit.is(EditSlot::RenameChild(id)) {
            col = col.child(inline_input("rename", &edit.buf, caret));
            continue;
        }
        let (g, gc) = glyph(p.lifecycle, p.stale);
        let next = next_lifecycle(p.lifecycle);
        let cycle = h.cycle_status.clone();
        let focus = h.focus_child.clone();
        let begin = h.begin_edit.clone();
        col = col.child(
            div()
                .id(SharedString::from(format!("och-{id}")))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(9.))
                .px(px(6.))
                .py(px(5.))
                .rounded(px(8.))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(CARD)))
                .on_click(move |_: &ClickEvent, window, app| focus(id, window, app))
                .child(
                    div()
                        .id(SharedString::from(format!("och-cyc-{id}")))
                        .flex_none()
                        .cursor_pointer()
                        .text_size(px(13.))
                        .text_color(rgb(gc))
                        .hover(|s| s.text_color(rgb(TEXT_STRONG)))
                        .child(g)
                        .on_click(move |_: &ClickEvent, window, app| {
                            app.stop_propagation();
                            cycle(id, next, window, app)
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(rgb(if p.lifecycle == Lifecycle::Done {
                                    MUTED
                                } else {
                                    TEXT
                                }))
                                .child(SharedString::from(p.name.clone())),
                        )
                        .when(!p.detail.is_empty(), |d| {
                            d.child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(MUTED2))
                                    .child(SharedString::from(p.detail.clone())),
                            )
                        }),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("och-edit-{id}")))
                        .flex_none()
                        .cursor_pointer()
                        .text_size(px(11.))
                        .text_color(rgb(MUTED2))
                        .hover(|s| s.text_color(rgb(TEXT_STRONG)))
                        .child("✎")
                        .on_click(move |_: &ClickEvent, window, app| {
                            app.stop_propagation();
                            begin(EditSlot::RenameChild(id), window, app)
                        }),
                ),
        );
    }
    col
}

/// (4) The add-row: three ghost chips; while an Add* slot is live the whole
/// row becomes the inline editor (RenameChild renders in place, in child_rows).
pub fn add_row(
    edit: &EditState,
    caret: crate::textedit::Caret,
    h: &OutlineHandlers,
) -> AnyElement {
    let live = match edit.active {
        Some(EditSlot::AddPart) => Some("＋ part"),
        Some(EditSlot::AddDecision) => Some("＋ decision"),
        Some(EditSlot::AddNote) => Some("＋ note"),
        _ => None,
    };
    if let Some(slot_label) = live {
        return div()
            .mt(px(6.))
            .child(inline_input(slot_label, &edit.buf, caret))
            .into_any_element();
    }
    let ghost = |id: &'static str, label: &'static str, slot: EditSlot, h: &OutlineHandlers| {
        let begin = h.begin_edit.clone();
        div()
            .id(id)
            .px(px(9.))
            .py(px(4.))
            .rounded(px(7.))
            .cursor_pointer()
            .border_1()
            .border_color(rgb(HAIR_SOFT))
            .text_size(px(11.5))
            .text_color(rgb(MUTED2))
            .hover(|s| s.text_color(rgb(TEXT)).border_color(rgb(HAIR)))
            .child(label)
            .on_click(move |_: &ClickEvent, window, app| begin(slot, window, app))
    };
    div()
        .mt(px(6.))
        .flex()
        .flex_row()
        .gap(px(6.))
        .child(ghost("oadd-part", "＋ part", EditSlot::AddPart, h))
        .child(ghost("oadd-dec", "＋ decision", EditSlot::AddDecision, h))
        .child(ghost("oadd-note", "＋ note", EditSlot::AddNote, h))
        .into_any_element()
}

/// (5) The per-op proposed-update card: `DiffOp::summary` + the proposing
/// session's evidence quote, with ✓ accept / ✕ dismiss PER OP (a drift
/// proposal is several independent claims, not one take-it-or-leave-it blob).
/// `ops` pairs each op with its optional evidence line; callback indices are
/// positions in this slice.
pub fn proposed_card(
    ops: &[(DiffOp, Option<String>)],
    name_of: &dyn Fn(PartId) -> String,
    h: &OutlineHandlers,
) -> impl IntoElement {
    let mut col = div().flex().flex_col().gap(px(6.)).child(
        div()
            .mt(px(10.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(7.))
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(rgb(AMBER))
                    .child("PROPOSED UPDATES"),
            )
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(rgb(MUTED2))
                    .child("nothing sticks until you ✓"),
            ),
    );
    for (ix, (op, evidence)) in ops.iter().enumerate() {
        let accept = h.accept_op.clone();
        let dismiss = h.dismiss_op.clone();
        col = col.child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(9.))
                .px(px(10.))
                .py(px(7.))
                .rounded(px(9.))
                .bg(rgb(CARD))
                .border_1()
                .border_color(rgb(PROPOSAL_HAIR))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .child(
                            div()
                                .text_size(px(12.5))
                                .text_color(rgb(TEXT_STRONG))
                                .child(SharedString::from(op.summary(|pid| name_of(pid)))),
                        )
                        .when_some(evidence.clone(), |c, q| {
                            c.child(
                                div()
                                    .text_size(px(11.5))
                                    .italic()
                                    .text_color(rgb(MUTED))
                                    .child(SharedString::from(format!("“{q}”"))),
                            )
                        }),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("opacc-{ix}")))
                        .flex_none()
                        .px(px(8.))
                        .py(px(3.))
                        .rounded(px(7.))
                        .bg(rgb(ACCENT))
                        .cursor_pointer()
                        .text_size(px(11.5))
                        .text_color(rgb(INK))
                        .child("✓")
                        .on_click(move |_: &ClickEvent, window, app| accept(ix, window, app)),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("opdis-{ix}")))
                        .flex_none()
                        .px(px(8.))
                        .py(px(3.))
                        .rounded(px(7.))
                        .border_1()
                        .border_color(rgb(HAIR))
                        .cursor_pointer()
                        .text_size(px(11.5))
                        .text_color(rgb(MUTED))
                        .child("✕")
                        .on_click(move |_: &ClickEvent, window, app| dismiss(ix, window, app)),
                ),
        );
    }
    col
}

/// The whole pane in contract order: focus card → link-a-session expander →
/// decision log → sessions (when any) → children → add-row → proposed
/// updates (when any). Each section is public so the integrator can
/// recompose.
pub fn outline_pane(
    node: &TreeNode,
    notes: &[NoteRow],
    pending: &[(DiffOp, Option<String>)],
    edit: &EditState,
    caret: crate::textedit::Caret,
    live_sessions: &[(String, String)],
    link_open: bool,
    sessions: &[SessionRow],
    ancestry: &str,
    breaking_down: bool,
    now_secs: u64,
    name_of: &dyn Fn(PartId) -> String,
    h: &OutlineHandlers,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .p(px(18.))
        .child(focus_card(node, ancestry, breaking_down, edit, caret, h))
        .child(link_row(node.part.id, live_sessions, link_open, h))
        .child(decision_log(notes, now_secs, h))
        .when(!sessions.is_empty(), |c| {
            c.child(sessions_section(sessions, h))
        })
        .child(child_rows(&node.children, edit, caret, h))
        .child(add_row(edit, caret, h))
        .when(!pending.is_empty(), |c| {
            c.child(proposed_card(pending, name_of, h))
        })
}

#[cfg(test)]
mod tests {
    // NOT `use super::*`: the parent's `use gpui::*` glob would leak in and
    // shadow the built-in `#[test]` with gpui's proc-macro of the same name
    // (recursion-limit blowup at expansion time).
    use super::{
        break_down_label, changeset_row, glyph, id8, kind_chip, kind_color, link_row_label,
        newest_first, next_lifecycle, ordered_sessions, overflow_label, phase_dot, provenance,
        ratio_label, state_tag, DiffRowKind, SessionRow, ACCENT, AMBER, GREEN, MUTED, MUTED2,
        SESSION_CYAN,
    };
    use orchestrator_store::store::NoteRow;
    use orchestrator_store::{DiffOp, Kind, Lifecycle, PartRef};

    fn note(id: i64, ts: u64, kind: &str, source: &str) -> NoteRow {
        NoteRow {
            id,
            part_id: 1,
            ts_secs: ts,
            kind: kind.into(),
            text: format!("n{id}"),
            source: source.into(),
        }
    }

    #[test]
    fn changeset_rows_classify_and_phrase_the_document_diff() {
        // 1 = Tech, 2 = Store (currently under Tech), 9 = the destination root.
        let name = |id| {
            match id {
                1 => "Tech",
                2 => "Store",
                9 => "Engine",
                _ => "?",
            }
            .to_string()
        };
        let parent = |id| {
            match id {
                2 => "Tech",
                _ => "",
            }
            .to_string()
        };
        let describe = |_op: &DiffOp| "Store → done".to_string();

        // Add resolves its parent from the op; Root = "top level".
        let add = DiffOp::Add {
            temp: "t".into(),
            parent: PartRef::Id(9),
            name: "New area".into(),
            detail: String::new(),
            lifecycle: Lifecycle::Todo,
            anchors: vec![],
            kind: Kind::Task,
            detail_md: None,
            sort_order: None,
            source_file: None,
            source_quote: None,
            rationale: None,
        };
        assert_eq!(
            changeset_row(&add, &name, &parent, &describe),
            (DiffRowKind::Add, "＋ New area  ·  under Engine".to_string())
        );

        // Move = current parent → new parent breadcrumb.
        let mv = DiffOp::Move {
            id: 2,
            parent: PartRef::Root,
            sort_order: 1.0,
        };
        assert_eq!(
            changeset_row(&mv, &name, &parent, &describe),
            (DiffRowKind::Move, "Store:  Tech  →  top level".to_string())
        );

        // Remove names the subject; the row styles it struck.
        let rm = DiffOp::Remove { id: 1 };
        assert_eq!(
            changeset_row(&rm, &name, &parent, &describe),
            (DiffRowKind::Remove, "－ Tech".to_string())
        );

        // Change-class ops defer their text to `describe` (reuses describe_op).
        let st = DiffOp::SetStatus {
            id: 2,
            lifecycle: Lifecycle::Done,
            source: orchestrator_store::StatusSource::User,
        };
        assert_eq!(
            changeset_row(&st, &name, &parent, &describe),
            (DiffRowKind::Change, "Store → done".to_string())
        );

        // Same-name Rename rows are detail updates, not "rename Store → Store".
        let same_name_detail = DiffOp::Rename {
            id: 2,
            name: "Store".into(),
            detail: "new detail".into(),
        };
        let describe_detail = |op: &DiffOp| match op {
            DiffOp::Rename {
                id,
                name: next_name,
                detail,
            } if next_name == &name(*id) && !detail.is_empty() => {
                format!("update detail of {}", name(*id))
            }
            _ => "fallback".to_string(),
        };
        assert_eq!(
            changeset_row(&same_name_detail, &name, &parent, &describe_detail),
            (DiffRowKind::Change, "update detail of Store".to_string())
        );
    }

    #[test]
    fn provenance_user_session_and_passthrough() {
        let now = 1_000_000;
        assert_eq!(provenance("user", now - 120, now), "— you · 2m");
        // long cli id truncates to 8; short ids survive whole.
        assert_eq!(
            provenance("sess-0123456789abcdef", now - 3600 * 18, now),
            "— session 01234567 · 18h"
        );
        assert_eq!(provenance("sess-a3f2", now, now), "— session a3f2 · now");
        // unknown sources pass through — never shown as "you".
        assert_eq!(provenance("agent", now - 86_400 * 3, now), "— agent · 3d");
        // a clock skewed into the future must not underflow.
        assert_eq!(provenance("user", now + 999, now), "— you · now");
    }

    #[test]
    fn overflow_only_past_cap() {
        assert_eq!(overflow_label(8, 8), None);
        assert_eq!(overflow_label(0, 8), None);
        assert_eq!(
            overflow_label(14, 8).as_deref(),
            Some("full log · 14 entries")
        );
    }

    #[test]
    fn chips_decisions_shout_unknown_degrades_to_note() {
        assert_eq!(kind_chip("decision"), ("DECIDED", AMBER));
        assert_eq!(kind_chip("note"), ("note", MUTED));
        assert_eq!(kind_chip("context"), ("context", MUTED2));
        assert_eq!(kind_chip("session"), ("SESSION", SESSION_CYAN));
        assert_eq!(kind_chip("wat"), ("note", MUTED));
    }

    #[test]
    fn id8_truncates_long_ids_only() {
        assert_eq!(id8("0123456789abcdef"), "01234567");
        assert_eq!(id8("a3f2"), "a3f2");
        assert_eq!(id8(""), "");
    }

    #[test]
    fn link_label_chevron_flips_with_open_state() {
        assert_eq!(link_row_label(false), "link a session ▸");
        assert_eq!(link_row_label(true), "link a session ▾");
    }

    #[test]
    fn glyph_and_tag_stale_overrides() {
        assert_eq!(glyph(Lifecycle::Done, false), ("●", GREEN));
        assert_eq!(glyph(Lifecycle::Building, false), ("◐", AMBER));
        assert_eq!(glyph(Lifecycle::Todo, false), ("○", MUTED));
        assert_eq!(glyph(Lifecycle::Idea, false), ("·", MUTED2));
        // stale wins over any asserted lifecycle.
        assert_eq!(glyph(Lifecycle::Done, true), ("◌", AMBER));
        assert_eq!(state_tag(Lifecycle::Done, true), "done · unverified");
        assert_eq!(state_tag(Lifecycle::Building, false), "building");
    }

    #[test]
    fn lifecycle_cycles_user_settable_states_only() {
        // the ONE canonical cycle (docs/019): idea → todo → done → idea;
        // building is derived from live sessions and unreachable by hand.
        assert_eq!(next_lifecycle(Lifecycle::Idea), Lifecycle::Todo);
        assert_eq!(
            next_lifecycle(Lifecycle::Todo),
            Lifecycle::Done,
            "building is derived-only — never hand-set"
        );
        assert_eq!(
            next_lifecycle(Lifecycle::Building),
            Lifecycle::Done,
            "legacy stored rows advance out"
        );
        assert_eq!(next_lifecycle(Lifecycle::Done), Lifecycle::Idea);
    }

    #[test]
    fn ratio_label_hides_zero_denominator() {
        assert_eq!(ratio_label(0, 0), None); // all-idea subtree: no fake "0 of 0"
        assert_eq!(ratio_label(3, 7).as_deref(), Some("3 of 7 built"));
    }

    #[test]
    fn log_orders_newest_first_id_breaks_ties() {
        let notes = vec![
            note(1, 10, "note", "user"),
            note(3, 30, "decision", "sess-x"),
            note(2, 30, "note", "user"),
        ];
        let ids: Vec<i64> = newest_first(&notes).iter().map(|n| n.id).collect();
        assert_eq!(ids, vec![3, 2, 1]);
    }

    fn srow(cli: &str, live: bool, touched: bool) -> SessionRow {
        SessionRow {
            cli_id: cli.into(),
            label: cli.into(),
            live,
            phase: String::new(),
            headline: String::new(),
            touched,
        }
    }

    #[test]
    fn sessions_order_live_ended_touched_stable_within_groups() {
        // touched outranks liveness (a live touch row still whispers last);
        // store order survives inside each group.
        let rows = vec![
            srow("t-live", true, true),
            srow("e1", false, false),
            srow("l1", true, false),
            srow("e2", false, false),
            srow("l2", true, false),
        ];
        let ids: Vec<&str> = ordered_sessions(&rows)
            .iter()
            .map(|r| r.cli_id.as_str())
            .collect();
        assert_eq!(ids, vec!["l1", "l2", "e1", "e2", "t-live"]);
    }

    #[test]
    fn phase_dot_known_words_unknown_stays_neutral() {
        assert_eq!(phase_dot("working"), ("●", AMBER));
        assert_eq!(phase_dot("your turn"), ("●", GREEN));
        assert_eq!(phase_dot("needs you"), ("⚠", AMBER));
        // an unrecognized phase must not masquerade as a known state.
        assert_eq!(phase_dot("compiling"), ("●", MUTED2));
    }

    #[test]
    fn break_down_label_flips_while_in_flight() {
        assert_eq!(break_down_label(false), "◇ break down");
        assert_eq!(break_down_label(true), "◇ breaking down…");
    }

    #[test]
    fn kind_chip_loudness_matches_the_glyph_ladder() {
        // docs/019 slice 1b: areas structural-accent, tasks neutral, ideas
        // whisper — kind never shouts over status.
        assert_eq!(kind_color(Kind::Area), ACCENT);
        assert_eq!(kind_color(Kind::Task), MUTED);
        assert_eq!(kind_color(Kind::Idea), MUTED2);
    }
}
