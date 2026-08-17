//! Relative-time labels, shared.
//!
//! Callers today: the standup's updated-projects tier and Recover's session rows.
//!
//! Recover had grown THREE copies of the same inline ladder, and all three shared
//! two edges: no "just now" floor (a fresh row read "0m ago") and no day rollover
//! (a three-day-old row read "72h ago"). Consolidating them here fixed both.
//!
//! Deliberately NOT folded in — they are not the same function despite the family
//! resemblance, and merging them would change what they say:
//!   * `mapview::building_badge` — has a "◔ building" floor below 90s and holds
//!     hours out to 48h before rolling to days.
//!   * `cockpit::warm_age_label` — reads "seen just now" for a full hour, and
//!     spells days out in words ("seen 3 days ago").
//! Both already roll over to days correctly. If they ever do converge, that is a
//! product decision about wording, not a mechanical de-duplication.

/// Format a millisecond age as a short relative label: `just now`, `5m ago`,
/// `3h ago`, `2d ago`.
///
/// Sub-minute ages collapse to "just now" rather than "0m ago", and anything
/// past a day rolls over to days so the number stays small enough to scan.
pub(crate) fn ago_label(age_ms: u64) -> String {
    let mins = age_ms / 60_000;
    if mins < 1 {
        "just now".to_string()
    } else if mins < 60 {
        format!("{mins}m ago")
    } else if mins < 60 * 24 {
        format!("{}h ago", mins / 60)
    } else {
        format!("{}d ago", mins / (60 * 24))
    }
}

/// Age in ms of an absolute `ts_ms`, saturating at 0 so a clock skew that puts
/// an event slightly in the future reads as "just now" instead of underflowing
/// to a ~584-million-year age.
pub(crate) fn age_ms_since(ts_ms: u64, now_ms: u64) -> u64 {
    now_ms.saturating_sub(ts_ms)
}

/// Wall-clock now in ms, from the same second-resolution clock the registry
/// uses (relative labels never need finer granularity than that).
pub(crate) fn now_ms() -> u64 {
    orchestrator_core::registry::now_secs().saturating_mul(1000)
}

#[cfg(test)]
mod tests {
    use super::{age_ms_since, ago_label};

    #[test]
    fn sub_minute_reads_as_just_now() {
        assert_eq!(ago_label(0), "just now");
        assert_eq!(ago_label(59_999), "just now");
    }

    #[test]
    fn minutes_hours_and_days_roll_over() {
        assert_eq!(ago_label(60_000), "1m ago");
        assert_eq!(ago_label(59 * 60_000), "59m ago");
        assert_eq!(ago_label(60 * 60_000), "1h ago");
        assert_eq!(ago_label(23 * 60 * 60_000), "23h ago");
        // the rollover the inline ladders were missing: a 3-day-old event read
        // as "72h ago" before this.
        assert_eq!(ago_label(24 * 60 * 60_000), "1d ago");
        assert_eq!(ago_label(3 * 24 * 60 * 60_000), "3d ago");
    }

    #[test]
    fn a_future_timestamp_saturates_instead_of_underflowing() {
        // clock skew (or an event written by a daemon on a different clock)
        // must not wrap into an astronomically large age.
        assert_eq!(age_ms_since(5_000, 1_000), 0);
        assert_eq!(ago_label(age_ms_since(5_000, 1_000)), "just now");
    }
}
