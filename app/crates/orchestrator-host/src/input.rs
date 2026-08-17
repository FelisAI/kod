//! Key input → terminal bytes, mode-aware (docs/013 §1 input.rs).
//!
//! The GUI translates `gpui::Keystroke` into the neutral [`KeyInput`] (its
//! only input job); this module owns the byte encoding, consulting the
//! session's live [`ModeFlags`] (from the emulator) for bracketed paste.
//! Kitty-keyboard encoding is deliberately deferred: legacy bytes answered
//! every dialog in the S1 live runs (digits/Esc/enter); revisit when a pinned
//! CLI is observed enabling DISAMBIGUATE_ESC_CODES (flag already surfaced).

use serde::{Deserialize, Serialize};

/// Live terminal modes the encoder must respect. Read from the emulator's
/// `GridSnapshot`, never assumed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeFlags {
    pub bracketed_paste: bool,
    pub kitty_keys: bool,
}

/// A neutral key event. `Char` carries the typed text (IME-friendly);
/// `Paste` carries multi-line text and gets bracketed-paste framing when the
/// application enabled it (claude requires this for multi-line input —
/// verified live, fixtures `claude_raw4.log`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyInput {
    Char(String),
    Paste(String),
    Ctrl(char),
    Enter,
    Tab,
    ShiftTab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
}

/// Encode a key for the PTY. `None` = nothing to send (e.g. unmapped ctrl).
pub fn encode(input: &KeyInput, mode: ModeFlags) -> Option<Vec<u8>> {
    use KeyInput::*;
    Some(match input {
        Char(s) => s.as_bytes().to_vec(),
        Paste(s) => {
            if mode.bracketed_paste {
                let mut v = b"\x1b[200~".to_vec();
                v.extend_from_slice(s.as_bytes());
                v.extend_from_slice(b"\x1b[201~");
                v
            } else {
                s.as_bytes().to_vec()
            }
        }
        Ctrl(c) => {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() {
                vec![c as u8 - b'a' + 1]
            } else {
                return None;
            }
        }
        Enter => b"\r".to_vec(),
        Tab => b"\t".to_vec(),
        // claude's permission-mode cycler (Shift+Tab) — legacy back-tab
        ShiftTab => b"\x1b[Z".to_vec(),
        Backspace => vec![0x7f],
        Escape => vec![0x1b],
        Up => b"\x1b[A".to_vec(),
        Down => b"\x1b[B".to_vec(),
        Right => b"\x1b[C".to_vec(),
        Left => b"\x1b[D".to_vec(),
        Home => b"\x1b[H".to_vec(),
        End => b"\x1b[F".to_vec(),
        PageUp => b"\x1b[5~".to_vec(),
        PageDown => b"\x1b[6~".to_vec(),
        Delete => b"\x1b[3~".to_vec(),
    })
}

/// A wheel scroll forwarded to an app that enabled MOUSE REPORTING (the alt-screen
/// case — vim/htop/claude-fullscreen). `delta` rows, >0 = up. One wheel event per
/// row (capped at 5), reported at cell (1,1) — a fullscreen app scrolls its whole
/// view regardless of position.
pub fn encode_mouse_scroll(delta: i32, sgr: bool) -> Vec<u8> {
    if delta == 0 {
        return Vec::new();
    }
    let up = delta > 0;
    let n = delta.unsigned_abs().min(5);
    let mut out = Vec::new();
    for _ in 0..n {
        if sgr {
            // SGR: ESC [ < btn ; col ; row M   (btn 64 = wheel-up, 65 = down; M = press)
            out.extend_from_slice(format!("\x1b[<{};1;1M", if up { 64 } else { 65 }).as_bytes());
        } else {
            // legacy X10: ESC [ M (32+btn) (32+col) (32+row); wheel btn 64|65 → +32 = 96|97
            out.extend_from_slice(&[0x1b, b'[', b'M', if up { 96 } else { 97 }, 33, 33]);
        }
    }
    out
}

