//! GUI terminal surface (docs/013 §2: A+C, drawer ⇄ stage).
//!
//! Renders a `HostedSession`'s styled grid, captures keystrokes and routes
//! them to the session, and draws the session strip. The GUI owns ONLY the
//! `gpui::Keystroke → KeyInput` translation; all plumbing lives in
//! orchestrator-host. The same surface is the bottom drawer (peek) and, when
//! expanded, the full stage — one component, two heights.

use std::sync::Arc;

use gpui::*;
use orchestrator_host::emulator::{GridSnapshot, StyleRun};
use orchestrator_host::grid_view;
use orchestrator_host::input::KeyInput;
use orchestrator_host::session::Phase;
use orchestrator_host::{SessionBackend, SessionInfo};

use crate::Orchestrator;

// Focused Dark tokens used by the terminal render (values match main.rs).
const TERM_BG: u32 = 0x0F1116;
const MUTED2: u32 = 0x5C636F;
const AMBER: u32 = 0xE6C07A;
const ACCENT: u32 = 0x62A0D8;
const SELECT_BG: u32 = 0x24364A; // drag-selection row tint (#9 3b)
/// search-match tints (⌘F): dim amber for every hit, cursor-style inversion
/// for the CURRENT one (amber bg + terminal-bg fg — light-gray-on-amber is
/// unreadable, the legibility critique).
pub const SEARCH_BG: u32 = 0x39301C;
pub const SEARCH_CUR_BG: u32 = 0xE6C07A;
const FONT: &str = "Menlo";
const LINE_H: f32 = 16.0;

/// Translate a GPUI keystroke into the neutral host `KeyInput`. Returns `None`
/// for app-reserved chords (⌘…) so the GUI keeps them.
pub fn to_key_input(ks: &Keystroke) -> Option<KeyInput> {
    let m = &ks.modifiers;
    if m.platform {
        return None; // ⌘ stays with the app (⌘T, ⌘K, …)
    }
    let key = ks.key.as_str();
    // ⌃` / ⇧⌃` are app-global (drawer fold / stage) — never leak to the CLI.
    if m.control && (key == "`" || key == "~") {
        return None;
    }
    if m.control && key.len() == 1 {
        let c = key.chars().next().unwrap();
        if c.is_ascii_alphabetic() {
            return Some(KeyInput::Ctrl(c));
        }
    }
    Some(match key {
        "enter" => KeyInput::Enter,
        "tab" if m.shift => KeyInput::ShiftTab,
        "tab" => KeyInput::Tab,
        "backspace" => KeyInput::Backspace,
        "escape" => KeyInput::Escape,
        "up" => KeyInput::Up,
        "down" => KeyInput::Down,
        "left" => KeyInput::Left,
        "right" => KeyInput::Right,
        "home" => KeyInput::Home,
        "end" => KeyInput::End,
        "pageup" => KeyInput::PageUp,
        "pagedown" => KeyInput::PageDown,
        "delete" => KeyInput::Delete,
        "space" => KeyInput::Char(" ".into()),
        _ => {
            // Prefer the IME-resolved character (handles shift/option layouts).
            if let Some(ch) = &ks.key_char {
                if !ch.is_empty() {
                    return Some(KeyInput::Char(ch.clone()));
                }
            }
            if key.chars().count() == 1 {
                KeyInput::Char(key.to_string())
            } else {
                return None;
            }
        }
    })
}

/// Status-dot color. The reserved accent (#7EE2C0) is NEVER used here — it
/// belongs only to the one recommended action (docs/013 §2). The status-dot
/// semantics: GREEN = idle/awaiting input ("come drive me"), ORANGE = working
/// (ambient, needs nothing), AMBER ⚠ = blocked on a decision.
pub fn phase_dot(p: Phase) -> u32 {
    match p {
        Phase::AwaitingDecision => AMBER,
        Phase::Busy => 0xE08A4E,
        Phase::Idle => 0x5BB99B,
        Phase::Spawning => MUTED2,
        Phase::Dead => 0xE68A8A,
    }
}

