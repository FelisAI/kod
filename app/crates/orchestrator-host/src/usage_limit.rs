//! Usage limits + the reset clock (docs/019) — the banner PARSE layer, split out
//! of session.rs so the hosted-session file stays about I/O planes and phase.
//!
//! Everything here is PURE and deterministic: text (a claude footer lifted off
//! the grid) or telemetry (codex's `rate_limits` rollout lines) in, a
//! [`UsageLimit`] out. That purity is what lets the whole banner corpus be
//! unit-tested directly against fixture strings with no PTY, no CLI, no clock —
//! the live `HostedSession::scan_limit` only feeds it `emu.bottom_plain(..)`.
//!
//! The consumers care about three things this module owns: is it a HARD block
//! (`hit`), WHEN does it reset (`reset_at_unix` — auto-continue's wake target),
//! and is the banner we still hold STILL TRUE (`is_expired` / `carry_forward` —
//! neither CLI ever retracts a limit, so the view must expire it on the clock).

use crate::session::AC_GIVEUP_MS;

/// A parsed claude usage-limit banner lifted off the live grid (docs/019).
/// Unlike [`crate::session::Trouble`] this is NOT Busy-gated and does NOT
/// latch: claude pins the banner as a persistent FOOTER that outlives a turn
/// and must survive an idle window, because auto-continue (slice 2) wakes on
/// the reset. Owned strings (not `Copy`) so it can carry the reset clock +
/// IANA zone verbatim.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UsageLimit {
    /// true = hard block ("You've hit your session limit"); false = the benign
    /// warning ("used N%" / "Approaching usage limit"). Auto-continue must gate
    /// strictly on `hit` — resuming on a mere warning types into a live turn.
    pub hit: bool,
    /// percent of the window used, when the warning form reports it.
    pub percent: Option<u8>,
    /// the reset wall-clock, normalized from what claude prints, e.g. "4:30pm".
    /// Empty only when the banner omitted a parseable time.
    pub reset_clock: String,
    /// the IANA zone claude prints in parens, e.g. "America/Los_Angeles"
    /// (empty when the banner omits it). Slice 2 turns clock+zone into an
    /// absolute wake instant; slice 1 carries only the raw components.
    pub reset_tz: String,
    /// the calendar date a WEEKLY banner prints ("Jun 5"), empty for the
    /// same-day session/generic forms. Kept verbatim so the absolute wake
    /// instant lands on the right day, not merely the next matching clock.
    pub reset_date: String,
    /// the ABSOLUTE wake instant in unix SECONDS, resolved from clock+date+zone
    /// (or codex's raw `resets_at`). `None` when the banner carried no parseable
    /// time or zone — auto-continue can only arm when this is `Some`.
    pub reset_at_unix: Option<i64>,
    /// wall-clock ms the banner was FIRST observed — the GUI ticks the age
    /// locally (timestamps over prose, design critique #4).
    pub since_ms: u64,
}

/// How far PAST its reset instant a limit is still shown. Absorbs clock skew and
/// a CLI that lags the window edge by a few seconds, so we never blink the chip
/// off a limit that is still genuinely blocking.
const LIMIT_EXPIRY_GRACE_SECS: i64 = 120;

/// A HIT that carries NO resolvable reset instant (a tz-less banner, a credit cap,
/// a codex window that reported neither `resets_at` nor `resets_in_seconds`) has
/// no clock to expire ON — but it must not be believed forever either, or a
/// rollout whose last `token_count` said 100% pins a permanent ⛔. Age it out from
/// FIRST observation on the same horizon auto-continue gives up at: past 6h we no
/// longer trust that an un-retracted, un-dated block is still live.
const LIMIT_UNDATED_MAX_AGE_MS: u64 = AC_GIVEUP_MS as u64;

/// A LEAD (reset − observation) this long can only come from `banner_reset_instant`
/// ROLLING a passed clock forward to TOMORROW — no live CLI window resets that far
/// out. It is the one signal that separates a re-parse of the STALE banner from a
/// genuinely NEW block wearing the same words (both resolve the identical instant).
const LIMIT_ROLLED_FORWARD_MS: i64 = 12 * 3600 * 1000;

impl UsageLimit {
    /// Same underlying banner — the same TEXT (ignoring a ticking percent), NOT the
    /// same resolved instant. `banner_reset_instant` rolls a PASSED clock forward to
    /// TOMORROW, so re-parsing the stale banner still sitting on the grid resolves a
    /// brand-new instant for the very same words; comparing it would launder that
    /// banner as FRESH (new `since_ms`, a reset 24h out) on nothing more than a
    /// window resize, defeating [`Self::is_expired`]. Callers therefore carry BOTH
    /// `since_ms` and `reset_at_unix` forward on a match — see [`Self::carry_forward`].
    fn same_banner(&self, other: &UsageLimit) -> bool {
        self.hit == other.hit
            && self.reset_clock == other.reset_clock
            && self.reset_date == other.reset_date
            && self.reset_tz == other.reset_tz
    }

    /// Fold a freshly observed banner onto the one we already hold. An IDENTICAL
    /// banner that is a RESTATEMENT of the one we hold keeps its first-seen
    /// `since_ms` (the chip age ticks from the real start, not from each rescan — a
    /// warning climbing 92%→93% is the same limit approach) and its FIRST-resolved
    /// `reset_at_unix`: it is the same banner still sitting on the grid, so its reset
    /// is whatever we resolved when we first saw it, never a re-resolution rolled a
    /// day forward. A genuinely NEW block wearing the SAME WORDS (a daily rhythm hits
    /// the same limit at the same hour) is NOT folded — see [`Self::is_restatement_of`].
    pub(crate) fn carry_forward(mut new: UsageLimit, old: Option<&UsageLimit>) -> UsageLimit {
        if let Some(old) = old.filter(|o| new.same_banner(o) && new.is_restatement_of(o)) {
            new.since_ms = old.since_ms;
            new.reset_at_unix = old.reset_at_unix;
        }
        new
    }

    /// Is `self` the SAME block RESTATED, or a genuinely NEW block reusing the same
    /// words? Only a restatement may be pinned to `old`'s clock: pinning a new block
    /// buries it under a corpse — born already-expired, so no chip, no BLOCKED tier,
    /// and auto-continue (which reads the limit RAW) arms on a reset a day in the past
    /// and immediately gives up.
    fn is_restatement_of(&self, old: &UsageLimit) -> bool {
        // `old` is still BELIEVED ⇒ the banner never died; the same block.
        if !old.is_expired(self.since_ms) {
            return true;
        }
        // `old` is DEAD. Two observations reach here that the TEXT cannot tell apart:
        // (a) a re-parse of the STALE banner still sitting on the grid — its clock has
        // PASSED, so `banner_reset_instant` rolled it a full day forward; and (b) a
        // genuinely NEW block whose reset is IMMINENT (inside the CLI's window). Only
        // (a) may be pinned; the LEAD is the one signal that separates them.
        self.reset_at_unix.is_none_or(|t| {
            t.saturating_mul(1000).saturating_sub(self.since_ms as i64) >= LIMIT_ROLLED_FORWARD_MS
        })
    }

