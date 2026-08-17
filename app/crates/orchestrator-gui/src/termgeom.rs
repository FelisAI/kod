//! Terminal geometry: how many rows/cols the hosted PTY gets, and the scrollback
//! gestures that ride the same cell metrics.
//!
//! One module because these numbers are a single closed system — the cell
//! metrics, the chrome the stage spends above the grid, the banner that steals
//! rows, the reflow that pushes the result at the PTY, and the wheel/scrollbar
//! math that reads the same `LINE_PX`. Splitting them across files is exactly how
//! the box the grid gets and the rows the PTY emits drifted apart before (see
//! `pty_rows`), so they stay together with their proof.

use gpui::*;

use crate::*;

// Terminal geometry — the PTY is sized to match what the drawer/stage shows so
// the hosted CLI lays out within the visible viewport (docs/013 §2).
pub(crate) const TERM_COLS: u16 = 110;
pub(crate) const STAGE_ROWS: u16 = 38;
/// Menlo 12.5px cell metrics — used to size the PTY rows/cols to the viewport
/// when the terminal is expanded (so it reflows with the window, #9).
pub(crate) const LINE_PX: f32 = 16.0;
pub(crate) const CHAR_PX: f32 = 8.3;
/// Pointer travel (px) that separates a click from a drag for terminal link-open:
/// below this the press counts as a click; at/above it a drag is latched. Smaller
/// than one cell (`CHAR_PX`), so click jitter still opens while a real drag never
/// does.
pub(crate) const CLICK_SLOP_PX: f32 = 4.0;
/// Window height MINUS the app chrome above/below the terminal box (header,
/// subhead, padding) — i.e. the stage the grid gets when NOTHING rides above it.
const STAGE_CHROME_PX: f32 = 210.0;
/// The one-line decision banner's EXACT laid-out height (`decision_banner` pins
/// it with `.h()`, no border and no margin, so this is its full outer box).
///
/// Deliberately an exact multiple of `LINE_PX`: the banner then costs the
/// terminal exactly 2 rows at EVERY window height, with no floor-rounding drift
/// between the box the grid gets and the rows the PTY emits into it.
pub(crate) const DECISION_BANNER_PX: f32 = 2.0 * LINE_PX;

/// The PTY row budget for the agent stage.
///
/// The decision banner rides ABOVE the grid in the SAME flex_col, so the rows the
/// PTY emits MUST be reduced by its height. If they aren't, the PTY keeps
/// emitting the full window's worth of rows while its box shrinks — and since the
/// grid is `overflow_hidden` + top-aligned with rows pinned at `LINE_PX`, the
/// surplus rows are MASKED OFF THE BOTTOM (not compressed, and not reachable by
/// scrolling). claude draws its `❯ 1. Yes / 2. No` dialog at the BOTTOM of the
/// viewport — exactly that masked band. This is what killed the old fat decision
/// card: it hid the very dialog it told you to answer. Pure so the math is
/// unit-tested; `banner_px` is 0.0 when nothing is pending.
pub(crate) fn pty_rows(win_h: f32, banner_px: f32) -> u16 {
    (((win_h - STAGE_CHROME_PX - banner_px) / LINE_PX).floor() as i32).clamp(8, 300) as u16
}

/// A live scrollbar thumb-drag (#9 3b step 5): cursor Y + display_offset + the
/// track height, captured when the grab began. Used by `grid_view::scrollbar_delta`.
#[derive(Clone, Copy)]
pub(crate) struct ScrollDragAnchor {
    pub grab_y: f32,
    pub off0: u32,
}

impl Orchestrator {
    /// Reflow the active session to the current window/drawer geometry. Called
    /// from render()'s head (where &mut self + window are available) and fires
    /// `host.resize` ONLY when the target dims changed — so the render tree
    /// itself stays side-effect-free and the PTY isn't ioctl'd every frame.
    ///
    /// The row budget also subtracts the DECISION BANNER, because the banner and
    /// the grid share one flex_col: rows the banner's height displaces are
    /// MASKED, not compressed — and the masked band is exactly where claude draws
    /// the `1. Yes / 2. No` dialog the banner points at. The condition here is the
    /// same one `render_agent_stage` renders the banner on (`host.pending` for the
    /// session ON SCREEN), so the subtraction can never disagree with the layout.
    ///
    /// No feedback loop: `DECISION_BANNER_PX` is a CONSTANT (the banner is one
    /// pinned-height line, never a mirror of the diff), so it depends on neither
    /// the window nor the PTY's row count — unlike the fat card it replaced, whose
    /// height grew with the payload. `last_resize` dedups and reflow runs per-frame
    /// from render, so the banner appearing and clearing each re-reflow exactly
    /// once, and the answer-in-terminal that clears the decision hands the two rows
    /// straight back.
    pub(crate) fn reflow_terminal(&mut self, window: &Window) {
        if self.screen != Screen::Workspace || self.mode != Mode::Agent {
            return;
        }
        let Some(id) = self.active_session_id() else {
            return;
        };
        let banner_px = if self.host.pending(id).is_some() {
            DECISION_BANNER_PX
        } else {
            0.0
        };
        let win_w = f32::from(window.viewport_size().width);
        let win_h = f32::from(window.viewport_size().height);
        // the Agent stage fills the main area (just the app sidebar to its left,
        // no in-stage list), so rows AND cols follow the viewport — the terminal
        // reflows when you resize the window (#9 slice 2).
        let rows = pty_rows(win_h, banner_px);
        // The rail is user-resizable (#52), so its LIVE width drives the column
        // count — a hard-coded 214 would mis-size every terminal once it moved.
        // `last_resize` keys on cols, so dragging the rail reflows the PTY.
        let cols =
            (((win_w - self.sidebar_w - 28.0) / CHAR_PX).floor() as i32).clamp(40, 400) as u16;
        let target = (id.0, rows, cols);
        if self.last_resize != Some(target) {
            self.last_resize = Some(target);
            self.host.resize(id, rows, cols);
        }
    }
}

