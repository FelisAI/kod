//! The per-session emulator (docs/013 §1 emulator.rs).
//!
//! Wraps the pinned alacritty fork's `Term` and FIXES the bug the parked
//! prototype shipped: `Term` composes CPR/DA1/XTVERSION/kitty replies
//! internally and emits them as `Event::PtyWrite` — drop them and claude's
//! terminal queries go unanswered (the known TUI-degrading bug class,
//! docs/012 §2). Here every event is captured and surfaced as a typed
//! [`EmuEvent`]; the PTY owner writes `PtyReply` bytes back.
//!
//! One emulator instance serves two consumers (012 §2): the GUI renders
//! [`GridSnapshot`]s; the anchor detector reads the same snapshots. Snapshots
//! are frame-coherent: `Processor` buffers synchronized-output (CSI ?2026)
//! internally, so the grid never shows a torn frame.

use std::sync::{Arc, Mutex};

use alacritty_terminal::{
    event::{Event, EventListener, WindowSize},
    grid::{Dimensions, Scroll},
    index::{Column, Direction, Line, Point},
    term::{cell::Flags, search::{RegexIter, RegexSearch}, Config, TermMode},
    vte::ansi::{Processor, Rgb, StdSyncHandler},
    Term,
};
use serde::{Deserialize, Serialize};

use crate::color;
use crate::input::ModeFlags;

// Focused Dark answers for OSC color queries (index 10 = fg, 11 = bg) — the
// app IS the terminal, so it reports its real palette.
const FG: Rgb = Rgb { r: 0xC7, g: 0xCB, b: 0xD4 };
const BG: Rgb = Rgb { r: 0x14, g: 0x16, b: 0x1B };

/// Typed emulator output the session loop consumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EmuEvent {
    /// Bytes the terminal must send back to the application (CPR, DA1,
    /// kitty-keyboard reports, color-query answers). Write them to the PTY.
    PtyReply(Vec<u8>),
    /// OSC 0/2 window title — claude's live activity channel (spinner/✳).
    Title(String),
    ResetTitle,
    Bell,
}

/// A run of cells sharing one style — the GUI render unit (run-merging keeps
/// per-cell styling cheap, the perf mitigation from docs/013 §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StyleRun {
    pub text: String,
    pub fg: u32,
    pub bg: u32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// OSC-8 hyperlink target for this run, if any (#9 3b step 4). Runs break at
    /// hyperlink boundaries so a clickable span is exactly the linked text.
    pub uri: Option<String>,
}

/// A frame-coherent, render-ready view of the terminal. Plain data: the GUI,
/// the detector, and (later) the daemon wire all consume exactly this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridSnapshot {
    /// Monotonic per-advance sequence — consumers detect staleness with it
    /// (the re-verify-before-inject rule keys on seq, 012 §1.5).
    pub seq: u64,
    /// Styled rows for rendering. Anchor matching uses [`plain_lines`].
    pub rows: Vec<Vec<StyleRun>>,
    /// (row, col) within the visible viewport.
    pub cursor: (usize, usize),
    pub cursor_visible: bool,
    pub bracketed_paste: bool,
    pub kitty_keys: bool,
    /// scrollback view state for the GUI scrollbar (#9 slice 3): how far up we're
    /// scrolled (0 = at the live bottom) and how many lines of history exist.
    pub display_offset: u32,
    pub history_size: u32,
    /// alternate screen active (vim/htop/fullscreen TUIs) — scroll-to-match
    /// must be a no-op there (view scroll would otherwise become app input).
    pub alt_screen: bool,
}

