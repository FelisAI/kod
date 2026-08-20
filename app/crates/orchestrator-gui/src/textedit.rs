//! The editing core behind every inline field (#64).
//!
//! Split out of the key router because the ROUTER needs a `Context` and a live
//! `App` — and the editing itself does not. Everything here is a pure function
//! over `(&mut String, &mut Caret)`, so the whole grammar (what ⌥⌫ deletes,
//! where ⌘← lands, what typing over a selection does) is unit-tested with no
//! window, no `App`, and no spawned process.
//!
//! Offsets are BYTE offsets and are always kept on a UTF-8 char boundary.
//! `String::replace_range` panics otherwise, so every motion walks by chars
//! rather than by arithmetic — a caret in a project name typed in Chinese is
//! the case that turns a `- 1` into a crash.

/// `anchor` is where a selection started, `head` is where the cursor is. They
/// are equal when nothing is selected, which is the overwhelmingly common case
/// — so no `Option` is worth the noise at every use site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Caret {
    pub anchor: usize,
    pub head: usize,
}

impl Caret {
    /// Collapsed at the end of `s` — what opening a prefilled field gives you.
    /// Deliberately NOT select-all: the old caret rendered after the whole
    /// buffer, so end-of-text is the position that preserves the previous
    /// behaviour exactly. ⌘A is the replace-everything gesture instead, which
    /// means no keystroke that used to append can now silently wipe a value.
    pub fn at_end(s: &str) -> Self {
        Caret { anchor: s.len(), head: s.len() }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// `(start, end)` with `start <= end` — the selection, in buffer order.
    pub fn range(&self) -> (usize, usize) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }

    fn collapse(&mut self, at: usize) {
        self.anchor = at;
        self.head = at;
    }

