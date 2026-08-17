//! Small shared helpers used across the host crate (#9 cleanup — de-dup of
//! basename + char-truncation that had drifted into 3–5 copies each).

/// The final path segment (basename). Borrows — no allocation.
pub(crate) fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Max chars kept for a tool target (file path / command) on an event line.
pub(crate) const TARGET_MAX: usize = 120;
/// Max chars kept for a turn-end summary.
pub(crate) const SUMMARY_MAX: usize = 200;

/// Truncate to at most `max` chars (char-safe). Accepts `&str`/`String`.
pub(crate) fn truncate_to(s: impl AsRef<str>, max: usize) -> String {
    s.as_ref().chars().take(max).collect()
}