/// The session's name — its title, or the CLI kind as a fallback. No leading
/// glyph: callers show state with a status dot (shape=noise, color=state).
pub fn session_label(s: &SessionInfo) -> String {
    if s.title.is_empty() {
        s.kind.label().to_string()
    } else {
        trim(&s.title, 24).to_string()
    }
}

/// One styled grid row → a flex line of styled spans. Cursor cell is inverted.
// Mouse handling is NO LONGER here — it moved to ONE canvas-level pixel→cell
// handler (render_agent_stage), so a 60-row grid drops ~180 per-frame listeners
// to 3 (#9 char-selection). `hl` is the selected CHAR range [a,b) for this row.
#[allow(clippy::too_many_arguments)]
fn render_row(
    runs: &[StyleRun],
    row_idx: usize,
    cursor: (usize, usize),
    cursor_visible: bool,
    links: &[Option<String>],
    marks: &[(usize, usize, u32)],
    cx: &mut Context<Orchestrator>,
) -> Div {
    let mut line = div().flex().flex_row().h(px(LINE_H)).items_center();
    if runs.is_empty() {
        return line.child(div().w(px(1.)).h(px(LINE_H)));
    }
    let cursor_here = cursor_visible && cursor.0 == row_idx;
    let mut col = 0usize;
    for (run_idx, run) in runs.iter().enumerate() {
        let len = run.text.chars().count();
        let run_end = col + len;
        if let Some(url) = links.get(run_idx).and_then(|o| o.as_ref()) {
            line = line.child(link_span(run, url, row_idx, run_idx, marks, col, run_end, cx));
            col = run_end;
            continue;
        }
        // highlight marks (selection AND search — ONE splitter, #⌘F DRY)
        // intersecting this run → split into pieces; the CURRENT search match
        // inverts fg like the cursor does (legibility on amber).
        let pieces = grid_view::run_marks(marks, col, run_end);
        if pieces.len() > 1 || pieces.first().is_some_and(|p| p.2.is_some()) {
            let chars: Vec<char> = run.text.chars().collect();
            for (ps, pe, bg) in pieces {
                let txt: String = chars
                    .get(ps..pe)
                    .map(|c| c.iter().collect())
                    .unwrap_or_default();
                if txt.is_empty() {
                    continue;
                }
                line = line.child(match bg {
                    Some(b) if b == SEARCH_CUR_BG => {
                        span(&txt, run).bg(rgb(b)).text_color(rgb(TERM_BG))
                    }
                    Some(b) => span(&txt, run).bg(rgb(b)),
                    None => span(&txt, run),
                });
            }
            col = run_end;
            continue;
        }
        // Split the run if the cursor falls inside it.
        if cursor_here && cursor.1 >= col && cursor.1 < col + len {
            let rel = cursor.1 - col;
            let chars: Vec<char> = run.text.chars().collect();
            let before: String = chars[..rel].iter().collect();
            let at: String = chars[rel..rel + 1].iter().collect();
            let after: String = chars[rel + 1..].iter().collect();
            if !before.is_empty() {
                line = line.child(span(&before, run));
            }
            line = line.child(div().bg(rgb(run.fg)).text_color(rgb(TERM_BG)).child(
                SharedString::from(if at == " " { " ".to_string() } else { at }),
            ));
            if !after.is_empty() {
                line = line.child(span(&after, run));
            }
        } else {
            line = line.child(span(&run.text, run));
        }
        col += len;
    }
    line
}

fn span(text: &str, run: &StyleRun) -> Div {
    // one heap alloc per run (the Arc<str>) instead of two — `text.to_string()`
    // added a throwaway String before the SharedString conversion (perf audit #9).
    let mut d = div()
        .text_color(rgb(run.fg))
        .child(SharedString::from(std::sync::Arc::<str>::from(text)));
    if run.bg != orchestrator_host::color::DEFAULT_BG {
        d = d.bg(rgb(run.bg));
    }
    if run.bold {
        d = d.font_weight(FontWeight::BOLD);
    }
    if run.italic {
        d = d.italic();
    }
    if run.underline {
        d = d.underline();
    }
    d
}

