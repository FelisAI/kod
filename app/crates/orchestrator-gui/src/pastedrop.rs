//! Dropping files onto the terminal, and pasting an image into it.
//!
//! Both are the SAME feature underneath, which is the non-obvious part: a PTY
//! carries bytes, so no terminal can receive an image. What every tool that
//! appears to support it actually does — Ghostty, iTerm2 — is write the bytes to
//! a temp file and type the PATH. So "drag a file in" and "paste a screenshot"
//! converge on: produce a path, escape it, write it to the PTY.
//!
//! Everything here is pure. Deliberately NO `use gpui::*` — a glob import brings
//! gpui's own `test` attribute macro, which shadows the real one and fails as
//! "recursion limit reached while expanding #[test]" (see rail_order.rs).

use gpui::ImageFormat;

/// Ghostty's cap, and a sane one: past this a paste is a mistake, not a
/// screenshot. Refusing loudly beats writing 80MB into $TMPDIR silently.
pub(crate) const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// Characters that never need quoting in any shell, and that no prompt will
/// mangle. Anything outside this set forces the whole path into single quotes.
fn is_bare_safe(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | '~' | '+' | ':' | ',' | '=' | '@')
}

/// Escape one path for insertion at a terminal cursor.
///
/// Bare when it can be — a bare path is what you want to read back, and what a
/// prompt (claude, codex) sees as a plain filename. Single-quoted otherwise,
/// because single quotes are the only shell construct with no interior escapes:
/// everything inside is literal, `$`, backticks and backslashes included. The
/// one exception is a single quote itself, which ends the run and has to be
/// re-opened as `'\''`.
///
/// NON-ASCII IS QUOTED TOO. A path in Chinese is perfectly legal for a shell,
/// but quoting costs nothing and removes any question about locale handling.
pub(crate) fn shell_quote(path: &str) -> String {
    if !path.is_empty() && path.chars().all(is_bare_safe) {
        return path.to_string();
    }
    let mut out = String::with_capacity(path.len() + 2);
    out.push('\'');
    for c in path.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// What a drop of one or more files types into the terminal.
///
/// Space-separated, and with a TRAILING space so you can keep typing straight
/// after the drop — the thing you drop a file for is almost never the last word
/// of the command.
pub(crate) fn drop_text<S: AsRef<str>>(paths: &[S]) -> String {
    let mut parts: Vec<String> = paths
        .iter()
        .map(|p| shell_quote(p.as_ref()))
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        return String::new();
    }
    parts.push(String::new()); // the trailing space
    parts.join(" ")
}

/// The extension for a pasted image, from what the clipboard says it is.
///
/// The bytes are written in their NATIVE format rather than transcoded to PNG
/// the way Ghostty does. Ghostty normalises for consistency; writing what we
/// were handed is lossless, needs no encoder in the dependency tree, and the
/// extension is what tells the reader how to decode it anyway.
pub(crate) fn image_ext(format: &ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Webp => "webp",
        ImageFormat::Gif => "gif",
        ImageFormat::Svg => "svg",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Tiff => "tiff",
    }
}

/// Filename for a pasted image. Timestamped and sequenced so two pastes in the
/// same second cannot collide and silently overwrite each other.
pub(crate) fn image_filename(now_ms: u64, seq: u64, format: &ImageFormat) -> String {
    format!("kod-paste-{now_ms}-{seq}.{}", image_ext(format))
}

/// Is this paste worth writing to disk at all?
pub(crate) fn image_paste_ok(len: usize) -> bool {
    len > 0 && len <= MAX_IMAGE_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_path_is_inserted_bare() {
        // What you want to read back, and what a prompt sees as a plain filename.
        assert_eq!(shell_quote("/Users/x/src/main.rs"), "/Users/x/src/main.rs");
        assert_eq!(shell_quote("~/notes.md"), "~/notes.md");
    }

    #[test]
    fn spaces_force_quoting() {
        // The classic corruption: an unquoted space splits one path into two
        // arguments and the command silently operates on the wrong thing.
        assert_eq!(
            shell_quote("/Users/x/My Documents/a.txt"),
            "'/Users/x/My Documents/a.txt'"
        );
    }

    #[test]
    fn an_apostrophe_is_reopened_not_escaped() {
        // Inside single quotes there is no escape character, so a quote must
        // CLOSE the run, emit an escaped quote, and reopen.
        assert_eq!(shell_quote("/tmp/it's here.txt"), r"'/tmp/it'\''s here.txt'");
    }

    #[test]
    fn shell_metacharacters_cannot_escape_the_quotes() {
        // The security-shaped case: a filename containing a command substitution
        // must arrive as TEXT, never as something the shell evaluates.
        assert_eq!(shell_quote("/tmp/$(rm -rf ~).txt"), "'/tmp/$(rm -rf ~).txt'");
        assert_eq!(shell_quote("/tmp/`whoami`"), "'/tmp/`whoami`'");
        assert_eq!(shell_quote("/tmp/a;b"), "'/tmp/a;b'");
        assert_eq!(shell_quote("/tmp/a\"b"), "'/tmp/a\"b'");
        assert_eq!(shell_quote("/tmp/back\\slash"), "'/tmp/back\\slash'");
    }

    #[test]
    fn non_ascii_paths_are_quoted_and_kept_whole() {
        assert_eq!(shell_quote("/Users/x/项目/笔记.md"), "'/Users/x/项目/笔记.md'");
        assert_eq!(shell_quote("/tmp/🐈.png"), "'/tmp/🐈.png'");
    }

    #[test]
    fn an_empty_path_is_quoted_not_dropped_silently() {
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn a_drop_ends_with_a_space_so_you_can_keep_typing() {
        assert_eq!(drop_text(&["/a/b.txt"]), "/a/b.txt ");
    }

    #[test]
    fn several_files_are_space_separated_and_each_escaped() {
        assert_eq!(
            drop_text(&["/a/one.txt", "/a/two three.txt"]),
            "/a/one.txt '/a/two three.txt' "
        );
    }

    #[test]
    fn dropping_nothing_types_nothing() {
        let none: [&str; 0] = [];
        assert_eq!(drop_text(&none), "");
    }

    #[test]
    fn a_pasted_image_keeps_the_format_it_arrived_in() {
        assert_eq!(image_ext(&ImageFormat::Png), "png");
        assert_eq!(image_ext(&ImageFormat::Jpeg), "jpg");
        assert_eq!(image_ext(&ImageFormat::Gif), "gif");
    }

    #[test]
    fn two_pastes_in_the_same_millisecond_cannot_collide() {
        let a = image_filename(1_700_000_000_000, 0, &ImageFormat::Png);
        let b = image_filename(1_700_000_000_000, 1, &ImageFormat::Png);
        assert_ne!(a, b, "a collision would silently overwrite the first paste");
        assert!(a.ends_with(".png"));
        assert!(a.starts_with("kod-paste-"));
    }

    #[test]
    fn oversized_and_empty_pastes_are_refused() {
        assert!(image_paste_ok(1));
        assert!(image_paste_ok(MAX_IMAGE_BYTES));
        assert!(!image_paste_ok(MAX_IMAGE_BYTES + 1), "10MB is the ceiling");
        assert!(!image_paste_ok(0), "an empty image is not a paste");
    }
}