    /// Force both ends into `s` and onto char boundaries. This is what makes a
    /// STALE caret safe: the buffer can be replaced out from under it (a
    /// section switch, a store reload), and without this the next edit would
    /// slice mid-codepoint and panic.
    pub fn clamp(&mut self, s: &str) {
        self.anchor = clamp_boundary(s, self.anchor);
        self.head = clamp_boundary(s, self.head);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Motion {
    Left,
    Right,
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EditOp {
    /// `extend` is the shift key: move the head, leave the anchor behind.
    Move(Motion, bool),
    Insert(String),
    /// ⌫ — `word` is ⌥⌫, `to_start` is ⌘⌫.
    DeleteBack { word: bool, to_start: bool },
    /// ⌦ — `word` is ⌥⌦.
    DeleteFwd { word: bool },
    SelectAll,
    Copy,
    Cut,
    /// Resolved by the ROUTER, which owns the `App` the clipboard needs; it
    /// reads the clipboard and applies `Insert` instead. `apply` never sees it.
    Paste,
}

/// What a keystroke MEANS in a text field, from primitives rather than a gpui
/// `KeyDownEvent` — that is what keeps the entire mapping testable.
///
/// `enter` and `escape` are deliberately absent: their meaning is per-FIELD
/// (commit vs newline, cancel vs leave-slot), so the router keeps them.
pub(crate) fn op_for(
    key: &str,
    shift: bool,
    cmd: bool,
    alt: bool,
    ctrl: bool,
    key_char: Option<&str>,
) -> Option<EditOp> {
    use Motion::*;
    // ⌘-chords FIRST. On macOS these are the standard editing commands, and the
    // old router dropped every one of them behind a blanket
    // `!modifiers.secondary()` guard — which is exactly why ⌘V did nothing.
    if cmd && !ctrl {
        return match key {
            "a" => Some(EditOp::SelectAll),
            "c" => Some(EditOp::Copy),
            "x" => Some(EditOp::Cut),
            "v" => Some(EditOp::Paste),
            // ⌘←/⌘→ are line-start/line-end on macOS, NOT word motions; ⌥←/⌥→
            // are the word ones. Swapping them is the classic port-from-Windows
            // bug and feels wrong immediately to anyone on a Mac.
            "left" => Some(EditOp::Move(LineStart, shift)),
            "right" => Some(EditOp::Move(LineEnd, shift)),
            "backspace" => Some(EditOp::DeleteBack { word: false, to_start: true }),
            _ => None,
        };
    }
    // ⌃A / ⌃E — the emacs bindings macOS honours in every native text field.
    if ctrl {
        return match key {
            "a" => Some(EditOp::Move(LineStart, shift)),
            "e" => Some(EditOp::Move(LineEnd, shift)),
            _ => None,
        };
    }
    match key {
        "left" => Some(EditOp::Move(if alt { WordLeft } else { Left }, shift)),
        "right" => Some(EditOp::Move(if alt { WordRight } else { Right }, shift)),
        "home" => Some(EditOp::Move(LineStart, shift)),
        "end" => Some(EditOp::Move(LineEnd, shift)),
        "backspace" => Some(EditOp::DeleteBack { word: alt, to_start: false }),
        "delete" => Some(EditOp::DeleteFwd { word: alt }),
        // gpui reports the space bar by NAME, so it never arrives as a char.
        "space" => Some(EditOp::Insert(" ".into())),
        _ => {
            // A printable character. `key_char` is what the platform actually
            // produced — dead keys, ⌥-accents, shifted symbols — and the
            // single-char key name is the fallback for paths that leave it None.
            let ch = key_char
                .filter(|c| !c.is_empty())
                .map(str::to_string)
                .or_else(|| (key.chars().count() == 1).then(|| key.to_string()))?;
            // Control characters would corrupt a single-line buffer: `tab` and
            // `enter` both arrive here as key_char "\t" / "\r" on some paths.
            if ch.chars().any(char::is_control) {
                return None;
            }
            Some(EditOp::Insert(ch))
        }
    }
}

/// Apply one op. Returns text the CALLER should put on the clipboard (⌘C/⌘X),
/// `None` otherwise — so this stays free of gpui and of the clipboard itself.
pub(crate) fn apply(buf: &mut String, c: &mut Caret, op: &EditOp) -> Option<String> {
    c.clamp(buf);
    match op {
        EditOp::Move(m, extend) => {
            let (s, e) = c.range();
            // Plain ←/→ with a selection COLLAPSES to that edge rather than
            // moving a character — what every native field does.
            if !extend && !c.is_empty() && matches!(m, Motion::Left | Motion::Right) {
                c.collapse(if matches!(m, Motion::Left) { s } else { e });
                return None;
            }
            let to = motion(buf, c.head, *m);
            c.head = to;
            if !extend {
                c.anchor = to;
            }
            None
        }
        EditOp::Insert(t) => {
            replace_sel(buf, c, t);
            None
        }
        EditOp::DeleteBack { word, to_start } => {
            if !c.is_empty() {
                replace_sel(buf, c, "");
                return None;
            }
            let from = motion(
                buf,
                c.head,
                if *to_start {
                    Motion::LineStart
                } else if *word {
                    Motion::WordLeft
                } else {
                    Motion::Left
                },
            );
            buf.replace_range(from..c.head, "");
            c.collapse(from);
            None
        }
        EditOp::DeleteFwd { word } => {
            if !c.is_empty() {
                replace_sel(buf, c, "");
                return None;
            }
            let to = motion(
                buf,
                c.head,
                if *word { Motion::WordRight } else { Motion::Right },
            );
            buf.replace_range(c.head..to, "");
            None
        }
        EditOp::SelectAll => {
            c.anchor = 0;
            c.head = buf.len();
            None
        }
        EditOp::Copy => {
            let (s, e) = c.range();
            (s != e).then(|| buf[s..e].to_string())
        }
        EditOp::Cut => {
            let (s, e) = c.range();
            if s == e {
                return None;
            }
            let t = buf[s..e].to_string();
            replace_sel(buf, c, "");
            Some(t)
        }
        // The router resolves Paste into Insert. Reaching here means a caller
        // forgot to, and doing nothing is the safe answer.
        EditOp::Paste => None,
    }
}

/// Replace the selection (or insert at a collapsed caret) and leave the caret
/// after what was written — the single primitive every mutating op goes through,
/// so "typing over a selection" cannot diverge from "pasting over a selection".
fn replace_sel(buf: &mut String, c: &mut Caret, t: &str) {
    let (s, e) = c.range();
    buf.replace_range(s..e, t);
    c.collapse(s + t.len());
}

fn motion(s: &str, i: usize, m: Motion) -> usize {
    match m {
        Motion::Left => prev_boundary(s, i),
        Motion::Right => next_boundary(s, i),
        Motion::WordLeft => prev_word(s, i),
        Motion::WordRight => next_word(s, i),
        // LINE-wise, not buffer-wise: the outline Detail slot is a real textarea
        // (⏎ inserts a newline, ⌘⏎ commits), so ⌘←/Home must land at the start
        // of THIS line. On a single-line buffer the two coincide, so every other
        // field is unaffected.
        Motion::LineStart => s[..i].rfind('\n').map(|n| n + 1).unwrap_or(0),
        Motion::LineEnd => s[i..].find('\n').map(|n| i + n).unwrap_or(s.len()),
    }
}

fn clamp_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    let mut i = i;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn prev_boundary(s: &str, i: usize) -> usize {
    s[..i].char_indices().next_back().map(|(j, _)| j).unwrap_or(0)
}

fn next_boundary(s: &str, i: usize) -> usize {
    s[i..].chars().next().map(|c| i + c.len_utf8()).unwrap_or(i)
}

/// ⌥← lands at the START of the word to the left: skip any whitespace, then the
/// word itself.
fn prev_word(s: &str, i: usize) -> usize {
    let mut j = i;
    while j > 0 {
        let p = prev_boundary(s, j);
        if !s[p..j].chars().all(char::is_whitespace) {
            break;
        }
        j = p;
    }
    while j > 0 {
        let p = prev_boundary(s, j);
        if s[p..j].chars().all(char::is_whitespace) {
            break;
        }
        j = p;
    }
    j
}

/// ⌥→ lands at the END of the word to the right — the mirror of `prev_word`,
/// not "the start of the next word". macOS does the former.
fn next_word(s: &str, i: usize) -> usize {
    let mut j = i;
    while j < s.len() {
        let n = next_boundary(s, j);
        if !s[j..n].chars().all(char::is_whitespace) {
            break;
        }
        j = n;
    }
    while j < s.len() {
        let n = next_boundary(s, j);
        if s[j..n].chars().all(char::is_whitespace) {
            break;
        }
        j = n;
    }
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a buffer with a script of ops, returning (text, caret).
    fn run(start: &str, at: usize, ops: &[EditOp]) -> (String, Caret) {
        let mut buf = start.to_string();
        let mut c = Caret { anchor: at, head: at };
        for op in ops {
            apply(&mut buf, &mut c, op);
        }
        (buf, c)
    }

    #[test]
    fn cmd_v_maps_to_paste_which_the_old_router_dropped() {
        // THE bug in #64: the old char branch was guarded by
        // `!modifiers.secondary()`, so ⌘V never reached anything.
        assert_eq!(op_for("v", false, true, false, false, Some("v")), Some(EditOp::Paste));
        assert_eq!(op_for("a", false, true, false, false, Some("a")), Some(EditOp::SelectAll));
        assert_eq!(op_for("c", false, true, false, false, Some("c")), Some(EditOp::Copy));
        assert_eq!(op_for("x", false, true, false, false, Some("x")), Some(EditOp::Cut));
    }

    #[test]
    fn typing_inserts_at_the_caret_not_at_the_end() {
        // The old buffer was append-only: this is the whole point of a cursor.
        let (s, c) = run("helloworld", 5, &[EditOp::Insert(" ".into())]);
        assert_eq!(s, "hello world");
        assert_eq!(c.head, 6);
    }

    #[test]
    fn backspace_deletes_at_the_caret_not_the_last_char() {
        // The old impl was `buf.pop()` — it would have produced "helloworl".
        let (s, _) = run("helloworld", 5, &[EditOp::DeleteBack { word: false, to_start: false }]);
        assert_eq!(s, "hellworld");
    }

    #[test]
    fn delete_forward_removes_the_char_under_the_caret() {
        let (s, c) = run("hello", 0, &[EditOp::DeleteFwd { word: false }]);
        assert_eq!(s, "ello");
        assert_eq!(c.head, 0, "⌦ must not drag the caret along");
    }

    #[test]
    fn select_all_then_type_replaces_everything() {
        // The gesture that matters most in a PREFILLED field: swapping a model
        // id without holding backspace thirty times.
        let (s, c) = run(
            "claude-opus-4",
            13,
            &[EditOp::SelectAll, EditOp::Insert("claude-opus-5".into())],
        );
        assert_eq!(s, "claude-opus-5");
        assert!(c.is_empty());
    }

    #[test]
    fn paste_over_a_selection_replaces_it_exactly_like_typing_does() {
        let (typed, _) = run("abcdef", 0, &[EditOp::SelectAll, EditOp::Insert("XY".into())]);
        let (pasted, _) = run("abcdef", 0, &[EditOp::SelectAll, EditOp::Insert("XY".into())]);
        assert_eq!(typed, pasted);
        assert_eq!(typed, "XY");
    }

    #[test]
    fn cut_returns_the_text_and_removes_it_copy_leaves_it() {
        let mut buf = "hello world".to_string();
        let mut c = Caret { anchor: 0, head: 5 };
        assert_eq!(apply(&mut buf, &mut c, &EditOp::Copy), Some("hello".into()));
        assert_eq!(buf, "hello world", "copy must not mutate");
        assert_eq!(apply(&mut buf, &mut c, &EditOp::Cut), Some("hello".into()));
        assert_eq!(buf, " world");
    }

    #[test]
    fn copy_with_no_selection_yields_nothing() {
        let mut buf = "hello".to_string();
        let mut c = Caret::at_end(&buf);
        assert_eq!(apply(&mut buf, &mut c, &EditOp::Copy), None);
        assert_eq!(apply(&mut buf, &mut c, &EditOp::Cut), None);
        assert_eq!(buf, "hello", "an empty cut must not eat the buffer");
    }

    #[test]
    fn arrow_with_a_selection_collapses_to_the_edge() {
        let mut buf = "abcdef".to_string();
        let mut c = Caret { anchor: 1, head: 4 };
        apply(&mut buf, &mut c, &EditOp::Move(Motion::Left, false));
        assert_eq!((c.anchor, c.head), (1, 1));
        let mut c = Caret { anchor: 1, head: 4 };
        apply(&mut buf, &mut c, &EditOp::Move(Motion::Right, false));
        assert_eq!((c.anchor, c.head), (4, 4));
    }

    #[test]
    fn shift_arrow_extends_from_the_anchor() {
        let (_, c) = run(
            "abcdef",
            2,
            &[
                EditOp::Move(Motion::Right, true),
                EditOp::Move(Motion::Right, true),
            ],
        );
        assert_eq!((c.anchor, c.head), (2, 4));
    }

    #[test]
    fn word_motions_land_on_word_edges() {
        let s = "one two three";
        assert_eq!(motion(s, 13, Motion::WordLeft), 8, "start of 'three'");
        assert_eq!(motion(s, 0, Motion::WordRight), 3, "end of 'one'");
        assert_eq!(motion(s, 3, Motion::WordRight), 7, "end of 'two'");
    }

    #[test]
    fn option_backspace_eats_one_word() {
        let (s, _) = run("one two three", 13, &[EditOp::DeleteBack { word: true, to_start: false }]);
        assert_eq!(s, "one two ");
    }

    #[test]
    fn cmd_backspace_eats_to_the_line_start_only() {
        // Line, not buffer — the textarea slot depends on this.
        let (s, _) = run("first\nsecond", 12, &[EditOp::DeleteBack { word: false, to_start: true }]);
        assert_eq!(s, "first\n");
    }

    #[test]
    fn home_and_end_are_line_wise_in_a_textarea() {
        let s = "first\nsecond";
        assert_eq!(motion(s, 8, Motion::LineStart), 6);
        assert_eq!(motion(s, 2, Motion::LineEnd), 5);
        // ...and buffer-wise when there is only one line, so single-line fields
        // are unaffected by the textarea accommodation.
        assert_eq!(motion("solo", 2, Motion::LineStart), 0);
        assert_eq!(motion("solo", 2, Motion::LineEnd), 4);
    }

    #[test]
    fn multibyte_text_never_slices_mid_codepoint() {
        // A project named in Chinese, and an emoji for good measure. Byte
        // arithmetic instead of char walking would panic on every one of these.
        let (s, c) = run("项目名称", 6, &[EditOp::Insert("X".into())]);
        assert_eq!(s, "项目X名称");
        assert_eq!(c.head, 7);

        let (s, _) = run("项目名称", 6, &[EditOp::DeleteBack { word: false, to_start: false }]);
        assert_eq!(s, "项名称");

        let (s, _) = run("a🐈b", 5, &[EditOp::DeleteBack { word: false, to_start: false }]);
        assert_eq!(s, "ab", "one ⌫ removes the whole 4-byte cat");

        let (_, c) = run("🐈🐈", 0, &[EditOp::Move(Motion::Right, false)]);
        assert_eq!(c.head, 4);
    }

    #[test]
    fn a_stale_caret_past_the_end_is_clamped_rather_than_panicking() {
        // The buffer can be swapped under a caret (section switch, store
        // reload). Without clamp() this slices out of bounds and takes the app
        // down; the field is worth less than the process.
        let (s, c) = run("hi", 999, &[EditOp::Insert("!".into())]);
        assert_eq!(s, "hi!");
        assert_eq!(c.head, 3);

        // ...and mid-codepoint offsets snap DOWN to a boundary.
        let mut buf = "项目".to_string();
        let mut c = Caret { anchor: 1, head: 2 };
        c.clamp(&buf);
        assert_eq!((c.anchor, c.head), (0, 0));
        apply(&mut buf, &mut c, &EditOp::Insert("x".into()));
        assert_eq!(buf, "x项目");
    }

    #[test]
    fn enter_and_escape_stay_with_the_router() {
        // Their meaning is per-field (commit vs newline, cancel vs leave-slot),
        // so op_for must not claim them.
        assert_eq!(op_for("enter", false, false, false, false, Some("\r")), None);
        assert_eq!(op_for("escape", false, false, false, false, None), None);
        assert_eq!(op_for("tab", false, false, false, false, Some("\t")), None);
    }

    #[test]
    fn macos_modifier_conventions_are_not_swapped() {
        // ⌘← is line-start; ⌥← is word-left. The reverse is the classic
        // port-from-Windows bug.
        assert_eq!(
            op_for("left", false, true, false, false, None),
            Some(EditOp::Move(Motion::LineStart, false))
        );
        assert_eq!(
            op_for("left", false, false, true, false, None),
            Some(EditOp::Move(Motion::WordLeft, false))
        );
        assert_eq!(
            op_for("a", false, false, false, true, None),
            Some(EditOp::Move(Motion::LineStart, false)),
            "⌃A is line-start, not select-all"
        );
    }

    #[test]
    fn space_arrives_by_name_and_still_inserts() {
        assert_eq!(op_for("space", false, false, false, false, None), Some(EditOp::Insert(" ".into())));
    }
}