/// A clickable URL span (#9 3b): underlined, recolors to ACCENT on hover, and is
/// split by the row's selection/search `marks` so a selected sub-range TINTS like
/// any other run — the link used to bypass the mark splitter entirely, so dragging
/// over a URL showed no highlight (looked unselectable). Pieces omit their own fg
/// so they INHERIT the wrapper's color (→ hover recolors the whole link). ⌘-click
/// opens via this element; a plain click opens via the window mouse-up handler
/// (which resolves the cell → `grid_view::link_at`), so opening never depends on
/// this element's hitbox winning over the canvas overlay.
#[allow(clippy::too_many_arguments)]
fn link_span(
    run: &StyleRun,
    url: &str,
    row_idx: usize,
    run_idx: usize,
    marks: &[(usize, usize, u32)],
    col: usize,
    run_end: usize,
    cx: &mut Context<Orchestrator>,
) -> impl IntoElement {
    let u = url.to_string();
    let chars: Vec<char> = run.text.chars().collect();
    let mut wrap = div()
        .flex()
        .flex_row()
        .id(SharedString::from(format!("lnk-{row_idx}-{run_idx}")))
        .text_color(rgb(run.fg))
        .cursor_pointer()
        .hover(|h| h.text_color(rgb(ACCENT)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |_this, e: &MouseDownEvent, _w, cx| {
                if e.modifiers.secondary() {
                    cx.open_url(&u);
                    cx.stop_propagation(); // don't also start a row drag-select
                }
            }),
        );
    for (ps, pe, bg) in grid_view::run_marks(marks, col, run_end) {
        let txt: String = chars
            .get(ps..pe)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        if txt.is_empty() {
            continue;
        }
        let mut piece = div().underline().child(SharedString::from(txt));
        match bg {
            // current search match inverts fg like the cursor does (legible on amber)
            Some(b) if b == SEARCH_CUR_BG => piece = piece.bg(rgb(b)).text_color(rgb(TERM_BG)),
            Some(b) => piece = piece.bg(rgb(b)),
            None if run.bg != orchestrator_host::color::DEFAULT_BG => piece = piece.bg(rgb(run.bg)),
            None => {}
        }
        if run.bold {
            piece = piece.font_weight(FontWeight::BOLD);
        }
        if run.italic {
            piece = piece.italic();
        }
        wrap = wrap.child(piece);
    }
    wrap
}

/// The grid render — fixed-width font, one div per row. `rows_visible` clamps how
/// many bottom rows show. Rows carry drag-select handlers + clickable link spans
/// (#9 3b); `sel` tints the selected range.
/// Per-viewport-row search ranges to paint: (row index in snap.rows, start
/// char, end char, is_current). Verified against the row text before painting
/// (matches drift as live output appends — the self-healing rule, critique).
pub type SearchPaint = Vec<(usize, usize, usize, bool)>;

pub fn render_grid(
    snap: &GridSnapshot,
    rows_visible: usize,
    sel: Option<((usize, usize), (usize, usize))>,
    search: &SearchPaint,
    query: &str,
    cx: &mut Context<Orchestrator>,
) -> impl IntoElement {
    let total = snap.rows.len();
    let start = total.saturating_sub(rows_visible);
    let plains = snap.plain_lines();
    let mut col = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .overflow_hidden()
        .bg(rgb(TERM_BG))
        .px(px(12.))
        .py(px(8.))
        .font_family(FONT)
        .text_size(px(13.5))
        // pin the line box to the row height — else gpui uses the font's larger
        // default line-height (~17-18px), which overflows the 16px row and shaves
        // the tops/bottoms of glyphs ("half-displayed" text).
        .line_height(px(LINE_H))
        .cursor(CursorStyle::IBeam); // text/I-beam cursor over the terminal (#9)
    let q: Vec<char> = query.chars().collect();
    for (i, runs) in snap.rows.iter().enumerate().skip(start) {
        let plain = plains.get(i).map(|s| s.as_str()).unwrap_or("");
        let links = grid_view::row_links(runs, plain);
        let hl = grid_view::row_highlight(sel, i, plain.chars().count());
        // verify-before-paint: only ranges whose text STILL matches the query
        // (ascii-ci) are painted — stale anchors self-heal instead of smearing.
        let chars: Vec<char> = plain.chars().collect();
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut current: Option<usize> = None;
        for &(_row, a, b, is_cur) in search.iter().filter(|&&(row, ..)| row == i) {
            let ok = !q.is_empty()
                && b <= chars.len()
                && b - a == q.len()
                && chars[a..b]
                    .iter()
                    .zip(&q)
                    .all(|(c, qc)| c.eq_ignore_ascii_case(qc));
            if ok {
                if is_cur {
                    current = Some(ranges.len());
                }
                ranges.push((a, b));
            }
        }
        let marks = grid_view::row_marks(hl, SELECT_BG, &ranges, SEARCH_BG, current, SEARCH_CUR_BG);
        col = col.child(render_row(
            runs,
            i,
            snap.cursor,
            snap.cursor_visible,
            &links,
            &marks,
            cx,
        ));
    }
    col
}