/// A wheel scroll translated to ARROW KEYS for an alt-screen app WITHOUT mouse
/// reporting (xterm "alternate scroll" mode). `delta` rows, >0 = up.
pub fn encode_arrow_scroll(delta: i32, app_cursor: bool) -> Vec<u8> {
    if delta == 0 {
        return Vec::new();
    }
    let seq: &[u8] = match (delta > 0, app_cursor) {
        (true, false) => b"\x1b[A",
        (true, true) => b"\x1bOA",
        (false, false) => b"\x1b[B",
        (false, true) => b"\x1bOB",
    };
    seq.repeat(delta.unsigned_abs().min(5) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_scroll_sgr_and_x10_and_cap() {
        assert_eq!(encode_mouse_scroll(1, true), b"\x1b[<64;1;1M".to_vec()); // up
        assert_eq!(encode_mouse_scroll(-1, true), b"\x1b[<65;1;1M".to_vec()); // down
        assert_eq!(encode_mouse_scroll(3, true), b"\x1b[<64;1;1M\x1b[<64;1;1M\x1b[<64;1;1M".to_vec());
        assert_eq!(encode_mouse_scroll(1, false), vec![0x1b, b'[', b'M', 96, 33, 33]); // X10 up
        assert_eq!(encode_mouse_scroll(100, true).len(), 5 * "\x1b[<64;1;1M".len()); // capped at 5
        assert!(encode_mouse_scroll(0, true).is_empty());
    }

    #[test]
    fn arrow_scroll_up_down_and_app_cursor() {
        assert_eq!(encode_arrow_scroll(1, false), b"\x1b[A".to_vec());
        assert_eq!(encode_arrow_scroll(-1, false), b"\x1b[B".to_vec());
        assert_eq!(encode_arrow_scroll(2, true), b"\x1bOA\x1bOA".to_vec());
        assert!(encode_arrow_scroll(0, false).is_empty());
    }

    const PLAIN: ModeFlags = ModeFlags { bracketed_paste: false, kitty_keys: false };
    const PASTE_MODE: ModeFlags = ModeFlags { bracketed_paste: true, kitty_keys: false };

    #[test]
    fn digit_answers_dialog() {
        // S1: injecting '1' answered the Write permission dialog instantly
        assert_eq!(encode(&KeyInput::Char("1".into()), PLAIN).unwrap(), b"1");
    }

    #[test]
    fn cjk_char_passes_through_as_utf8() {
        // an IME commit ("你好" from a Pinyin candidate) is one Char(String) → raw
        // UTF-8 bytes to the PTY, so no new send-path is needed for CJK (#9 3c).
        assert_eq!(encode(&KeyInput::Char("你好".into()), PLAIN).unwrap(), "你好".as_bytes());
        assert_eq!(encode(&KeyInput::Char("你好".into()), PLAIN).unwrap(), vec![0xe4, 0xbd, 0xa0, 0xe5, 0xa5, 0xbd]);
    }

    #[test]
    fn esc_rejects_and_enter_confirms() {
        assert_eq!(encode(&KeyInput::Escape, PLAIN).unwrap(), vec![0x1b]);
        assert_eq!(encode(&KeyInput::Enter, PLAIN).unwrap(), b"\r");
    }

    #[test]
    fn ctrl_c_interrupts() {
        assert_eq!(encode(&KeyInput::Ctrl('c'), PLAIN).unwrap(), vec![0x03]);
        assert_eq!(encode(&KeyInput::Ctrl('C'), PLAIN).unwrap(), vec![0x03]);
    }

    #[test]
    fn shift_tab_is_backtab() {
        // claude cycles permission mode on Shift+Tab
        assert_eq!(encode(&KeyInput::ShiftTab, PLAIN).unwrap(), b"\x1b[Z");
    }

    #[test]
    fn arrows_navigate_option_lists() {
        assert_eq!(encode(&KeyInput::Up, PLAIN).unwrap(), b"\x1b[A");
        assert_eq!(encode(&KeyInput::Down, PLAIN).unwrap(), b"\x1b[B");
    }

    #[test]
    fn multiline_paste_is_bracketed_when_mode_set() {
        let text = "line one\nline two\nline three";
        let bytes = encode(&KeyInput::Paste(text.into()), PASTE_MODE).unwrap();
        assert!(bytes.starts_with(b"\x1b[200~"));
        assert!(bytes.ends_with(b"\x1b[201~"));
        // without the mode, raw passthrough (newlines would submit — caller's
        // mode flags came from the live emulator, so this only happens when
        // the app genuinely didn't enable paste mode)
        let raw = encode(&KeyInput::Paste(text.into()), PLAIN).unwrap();
        assert_eq!(raw, text.as_bytes());
    }

    #[test]
    fn typed_text_passes_through() {
        assert_eq!(encode(&KeyInput::Char("claude".into()), PLAIN).unwrap(), b"claude");
    }
}