    /// A limit whose reset instant has PASSED is dead — but neither CLI ever
    /// retracts one. Claude will not re-render a banner it already painted (the
    /// text simply stays on the emulator grid until something overwrites it), and
    /// codex re-asserts its last `token_count` telemetry on every poll forever. So
    /// nothing upstream can produce the "cleared" edge, and the VIEW must expire the
    /// limit on the clock or a long-reset window shows until the session dies.
    ///
    /// `since_ms == 0` is the codex 1970 landmine: `transcript::codex_rate_limits`
    /// falls back to `observed_ms = 0` for a timestamp-less rollout line, and a
    /// `resets_in_seconds` then resolves against the EPOCH — expiring a LIVE limit
    /// instantly. An unknown first-observation time is never aged out.
    pub(crate) fn is_expired(&self, now_ms: u64) -> bool {
        if self.since_ms == 0 {
            return false;
        }
        // a reset instant at/before the FIRST observation cannot describe this
        // banner (the same landmine reached via bogus telemetry) — distrust it and
        // fall through to the age rule rather than expiring on it.
        let dated = self
            .reset_at_unix
            .filter(|t| t.saturating_mul(1000) >= self.since_ms as i64);
        match dated {
            Some(t) => (now_ms / 1000) as i64 >= t + LIMIT_EXPIRY_GRACE_SECS,
            // SAFETY NET on the undated age-out: a banner that printed a reset DATE is
            // a WEEKLY block — it runs for DAYS, so 6h of age proves nothing. Only an
            // undated hit that also carries NO date (a credit cap, a codex window with
            // no reset telemetry, a truly zone-less banner) may be aged out. Without
            // this, a weekly banner whose zone we failed to resolve is hidden while it
            // is still very much blocking.
            None => {
                self.hit
                    && self.reset_date.is_empty()
                    && now_ms.saturating_sub(self.since_ms) >= LIMIT_UNDATED_MAX_AGE_MS
            }
        }
    }

    /// A short LIVE countdown to the reset instant — "2h 15m", "5d 3h", "12m",
    /// "<1m" — or "" when the reset is unknown (`reset_at_unix` is `None`) or
    /// already past. This is what claude's own footer shows and the single
    /// biggest fidelity gap: the app parsed `reset_at_unix` but only ever printed
    /// the bare wall-clock, so a reset that crossed local midnight read as *today*
    /// and a weekly reset gave no sense of how far out it was. Coarse (whole
    /// minutes) and recomputed each render — accurate enough for an hours-out
    /// reset without a per-second timer.
    pub fn reset_countdown(&self, now_ms: u64) -> String {
        let Some(reset) = self.reset_at_unix else {
            return String::new();
        };
        let rem = reset - (now_ms / 1000) as i64;
        if rem <= 0 {
            return String::new();
        }
        let (d, h, m) = (rem / 86_400, (rem % 86_400) / 3_600, (rem % 3_600) / 60);
        if d > 0 {
            if h > 0 {
                format!("{d}d {h}h")
            } else {
                format!("{d}d")
            }
        } else if h > 0 {
            if m > 0 {
                format!("{h}h {m}m")
            } else {
                format!("{h}h")
            }
        } else if m > 0 {
            format!("{m}m")
        } else {
            "<1m".to_string()
        }
    }

    /// The reset wall-time as a short label: "Jun 5, 7am" (weekly — the DATE the
    /// banner printed, previously parsed into `reset_date` but never shown, so a
    /// weekly reset was indistinguishable from a same-day one) / "9:50pm"
    /// (same-day) / "" (none). Weekday-qualified codex clocks already embed their
    /// day in `reset_clock`, so they pass straight through.
    pub fn reset_label(&self) -> String {
        match (self.reset_date.is_empty(), self.reset_clock.is_empty()) {
            (false, false) => format!("{}, {}", self.reset_date, self.reset_clock),
            (false, true) => self.reset_date.clone(),
            (true, false) => self.reset_clock.clone(),
            (true, true) => String::new(),
        }
    }
}

/// Lift a usage-limit banner out of a blob of rendered grid text (the bottom
/// rows). Returns `None` when no banner is present. PURE + deterministic so the
/// parse is unit-tested directly against the real fixture strings; the live
/// `scan_limit` just feeds it `emu.bottom_plain(..)`. Anchors on "session
/// limit"/"usage limit" and tolerates the ANSI-positioned spacing (words are
/// column-placed, so runs of spaces vary) plus the "·"/apostrophe glyphs.
pub fn parse_usage_limit(text: &str, now_ms: u64) -> Option<UsageLimit> {
    let lower = text.to_lowercase();
    // overload noise: "API Error: … (not your usage limit) · Rate limited" is a
    // transient 529, NOT a subscription limit — reject it before anything else so
    // its "usage limit" substring can't fabricate a banner (auto-continue would
    // wake on a phantom reset).
    if lower.contains("not your usage limit") {
        return None;
    }
    // hit vs warning. The block form is "You've hit your <window> limit" where the
    // window word varies (session / weekly / bare) — anchor on "hit your" and
    // require "limit" in the SAME clause (up to the next "·" / newline) so a
    // percentage or a later sentence can't smuggle a false hit. Auto-continue
    // gates strictly on this bit, so it stays NARROW.
    let hit_window = lower.split_once("hit your").is_some_and(|(_, rest)| {
        rest.split(['·', '\n']).next().unwrap_or(rest).contains("limit")
    });
    // CREDIT CAP ("You've reached your Opus limit. Run /usage-credits to add
    // more.") is ALSO a hard block, but it carries NO reset — you top up credits,
    // you don't wait. It used to parse to `None`, so a credit-capped session read
    // as plain IDLE (invisible). Surface it as a hit: `reset_at_unix` stays `None`,
    // so auto-continue can NEVER arm on it (the `reset_known` gate in
    // `auto_continue_step` + `ac_decide`'s `Some(reset_at)` requirement), which is
    // exactly the safety the old `None` was reaching for — minus the invisibility.
    // Anchored NARROW: "/usage-credits" is unique to the credit message, and
    // "reached your … limit" mirrors the "hit your" clause guard so a "reached 90%
    // of your limit" warning can't trip it.
    let credit_cap = lower.contains("usage-credits")
        || lower.split_once("reached your").is_some_and(|(_, rest)| {
            rest.split(['·', '\n']).next().unwrap_or(rest).contains("limit")
        });
    let hit = hit_window || credit_cap;
    // the benign warning form: "used N%" / "approaching … limit".
    let warning = (lower.contains("used") || lower.contains("approaching")) && lower.contains("limit");
    if !hit && !warning {
        return None;
    }
    // percent only from the "used N%" warning form, anchored on "used" so a
    // stray percentage elsewhere in the bottom rows can't be mislabeled.
    let percent = lower.find("used").and_then(|i| parse_percent(&lower[i..]));
    let (reset_clock, reset_date, reset_tz) = parse_reset(text);
    let reset_at_unix = banner_reset_instant(&reset_clock, &reset_date, &reset_tz, now_ms);
    Some(UsageLimit {
        hit,
        percent,
        reset_clock,
        reset_date,
        reset_tz,
        reset_at_unix,
        since_ms: now_ms,
    })
}