/// Terminal modes that decide where a wheel-scroll goes (host-side only — read
/// by `Session::scroll`, never crosses the wire).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScrollMode {
    /// alternate screen active (vim/htop/claude-fullscreen) → scrollback is empty.
    pub alt_screen: bool,
    /// app enabled mouse reporting → forward the wheel as mouse-wheel events.
    pub mouse: bool,
    /// SGR mouse encoding (vs legacy X10).
    pub sgr: bool,
    /// application cursor keys (DECCKM) → arrows are `ESC O A/B` not `ESC [ A/B`.
    pub app_cursor: bool,
    /// alternate-scroll enabled (default on) → translate the wheel to arrow keys.
    pub alt_scroll: bool,
}

impl GridSnapshot {
    /// Plain text per row — the WHEN/anchor layer (detector) reads this; the
    /// GUI render path reads `rows`.
    pub fn plain_lines(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|runs| runs.iter().map(|r| r.text.as_str()).collect::<String>())
            .map(|mut s| {
                while s.ends_with(' ') {
                    s.pop();
                }
                s
            })
            .collect()
    }

    /// Convenience for anchor checks: does any visible line contain `needle`?
    pub fn contains(&self, needle: &str) -> bool {
        self.plain_lines().iter().any(|l| l.contains(needle))
    }
}

#[derive(Clone)]
struct Proxy {
    events: Arc<Mutex<Vec<Event>>>,
}

impl EventListener for Proxy {
    fn send_event(&self, event: Event) {
        if let Ok(mut g) = self.events.lock() {
            g.push(event);
        }
    }
}

struct Dims {
    rows: usize,
    cols: usize,
}
impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

pub struct Emulator {
    term: Term<Proxy>,
    proc: Processor<StdSyncHandler>,
    events: Arc<Mutex<Vec<Event>>>,
    seq: u64,
    rows: usize,
    cols: usize,
}

impl Emulator {
    pub fn new(rows: u16, cols: u16) -> Self {
        let events = Arc::new(Mutex::new(Vec::new()));
        let config = Config { scrolling_history: 2000, ..Default::default() };
        let term = Term::new(
            config,
            &Dims { rows: rows as usize, cols: cols as usize },
            Proxy { events: events.clone() },
        );
        Emulator {
            term,
            proc: Processor::new(),
            events,
            seq: 0,
            rows: rows as usize,
            cols: cols as usize,
        }
    }

    /// Feed PTY output. Returns the typed events this chunk produced —
    /// `PtyReply` bytes MUST be written back to the PTY by the caller.
    pub fn advance(&mut self, bytes: &[u8]) -> Vec<EmuEvent> {
        self.proc.advance(&mut self.term, bytes);
        self.seq += 1;
        let drained: Vec<Event> = match self.events.lock() {
            Ok(mut g) => g.drain(..).collect(),
            Err(_) => Vec::new(),
        };
        drained
            .into_iter()
            .filter_map(|e| match e {
                Event::PtyWrite(s) => Some(EmuEvent::PtyReply(s.into_bytes())),
                Event::Title(t) => Some(EmuEvent::Title(t)),
                Event::ResetTitle => Some(EmuEvent::ResetTitle),
                Event::Bell => Some(EmuEvent::Bell),
                Event::ColorRequest(index, format) => {
                    // `index` is alacritty's NamedColor index, NOT the OSC code:
                    // Foreground=256, Background=257, Cursor=258; 0..=255 are
                    // the palette. Answering with the configured palette keeps
                    // claude/codex from hanging on an OSC 10/11/4 query.
                    let c = match index {
                        256 | 258 => FG,
                        257 => BG,
                        i if i < 256 => rgb_of(color::resolve_index(i as u8)),
                        _ => FG,
                    };
                    Some(EmuEvent::PtyReply(format(c).into_bytes()))
                }
                Event::TextAreaSizeRequest(format) => {
                    let ws = WindowSize {
                        num_lines: self.rows as u16,
                        num_cols: self.cols as u16,
                        cell_width: 8,
                        cell_height: 16,
                    };
                    Some(EmuEvent::PtyReply(format(ws).into_bytes()))
                }
                _ => None,
            })
            .collect()
    }

