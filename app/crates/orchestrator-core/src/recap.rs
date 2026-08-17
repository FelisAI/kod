//! Shared text helpers for the "what did this agent just do" recap line and its
//! relative timestamp — used by the Standup, Workspace, and Recover surfaces so
//! they all sanitize identically (DRY). Dogfooding found the raw "last assistant
//! line" is a coin-flip: it's often a preamble, a markdown fragment, raw tool
//! JSON, or pure noise ("PONG"). `recap_line` picks the first SUBSTANTIVE line.

/// The first line of `text` that reads like a real recap — skipping blanks,
/// pure noise (PONG / ok / done), code fences, raw tool JSON, command tags, and
/// markdown-structure fragments (a lone header / bold phrase). Returns the
/// cleaned line (callers truncate to their width); falls back to `"…"`.
pub fn recap_line(text: &str) -> String {
    for raw in text.lines() {
        let s = clean_md(raw.trim());
        if !is_substantive(&s) {
            continue;
        }
        return s;
    }
    "…".into()
}

fn is_substantive(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let lower = s.to_lowercase();
    // pure-noise tails an agent commonly emits.
    if matches!(lower.as_str(), "pong" | "ok" | "ok." | "done" | "done." | "ping" | "(no message)" | "(no recap)") {
        return false;
    }
    // raw structure / tool output / xml-ish tags / table / code fence.
    match s.chars().next() {
        Some('{') | Some('[') | Some('<') | Some('|') | Some('`') => return false,
        _ => {}
    }
    // need real substance: a lone header like "1. Biggest Risk" (after md strip)
    // or "PONG" shouldn't win — require a few words.
    s.split_whitespace().count() >= 4 && s.chars().count() >= 14
}

/// Strip leading markdown markers (`#`, `>`, `-`, `*`, backtick, an ordered-list
/// `12.` prefix) and surrounding bold/emphasis, so `**1. Biggest Risk**` becomes
/// `1. Biggest Risk` (then rejected for being too short) and `- shipped the X`
/// becomes `shipped the X`.
fn clean_md(s: &str) -> String {
    let t = s.trim_start_matches(|c: char| matches!(c, '#' | '>' | '-' | '*' | '`' | ' ' | '\t'));
    let t = strip_ordered_prefix(t);
    t.trim().trim_matches('*').trim().to_string()
}

/// Drop a leading `12. ` / `3) ` ordered-list prefix.
fn strip_ordered_prefix(s: &str) -> &str {
    let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && digits <= 3 {
        let rest = &s[digits..];
        if let Some(after) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return after;
        }
    }
    s
}

/// Compact relative-time label from "seconds ago" — `now`, `2m`, `18h`, `3d`.
pub fn rel_time(secs_ago: u64) -> String {
    if secs_ago < 45 {
        "now".into()
    } else if secs_ago < 3600 {
        format!("{}m", secs_ago / 60)
    } else if secs_ago < 86_400 {
        format!("{}h", secs_ago / 3600)
    } else {
        format!("{}d", secs_ago / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_noise_and_fragments_picks_substance() {
        // the real dogfooding offenders:
        assert_eq!(recap_line("PONG"), "…");
        assert_eq!(recap_line("```json\n{\"x\":1}\n```"), "…");
        assert_eq!(recap_line("**1. Biggest Risk**"), "…"); // lone header → too short
        assert_eq!(recap_line("(no message)"), "…");
        assert_eq!(recap_line("<command-name>/model</command-name>"), "…");
        // a real recap survives, with leading markdown stripped.
        assert_eq!(recap_line("- shipped the daemon and all tests pass"), "shipped the daemon and all tests pass");
        assert_eq!(recap_line("368/368 green. Let me refresh the dashboard"), "368/368 green. Let me refresh the dashboard");
        // skip a junk first line, return the substantive second.
        assert_eq!(
            recap_line("**Summary**\nRewrote the identity resolver and added a fold-on-rescan test."),
            "Rewrote the identity resolver and added a fold-on-rescan test."
        );
        // a one-word/short message is not a recap.
        assert_eq!(recap_line("ok."), "…");
    }

    #[test]
    fn rel_time_buckets() {
        assert_eq!(rel_time(10), "now");
        assert_eq!(rel_time(120), "2m");
        assert_eq!(rel_time(3600 * 18), "18h");
        assert_eq!(rel_time(86_400 * 3), "3d");
    }
}