/// First `\d{1,3}%` in the (lowercased) text → the used-percent, clamped ≤100.
fn parse_percent(lower: &str) -> Option<u8> {
    let bytes = lower.as_bytes();
    let pct = lower.find('%')?;
    let mut i = pct;
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i == pct {
        return None; // a bare '%' with no leading digits
    }
    // `i..pct` is all ASCII digits → a valid UTF-8 slice.
    lower[i..pct].parse::<u16>().ok().map(|v| v.min(100) as u8)
}

/// Parse "resets 4:30pm (America/Los_Angeles)" / "resets at 4pm" / the weekly
/// "resets Jun 5 at 7am (tz)" into (clock, date, tz). Scans the region AFTER
/// "resets" so an unrelated earlier parenthesized path can't be mistaken for the
/// zone, and a stray earlier clock can't be read as the reset time.
fn parse_reset(orig: &str) -> (String, String, String) {
    // Find "resets" in ORIG directly (case-insensitive over ASCII) so the byte
    // offset is valid for `orig`. Do NOT reuse an index computed on a lowercased
    // COPY: a non-ASCII uppercase char earlier in the 6-row grid window (e.g. 'İ'
    // U+0130 → "i̇", +1 byte) desyncs the two strings' byte lengths, so `orig[cut..]`
    // would land off a char boundary or past the end → panic on the daemon sweep.
    let ob = orig.as_bytes();
    let needle = b"resets";
    let Some(pos) = (0..=ob.len().saturating_sub(needle.len()))
        .find(|&i| ob[i..i + needle.len()].eq_ignore_ascii_case(needle))
    else {
        return (String::new(), String::new(), String::new());
    };
    let cut = pos + needle.len();
    // WRAP-TOLERANT: on a narrow grid the banner SOFT-WRAPS, and `bottom_plain` emits
    // one line per GRID ROW — so a row break lands mid-token ("(America/Los_\nAngeles)"
    // at ~52-71 cols, "4:3\n0pm" at ~48). Rejoin the rows before scanning, or the zone
    // (or the clock) is lost, `banner_reset_instant` returns None, and the hit goes
    // UNDATED — no auto-continue wake at all, and the view then ages the block out at
    // 6h while it is still live. The `'\n'` clause guard in the `hit` detection is
    // deliberately NOT touched: there it is load-bearing (it rejects "not your usage
    // limit"). Only the reset REGION is rejoined.
    let region = orig[cut..].replace('\n', "");
    let region_lower = region.to_lowercase();
    let clock = parse_clock_token(&region_lower).unwrap_or_default();
    let date = parse_reset_date(&region_lower);
    let tz = parse_tz(&region).unwrap_or_default();
    (clock, date, tz)
}

/// The 3-letter capitalized month tokens, indexed 0=Jan..11=Dec.
const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// Scan the (lowercased) reset region for a weekly date → "Jun 5", else "".
/// Finds the earliest WORD-BOUNDARY month token (so full "June" matches on its
/// "jun" prefix, but "adjust" never does), skips the rest of the name + spaces,
/// then reads the day digits. Returns the first month token that is actually
/// followed by a day so a month-shaped fragment of a zone name (no digits) is
/// skipped rather than swallowing the real date.
fn parse_reset_date(region: &str) -> String {
    let b = region.as_bytes();
    let mut i = 0;
    while i < b.len() {
        // only test at a word boundary (start, or preceded by a non-alpha byte).
        // `get(i..)` also skips mid-UTF-8 offsets (the banner's "·" is 2 bytes) —
        // a month token is ASCII, so it can never start there anyway.
        let at_boundary = i == 0 || !b[i - 1].is_ascii_alphabetic();
        if at_boundary {
            if let Some(mi) = region
                .get(i..)
                .and_then(|rest| MONTHS.iter().position(|m| rest.starts_with(m)))
            {
                // skip the rest of the (possibly full) month name, then spaces.
                let mut j = i;
                while j < b.len() && b[j].is_ascii_alphabetic() {
                    j += 1;
                }
                while j < b.len() && b[j] == b' ' {
                    j += 1;
                }
                let d_start = j;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                if j > d_start && j - d_start <= 2 {
                    let cap = capitalize_month(mi);
                    return format!("{cap} {}", &region[d_start..j]);
                }
                // month-shaped but no day → keep scanning past this word.
                i = j.max(i + 1);
                continue;
            }
        }
        i += 1;
    }
    String::new()
}

/// "jun" (index 5) → "Jun". Uppercases the first byte of the 3-letter token.
fn capitalize_month(month_index: usize) -> String {
    let m = MONTHS[month_index];
    let mut s = String::with_capacity(3);
    s.push(m.as_bytes()[0].to_ascii_uppercase() as char);
    s.push_str(&m[1..]);
    s
}

/// SCAN `s` (already lowercased) for the first real clock token `H[:MM](am|pm)`,
/// skipping digit runs that aren't clocks. The weekly form "Jun 5 at 7am" has a
/// bare day digit BEFORE the clock, so a first-digit-run reader would lock onto
/// "5" and miss "7am" — the scanner walks each digit run through `try_clock_at`
/// and keeps going until one carries an am/pm suffix. Normalized to "4:30pm" /
/// "4pm".
fn parse_clock_token(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        if let Some(clock) = try_clock_at(s, i) {
            return Some(clock);
        }
        // not a clock — skip the rest of this digit run and look for the next.
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    None
}

/// Try to read a clock token anchored at the digit run starting at `start`.
/// Requires 1–2 hour digits, an optional ":" that MUST be followed by exactly 2
/// minute digits (else reject — a stray "5:3" is not a clock), optional spaces,
/// then "am"/"pm". `None` when the run isn't a clock, so the scanner moves on.
fn try_clock_at(s: &str, start: usize) -> Option<String> {
    let b = s.as_bytes();
    let mut i = start;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i - start == 0 || i - start > 2 {
        return None; // no hour, or too many digits to be a clock
    }
    let hour = &s[start..i];
    let mut minute = "";
    if i < b.len() && b[i] == b':' {
        let m_start = i + 1;
        let mut j = m_start;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j - m_start != 2 {
            return None; // ':' not followed by exactly 2 digits → not a clock
        }
        minute = &s[m_start..j];
        i = j;
    }
    while i < b.len() && b[i] == b' ' {
        i += 1;
    }
    let rest = &s[i..];
    let ampm = if rest.starts_with("am") {
        "am"
    } else if rest.starts_with("pm") {
        "pm"
    } else {
        return None;
    };
    Some(if minute.is_empty() {
        format!("{hour}{ampm}")
    } else {
        format!("{hour}:{minute}{ampm}")
    })
}