/// The terminal scrollbar (#9 slice 3): a thumb showing how far up into scrollback
/// you are (top = oldest, bottom = live), DRAGGABLE (step 5 — grab it to scroll).
/// `None` when there's no history. Overlay it on a `relative()` grid container
/// that also carries the on_mouse_move/up that drives the drag.
pub fn render_scrollbar(
    snap: &GridSnapshot,
    visible_rows: usize,
    cx: &mut Context<Orchestrator>,
) -> Option<Div> {
    let hist = snap.history_size as f32;
    if hist < 1.0 {
        return None;
    }
    let vis = visible_rows.max(1) as f32;
    let total = hist + vis;
    let off = snap.display_offset as f32;
    let thumb_h = (vis / total).clamp(0.07, 1.0);
    let thumb_top = ((hist - off) / total).clamp(0.0, 1.0 - thumb_h);
    let off0 = snap.display_offset;
    Some(
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .right_0()
            .w(px(9.))
            .flex()
            .flex_col()
            .child(
                div()
                    .id("term-scrollbar-thumb")
                    .absolute()
                    .left(px(2.))
                    .right(px(2.))
                    .top(gpui::relative(thumb_top))
                    .h(gpui::relative(thumb_h))
                    .rounded(px(3.))
                    .cursor_pointer()
                    .bg(rgb(if off > 0.0 { 0x4a5662 } else { 0x333d47 }))
                    .hover(|h| h.bg(rgb(0x5a6776)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, e: &MouseDownEvent, _w, cx| {
                            this.scrollbar_drag = Some(crate::ScrollDragAnchor {
                                grab_y: f32::from(e.position.y),
                                off0,
                            });
                            cx.notify();
                        }),
                    ),
            ),
    )
}

pub fn trim(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

/// Drive coalesced repaints while any session is producing output. Spawns one
/// async task bound to the view; ticks ~every 16ms and notifies only when a
/// session's dirty counter (or alive set) changed (no busy-repaint when idle).
/// The view-liveness check runs EVERY tick so the loop — and the `Arc` it holds
/// (which keeps live CLI children alive) — is released the moment the view is
/// dropped, even during a fully idle period.
pub fn drive_repaints<T: 'static>(host: Arc<dyn SessionBackend>, cx: &mut Context<T>) {
    cx.spawn(async move |this, cx| {
        let mut last: u64 = 0;
        loop {
            Timer::after(std::time::Duration::from_millis(16)).await;
            // clear decision cards whose dialog the human already dismissed in
            // the terminal (rejection is hook-invisible — docs/014).
            host.reconcile_pending();
            // signature folds output activity AND liveness transitions so a
            // process exit (no further output) still triggers one repaint.
            // fold PHASE in too, so a time-based busy→idle transition (output
            // stopped → no longer "recently working") triggers one repaint even
            // with no new dirty bytes.
            let sig: u64 = host
                .infos()
                .iter()
                .map(|s| {
                    s.dirty
                        .wrapping_add(if s.alive { 0 } else { 1 << 40 })
                        .wrapping_add((s.phase as u64) << 44)
                })
                .fold(0u64, |a, x| a.wrapping_add(x));
            let changed = sig != last;
            last = sig;
            // checked unconditionally: if the view is gone, break and drop Arc.
            if this
                .update(cx, |_, cx| {
                    if changed {
                        cx.notify();
                    }
                })
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
}
