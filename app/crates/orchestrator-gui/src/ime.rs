//! IME / CJK text input for the focused terminal (#9 3c).
//!
//! Its own module because the trait is a platform CONTRACT, not app logic: every
//! method here is called by macOS' input manager, and the invariants that matter
//! (marked_text_range must be Some while composing, the ranges are UTF-16) are
//! the platform's, not ours. Kept together so the contract reads as one piece.

use gpui::*;
use std::ops::Range;

use orchestrator_host::input::KeyInput;

use crate::*;

/// The terminal is a passthrough SINK — the CLI owns its buffer — so we only
/// track the in-progress composition (`ime_preedit`) and forward COMMITTED text
/// to the PTY. Registered per-frame via a `canvas` overlay in
/// render_agent_stage; macOS-validated.
impl EntityInputHandler for Orchestrator {
    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // COMMIT: the IME finalized text (a CJK candidate or a plain Latin char).
        self.ime_preedit.clear();
        // ⌘F bar open → committed text edits the QUERY, never the PTY (the #1
        // routing blocker: this is where plain chars actually arrive, critique).
        if self.search.open {
            self.search.query.push_str(text);
            self.kick_search(false);
            cx.notify();
            return;
        }
        // The range is meaningless for a terminal; forward the bytes to the PTY.
        if let Some(id) = self.active_session_id() {
            self.clear_term_selection();
            self.host.send_key(id, &KeyInput::Char(text.to_string()));
        }
        cx.notify();
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _sel: Option<Range<usize>>,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // PREEDIT: store the whole in-progress composition (macOS resends the full
        // marked string each keystroke). NOT sent to the PTY until committed.
        self.ime_preedit = new_text.to_string();
        cx.notify();
    }
    fn unmark_text(&mut self, _w: &mut Window, cx: &mut Context<Self>) {
        self.ime_preedit.clear();
        cx.notify();
    }
    fn marked_text_range(&self, _w: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        // MUST be Some during composition or the platform's is_composing flag is
        // false and arrows/enter/space leak to on_key_down instead of the IME.
        if self.ime_preedit.is_empty() {
            None
        } else {
            Some(0..self.ime_preedit.encode_utf16().count())
        }
    }
    fn selected_text_range(
        &mut self,
        _ignore: bool,
        _w: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let len = self.ime_preedit.encode_utf16().count();
        Some(UTF16Selection {
            range: len..len,
            reversed: false,
        })
    }
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _w: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let u: Vec<u16> = self.ime_preedit.encode_utf16().collect();
        let end = range.end.min(u.len());
        let start = range.start.min(end);
        *adjusted = Some(start..end);
        Some(String::from_utf16_lossy(&u[start..end]))
    }
    fn bounds_for_range(
        &mut self,
        _range: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _w: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // v1: anchor the candidate window at the grid's bottom-left (where the
        // prompt usually is). True-caret placement is a later refinement.
        Some(Bounds {
            origin: point(
                element_bounds.origin.x + px(12.),
                element_bounds.origin.y + element_bounds.size.height - px(LINE_PX + 8.),
            ),
            size: size(px(2.), px(LINE_PX)),
        })
    }
    fn character_index_for_point(
        &mut self,
        _p: Point<Pixels>,
        _w: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}