/// First parenthesized group that looks like an IANA zone (contains '/', only
/// alnum + '_' + '/'), e.g. "(America/Los_Angeles)" → "America/Los_Angeles".
fn parse_tz(orig: &str) -> Option<String> {
    let mut rest = orig;
    while let Some(open) = rest.find('(') {
        let tail = &rest[open + 1..];
        let Some(close) = tail.find(')') else { break };
        let inner = &tail[..close];
        if inner.contains('/')
            && inner
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '_')
        {
            return Some(inner.to_string());
        }
        rest = &tail[close + 1..];
    }
    None
}

/// Resolve a claude banner's reset components into an ABSOLUTE unix-SECONDS
/// instant, honoring the IANA zone (and DST) via `jiff`. `clock` is the
/// normalized "H[:MM]am/pm"; `date` is the weekly "Mon D" (empty for the
/// same-day session/generic forms); `tz` is the IANA name. Returns `None` on any
/// missing/unparseable component — auto-continue only arms when this is `Some`.
///
/// DST-correct BY CONSTRUCTION: the wake time is a WALL-CLOCK civil datetime
/// re-resolved through the zone (`TimeZone::to_zoned`), never a fixed offset — so
/// a "9pm tomorrow" that straddles a spring-forward still lands at 9pm local, not
/// 8pm/10pm. "Next occurrence": a DATED (weekly) banner uses the next matching
/// month/day (this year, else next); a DATELESS banner uses today when the clock
/// is still ahead in-zone, else tomorrow — the day rolls in CIVIL space so the
/// re-resolve re-derives the correct offset.
fn banner_reset_instant(clock: &str, date: &str, tz: &str, now_ms: u64) -> Option<i64> {
    if clock.is_empty() || tz.is_empty() {
        return None;
    }
    let zone = jiff::tz::TimeZone::get(tz).ok()?;
    let (hour, minute) = parse_clock_24h(clock)?;
    let now_ts = jiff::Timestamp::from_millisecond(now_ms as i64).ok()?;
    let today = now_ts.to_zoned(zone.clone()).date();
    // wall-clock (civil) → absolute instant through the zone, DST-resolved. On a
    // DST FALL-BACK fold (an ambiguous local time) prefer the LATER offset (#3) so
    // the estimate is never EARLY — auto-continue must not fire before the real
    // reset. `.later()` is also correct on a spring-forward gap and a no-op on an
    // unambiguous time.
    let at = |d: jiff::civil::Date| -> Option<jiff::Timestamp> {
        zone.to_ambiguous_zoned(d.at(hour, minute, 0, 0))
            .later()
            .ok()
            .map(|z| z.timestamp())
    };
    let reset_ts = if let Some((month, day)) = parse_month_day(date) {
        let this_year = jiff::civil::Date::new(today.year(), month, day).ok()?;
        let ts = at(this_year)?;
        if ts > now_ts {
            ts
        } else if now_ts.as_second() - ts.as_second() <= WEEKLY_RECENT_PAST_SECS {
            // a weekly reset that JUST passed (within a few hours) means we're
            // observing the banner right around its reset — treat it as DUE NOW
            // rather than rolling a full YEAR forward (#3), which would push the
            // wake past the GiveUp horizon and silently disable auto-continue.
            ts
        } else {
            at(jiff::civil::Date::new(today.year() + 1, month, day).ok()?)?
        }
    } else {
        match at(today) {
            Some(ts) if ts > now_ts => ts,
            _ => at(today.tomorrow().ok()?)?,
        }
    };
    Some(reset_ts.as_second())
}

/// A weekly reset date parsed no more than this far in the PAST is treated as
/// due-now rather than rolled a full year (#3 — a banner observed right at reset).
const WEEKLY_RECENT_PAST_SECS: i64 = 6 * 3600;

/// Normalized "H[:MM]am/pm" → 24h (hour, minute). 12am→00, 12pm→12.
fn parse_clock_24h(clock: &str) -> Option<(i8, i8)> {
    let (body, pm) = if let Some(b) = clock.strip_suffix("am") {
        (b, false)
    } else if let Some(b) = clock.strip_suffix("pm") {
        (b, true)
    } else {
        return None;
    };
    let (h_str, m_str) = body.split_once(':').unwrap_or((body, "0"));
    let hour12: i8 = h_str.parse().ok()?;
    let minute: i8 = m_str.parse().ok()?;
    if !(1..=12).contains(&hour12) || !(0..=59).contains(&minute) {
        return None;
    }
    let hour24 = match (hour12, pm) {
        (12, false) => 0,
        (12, true) => 12,
        (h, false) => h,
        (h, true) => h + 12,
    };
    Some((hour24, minute))
}

/// "Mon D" (e.g. "Jun 5") → (month 1-12, day). `None` when not a month + day.
fn parse_month_day(date: &str) -> Option<(i8, i8)> {
    let mut parts = date.split_whitespace();
    let mon = parts.next()?.to_lowercase();
    let day: i8 = parts.next()?.parse().ok()?;
    let month = MONTHS.iter().position(|m| *m == mon)? as i8 + 1;
    if !(1..=31).contains(&day) {
        return None;
    }
    Some((month, day))
}

impl crate::transcript::CodexRateLimits {
    /// Map codex's STRUCTURED rate-limit telemetry onto the SAME [`UsageLimit`]
    /// claude's grid scan produces, so the chip + Standup BLOCKED tier render
    /// identically for codex with ZERO render changes (docs/019 codex limit).
    ///
    /// `hit` = any window at ≥100%; `percent` = the busier window, rounded and
    /// clamped. The reset clock comes from the EXHAUSTED window when hit (the one
    /// the user is actually waiting on), else `primary` (fallback `secondary`),
    /// formatted in the user's local zone via `local_off_secs`. The rollout
    /// carries no IANA zone, so `reset_tz` is empty. `None` when neither window
    /// is present (so an empty `rate_limits` can't fabricate a clear).
    pub fn to_usage_limit(&self, local_off_secs: i64) -> Option<UsageLimit> {
        let p = self.primary.as_ref();
        let s = self.secondary.as_ref();
        if p.is_none() && s.is_none() {
            return None;
        }
        let pct = |w: Option<&crate::transcript::RateWindow>| w.map(|w| w.used_percent).unwrap_or(0.0);
        let hit = pct(p) >= 100.0 || pct(s) >= 100.0;
        let percent = pct(p).max(pct(s)).round().clamp(0.0, 100.0) as u8;
        // reset source: the exhausted window if hit (waited-on), else primary.
        let src = if hit {
            if pct(p) >= 100.0 {
                p
            } else {
                s
            }
        } else {
            p.or(s)
        };
        // the ABSOLUTE wake instant comes STRAIGHT from codex's raw telemetry
        // (`resets_at`, else observed + `resets_in_seconds`) — never re-derived
        // from the formatted clock, which would lose the day for a weekly window.
        let reset_at_unix = src.and_then(|w| reset_instant(w, self.observed_ms));
        let reset_clock = reset_at_unix
            .map(|secs| fmt_reset_clock(secs, self.observed_ms, local_off_secs))
            .unwrap_or_default();
        Some(UsageLimit {
            hit,
            percent: Some(percent),
            reset_clock,
            reset_date: String::new(),
            reset_tz: String::new(),
            reset_at_unix,
            since_ms: self.observed_ms,
        })
    }
}