    /// Resize the emulated terminal (paired with the PTY resize so the child
    /// and the grid agree on geometry — docs/013 §4 "PTY + Term::resize
    /// together"). Caller holds the session lock; bump dirty after.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows as usize;
        self.cols = cols as usize;
        self.term.resize(Dims { rows: rows as usize, cols: cols as usize });
    }

    /// The live input-relevant terminal modes, without walking the grid (the
    /// keystroke hot path reads this instead of a full snapshot).
    pub fn mode_flags(&self) -> ModeFlags {
        let m = self.term.mode();
        ModeFlags {
            bracketed_paste: m.contains(TermMode::BRACKETED_PASTE),
            kitty_keys: m.contains(TermMode::DISAMBIGUATE_ESC_CODES),
        }
    }

    /// Terminal modes relevant to SCROLL ROUTING. In the alternate screen (vim /
    /// htop / claude-fullscreen) the scrollback is empty, so the wheel must be
    /// FORWARDED to the app — as mouse-wheel events if it enabled mouse reporting,
    /// else as arrow keys (xterm "alternate scroll"). Read on the wheel hot path.
    pub fn scroll_mode(&self) -> ScrollMode {
        let m = self.term.mode();
        ScrollMode {
            alt_screen: m.contains(TermMode::ALT_SCREEN),
            mouse: m.intersects(TermMode::MOUSE_MODE),
            sgr: m.contains(TermMode::SGR_MOUSE),
            app_cursor: m.contains(TermMode::APP_CURSOR),
            alt_scroll: m.contains(TermMode::ALTERNATE_SCROLL),
        }
    }

    /// The current visible viewport (bottom `rows` lines), frame-coherent,
    /// styled and run-merged.
    /// Scroll the viewport through scrollback (positive = up into history,
    /// negative = down toward the live bottom). The next snapshot reflects it —
    /// no scrollback is copied, the view just moves over the existing grid.
    pub fn scroll(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    /// Jump back to the live bottom (e.g. when the user types).
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// The LIVE tail as plain text: the `n` rows ending at the CURSOR line,
    /// scroll-immune (snapshot() honors the scroll, which would turn a tail
    /// scan into a scrollback scan). Cursor-relative, not grid-bottom: a young
    /// session's content is top-anchored, so the bottom rows are blank and an
    /// on-screen anchor would be missed (review).
    pub fn tail_plain(&self, n: usize) -> String {
        let grid = self.term.grid();
        let bottom = grid.bottommost_line().0;
        let top = grid.topmost_line().0;
        let end = grid.cursor.point.line.0.clamp(top, bottom);
        let first = (end - (n as i32 - 1)).max(top);
        let mut out = String::new();
        for i in first..=end {
            let row = &grid[Line(i)];
            for run in merge_row(row, self.cols) {
                out.push_str(&run.text);
            }
            out.push('\n');
        }
        out
    }

    /// The bottom `n` rows as plain text, anchored to the GRID BOTTOM (not the
    /// cursor) and immune to scroll position — for a persistent FOOTER that
    /// claude paints BELOW the composer/cursor (the usage-limit banner), which
    /// `tail_plain` (cursor-anchored) would miss (docs/019).
    pub fn bottom_plain(&self, n: usize) -> String {
        let grid = self.term.grid();
        let bottom = grid.bottommost_line().0;
        let top = grid.topmost_line().0;
        let first = (bottom - (n as i32 - 1)).max(top);
        let mut out = String::new();
        for i in first..=bottom {
            let row = &grid[Line(i)];
            for run in merge_row(row, self.cols) {
                out.push_str(&run.text);
            }
            out.push('\n');
        }
        out
    }

    pub fn snapshot(&self) -> GridSnapshot {
        let grid = self.term.grid();
        let bottom = grid.bottommost_line().0;
        let top = grid.topmost_line().0;
        let rows = self.rows as i32;
        // honor the scroll position: the displayed window is `display_offset`
        // lines above the live bottom (0 = at the bottom).
        let off = grid.display_offset() as i32;
        let first = bottom - off - (rows - 1);
        let mut out_rows = Vec::with_capacity(self.rows);
        for i in 0..rows {
            let line_idx = first + i;
            if line_idx < top {
                out_rows.push(Vec::new());
                continue;
            }
            let row = &grid[Line(line_idx)];
            out_rows.push(merge_row(row, self.cols));
        }
        let cur = grid.cursor.point;
        // the cursor only shows when it's within the displayed window (when you
        // scroll up into history, it's below the view → hidden).
        let cur_line_rel = cur.line.0 - first;
        let cur_on_screen = cur_line_rel >= 0 && cur_line_rel < rows;
        let cur_row = cur_line_rel.max(0) as usize;
        // `merge_row` skips wide-char spacers, so the rendered run model is in
        // CHARACTER units while `cur.column` is a grid COLUMN. Convert the
        // cursor to a char index by counting non-spacer cells left of it, so
        // the GUI's char-based cursor split lands on the right glyph.
        let cur_char = if cur.line.0 >= top && cur.line.0 <= bottom {
            let row = &grid[Line(cur.line.0)];
            (0..cur.column.0.min(self.cols))
                .filter(|&c| {
                    let f = row[Column(c)].flags;
                    !f.contains(Flags::WIDE_CHAR_SPACER) && !f.contains(Flags::LEADING_WIDE_CHAR_SPACER)
                })
                .count()
        } else {
            cur.column.0
        };
        let mode = self.term.mode();
        GridSnapshot {
            seq: self.seq,
            rows: out_rows,
            cursor: (cur_row, cur_char),
            cursor_visible: cur_on_screen && mode.contains(TermMode::SHOW_CURSOR),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            kitty_keys: mode.contains(TermMode::DISAMBIGUATE_ESC_CODES),
            display_offset: grid.display_offset() as u32,
            history_size: grid.history_size() as u32,
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
        }
    }
}