/// Scroll-wheel → scroll the active session's terminal through scrollback (#9).
/// Shared by the peek drawer and the expanded Sessions terminal.
pub(crate) fn scroll_terminal(
    this: &mut Orchestrator,
    ev: &ScrollWheelEvent,
    _w: &mut Window,
    cx: &mut Context<Orchestrator>,
) {
    if let Some(id) = this.active_session_id() {
        let lines = match ev.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => f32::from(p.y) / LINE_PX,
        };
        let n = lines.round() as i32;
        if n != 0 {
            this.host.scroll(id, n);
            this.clear_term_selection(); // selection is viewport-relative — drop on scroll
            cx.notify();
        }
    }
}

#[cfg(test)]
mod reflow_tests {
    // NOTE: no `use super::*` — gpui is glob-imported at the top of this file.
    use super::{pty_rows, DECISION_BANNER_PX, LINE_PX, STAGE_CHROME_PX};
    use crate::empty_project;

    // Regression (review HIGH): a fresh OSS user has an EMPTY portfolio
    // (seed_projects() == []), so project() must fall back to this sentinel rather
    // than index-panicking on the first render frame. Guards that the sentinel
    // initializes (Project::idea doesn't panic) and is a stable singleton.
    #[test]
    fn empty_project_sentinel_is_safe() {
        let p = empty_project();
        assert!(!p.name.is_empty(), "sentinel must be renderable, not a crash");
        assert!(
            std::ptr::eq(p, empty_project()),
            "sentinel is a stable OnceLock singleton"
        );
    }

    /// How many rows the grid's BOX can actually SHOW, given what rides above it.
    /// The grid is `overflow_hidden`, top-aligned, rows pinned at `LINE_PX` — so
    /// any row the PTY emits beyond this is not compressed and not scrollable to:
    /// it is painted under the mask, off the bottom.
    fn rows_the_box_can_show(win_h: f32, banner_px: f32) -> u16 {
        ((win_h - STAGE_CHROME_PX - banner_px) / LINE_PX).floor().max(0.0) as u16
    }

    #[test]
    fn the_banner_takes_its_rows_from_the_pty_instead_of_masking_them() {
        // THE BUG THIS FIXES: the row budget was computed from the WINDOW alone
        // (`(win_h - 210) / 16`), so a decision riding above the grid shrank the
        // grid's BOX while the PTY kept emitting the full count. The surplus rows
        // were masked off the BOTTOM — exactly where claude draws `❯ 1. Yes / 2.
        // No`. The affordance hid the dialog it pointed at.
        //
        // DECISION_BANNER_PX is an exact multiple of LINE_PX, so the terminal gives
        // up EXACTLY the rows the banner occupies — 2 — at every window height, with
        // no floor-rounding drift in either direction.
        assert_eq!(DECISION_BANNER_PX % LINE_PX, 0.0, "the banner must be a whole number of rows");
        let cost = (DECISION_BANNER_PX / LINE_PX) as u16;
        assert_eq!(cost, 2);

        for win in [700.0_f32, 760.0, 820.0, 900.0, 1013.0, 1200.0, 1440.0] {
            let bare = pty_rows(win, 0.0);
            let with = pty_rows(win, DECISION_BANNER_PX);
            // 1. the terminal gives the banner its rows back — it does not keep them.
            assert_eq!(
                with,
                bare - cost,
                "win {win}: banner must cost EXACTLY {cost} rows (bare {bare}, with {with})",
            );
            // 2. and the proof that nothing is masked: the rows the PTY emits fit
            //    INSIDE the box the grid is left with. The last row the CLI draws —
            //    the `1. Yes / 2. No` prompt — lands on a visible line.
            assert!(
                with <= rows_the_box_can_show(win, DECISION_BANNER_PX),
                "win {win}: PTY emits {with} rows into a box that can only show {} — the bottom \
                 of the dialog is masked",
                rows_the_box_can_show(win, DECISION_BANNER_PX),
            );
            // 3. the no-banner case is UNCHANGED (this is not a regression on the
            //    normal terminal) and also fits.
            assert_eq!(bare, rows_the_box_can_show(win, 0.0));
        }

        // the default window: 38 rows bare, 36 with the banner up.
        assert_eq!(pty_rows(820.0, 0.0), 38);
        assert_eq!(pty_rows(820.0, DECISION_BANNER_PX), 36);

        // the clamps still hold at the extremes — a banner can never drive the PTY
        // to zero rows, and a huge window is still capped. (Below ~370px the min-8
        // clamp outruns the box either way, banner or not: pre-existing, and far
        // under any window this app can be dragged to.)
        assert_eq!(pty_rows(300.0, DECISION_BANNER_PX), 8);
        assert_eq!(pty_rows(9000.0, DECISION_BANNER_PX), 300);
        assert!(pty_rows(370.0, DECISION_BANNER_PX) <= rows_the_box_can_show(370.0, DECISION_BANNER_PX));
    }
}