/// A window's reset instant in unix SECONDS: an explicit `resets_at` epoch, else
/// the observation time plus `resets_in_seconds`. `None` when the window carries
/// neither (the clock is then omitted, mirroring claude's no-time banner).
fn reset_instant(w: &crate::transcript::RateWindow, observed_ms: u64) -> Option<i64> {
    w.resets_at
        .or_else(|| w.resets_in_seconds.map(|d| (observed_ms / 1000) as i64 + d))
}

/// A unix instant already shifted into local time → "H:MMam/pm", the 12h shape
/// claude's chip prints (claude's `parse_clock_token` normalizes to the same).
fn fmt_clock_12h(local_unix_secs: i64) -> String {
    let secs_of_day = local_unix_secs.rem_euclid(86_400);
    let mut hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let ampm = if hour < 12 { "am" } else { "pm" };
    hour %= 12;
    if hour == 0 {
        hour = 12;
    }
    format!("{hour}:{minute:02}{ampm}")
}

/// Render a reset instant (unix SECONDS) as a local 12h clock, QUALIFIED with a
/// weekday ("Fri 4:30pm") when it lands on a DIFFERENT local day than the
/// observation (or is >~20h out). Codex's SECONDARY (weekly, `window_minutes`
/// 10080) reset can be ~6 days ahead, and a bare "4:30pm" would read as *today*
/// (F4). Same-day resets stay bare "4:30pm", so claude's same-day banner is
/// unaffected (claude sets `reset_clock` via `parse_reset`, never this path).
fn fmt_reset_clock(reset_secs: i64, observed_ms: u64, local_off_secs: i64) -> String {
    let reset_local = reset_secs + local_off_secs;
    let observed_local = (observed_ms / 1000) as i64 + local_off_secs;
    let clock = fmt_clock_12h(reset_local);
    let reset_day = reset_local.div_euclid(86_400);
    let observed_day = observed_local.div_euclid(86_400);
    // qualify when it's not the same local calendar day, or is far enough out
    // that "today" would mislead even within one calendar day.
    let multi_day = reset_day != observed_day || reset_local - observed_local > 20 * 3600;
    if multi_day {
        // day 0 (1970-01-01) is a Thursday → index 4 with 0 = Sunday.
        const WD: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        let wd = WD[(reset_day + 4).rem_euclid(7) as usize];
        format!("{wd} {clock}")
    } else {
        clock
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks EVERY real usage-limit banner form (docs/019 auto-continue): the
    /// session/generic/weekly hits, a dateless weekly, and the two look-alikes
    /// that must NOT read as a hit (a credit cap and the 529 overload). Auto-
    /// continue arms on `hit` and wakes at the reset, so every field is pinned.
    #[test]
    fn usage_limit_parser_locks_every_real_banner_form() {
        // ── session limit (same-day) ──
        let session =
            parse_usage_limit("You've hit your session limit · resets 12:30am (America/Los_Angeles)", 0)
                .expect("session-limit hit must parse");
        assert!(session.hit);
        assert_eq!(session.reset_clock, "12:30am");
        assert_eq!(session.reset_date, "");
        assert_eq!(session.reset_tz, "America/Los_Angeles");
        assert!(session.reset_at_unix.is_some(), "clock+tz must resolve an instant");

        // ── generic "hit your limit" (window word dropped) — previously MISSED
        // because the anchor demanded "session/usage limit" ──
        let generic = parse_usage_limit("You've hit your limit · resets 9pm (America/New_York)", 0)
            .expect("generic hit must parse");
        assert!(generic.hit);
        assert_eq!(generic.reset_clock, "9pm");
        assert_eq!(generic.reset_date, "");
        assert_eq!(generic.reset_tz, "America/New_York");
        assert!(generic.reset_at_unix.is_some());

        // ── weekly limit with a DATE — previously MISSED + the date dropped ──
        let weekly =
            parse_usage_limit("You've hit your weekly limit · resets Jun 5 at 7am (America/Los_Angeles)", 0)
                .expect("weekly hit must parse");
        assert!(weekly.hit);
        assert_eq!(weekly.reset_clock, "7am", "must skip the bare day '5' and find '7am'");
        assert_eq!(weekly.reset_date, "Jun 5");
        assert_eq!(weekly.reset_tz, "America/Los_Angeles");
        assert!(weekly.reset_at_unix.is_some());

        // ── weekly limit WITHOUT a date (dateless) — hit, empty date ──
        let dateless =
            parse_usage_limit("You've hit your weekly limit · resets 7am (America/Los_Angeles)", 0)
                .expect("dateless weekly hit must parse");
        assert!(dateless.hit);
        assert_eq!(dateless.reset_clock, "7am");
        assert_eq!(dateless.reset_date, "");
        assert_eq!(dateless.reset_tz, "America/Los_Angeles");
        assert!(dateless.reset_at_unix.is_some());

        // ── credit cap: a hard block that surfaces as a HIT but carries NO reset,
        // so auto-continue can never arm on it (the `reset_known` gate) while the
        // session stops reading as plain Idle. ──
        let credit =
            parse_usage_limit("You've reached your Opus limit. Run /usage-credits to add more.", 0)
                .expect("a credit cap must surface as a hit, not vanish");
        assert!(credit.hit, "a credit cap is a hard block");
        assert_eq!(credit.reset_clock, "", "a credit cap prints no reset clock");
        assert_eq!(credit.reset_date, "");
        assert!(
            credit.reset_at_unix.is_none(),
            "no reset instant → auto-continue's reset_known gate keeps it from ever arming"
        );

        // ── 529 overload: "(not your usage limit)" — never a usage limit ──
        assert!(
            parse_usage_limit(
                "API Error: server overloaded (not your usage limit) · Rate limited",
                0
            )
            .is_none(),
            "the overload notice must not be read as a usage limit"
        );

        // ── the benign 92% warning still parses as a warning (not a hit) ──
        let warn = parse_usage_limit(
            "You've used 92% of your session limit · resets 4:30pm (America/Los_Angeles)",
            0,
        )
        .expect("92% warning must parse");
        assert!(!warn.hit, "a used-N% line is a warning, never a hard hit");
        assert_eq!(warn.percent, Some(92));
        assert_eq!(warn.reset_clock, "4:30pm");
        assert_eq!(warn.reset_date, "");
        assert_eq!(warn.reset_tz, "America/Los_Angeles");
    }

    /// The display helpers claude parity leans on: a live countdown off
    /// `reset_at_unix` and the weekly-date label off `reset_date` — both pure.
    #[test]
    fn reset_countdown_and_label_format() {
        let mk = |reset_at: Option<i64>, date: &str, clock: &str| UsageLimit {
            hit: true,
            percent: None,
            reset_clock: clock.into(),
            reset_tz: String::new(),
            reset_date: date.into(),
            reset_at_unix: reset_at,
            since_ms: 0,
        };
        let now_ms = 1_000_000u64; // now = 1000s
        let now_s = 1_000i64;
        assert_eq!(mk(None, "", "").reset_countdown(now_ms), ""); // unknown reset
        assert_eq!(mk(Some(now_s - 60), "", "").reset_countdown(now_ms), ""); // already past
        assert_eq!(mk(Some(now_s + 30), "", "").reset_countdown(now_ms), "<1m");
        assert_eq!(mk(Some(now_s + 12 * 60), "", "").reset_countdown(now_ms), "12m");
        assert_eq!(mk(Some(now_s + 2 * 3600 + 15 * 60), "", "").reset_countdown(now_ms), "2h 15m");
        assert_eq!(mk(Some(now_s + 3 * 3600), "", "").reset_countdown(now_ms), "3h"); // whole hours
        assert_eq!(mk(Some(now_s + 5 * 86400 + 3 * 3600), "", "").reset_countdown(now_ms), "5d 3h");
        assert_eq!(mk(Some(now_s + 2 * 86400), "", "").reset_countdown(now_ms), "2d"); // whole days
        // label: weekly date+clock / same-day clock / date-only / none.
        assert_eq!(mk(None, "Jun 5", "7am").reset_label(), "Jun 5, 7am");
        assert_eq!(mk(None, "", "9:50pm").reset_label(), "9:50pm");
        assert_eq!(mk(None, "Jun 5", "").reset_label(), "Jun 5");
        assert_eq!(mk(None, "", "").reset_label(), "");
    }

    /// The reset instant is DST-correct, rolls a passed clock to tomorrow, and
    /// lands a weekly banner on its printed date — proven against independently
    /// constructed `jiff` instants (pure logic; never spawns a CLI).
    #[test]
    fn banner_reset_instant_is_dst_correct_and_next_occurrence() {
        use jiff::civil::date;
        let tz = jiff::tz::TimeZone::get("America/Los_Angeles").unwrap();
        // "now" = 2026-03-08 00:30 local, BEFORE the 02:00 spring-forward gap.
        let now = tz.to_zoned(date(2026, 3, 8).at(0, 30, 0, 0)).unwrap();
        let now_ms = now.timestamp().as_millisecond() as u64;

        // a 9pm reset the SAME evening is AFTER the DST gap → PDT (-7), not PST.
        // Re-resolving the wall clock through the zone (not adding a fixed
        // offset) is what makes this land at 21:00 local rather than 20:00/22:00.
        let got = banner_reset_instant("9pm", "", "America/Los_Angeles", now_ms).unwrap();
        let want = tz.to_zoned(date(2026, 3, 8).at(21, 0, 0, 0)).unwrap().timestamp().as_second();
        assert_eq!(got, want, "9pm today must resolve at the post-DST offset");

        // 12am (00:00) already passed 00:30 → rolls to tomorrow's civil day.
        let got2 = banner_reset_instant("12am", "", "America/Los_Angeles", now_ms).unwrap();
        let want2 = tz.to_zoned(date(2026, 3, 9).at(0, 0, 0, 0)).unwrap().timestamp().as_second();
        assert_eq!(got2, want2, "a clock earlier than now must roll to tomorrow");

        // the weekly date wins: Jun 5 at 7am, not merely the next 7am.
        let got3 = banner_reset_instant("7am", "Jun 5", "America/Los_Angeles", now_ms).unwrap();
        let want3 = tz.to_zoned(date(2026, 6, 5).at(7, 0, 0, 0)).unwrap().timestamp().as_second();
        assert_eq!(got3, want3, "a weekly reset must land on the printed date");

        // a missing zone or clock yields no instant (auto-continue can't arm).
        assert!(banner_reset_instant("9pm", "", "", now_ms).is_none());
        assert!(banner_reset_instant("", "", "America/Los_Angeles", now_ms).is_none());
    }

    #[test]
    fn codex_rate_limits_map_to_usage_limit() {
        use crate::transcript::{CodexRateLimits, RateWindow};
        // HIT: primary exhausted → hit=true, percent=max, reset from the
        // exhausted window's resets_at, in local zone (UTC here → off=0).
        let hit = CodexRateLimits {
            observed_ms: 1_000_000,
            primary: Some(RateWindow {
                used_percent: 100.0,
                window_minutes: 300,
                resets_at: Some(16 * 3600 + 30 * 60), // 16:30 UTC on day 0
                resets_in_seconds: None,
            }),
            secondary: Some(RateWindow {
                used_percent: 73.4,
                window_minutes: 10080,
                resets_at: None,
                resets_in_seconds: Some(600),
            }),
        }
        .to_usage_limit(0)
        .expect("windows present → Some");
        assert!(hit.hit);
        assert_eq!(hit.percent, Some(100));
        assert_eq!(hit.reset_clock, "4:30pm");
        assert_eq!(hit.reset_tz, "");
        assert_eq!(hit.since_ms, 1_000_000);

        // WARNING: neither exhausted → hit=false, percent rounds the busier
        // window; reset falls back to primary via resets_in_seconds off observed.
        let warn = CodexRateLimits {
            observed_ms: 0,
            primary: Some(RateWindow {
                used_percent: 91.6,
                window_minutes: 300,
                resets_at: None,
                resets_in_seconds: Some(9 * 3600), // 09:00 after epoch 00:00
            }),
            secondary: None,
        }
        .to_usage_limit(0)
        .expect("primary present → Some");
        assert!(!warn.hit);
        assert_eq!(warn.percent, Some(92));
        assert_eq!(warn.reset_clock, "9:00am");

        // no windows → None (an empty rate_limits can't fabricate a clear).
        assert!(CodexRateLimits {
            observed_ms: 0,
            primary: None,
            secondary: None,
        }
        .to_usage_limit(0)
        .is_none());

        // F4: a WEEKLY (secondary) hit resets ~6 days out — the clock must be
        // QUALIFIED with the weekday, not a bare "4:30pm" that implies today.
        // observed at epoch (day 0 = Thu 1970-01-01 00:00 UTC); reset 6d 16h30m
        // later lands on day 6 = Wed at 16:30 local → "Wed 4:30pm".
        let weekly = CodexRateLimits {
            observed_ms: 0,
            primary: Some(RateWindow {
                used_percent: 50.0,
                window_minutes: 300,
                resets_at: None,
                resets_in_seconds: Some(3600),
            }),
            secondary: Some(RateWindow {
                used_percent: 100.0, // exhausted → reset source is this window
                window_minutes: 10080,
                resets_at: None,
                resets_in_seconds: Some(6 * 86_400 + 16 * 3600 + 30 * 60),
            }),
        }
        .to_usage_limit(0)
        .expect("windows present → Some");
        assert!(weekly.hit);
        assert_eq!(weekly.percent, Some(100));
        assert_eq!(weekly.reset_clock, "Wed 4:30pm");

        // PART 3 / the unexpirable case: a window may carry `used_percent` with NO
        // reset info at all (`rate_window` needs only `used_percent`), so a
        // 100%-forever rollout maps to hit + `reset_at_unix: None` — nothing to
        // expire ON. It must age out instead (see `undated_hit_ages_out_...`).
        let undated = CodexRateLimits {
            observed_ms: 1_000,
            primary: Some(RateWindow {
                used_percent: 100.0,
                window_minutes: 300,
                resets_at: None,
                resets_in_seconds: None,
            }),
            secondary: None,
        }
        .to_usage_limit(0)
        .expect("primary present → Some");
        assert!(undated.hit);
        assert_eq!(undated.reset_at_unix, None, "no reset info → no instant to expire on");
        assert_eq!(undated.reset_clock, "");
    }

    /// An arbitrary reset instant (unix seconds) + the observation that preceded it.
    const RESET_AT: i64 = 2_000;
    const SEEN_MS: u64 = 1_000_000; // 1000s — before RESET_AT

    fn limit(hit: bool, since_ms: u64, reset_at_unix: Option<i64>) -> UsageLimit {
        UsageLimit {
            hit,
            percent: None,
            reset_clock: "4:30pm".into(),
            reset_date: String::new(),
            reset_tz: "America/Los_Angeles".into(),
            reset_at_unix,
            since_ms,
        }
    }

    /// The VIEW expiry (docs/019 stale-limit fix): neither CLI ever retracts a limit,
    /// so a passed reset must expire on the CLOCK — but only once we're past the
    /// grace window, and never for a limit we can't date.
    #[test]
    fn usage_limit_expires_only_once_its_reset_is_grace_past() {
        let l = limit(true, SEEN_MS, Some(RESET_AT));
        assert!(!l.is_expired(SEEN_MS), "first seen ⇒ live");
        assert!(!l.is_expired(1_999_000), "before the reset ⇒ live");
        assert!(!l.is_expired(RESET_AT as u64 * 1000), "AT the reset ⇒ still in grace");
        assert!(
            !l.is_expired((RESET_AT + LIMIT_EXPIRY_GRACE_SECS - 1) as u64 * 1000),
            "1s inside the grace window ⇒ live"
        );
        assert!(
            l.is_expired((RESET_AT + LIMIT_EXPIRY_GRACE_SECS) as u64 * 1000),
            "grace elapsed ⇒ DEAD"
        );
        assert!(l.is_expired(9_999_999_000), "long past ⇒ DEAD");
    }

    /// The codex 1970 landmine: a timestamp-less rollout line yields `observed_ms: 0`,
    /// and a `resets_in_seconds` then resolves against the EPOCH. Expiring on that
    /// clock would kill a LIVE limit instantly — an unknown first-observation time is
    /// never aged out. Same for a reset instant that predates the observation.
    #[test]
    fn usage_limit_with_unknown_since_never_expires() {
        assert!(
            !limit(true, 0, Some(RESET_AT)).is_expired(9_999_999_000),
            "since_ms == 0 ⇒ undatable, NOT expired"
        );
        assert!(!limit(false, 0, None).is_expired(9_999_999_000));
        // an incoherent reset (before we ever saw the banner) is distrusted, so the
        // limit falls through to the AGE rule rather than expiring on a bogus clock.
        let incoherent = limit(true, SEEN_MS, Some(5)); // 5s ≪ since_ms (1000s)
        assert!(!incoherent.is_expired(SEEN_MS + 1000), "bogus clock ⇒ not expired ON it");
        assert!(
            incoherent.is_expired(SEEN_MS + LIMIT_UNDATED_MAX_AGE_MS),
            "…but it still ages out"
        );
    }

    /// A HIT with no resolvable reset (tz-less banner / a codex window with neither
    /// `resets_at` nor `resets_in_seconds`) can't expire on a clock — it must age out,
    /// or a 100%-forever rollout pins a permanent ⛔. A benign WARNING is not a block,
    /// so it is left alone.
    #[test]
    fn undated_hit_ages_out_but_undated_warning_stands() {
        let hit = limit(true, SEEN_MS, None);
        assert!(!hit.is_expired(SEEN_MS), "just seen ⇒ live");
        assert!(
            !hit.is_expired(SEEN_MS + LIMIT_UNDATED_MAX_AGE_MS - 1),
            "1ms short of the horizon ⇒ live"
        );
        assert!(
            hit.is_expired(SEEN_MS + LIMIT_UNDATED_MAX_AGE_MS),
            "past the horizon ⇒ no longer believed"
        );
        let warning = limit(false, SEEN_MS, None);
        assert!(
            !warning.is_expired(SEEN_MS + 100 * LIMIT_UNDATED_MAX_AGE_MS),
            "a warning is not a block — nothing to time out"
        );
    }

    /// `same_banner` compares the banner TEXT, never the resolved instant, and
    /// `carry_forward` pins BOTH `since_ms` and `reset_at_unix` from the first sight.
    #[test]
    fn same_banner_is_text_equality_and_carries_the_first_resolution() {
        let old = limit(true, SEEN_MS, Some(RESET_AT));
        // same words, a DIFFERENT resolved instant (a re-parse rolled it forward).
        let reparse = limit(true, 9_000_000, Some(RESET_AT + 86_400));
        assert!(reparse.same_banner(&old), "same text ⇒ the same banner");
        let folded = UsageLimit::carry_forward(reparse, Some(&old));
        assert_eq!(folded.since_ms, SEEN_MS, "age ticks from FIRST sight");
        assert_eq!(
            folded.reset_at_unix,
            Some(RESET_AT),
            "the reset is whatever we resolved when we FIRST saw this banner"
        );

        // a genuinely different banner replaces it wholesale (no carry-forward).
        let mut other = limit(true, 9_000_000, Some(RESET_AT + 86_400));
        other.reset_clock = "9:00pm".into();
        assert!(!other.same_banner(&old));
        let fresh = UsageLimit::carry_forward(other, Some(&old));
        assert_eq!(fresh.since_ms, 9_000_000);
        assert_eq!(fresh.reset_at_unix, Some(RESET_AT + 86_400));
        // …and so does a warning→hit transition on otherwise identical text.
        assert!(!limit(false, 0, Some(RESET_AT)).same_banner(&old));
    }

    /// REGRESSION (the stale-forever laundering path): claude's banner text STAYS on
    /// the emulator grid — a cell only clears when something overwrites it — so any
    /// repaint (a window resize bumps `dirty`) re-parses the very same words. Because
    /// `banner_reset_instant` rolls a PASSED clock forward to TOMORROW, a naive
    /// re-parse resolves a reset 24h out and, with `reset_at_unix` in `same_banner`,
    /// would read as a BRAND-NEW banner — resetting `since_ms` and defeating expiry
    /// forever. Text-only `same_banner` + `carry_forward` pins the first resolution,
    /// so the stale banner stays expired.
    #[test]
    fn stale_claude_banner_reparsed_after_reset_does_not_roll_forward_to_tomorrow() {
        const BANNER: &str = "You've hit your session limit · resets 4:30pm (America/Los_Angeles)";
        let t0: u64 = 1_700_000_000_000; // arbitrary wall clock
        let first = parse_usage_limit(BANNER, t0).expect("banner must parse");
        let reset = first.reset_at_unix.expect("clock + tz ⇒ a resolved instant");
        assert!(!first.is_expired(t0), "freshly painted ⇒ live");

        // an hour PAST the reset, the same banner is still sitting on the grid.
        let later = (reset as u64 + 3600) * 1000;
        let reparsed = parse_usage_limit(BANNER, later).expect("same text still parses");
        assert!(
            reparsed.reset_at_unix.unwrap() > reset,
            "the naive re-parse DOES roll forward — this is the bug being contained"
        );

        let folded = UsageLimit::carry_forward(reparsed, Some(&first));
        assert_eq!(folded.reset_at_unix, Some(reset), "pinned to the first resolution");
        assert_eq!(folded.since_ms, t0, "and to the first sighting");
        assert!(folded.is_expired(later), "so the stale banner STAYS expired");
    }

    /// REGRESSION (the other side of that containment): `same_banner` is pure TEXT
    /// equality, so a genuinely NEW block a day later whose banner prints the IDENTICAL
    /// words matches the DEAD one — and a blind `carry_forward` would pin it to
    /// YESTERDAY's `since_ms` + reset, burying a live block under a corpse: born
    /// already-expired ⇒ no chip, no BLOCKED tier, and auto-continue (which reads the
    /// limit RAW) arms on a reset a day in the PAST and instantly gives up.
    /// `is_restatement_of` separates them on the LEAD: a stale re-parse has been rolled
    /// a full day forward by `banner_reset_instant`; a real new block resets IMMINENTLY.
    #[test]
    fn a_new_block_with_identical_banner_text_is_not_buried_under_the_dead_one() {
        const BANNER: &str = "You've hit your session limit · resets 3pm (America/Los_Angeles)";
        let day1: u64 = 1_700_000_000_000;
        let first = parse_usage_limit(BANNER, day1).expect("banner must parse");
        let reset1 = first.reset_at_unix.expect("clock + tz ⇒ a resolved instant");

        // ~24h later the user (daily rhythm) hits the limit again and claude prints
        // the very same words — a NEW block, indistinguishable by TEXT.
        let day2 = day1 + 24 * 3600 * 1000;
        let second = parse_usage_limit(BANNER, day2).expect("the same words parse again");
        assert!(second.same_banner(&first), "identical text ⇒ same_banner matches");
        assert!(first.is_expired(day2), "…and yesterday's block is long dead");

        let folded = UsageLimit::carry_forward(second, Some(&first));
        assert_eq!(folded.since_ms, day2, "a NEW block was seen NOW, not yesterday");
        assert!(
            folded.reset_at_unix.unwrap() > reset1
                && folded.reset_at_unix.unwrap() * 1000 > day2 as i64,
            "…and its reset is AHEAD of us, not a day in the past"
        );
        assert!(!folded.is_expired(day2), "so the chip SHOWS instead of being born expired");
    }

    /// REGRESSION (docs/019, the undated 6h age-out hiding a LIVE block): on a narrow
    /// window the footer SOFT-WRAPS, and `bottom_plain` emits one line per GRID row —
    /// so the zone arrives split across rows ("(America/Los_\nAngeles)"). A row-blind
    /// scan loses it, `banner_reset_instant` returns None, the hit goes UNDATED, and
    /// the view ages a WEEKLY block (which runs for DAYS) out after 6h. The reset parse
    /// rejoins the rows first.
    #[test]
    fn soft_wrapped_banner_still_resolves_its_reset_instant() {
        use crate::emulator::Emulator;
        let mut emu = Emulator::new(10, 100);
        // the footer sits at the GRID BOTTOM (below the composer), which is exactly
        // where `bottom_plain` reads — put it there.
        emu.advance(
            "\r\n\r\n\r\n\r\n\r\n\r\nYou've hit your weekly limit · resets Jun 5 at 7am (America/Los_Angeles)\r\n"
                .as_bytes(),
        );
        emu.resize(10, 67); // an ordinary narrow window (reflow_terminal clamps at 40)
        let text = emu.bottom_plain(6);
        // MEASURED: the reflow splits the zone mid-token across two GRID rows.
        assert!(
            text.contains("(America/Los_Ang\neles)"),
            "the wrap must actually split the zone, else this test proves nothing: {text:?}"
        );
        let t: u64 = 1_700_000_000_000;
        let ul = parse_usage_limit(&text, t).expect("a wrapped banner is still a banner");
        assert!(ul.hit);
        assert_eq!(ul.reset_tz, "America/Los_Angeles", "the zone survives the row break");
        assert_eq!(ul.reset_clock, "7am");
        assert_eq!(ul.reset_date, "Jun 5");
        assert!(
            ul.reset_at_unix.is_some(),
            "…so the reset RESOLVES: the block is dated, and auto-continue can arm"
        );
    }

    /// The undated age-out is for a block with NO reset info at all (a credit cap, a
    /// codex window with no reset telemetry). A banner that printed a DATE is a WEEKLY
    /// block — it runs for DAYS, so 6h of age proves nothing and must never hide it.
    #[test]
    fn undated_but_dated_weekly_hit_is_never_aged_out() {
        let mut weekly = limit(true, SEEN_MS, None);
        weekly.reset_date = "Jun 5".into();
        weekly.reset_tz = String::new(); // the zone we failed to resolve
        assert!(
            !weekly.is_expired(SEEN_MS + 100 * LIMIT_UNDATED_MAX_AGE_MS),
            "a dated weekly block outlives the 6h undated horizon"
        );
        // …while a truly date-less hit still ages out (the 100%-forever rollout).
        assert!(limit(true, SEEN_MS, None).is_expired(SEEN_MS + LIMIT_UNDATED_MAX_AGE_MS));
    }
}
