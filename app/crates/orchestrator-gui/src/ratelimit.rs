//! "Did the PROVIDER turn us away?" — the one place that classifies a worker-loop
//! error as a rate limit / overload.
//!
//! Split out of main.rs because three unrelated background lanes (summaries,
//! extraction, the cartographer) all back off on this answer, and getting it
//! wrong is expensive in both directions: a missed limit burns quota, and a
//! false positive DEFERS a job with its attempt refunded — which is how a
//! permanent parse failure once became an immortal job at the head of a FIFO
//! queue. The stage gate below is the fix, and it lives here with its tests.

/// Phrases that mean the PROVIDER turned us away. Whole phrases, not fragments:
/// the predicate below matches them on word boundaries, because the previous
/// bare-substring version (`contains("rate")`) matched the middle of "gene-RATE",
/// "ite-RATE", "accu-RATE" and `contains("limit")` matched "un-LIMIT-ed".
const RATE_LIMIT_PHRASES: &[&str] = &[
    "rate limit",
    "rate-limit",
    "rate_limit",
    "ratelimit",
    "429",
    "too many requests",
    "overloaded",
    "usage limit",
    "limit reached",
    "limit exceeded",
    "quota",
    "resource exhausted",
    "resource_exhausted",
];

/// Does `text` contain any RATE_LIMIT_PHRASES on word-ish boundaries? Pure text
/// test — see `is_rate_limited` for the stage gate that decides whose text may
/// be tested at all.
pub(crate) fn is_rate_limit_text(text: &str) -> bool {
    let lower = text.to_lowercase();
    RATE_LIMIT_PHRASES
        .iter()
        .any(|p| contains_word(&lower, p))
}

/// `needle` (ASCII, already lowercase) occurs in `hay` with a non-alphanumeric
/// char on each side — so "429" hits in `"status":429,` but not inside "14290",
/// and "quota" hits in "quota exceeded" but not inside "quotable".
fn contains_word(hay: &str, needle: &str) -> bool {
    let mut from = 0usize;
    while let Some(i) = hay[from..].find(needle) {
        let start = from + i;
        let end = start + needle.len();
        let before_ok = hay[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let after_ok = hay[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        from = start + 1; // needle is ASCII, so this is a char boundary
    }
    false
}

/// A worker-loop error meaning the PROVIDER rate-limited/overloaded us, so we
/// should back off hard (and, for a summary job, defer rather than spend an
/// attempt).
///
/// STAGE-GATED, and that gate is load-bearing. Only `cli:` errors — the ones
/// `wait_output` builds out of the CLI's own exit code and stderr — may be
/// classified. Every other stage is either OUR text (`spawn:`, `transcript:`,
/// `write:`) or, far worse, the MODEL's (`parse:` carries the model's prose so a
/// non-JSON reply is legible in the store). Ungated, a model that merely says
/// "I'd be happy to generate a summary" was read as a rate limit — and a rate
/// limit now DEFERS with the attempt refunded, so a permanent, deterministic
/// parse failure became an immortal job that could never die and (FIFO claim)
/// starved every other session. A rate limit is only ever something the CLI
/// reports; the model's opinion of one is not evidence.
pub(crate) fn is_rate_limited(err: &str) -> bool {
    err.starts_with("cli:") && is_rate_limit_text(err)
}

#[cfg(test)]
mod rate_limit_tests {
    use super::{is_rate_limit_text, is_rate_limited};

    /// The words that ATE the standup. `is_rate_limited` was a bare substring
    /// grep, and this branch newly pastes the model's own prose into the error
    /// (`parse: no JSON in summary output: …`) — so a model politely saying it
    /// would "gene-RATE a summary" was classified RATE LIMITED, deferred, and
    /// refunded its attempt, forever: an immortal job at the head of a FIFO
    /// queue, starving every other session. Strictly worse than the permanent
    /// blacklist this branch came to delete.
    #[test]
    fn model_prose_is_never_a_rate_limit() {
        for prose in [
            "I'd be happy to generate a summary of this session.",
            "The agent will iterate over the remaining files.",
            "An accurate account of the work done today.",
            "unlimited retries are enabled",
            "corporate boilerplate, moderate progress, deliberate pace",
        ] {
            let err = format!("parse: no JSON in summary output: {prose}");
            assert!(
                !is_rate_limited(&err),
                "the MODEL's words are not evidence of a rate limit: {prose}"
            );
            assert!(
                !is_rate_limit_text(prose),
                "…and the phrase itself must not match either: {prose}"
            );
        }
        // the stage gate stands alone: even a `parse:` error whose text DOES say
        // "rate limit" is the model talking about one, not the CLI reporting one.
        assert!(!is_rate_limited(
            "parse: no JSON in summary output: I fixed the rate limit bug in the client"
        ));
        // …and neither is any other stage of OUR OWN making.
        assert!(!is_rate_limited("transcript: empty or unreadable"));
        assert!(!is_rate_limited("spawn: couldn't run claude: No such file"));
    }

    /// The real thing: what the CLI actually prints when the provider says no.
    /// These are `cli:` errors — the CLI's own exit code + stderr tail, which is
    /// the ONLY place a rate limit can truthfully come from.
    #[test]
    fn a_real_cli_rate_limit_is_caught() {
        for stderr in [
            r#"ERROR: {"type":"error","status":429,"message":"rate_limit_error"}"#,
            "stream error: Too Many Requests (429)",
            "Claude AI usage limit reached|1752460000",
            "API Error: server overloaded · Rate limited",
            "error: You have hit your usage limit. Resets at 4pm.",
            "429 Too Many Requests",
            "quota exceeded for this organization",
            "rate limit exceeded, retry after 60s",
        ] {
            let err = format!("cli: codex exec exit=1 | stderr(2167B) tail: …{stderr}");
            assert!(is_rate_limited(&err), "a real rate limit must back us off: {stderr}");
        }
    }

    /// Word-ish boundaries, so a number or a longer word can't smuggle a match.
    #[test]
    fn phrases_match_on_boundaries_not_inside_words() {
        assert!(!is_rate_limit_text("read 14290 bytes"), "429 inside a number");
        assert!(!is_rate_limit_text("a quotable headline"), "quota inside a word");
        assert!(!is_rate_limit_text("stderr(2167B) tail: exit=1"));
        assert!(is_rate_limit_text(r#"{"status":429,"x":1}"#), "punctuation bounds it");
        assert!(is_rate_limit_text("HTTP 429"));
    }
}