/// `Rgb` from a `0xRRGGBB` value (for color-query replies).
fn rgb_of(c: u32) -> Rgb {
    Rgb {
        r: ((c >> 16) & 0xff) as u8,
        g: ((c >> 8) & 0xff) as u8,
        b: (c & 0xff) as u8,
    }
}

/// Collapse a grid row into run-merged styled spans (adjacent same-style
/// cells share one `StyleRun`). Wide-char spacers are skipped.
fn merge_row(row: &alacritty_terminal::grid::Row<alacritty_terminal::term::cell::Cell>, cols: usize) -> Vec<StyleRun> {
    let mut runs: Vec<StyleRun> = Vec::new();
    for c in 0..cols {
        let cell = &row[Column(c)];
        // read the cell's flags ONCE — this is the innermost per-tick loop.
        let f = cell.flags;
        if f.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            continue;
        }
        let inverse = f.contains(Flags::INVERSE);
        let dim = f.contains(Flags::DIM) || f.contains(Flags::DIM_BOLD);
        let (fg, bg) = color::resolve_pair(cell.fg, cell.bg, inverse, dim);
        let bold = f.contains(Flags::BOLD) || f.contains(Flags::BOLD_ITALIC);
        let italic = f.contains(Flags::ITALIC) || f.contains(Flags::BOLD_ITALIC);
        let underline = f.intersects(Flags::ALL_UNDERLINES);
        let hidden = f.contains(Flags::HIDDEN);
        let ch = if hidden { ' ' } else { cell.c };
        // OSC-8 hyperlink (#9 3b step 4). cell.hyperlink() is cheap (None) for the
        // common no-link cell; only linked cells clone the Arc-backed uri.
        let uri = cell.hyperlink().map(|h| h.uri().to_string());
        match runs.last_mut() {
            Some(r) if r.fg == fg && r.bg == bg && r.bold == bold && r.italic == italic && r.underline == underline && r.uri == uri => {
                r.text.push(ch);
            }
            _ => runs.push(StyleRun {
                text: { let mut s = String::with_capacity(16); s.push(ch); s },
                fg,
                bg,
                bold,
                italic,
                underline,
                uri,
            }),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(rel: &str) -> Vec<u8> {
        std::fs::read(format!("{}/../../fixtures/{rel}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn snapshot_is_styled_and_run_merged() {
        // SGR: bold bright-green "OK" then reset + plain " go"
        let mut emu = Emulator::new(4, 20);
        emu.advance(b"\x1b[1;92mOK\x1b[0m go");
        let snap = emu.snapshot();
        let row0 = &snap.rows[0];
        // first run is the bold green "OK"
        let ok = row0.iter().find(|r| r.text.starts_with("OK")).unwrap();
        assert!(ok.bold, "OK run should be bold");
        assert_ne!(ok.fg, crate::color::DEFAULT_FG, "bright green != default fg");
        // plain-text projection still works for the detector
        assert_eq!(snap.plain_lines()[0], "OK go");
    }

    /// Replay a recorded stream incrementally; return every anchor that was
    /// ever visible on the grid plus all events. Chunks are small (256B) to
    /// mirror how a live reader sees an interactive TUI draw — a recording
    /// captures the full dialog lifecycle (render → answer → clear) so coarse
    /// chunking can skip the transient blocked-dialog frame the detector
    /// would, in production, see persist.
    fn replay(bytes: &[u8], anchors: &[&str]) -> (Vec<String>, Vec<EmuEvent>) {
        let mut emu = Emulator::new(30, 110);
        let mut seen = Vec::new();
        let mut events = Vec::new();
        for chunk in bytes.chunks(256) {
            events.extend(emu.advance(chunk));
            let snap = emu.snapshot();
            for a in anchors {
                if snap.contains(a) && !seen.iter().any(|s: &String| s == a) {
                    seen.push((*a).to_string());
                }
            }
        }
        (seen, events)
    }

    #[test]
    fn grid_not_stream_write_dialog_regression() {
        // THE pinned regression (013 §3 test #4): the Write permission dialog
        // text is ABSENT from the raw byte stream (cursor-positioned writes
        // mangle it) but PRESENT after emulator replay.
        let bytes = fixture("claude/2.1.172/pty/claude_raw3.log");
        let raw_text = String::from_utf8_lossy(&bytes);
        assert!(
            !raw_text.contains("Do you want"),
            "fixture changed: dialog text now in raw stream?"
        );
        let (seen, _) = replay(&bytes, &["Do you want", "Esc to cancel"]);
        assert!(
            seen.iter().any(|s| s == "Do you want"),
            "Write dialog never appeared on the replayed grid"
        );
    }

    #[test]
    fn usage_limit_parser_warning_hit_and_none() {
        use crate::session::parse_usage_limit;
        // the warning form (leading blanks mimic the column-positioned render).
        let warn = parse_usage_limit(
            "      You've used 92% of your session limit · resets 4:30pm (America/Los_Angeles) · /upgrade to keep using Claude Code",
            7,
        )
        .expect("warning banner should parse");
        assert!(!warn.hit, "a used-N% line is a warning, not a hard hit");
        assert_eq!(warn.percent, Some(92));
        assert_eq!(warn.reset_clock, "4:30pm");
        assert_eq!(warn.reset_tz, "America/Los_Angeles");
        assert_eq!(warn.since_ms, 7);

        // the hard-block form (what actually kills a turn) — no percent.
        let hit = parse_usage_limit("You've hit your session limit · resets 4:30pm (America/Los_Angeles)", 0)
            .expect("hit banner should parse");
        assert!(hit.hit, "‘hit your session limit’ must set hit=true");
        assert_eq!(hit.percent, None);
        assert_eq!(hit.reset_clock, "4:30pm");
        assert_eq!(hit.reset_tz, "America/Los_Angeles");

        // older phrasing: "resets at", no minutes, no zone.
        let approaching =
            parse_usage_limit("Approaching usage limit · resets at 4pm", 0).expect("approaching should parse");
        assert!(!approaching.hit);
        assert_eq!(approaching.reset_clock, "4pm");
        assert_eq!(approaching.reset_tz, "");

        // a stray percentage above the footer must NOT be read as used-percent
        // (the hit form has no percent; auto-continue gates on `hit`, not this).
        let stray = parse_usage_limit(
            "  Bash(cargo build) 45% done\n  You've hit your session limit · resets 4:30pm (America/Los_Angeles)",
            0,
        )
        .expect("hit banner should still parse past stray output");
        assert!(stray.hit);
        assert_eq!(stray.percent, None, "45% is tool output, not the used-percent");
        assert_eq!(stray.reset_clock, "4:30pm");

        // ordinary output → no banner.
        assert!(parse_usage_limit("Bash(cargo test) running… 12 passed", 0).is_none());
    }

    #[test]
    fn usage_limit_banner_parses_off_the_replayed_grid() {
        // claude_raw2.log carries the real "You've used 92% of your session
        // limit · resets 4:30pm (America/Los_Angeles)" footer, ANSI cursor-
        // positioned so it never appears contiguously in the raw bytes (proven
        // below) — only after emulator replay does it reassemble on the grid,
        // which is exactly the surface `scan_limit` reads via `bottom_plain`.
        let bytes = fixture("claude/2.1.172/pty/claude_raw2.log");
        assert!(
            !String::from_utf8_lossy(&bytes).contains("session limit"),
            "fixture changed: banner now contiguous in the raw stream?"
        );
        let mut emu = Emulator::new(30, 130);
        let mut best: Option<crate::session::UsageLimit> = None;
        for chunk in bytes.chunks(256) {
            emu.advance(chunk);
            // keep the most COMPLETE parse across frames (a chunk boundary can
            // land mid-draw, before the reset time is painted).
            if let Some(u) = crate::session::parse_usage_limit(&emu.bottom_plain(30), 1234) {
                if u.reset_clock == "4:30pm" && u.reset_tz == "America/Los_Angeles" {
                    best = Some(u);
                }
            }
        }
        let u = best.expect("usage-limit banner never fully parsed off the replayed grid");
        assert!(!u.hit, "the used-N% banner is a warning, not a hard hit");
        assert!(matches!(u.percent, Some(92) | Some(93)), "percent = {:?}", u.percent);
        assert_eq!(u.since_ms, 1234);
    }

    #[test]
    fn claude_titles_flow_as_events() {
        // claude_raw2.log carries 11 OSC-0 title updates (busy spinner etc.)
        let bytes = fixture("claude/2.1.172/pty/claude_raw2.log");
        let (_, events) = replay(&bytes, &[]);
        let titles = events.iter().filter(|e| matches!(e, EmuEvent::Title(_))).count();
        assert!(titles >= 1, "no Title events from a stream with 11 OSC-0s");
    }

    #[test]
    fn codex_approval_modal_appears_on_grid() {
        let bytes = fixture("codex/0.132.0/pty/codex_raw.log");
        let (seen, _) = replay(&bytes, &["Would you like to run the following command?"]);
        assert_eq!(seen.len(), 1, "codex exec-approval header never rendered");
    }

    fn replies(events: &[EmuEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                EmuEvent::PtyReply(b) => Some(String::from_utf8_lossy(b).to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn terminal_queries_are_answered_not_dropped() {
        // The parked prototype's bug: DSR/CPR must produce a PtyReply.
        let mut emu = Emulator::new(30, 110);
        let r = replies(&emu.advance(b"\x1b[6n")); // DSR cursor-position report
        assert!(!r.is_empty(), "CPR query produced no PtyReply — the prototype bug is back");
        assert!(r[0].starts_with("\x1b["), "CPR reply malformed: {:?}", r[0]);
        assert!(r[0].ends_with('R'), "CPR reply malformed: {:?}", r[0]);
    }

    #[test]
    fn background_color_query_answers_focused_dark() {
        // OSC 11 ? — claude/codex query the bg; must get the real palette, not
        // black (the index-space bug). alacritty maps bg → NamedColor 257.
        let mut emu = Emulator::new(10, 40);
        let r = replies(&emu.advance(b"\x1b]11;?\x07"));
        assert!(!r.is_empty(), "OSC 11 bg query was dropped");
        // reply encodes 0x14/0x16/0x1B as 4-hex-digit components (rgb:RRRR/GGGG/BBBB)
        assert!(r[0].contains("1414") || r[0].contains("14"), "bg reply not Focused Dark: {:?}", r[0]);
    }

    #[test]
    fn resize_changes_grid_geometry() {
        let mut emu = Emulator::new(17, 80);
        assert_eq!(emu.snapshot().rows.len(), 17);
        emu.resize(38, 120);
        let snap = emu.snapshot();
        assert_eq!(snap.rows.len(), 38, "emulator did not grow to stage height");
        // a write addressing the new rows lands without clamping onto row 16
        emu.advance(b"\x1b[30;1Hdeep");
        assert!(emu.snapshot().plain_lines()[29].starts_with("deep"));
    }

    #[test]
    fn bracketed_paste_mode_is_reported() {
        let mut emu = Emulator::new(30, 110);
        assert!(!emu.snapshot().bracketed_paste);
        emu.advance(b"\x1b[?2004h");
        assert!(emu.snapshot().bracketed_paste);
        emu.advance(b"\x1b[?2004l");
        assert!(!emu.snapshot().bracketed_paste);
    }

    #[test]
    fn snapshot_seq_is_monotonic_per_advance() {
        let mut emu = Emulator::new(10, 40);
        let s0 = emu.snapshot().seq;
        emu.advance(b"hello");
        let s1 = emu.snapshot().seq;
        emu.advance(b" world");
        let s2 = emu.snapshot().seq;
        assert!(s0 < s1 && s1 < s2);
    }
}

/// One search hit in the grid (incl. scrollback): how many lines ABOVE the
/// live bottom its line sits (0 = the bottom line — the same coordinate space
/// as `display_offset`, so scroll-to-match is `target - current_offset`), plus
/// the CHAR range within that line's plain text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchMatch {
    pub lines_above_bottom: u32,
    pub start: u32,
    pub end: u32,
}

/// Cap search results (a 1-char query over 10k history lines is not a plan).
pub const SEARCH_CAP: usize = 300;

impl Emulator {
    /// ASCII-case-insensitive substring search over the WHOLE grid (scrollback
    /// included), newest lines first so the cap keeps the most recent hits.
    /// Wrapped lines are separate grid lines (a match broken across the wrap
    /// boundary is not found — documented v1 limit).
    pub fn search_grid(&self, query: &str) -> Vec<SearchMatch> {
        if query.is_empty() {
            return Vec::new();
        }
        // literal query → escaped regex; the fork's RegexSearch is wrapline-
        // aware and wide-char-correct — never re-implement grid walking (DRY,
        // design critique). (?i) = case-insensitive (full Unicode, not ASCII).
        let escaped: String = query
            .chars()
            .flat_map(|c| if c.is_ascii_alphanumeric() || c == ' ' { vec![c] } else { vec!['\\', c] })
            .collect();
        let Ok(mut rx) = RegexSearch::new(&format!("(?i){escaped}")) else {
            return Vec::new();
        };
        let grid = self.term.grid();
        let top = grid.topmost_line();
        let bottom = grid.bottommost_line();
        // scan NEWEST-first (Direction::Left from the bottom-right corner) so
        // the cap keeps recent hits, then sort into document order below.
        let start = Point::new(bottom, Column(self.cols - 1));
        let end = Point::new(top, Column(0));
        let mut hits: Vec<SearchMatch> = Vec::new();
        for m in RegexIter::new(start, end, Direction::Left, &self.term, &mut rx) {
            if hits.len() >= SEARCH_CAP {
                break;
            }
            let (s_pt, e_pt) = (*m.start(), *m.end());
            // a wrapped match is clamped to its start line (v1: the highlight
            // covers the first segment; the jump still lands right).
            let end_col = if e_pt.line == s_pt.line { e_pt.column.0 + 1 } else { self.cols };
            // grid COLUMN → rendered CHAR index (wide chars own 2 columns but
            // render as 1 char — mirror merge_row's spacer skipping).
            let row = &grid[s_pt.line];
            let col_to_char = |col: usize| -> usize {
                (0..col.min(self.cols)).filter(|&c| !row[Column(c)].flags.intersects(Flags::WIDE_CHAR_SPACER)).count()
            };
            hits.push(SearchMatch {
                lines_above_bottom: (bottom.0 - s_pt.line.0) as u32,
                start: col_to_char(s_pt.column.0) as u32,
                end: col_to_char(end_col) as u32,
            });
        }
        // document order: oldest→newest, left→right (Enter walks toward live).
        hits.sort_by_key(|h| (std::cmp::Reverse(h.lines_above_bottom), h.start));
        hits
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    fn emu_with(lines: &[&str], rows: u16, cols: u16) -> Emulator {
        let mut e = Emulator::new(rows, cols);
        for (i, l) in lines.iter().enumerate() {
            if i > 0 {
                e.advance(b"\r\n");
            }
            e.advance(l.as_bytes());
        }
        e
    }

    #[test]
    fn finds_matches_with_char_ranges_case_insensitive() {
        let e = emu_with(&["hello world", "an ERROR here", "ok"], 10, 40);
        let m = e.search_grid("error");
        assert_eq!(m.len(), 1);
        // "an ERROR here" sits on grid line 1 of a 10-row grid whose bottom is
        // line 9 — lines_above_bottom is in the display_offset coordinate space
        // (grid-bottom-relative), so 9 - 1 = 8.
        assert_eq!(m[0], SearchMatch { lines_above_bottom: 8, start: 3, end: 8 });
        assert!(e.search_grid("zzz").is_empty());
        assert!(e.search_grid("").is_empty());
    }

    #[test]
    fn finds_multiple_per_line_and_in_scrollback() {
        // 4-row grid, 20 lines → 16 lines of history; needle deep in scrollback
        let lines: Vec<String> = (0..20).map(|i| if i == 2 { "hit one hit".into() } else { format!("line{i:02}") }).collect();
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let e = emu_with(&refs, 4, 40);
        let m = e.search_grid("hit");
        assert_eq!(m.len(), 2);
        // line 2 of 20 (0-indexed): bottom is line 19 → 17 above bottom
        assert!(m.iter().all(|x| x.lines_above_bottom == 17));
        assert_eq!(m[0].start, 0);
        assert_eq!(m[1].start, 8);
    }

    #[test]
    fn wide_chars_map_to_char_indices() {
        // 日本 occupy 4 columns but 2 chars; "error" starts at char 3
        let e = emu_with(&["\u{65e5}\u{672c} error"], 6, 30);
        let m = e.search_grid("error");
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].start, m[0].end), (3, 8));
    }

    #[test]
    fn caps_results_keeping_newest() {
        let lines: Vec<String> = (0..400).map(|i| format!("needle {i}")).collect();
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let e = emu_with(&refs, 5, 30);
        let m = e.search_grid("needle");
        assert_eq!(m.len(), SEARCH_CAP);
        // newest-first cap: the closest-to-bottom lines are kept
        assert!(m.iter().any(|x| x.lines_above_bottom == 0));
        assert!(m.iter().all(|x| (x.lines_above_bottom as usize) < SEARCH_CAP + 5));
    }
}
