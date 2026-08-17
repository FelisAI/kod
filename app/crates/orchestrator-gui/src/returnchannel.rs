//! returnchannel — the PURE decision logic of docs/019 slice 3 (the return
//! channel that died silently in v2). Kept in its own module, away from the
//! 8k-line main.rs, so every trust-critical rule here is unit-tested without a
//! GPUI render tree (a `#[cfg(test)]` mod in main.rs overflows the macro
//! recursion limit) and without a live daemon/LLM/DB (those are gated behind the
//! durable summary_job queue + `#[ignore]`, never here).
//!
//! NOTHING in this module does I/O, touches the `Store`, or draws: every fact
//! is passed in and every result is plain data. That is the whole point —
//! v2's channel failed because its truth signal was tangled in live state that
//! could silently stop. These are the arithmetic pieces the live wiring stands
//! on, each provable in isolation:
//!   1. TRUTH METER  — upstream-vs-derived lag (NOT queue state).
//!   2. DERIVED BUILDING — ◔ iff an alive/<48h dispatch|declared link (never a touch).
//!   3. TOUCH TAP — parse tool-use file paths + intersect anchors + weight.
//!   4. CHIP EPISTEMICS — dispatch/declared/observed-hollow/whisper tiering + drift.
//!   5. COALESCING + EXPIRY — one card per (session,node); asymmetric 7d expiry.
//!   6. TRIGGER — End / Idle / Delta summarize decision.
//!
//! The module is FOUNDATIONS: a later daemon/UI pass wires these into the live
//! loop. Until then the `pub` items have no non-test caller, so the module
//! carries a scoped `dead_code` allow rather than half-wiring main.rs.
#![allow(dead_code)]

use std::collections::HashMap;

use orchestrator_store::PartId;

// ===========================================================================
// Shared: the session_part role, mirrored as a pure enum (no store access).
// ===========================================================================

/// The role of a (session, node) link — the store keeps this as TEXT with
/// precedence dispatch > declared > trail > touch (store.rs). Mirrored here as a
/// closed enum so the pure logic can pattern-match without a DB round-trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkRole {
    /// declared intent at spawn — the solid chip.
    Dispatch,
    /// session-asserted mid-flight (`map here <node>`) — the small solid chip.
    Declared,
    /// a demoted prior dispatch — the resumption warm band, not a chip.
    Trail,
    /// observed file activity — a hollow chip only past the threshold.
    Touch,
}

impl LinkRole {
    /// Parse the store's TEXT role; unknown text folds to `Touch` (the weakest
    /// tier — an unrecognized role can never masquerade as declared intent).
    pub fn from_str(s: &str) -> LinkRole {
        match s {
            "dispatch" => LinkRole::Dispatch,
            "declared" => LinkRole::Declared,
            "trail" => LinkRole::Trail,
            _ => LinkRole::Touch,
        }
    }

    /// True iff this role expresses DECLARED INTENT — the only roles that may
    /// ever light the building glyph (docs/019 commitment 2). Observation
    /// (touch) and history (trail) never qualify.
    pub fn is_declared_intent(self) -> bool {
        matches!(self, LinkRole::Dispatch | LinkRole::Declared)
    }
}

// ===========================================================================
// 1. TRUTH METER — upstream (events) vs derived (summaries) LAG.
// ===========================================================================
//
// docs/019 commitment 4: "the map may be behind, never silently stale." The
// meter is defined as upstream-vs-derived lag, NOT queue depth — a queue-only
// meter is blind to the exact failure that killed v2 (the silent non-enqueue:
// events kept flowing while nothing was ever queued, so a job-count meter read
// a cheerful zero). We compare max(session_event ts) against max(summary ts):
// when upstream has moved past downstream beyond a small grace, the map is
// BLIND and says so.

/// One summarize cycle of slack: a summary always lands a little after the
/// events it covers, so a lag under this is just in-flight, not blindness.
pub const TRUTH_GRACE_MS: u64 = 90_000;

/// The map's honesty state for one session (or one project, folded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruthMeter {
    /// Downstream (summaries) has caught up to upstream (events) within grace.
    /// `thru_ms` = how far the evidence reaches ("evidence thru 14:32").
    Current { thru_ms: u64 },
    /// Upstream is moving while downstream is not — the map cannot see the
    /// latest work. `since_ms` = the last moment we had evidence (0 = never);
    /// `events_behind` = session_event rows past that evidence.
    Blind { since_ms: u64, events_behind: u64 },
}

/// Compute the truth meter from the two timestamps + the count of events past
/// the last summary. `now_ms` is intentionally NOT an input: blindness is a
/// property of upstream-vs-downstream, not of the wall clock — a session quiet
/// for an hour with its last turn summarized is CURRENT, not blind.
///
/// - No events at all → `Current { thru_ms: summary }` (nothing to be behind on).
/// - Events but never a summary → `Blind { since_ms: 0, .. }` (never had evidence).
/// - Events newer than summary + grace → `Blind` (upstream outran downstream).
/// - Otherwise → `Current { thru_ms: summary }`.
pub fn truth_meter(
    latest_event_ms: Option<u64>,
    latest_summary_ms: Option<u64>,
    events_behind: u64,
    grace_ms: u64,
) -> TruthMeter {
    let Some(event_ms) = latest_event_ms else {
        // No upstream activity: the map is trivially current with whatever
        // (possibly no) evidence it has.
        return TruthMeter::Current {
            thru_ms: latest_summary_ms.unwrap_or(0),
        };
    };
    match latest_summary_ms {
        // Upstream exists, downstream never did — blind since the beginning.
        None => TruthMeter::Blind {
            since_ms: 0,
            events_behind,
        },
        Some(summary_ms) => {
            if event_ms > summary_ms.saturating_add(grace_ms) {
                TruthMeter::Blind {
                    since_ms: summary_ms,
                    events_behind,
                }
            } else {
                TruthMeter::Current {
                    thru_ms: summary_ms,
                }
            }
        }
    }
}

/// Fold PER-SESSION freshness into a project meter (review finding 5: a project
/// MAX(summary) vs MAX(event) let one session's fresh summary MASK another that
/// is behind). The project is Blind if ANY session is behind; `since_ms` is the
/// OLDEST evidence-gap (the worst-lagging session) and `events_behind` sums the
/// count each blind session has past its own last summary. When every session is
/// current, the project reads Current at the newest evidence.
pub fn project_truth_meter(
    sessions: &[(Option<u64>, Option<u64>, u64)],
    grace_ms: u64,
) -> TruthMeter {
    let mut worst_since: Option<u64> = None; // min summary_ms among blind sessions
    let mut total_behind = 0u64;
    let mut best_thru = 0u64;
    let mut any_blind = false;
    for (ev, sum, behind) in sessions {
        match truth_meter(*ev, *sum, *behind, grace_ms) {
            TruthMeter::Blind {
                since_ms,
                events_behind,
            } => {
                any_blind = true;
                total_behind = total_behind.saturating_add(events_behind);
                worst_since = Some(match worst_since {
                    Some(w) => w.min(since_ms),
                    None => since_ms,
                });
            }
            TruthMeter::Current { thru_ms } => best_thru = best_thru.max(thru_ms),
        }
    }
    if any_blind {
        TruthMeter::Blind {
            since_ms: worst_since.unwrap_or(0),
            events_behind: total_behind,
        }
    } else {
        TruthMeter::Current { thru_ms: best_thru }
    }
}

/// Render the meter as a plain-words line (docs/019 ruling 13: design vocabulary
/// never ships as UI copy). Wall-clock formatting is the caller's job — pass a
/// pure `fmt_clock` (ms → "14:32" / "Tue 14:05") so this module stays IO-free
/// and the sentence stays unit-testable.
pub fn truth_meter_line(m: &TruthMeter, fmt_clock: impl Fn(u64) -> String) -> String {
    match m {
        TruthMeter::Current { thru_ms } => format!("evidence thru {}", fmt_clock(*thru_ms)),
        TruthMeter::Blind {
            since_ms: 0,
            events_behind,
        } => {
            format!("map is blind — no evidence yet ({events_behind} events waiting)")
        }
        TruthMeter::Blind {
            since_ms,
            events_behind,
        } => {
            format!(
                "map is blind since {} ({events_behind} events behind)",
                fmt_clock(*since_ms)
            )
        }
    }
}

/// The TAP-health variant (docs/019 commitment 4: "tap health rides the truth
/// meter — tool-use events seen vs touch rows written over the same window").
/// The live touch tap is the riskiest plumbing (format drift across
/// claude-code/codex); this catches a silently-broken tap the same way the
/// truth meter catches a silently-stalled summarizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapHealth {
    /// The tap is writing rows in proportion to the tool-use events observed.
    Healthy,
    /// Tool-use events were seen but (almost) no touch rows landed — the parser
    /// is dropping everything. `events_seen` for the "N events, 0 recorded" line.
    Blind { events_seen: u64, rows_written: u64 },
}

/// A tap is BLIND when a meaningful volume of tool-use events produced no touch
/// rows at all — the format-drift failure mode. A partial ratio is tolerated
/// (many tool calls are Bash/out-of-anchor and legitimately write nothing); a
/// FLAT ZERO against real traffic is the alarm.
pub fn tap_health(
    tool_events_seen: u64,
    touch_rows_written: u64,
    min_events_for_alarm: u64,
) -> TapHealth {
    if tool_events_seen >= min_events_for_alarm && touch_rows_written == 0 {
        TapHealth::Blind {
            events_seen: tool_events_seen,
            rows_written: touch_rows_written,
        }
    } else {
        TapHealth::Healthy
    }
}

// ===========================================================================
// 2. DERIVED BUILDING — ◔ is computed per frame, NEVER stored.
// ===========================================================================
//
// docs/019 commitment 2 / ruling 3: a node is building iff it has an alive
// dispatch|declared session link, OR such a link's own last activity is <48h
// (rendered with an age badge "built 3h ago"). Observed-touch recency NEVER
// lights ◔; part_note timestamps are excluded entirely. A state that cannot be
// stored cannot rot; fenced to declared intent, it cannot be lit by a drive-by.
//
// "Alive" is a runtime property this pure fn cannot know — the caller encodes it
// by passing `last_activity_secs = now_secs` for links whose session process is
// alive (an alive session's activity IS current), so the single <48h test
// covers both legs.

/// 48h: the window over which a dispatched-and-since-quiet node still reads as
/// "recently built" rather than dead.
pub const BUILDING_MAX_AGE_SECS: u64 = 48 * 60 * 60;

/// The derived building state — carries the age for the "built Nh ago" badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildingState {
    pub age_secs: u64,
}

/// Building iff SOME dispatch|declared link is within 48h (alive links are
/// stamped `now`, so they always qualify). Returns the FRESHEST such link's age
/// (the smallest) for the badge. Touch/trail links are ignored entirely — the
/// whole point is that observation cannot light this glyph.
pub fn derived_building(links: &[(LinkRole, u64)], now_secs: u64) -> Option<BuildingState> {
    links
        .iter()
        .filter(|(role, _)| role.is_declared_intent())
        .map(|(_, last)| now_secs.saturating_sub(*last))
        .filter(|age| *age <= BUILDING_MAX_AGE_SECS)
        .min()
        .map(|age_secs| BuildingState { age_secs })
}

// ===========================================================================
// 3. TOUCH TAP — parse tool-use file paths, intersect anchors, weight.
// ===========================================================================
//
// docs/019 return channel: "the event tap extracts tool-use file paths per turn;
// a matcher intersects part_anchor globs, writing weighted touch rows
// continuously." This is the riskiest single piece of plumbing (format drift
// across claude-code hooks / claude transcripts / codex rollouts), so it lives
// behind a VERSIONED, FAIL-SOFT interface: any unrecognized shape yields an
// empty result, never a panic, and the tap-health meter (above) makes a silent
// all-drops failure observable.

/// The parser contract version. Bump when a new agent shape is added so a
/// tap-health regression can be attributed to a specific parser generation.
pub const TOUCH_PARSER_VERSION: u32 = 1;

/// How a tool touched the filesystem — the weight axis. Mirrors host::ToolVerb
/// but is derived from the raw tool NAME (the pure parse never sees the reduced
/// event) and folds the whole tool vocabulary onto the weight ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchVerb {
    /// Edit / MultiEdit / Write / NotebookEdit / codex apply_patch — a mutation.
    Mutate,
    /// Read / NotebookRead — an in-anchor read (weak intent signal).
    Read,
    /// Grep / Glob — a search over a directory (the weakest touch).
    Search,
    /// Bash / shell / anything else — not a file touch we attribute.
    Other,
}

/// Tool NAME → verb. Unknown names fold to `Other` (weight 0) — an unrecognized
/// tool can never accrue touch mass toward a chip.
pub fn classify_tool(name: &str) -> TouchVerb {
    match name {
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" | "apply_patch" => TouchVerb::Mutate,
        "Read" | "NotebookRead" => TouchVerb::Read,
        "Grep" | "Glob" => TouchVerb::Search,
        _ => TouchVerb::Other,
    }
}

/// Per-file weight by verb, accumulated into session_part.weight. Mutations
/// dominate; drive-by reads and searches stay whispers (docs/019 chips:
/// "drive-by reads stay outline whispers"). `Other` contributes nothing.
pub fn touch_weight(verb: TouchVerb) -> f64 {
    match verb {
        TouchVerb::Mutate => 1.0,
        TouchVerb::Read => 0.3,
        TouchVerb::Search => 0.2,
        TouchVerb::Other => 0.0,
    }
}

/// One parsed tool-use event: the verb + the file paths it touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolTouch {
    pub verb: TouchVerb,
    pub paths: Vec<String>,
}

/// The file-path keys a tool_input may carry. `file_path` (Edit/Write/Read),
/// `notebook_path` (Notebook*), `path` (Grep/Glob search root).
const PATH_KEYS: [&str; 3] = ["file_path", "notebook_path", "path"];

/// Extract just the touched paths (the spec's `parse_touched_paths`). A thin
/// projection of `parse_tool_touch` — same fail-soft guarantee.
pub fn parse_touched_paths(tool_event: &serde_json::Value) -> Vec<String> {
    parse_tool_touch(tool_event)
        .map(|t| t.paths)
        .unwrap_or_default()
}

/// Parse a tool-use event to `{verb, paths}`, robust across three shapes and
/// failing soft (any missing/foreign field → the next strategy → `None`):
///   1. claude-code PreToolUse HOOK — `{tool_name, tool_input:{file_path,…}}`
///      (the LIVE tap source: what the daemon's hooks emit per turn).
///   2. claude TRANSCRIPT assistant turn — `{message:{content:[{type:"tool_use",
///      name, input:{…}}]}}` (the summarize-time fallback source).
///   3. codex ROLLOUT function_call — `{type:"function_call", name,
///      arguments:"<json-string>"}` (arguments is a JSON-encoded string).
///
/// Returns `None` only when no known tool shape is present at all.
pub fn parse_tool_touch(ev: &serde_json::Value) -> Option<ToolTouch> {
    parse_hook_shape(ev)
        .or_else(|| parse_transcript_shape(ev))
        .or_else(|| parse_codex_shape(ev))
}

/// Pull the value under any of `PATH_KEYS`, as a string, into `out` (dedup-free
/// here; the caller dedups). Non-string / absent keys are skipped.
fn collect_paths(input: &serde_json::Value, out: &mut Vec<String>) {
    for k in PATH_KEYS {
        if let Some(p) = input.get(k).and_then(|v| v.as_str()) {
            let p = p.trim();
            if !p.is_empty() && !out.iter().any(|x| x == p) {
                out.push(p.to_string());
            }
        }
    }
}

/// Shape 1: claude-code PreToolUse hook (top-level tool_name + tool_input).
fn parse_hook_shape(ev: &serde_json::Value) -> Option<ToolTouch> {
    let name = ev
        .get("tool_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let verb = classify_tool(name);
    let mut paths = Vec::new();
    if let Some(input) = ev.get("tool_input") {
        collect_paths(input, &mut paths);
    }
    Some(ToolTouch { verb, paths })
}

/// Shape 2: claude transcript assistant message with a tool_use content block.
fn parse_transcript_shape(ev: &serde_json::Value) -> Option<ToolTouch> {
    let content = ev
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())?;
    let block = content
        .iter()
        .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))?;
    let name = block
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let verb = classify_tool(name);
    let mut paths = Vec::new();
    if let Some(input) = block.get("input") {
        collect_paths(input, &mut paths);
    }
    Some(ToolTouch { verb, paths })
}

/// Shape 3: codex rollout function_call. `arguments` is a JSON-ENCODED STRING;
/// the tool name lives in `name` (mirrors extract::files_touched_text).
fn parse_codex_shape(ev: &serde_json::Value) -> Option<ToolTouch> {
    // codex nests the call under `payload` in some rollout versions.
    let call = ev.get("payload").unwrap_or(ev);
    if call.get("type").and_then(|t| t.as_str()) != Some("function_call") {
        return None;
    }
    let name = call
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let verb = classify_tool(name);
    let mut paths = Vec::new();
    if let Some(args_str) = call.get("arguments").and_then(|a| a.as_str()) {
        if let Ok(args) = serde_json::from_str::<serde_json::Value>(args_str) {
            collect_paths(&args, &mut paths);
        }
    }
    Some(ToolTouch { verb, paths })
}

/// Whether a touched path intersects ANY of a node's anchor globs, reusing the
/// same `glob` matcher orchestrator-store uses for anchors — but PURELY
/// (`Pattern::matches` on the string, no filesystem walk).
///
/// Robust to absolute-vs-relative mismatch: anchors are repo-relative globs
/// (`crates/gui/src/**`) while a tool-use path is often absolute
/// (`/Users/…/crates/gui/src/main.rs`). So each glob is tested against the full
/// path AND against every `/`-boundary suffix of it — a relative anchor matches
/// an absolute path at the right component boundary, and nowhere spurious.
pub fn touch_intersects_anchor(path: &str, globs: &[String]) -> bool {
    let path = path.trim();
    if path.is_empty() {
        return false;
    }
    for g in globs {
        let g = g.trim();
        if g.is_empty() {
            continue;
        }
        let Ok(pat) = glob::Pattern::new(g) else {
            continue;
        };
        if pat.matches(path) {
            return true;
        }
        // Try each suffix starting after a '/', so `a/b/**` matches
        // `/repo/a/b/c.rs` at the `a/b/c.rs` boundary.
        let bytes = path.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'/' && i + 1 < path.len() && pat.matches(&path[i + 1..]) {
                return true;
            }
        }
    }
    false
}

// ===========================================================================
// 4. CHIP EPISTEMICS — the tier drawn on the glass + drift detection.
// ===========================================================================
//
// docs/019 chips (T4): solid = dispatch · small solid = declared · hollow dotted
// = observed touch above threshold (≥3 distinct files OR ≥10min in-anchor) ·
// drive-by reads stay whispers. Observation NEVER silently becomes intent.

/// The observed-touch promotion threshold: ≥ this many distinct in-anchor files.
pub const TOUCH_FILES_THRESHOLD: usize = 3;
/// …OR ≥ this many seconds of in-anchor activity (10 minutes).
pub const TOUCH_SECS_THRESHOLD: u64 = 600;
/// …OR this much accumulated touch weight (e.g. 3 mutating edits to one file —
/// one distinct file but real, sustained work). A third path to the hollow chip
/// so repeated edits of a single anchored file aren't stuck as a whisper.
pub const TOUCH_WEIGHT_THRESHOLD: f64 = 3.0;

/// What a session's chip looks like on a given node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipTier {
    /// solid — dispatched (declared intent at spawn).
    Dispatch,
    /// small solid — session-declared mid-flight (`map here`).
    Declared,
    /// hollow dotted — observed touch above threshold.
    ObservedHollow,
    /// an outline whisper — a drive-by below threshold; not a chip.
    Whisper,
}

/// Whether observed touch mass has crossed the promotion threshold (any of the
/// three axes). Split out so it's independently testable and reused by drift.
pub fn observed_meets_threshold(
    touch_weight: f64,
    distinct_files: usize,
    in_anchor_secs: u64,
) -> bool {
    distinct_files >= TOUCH_FILES_THRESHOLD
        || in_anchor_secs >= TOUCH_SECS_THRESHOLD
        || touch_weight >= TOUCH_WEIGHT_THRESHOLD
}

/// The chip tier for a (session, node) link. Declared intent (dispatch/declared)
/// renders by ROLE regardless of touch stats. A touch/trail link renders as a
/// hollow chip only past the threshold — otherwise it's a whisper. Observation
/// can never reach the solid tiers here: promotion to intent is a CARDED re-file
/// (`drift_should_refile`), never a silent chip upgrade.
pub fn chip_tier(
    role: LinkRole,
    touch_weight: f64,
    distinct_files: usize,
    in_anchor_secs: u64,
) -> ChipTier {
    match role {
        LinkRole::Dispatch => ChipTier::Dispatch,
        LinkRole::Declared => ChipTier::Declared,
        LinkRole::Trail | LinkRole::Touch => {
            if observed_meets_threshold(touch_weight, distinct_files, in_anchor_secs) {
                ChipTier::ObservedHollow
            } else {
                ChipTier::Whisper
            }
        }
    }
}

/// Drift ratio: observed mass on another node must OUTWEIGH the dispatched home
/// by this factor before a re-file is even proposed (docs/019: "≈3× for 15min").
pub const DRIFT_RATIO: f64 = 3.0;
/// …and it must be SUSTAINED for at least this long (15 minutes) — a brief spike
/// on a neighbouring file is not a change of intent.
pub const DRIFT_MIN_SECS: u64 = 900;

/// Whether a session's touch mass on some OTHER node has drifted far enough from
/// its dispatched home to propose a CARDED re-file (docs/019 chips: "when touch
/// mass on B outweighs dispatched A (≈3× for 15min) a carded re-file proposal
/// appears — observation PROPOSES, never rewrites").
///
/// Returns only the DECISION; the caller mints the changeset. Both masses are
/// the same unit (accumulated touch weight); `other_sustained_secs` is the wall
/// duration B has been active, enforcing the 15-minute floor separately so a
/// huge instantaneous burst can't trip it. `dispatch_mass` is floored at a tiny
/// positive so a zero-mass home (dispatched-but-idle) still needs real,
/// sustained mass on B — never a divide-by-zero, never a hair-trigger.
pub fn drift_should_refile(dispatch_mass: f64, other_mass: f64, other_sustained_secs: u64) -> bool {
    other_sustained_secs >= DRIFT_MIN_SECS
        && other_mass >= TOUCH_WEIGHT_THRESHOLD
        && other_mass >= DRIFT_RATIO * dispatch_mass.max(f64::MIN_POSITIVE)
}

// ===========================================================================
// 5. COALESCING + EXPIRY — bounded card volume, asymmetric decay.
// ===========================================================================
//
// docs/019 ruling 6 / card-volume amendment: at most ONE open auto-generated
// card per (session, node) — a newer summary SUPERSEDES its un-reviewed
// predecessor in place. Auto-proposals expire at 7d ASYMMETRICALLY: a claim that
// merely SUGGESTS (new node, detail refresh) expires to an append-only
// part_note; a claim that CONTRADICTS an asserted status never expires silently
// — it decays the node to unverified/hollow.

/// The coalescing key: at most one open auto-card per (session, node).
pub type CoalesceKey = (String, PartId);

/// Build the coalescing key for a proposal.
pub fn coalesce_key(session: &str, node: PartId) -> CoalesceKey {
    (session.to_string(), node)
}

/// Whether `incoming` (a fresh summary's proposal) SUPERSEDES `existing` (an
/// un-reviewed open card on the same key). Newer-or-equal wins in place —
/// same-key delta cards merge, never stack. Equal timestamps supersede so a
/// re-run at the same second still replaces (idempotent supersession).
pub fn supersedes(existing_created_ms: u64, incoming_created_ms: u64) -> bool {
    incoming_created_ms >= existing_created_ms
}

/// What an auto-generated proposal claims — the axis the asymmetric expiry
/// partitions on (docs/019 ruling 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalKind {
    /// A new node / detail refresh — may only ADD confidence. On expiry it
    /// lands as an append-only part_note (the channel never loses the words).
    Suggests,
    /// Contradicts an asserted status (e.g. claims a done node isn't done) —
    /// may only LOWER confidence. On expiry it decays the node to
    /// unverified/hollow so a neglected map visibly rots instead of lying.
    Contradicts,
}

/// One open auto-generated proposal, as the expiry pass sees it. Pure data —
/// the store row shape is the caller's concern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoProposal {
    pub session: String,
    pub node: PartId,
    /// when the proposal was created (ms) — the expiry clock.
    pub created_ms: u64,
    pub kind: ProposalKind,
    /// the session's own words, appended as a part_note if a Suggests expires.
    pub note_text: String,
}

impl AutoProposal {
    /// This proposal's coalescing key.
    pub fn key(&self) -> CoalesceKey {
        coalesce_key(&self.session, self.node)
    }
}

/// 7 days — the auto-proposal TTL.
pub const PROPOSAL_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// Coalesce a batch of proposals to at most ONE per (session, node): the newest
/// per key survives (docs/019: a newer summary supersedes its un-reviewed
/// predecessor). Order-independent; ties keep the later-scanned row (callers
/// pass oldest-first, so the freshest wins). Returns survivors keyed for the
/// caller to reconcile against the open-card set.
pub fn coalesce_proposals(proposals: &[AutoProposal]) -> Vec<&AutoProposal> {
    let mut winner: HashMap<CoalesceKey, &AutoProposal> = HashMap::new();
    for p in proposals {
        match winner.get(&p.key()) {
            Some(cur) if !supersedes(cur.created_ms, p.created_ms) => {}
            _ => {
                winner.insert(p.key(), p);
            }
        }
    }
    winner.into_values().collect()
}

/// Partition EXPIRED auto-proposals asymmetrically (docs/019 ruling 6):
///   - `to_note`  — expired `Suggests`: append `note_text` as a part_note.
///   - `to_decay` — expired `Contradicts`: decay the node to unverified/hollow.
///
/// Un-expired proposals stay open (in neither bucket). An age `>= ttl_ms` is
/// expired (7d exactly counts as elapsed).
///
/// The PARTITION is fully implemented and tested here. The DOWNSTREAM decay
/// ACTION (mutating the node to hollow + attaching the note) is deliberately
/// deferred: the pass that wires this into the live loop takes `to_note` (the
/// suggestion→note path) and leaves `to_decay` a DOCUMENTED STUB — the bucket is
/// computed correctly here, and nothing acts on it yet.
/// TODO(decay-sink): give `to_decay` a consumer that, for each returned
/// `Contradicts` proposal, sets its node's status to unverified/hollow and
/// attaches `note_text` as a part_note. Nothing in this function changes for it.
pub fn expire_proposals(
    proposals: &[AutoProposal],
    now_ms: u64,
    ttl_ms: u64,
) -> (Vec<&AutoProposal>, Vec<&AutoProposal>) {
    let mut to_note = Vec::new();
    let mut to_decay = Vec::new();
    for p in proposals {
        let expired = now_ms.saturating_sub(p.created_ms) >= ttl_ms;
        if !expired {
            continue;
        }
        match p.kind {
            ProposalKind::Suggests => to_note.push(p),
            ProposalKind::Contradicts => to_decay.push(p),
        }
    }
    (to_note, to_decay)
}

// ===========================================================================
// 6. TRIGGER DECISION — when to enqueue a summarize job.
// ===========================================================================
//
// docs/019 commitment 4: the return channel enqueues on session END, on idle
// ≥5min, and every ~12 turns — through the durable queue. This is the pure
// decision; the daemon owns the clock and the enqueue.

/// Idle floor: a session quiet this long WITH new events earns an Idle summary.
pub const IDLE_TRIGGER_SECS: u64 = 300;
/// Turn cadence: every ~this many new events, capture a Delta summary without
/// waiting for idle (a long-running session must not go unsummarized).
pub const DELTA_TRIGGER_EVENTS: u64 = 12;

/// Why a summarize job should be enqueued now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// the session exited — its final chapter ("what we achieved").
    End,
    /// idle ≥5min with unsummarized events — a natural pause to snapshot.
    Idle,
    /// ~12 turns accumulated since the last summary — cadence capture.
    Delta,
}

/// Decide whether to enqueue a summarize job (docs/019: End / Idle / Delta).
/// `events_since_last_summary` gates ALL triggers — summarizing zero new work is
/// wasteful and produces a duplicate row, so even End no-ops on nothing new.
///
/// Priority End > Delta > Idle: a dead session's final chapter outranks
/// everything; a full delta window fires WITHOUT waiting for idle (the whole
/// point of the cadence trigger); idle is the fallback pause-snapshot.
pub fn summarize_trigger(
    events_since_last_summary: u64,
    idle_secs: u64,
    session_ended: bool,
) -> Option<Trigger> {
    if events_since_last_summary == 0 {
        return None;
    }
    if session_ended {
        return Some(Trigger::End);
    }
    if events_since_last_summary >= DELTA_TRIGGER_EVENTS {
        return Some(Trigger::Delta);
    }
    if idle_secs >= IDLE_TRIGGER_SECS {
        return Some(Trigger::Idle);
    }
    None
}

/// The store's trigger TEXT (summary_job.trigger enum: end|delta|idle|backfill).
pub fn trigger_str(t: Trigger) -> &'static str {
    match t {
        Trigger::End => "end",
        Trigger::Idle => "idle",
        Trigger::Delta => "delta",
    }
}

// ---------------------------------------------------------------------------
// 6b. COOL-OFF — what a session's summary DEATHS earn it.
// ---------------------------------------------------------------------------
//
// The enqueue loop used to skip any session that had ever had a summary job die
// (`session_has_dead_job`). The intent was sound — a hot-looping failure would
// re-enqueue every tick and drain the shared 20/hr LLM budget, blinding every
// other session — but the mechanism was PERMANENT DEATH for a retryable
// operation, with no expiry, no cooldown, and no working revival path. It cost
// the user his standup for two days: a transient codex failure window on
// Jul 10-11 burned 3 attempts in ~3 minutes and blacklisted 10 sessions
// forever (cli_session_id survives `claude --resume`, so it outlived restarts).
//
// The replacement keeps the budget guarantee and drops the permanence: recent
// deaths buy a COOLING-OFF window that grows with how many there were, and it
// expires on its own from the death's own timestamp — no migration, no startup
// hook, no user gesture.

/// First death → wait this long before trying the session again. Long enough
/// that a rate-limit window (codex's is hours; a 3-attempt burn takes ~3 min)
/// has usually passed, short enough that a one-off blip costs the user at
/// most one summary cadence.
pub const DEATH_COOLOFF_BASE_SECS: u64 = 30 * 60;
/// The ceiling on the doubling. A session failing this persistently is broken
/// in a way retries won't fix, so it costs the budget at most ~3 attempts per
/// 8h — but it is still never abandoned, and one good night clears it.
pub const DEATH_COOLOFF_MAX_SECS: u64 = 8 * 60 * 60;
/// Deaths older than this stop counting toward escalation: the session has
/// since had a full day to prove itself, and a blip last week must not make
/// today's blip expensive. This is also what makes the cool-off self-clearing —
/// after a quiet day a session is indistinguishable from one that never failed.
pub const DEATH_DECAY_SECS: u64 = 24 * 60 * 60;

/// How recently a summary job must have DIED to still be news. BOTH failure
/// surfaces filter on it — the Standup's amber warning and the Map's per-project
/// badge — because an unfiltered count of every dead row ever is not a signal,
/// it is a scar: the user's 20 old deaths pinned a permanent "8 summary jobs
/// failed" on the Map that no amount of healing could clear, which is precisely
/// how a broken pipeline hides in plain sight. A failure surface that cannot
/// clear itself teaches the user to ignore it.
pub const FAILURE_WINDOW_SECS: u64 = 24 * 60 * 60;

/// Base retry-after for a summary job DEFERRED because the provider turned us
/// away. The store escalates it (`defer_backoff_secs`: 900s → 1800s → 3600s,
/// capped) and bounds the defers, so a rate limit is ridden out for ~8.7h —
/// past a provider's 5h usage window — and then, if it STILL hasn't lifted, the
/// job fails normally rather than deferring forever. It matches the worker's own
/// 900s rate-limit cooldown: there is no point re-claiming a job before the
/// worker itself is allowed to run again.
pub const RATE_LIMIT_DEFER_SECS: u64 = 900;
/// Base retry-after for a job deferred because the WORLD isn't ready — a codex
/// rollout with no promptable message yet. Short (the transcript usually lands
/// within a tick or two) but escalating, so a rollout that never becomes
/// promptable stops costing a claim every minute; after ~5h of doubling it runs
/// out of defers and dies like any other failure.
pub const NOT_READY_DEFER_SECS: u64 = 60;

/// Fold a session's raw death stamps into (deaths that still count, newest).
/// Deaths older than `DEATH_DECAY_SECS` are dropped — see the constant.
pub fn recent_deaths(death_ms: &[u64], now_ms: u64) -> (u32, u64) {
    let decay_ms = DEATH_DECAY_SECS.saturating_mul(1000);
    let mut count = 0u32;
    let mut newest = 0u64;
    for &t in death_ms {
        // a future stamp (clock moved backwards since the death) has age 0, so
        // it counts — `cooling_off` is where an untrustworthy stamp is handled.
        if now_ms.saturating_sub(t) < decay_ms || t > now_ms {
            count = count.saturating_add(1);
            newest = newest.max(t);
        }
    }
    (count, newest)
}

/// How long `deaths` recent deaths buy: 30min, 1h, 2h, 4h, 8h, 8h, … Doubling
/// is the right shape because the failures we must survive (a provider outage,
/// an account rate-limit window) are themselves order-of-magnitude in duration:
/// linear backoff would either retry too eagerly through a 5-hour quota window
/// or punish a single blip for hours.
pub fn cooloff_window_secs(deaths: u32) -> u64 {
    if deaths == 0 {
        return 0;
    }
    DEATH_COOLOFF_BASE_SECS
        .saturating_mul(1u64 << deaths.saturating_sub(1).min(16))
        .min(DEATH_COOLOFF_MAX_SECS)
}

/// Is this session still cooling off from its recent deaths? The ONLY reason a
/// session's summaries are skipped, and it always expires.
///
/// A death stamped in the FUTURE (the clock moved backwards, or a bogus row)
/// fails OPEN — we retry. The alternative is `now - last` saturating to 0 for
/// as long as the bogus stamp leads the clock, which is exactly the permanent
/// blacklist this function exists to abolish. An over-eager retry costs at most
/// 3 attempts against a budget that has its own hard ceiling; a permanent skip
/// costs the user his standup, silently, forever. Fail toward the recoverable
/// mistake.
pub fn cooling_off(deaths: u32, last_death_ms: u64, now_ms: u64) -> bool {
    if deaths == 0 {
        return false;
    }
    if last_death_ms > now_ms {
        return false;
    }
    let elapsed_secs = (now_ms - last_death_ms) / 1000;
    elapsed_secs < cooloff_window_secs(deaths)
}

// ===========================================================================
// 7. HEARTBEAT GATE — the ARE layer washes grey when the live link is lost.
// ===========================================================================
//
// docs/019 commitment 4: "the ARE layer is gated on a daemon heartbeat:
// heartbeat stale → chips wash grey with a 'live link lost since <t>' line …
// an honest empty beats a lying chip." Pure so the render path can ask "is the
// live link stale?" without owning the clock. The caller stamps `last_beat_ms`
// each time it successfully observes the daemon; blindness is time-since-beat,
// so a genuinely dead daemon (whose observations stop) reads stale, never a
// frozen-but-cheerful alive chip.

/// A live link this quiet reads as lost — one missed heartbeat window plus
/// slack over the ~500ms observe tick.
pub const HEARTBEAT_GRACE_MS: u64 = 6_000;

/// True iff the daemon hasn't been observed within `grace_ms` — the chips must
/// wash grey. `last_beat_ms == 0` (never observed) is stale unless `now` is
/// also 0 (the very first frame, before any tick) — the caller passes the
/// launch beat so a cold start doesn't flash "link lost".
pub fn heartbeat_stale(last_beat_ms: u64, now_ms: u64, grace_ms: u64) -> bool {
    now_ms.saturating_sub(last_beat_ms) > grace_ms
}

// ===========================================================================
// 8. MAP VERBS — the session's own `map here|done|note` markers (docs/019 T11).
// ===========================================================================
//
// docs/019: a dispatched session steers its own chip with `map here <node>`
// (declared link), claims a leaf with `map done <part> — <shipped claim>`, and
// annotates with `map note <part> :: <text>`. This is a TEXT-MARKER parse (an
// honest shim, not a robust CLI): the session writes the line in its output and
// the tap lifts it. Fail-soft — any malformed line yields None, and the caller
// still fences every apply to the session's dispatch/declared subtree.

/// The delimiters that separate a `done`/`note` TARGET from its payload. A node
/// name may contain spaces, so a payload verb needs an explicit boundary — the
/// first occurrence of any of these splits target from claim/text.
const VERB_DELIMS: [&str; 3] = [" :: ", " — ", " -- "];

/// A session-emitted map verb (docs/019 T11), parsed off one output line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapVerb {
    /// `map here <node>` — declare the session is now working under <target>
    /// (applied as a `declared` link; observation-grade until then).
    Here { target: String },
    /// `map done <part> — <claim>` — claim <target> shipped, carrying the
    /// one-line shipped-claim payload verbatim (a done with no claim is the lie
    /// the payload guards, so it never parses).
    Done { target: String, claim: String },
    /// `map note <part> :: <text>` — append <text> to <target>'s notes
    /// (append-only, the session's own words).
    Note { target: String, text: String },
}

/// Split `rest` into (target, payload) on the FIRST verb delimiter, both sides
/// trimmed and non-empty. None when no delimiter or an empty side.
fn split_target_payload(rest: &str) -> Option<(String, String)> {
    let mut best: Option<(usize, usize)> = None; // (byte index, delim len)
    for d in VERB_DELIMS {
        if let Some(i) = rest.find(d) {
            if best.map(|(bi, _)| i < bi).unwrap_or(true) {
                best = Some((i, d.len()));
            }
        }
    }
    let (i, len) = best?;
    let target = rest[..i].trim();
    let payload = rest[i + len..].trim();
    (!target.is_empty() && !payload.is_empty()).then(|| (target.to_string(), payload.to_string()))
}

/// Parse ONE line for a leading `map <verb> …` marker (docs/019 T11). Returns
/// None for any non-marker or malformed line — never a panic, never a partial
/// verb. Leading list bullets / whitespace are tolerated so a verb inside a
/// markdown message body still lifts.
pub fn parse_map_verb(line: &str) -> Option<MapVerb> {
    let l = line
        .trim()
        .trim_start_matches(['-', '*', '`', '>', ' '])
        .trim();
    let rest = l.strip_prefix("map ")?.trim();
    if let Some(t) = rest.strip_prefix("here ") {
        let target = t.trim();
        return (!target.is_empty()).then(|| MapVerb::Here {
            target: target.to_string(),
        });
    }
    if let Some(t) = rest.strip_prefix("done ") {
        let (target, claim) = split_target_payload(t)?;
        return Some(MapVerb::Done { target, claim });
    }
    if let Some(t) = rest.strip_prefix("note ") {
        let (target, text) = split_target_payload(t)?;
        return Some(MapVerb::Note { target, text });
    }
    None
}

/// Scan a multi-line blob (a turn's assistant message) for every map verb,
/// preserving order. At most `cap` are returned so a pathological message can't
/// flood the apply path.
pub fn scan_map_verbs(text: &str, cap: usize) -> Vec<MapVerb> {
    text.lines().filter_map(parse_map_verb).take(cap).collect()
}

/// Extract `[title](path)` markdown links from a node's detail_md (docs/019:
/// "Reading links … are markdown links inside detail_md — the kickoff composer
/// parses them"). Only project-relative doc paths are kept — a `http(s)://`,
/// `mailto:`, or absolute-URL target is skipped (the kickoff reads repo files,
/// not the web). Deduped, capped, order-preserving. Pure — the caller resolves
/// + reads the paths.
pub fn parse_doc_links(detail_md: &str, cap: usize) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let bytes = detail_md.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            // find the matching `](`…`)`
            if let Some(close) = detail_md[i + 1..].find("](") {
                let title = &detail_md[i + 1..i + 1 + close];
                let rest = &detail_md[i + 1 + close + 2..];
                if let Some(end) = rest.find(')') {
                    let path = rest[..end].trim();
                    let title = title.trim();
                    let is_url = path.contains("://")
                        || path.starts_with("mailto:")
                        || path.starts_with('#');
                    if !title.is_empty()
                        && !path.is_empty()
                        && !is_url
                        && !out.iter().any(|(_, p)| p == path)
                    {
                        out.push((title.to_string(), path.to_string()));
                        if out.len() >= cap {
                            break;
                        }
                    }
                    i = i + 1 + close + 2 + end + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- 1. truth meter ---------------------------------------------------

    #[test]
    fn truth_meter_current_when_summary_keeps_up() {
        // summary at 1000, latest event 1000 → current thru 1000.
        assert_eq!(
            truth_meter(Some(1000), Some(1000), 0, TRUTH_GRACE_MS),
            TruthMeter::Current { thru_ms: 1000 }
        );
        // event a little newer but within grace → still current.
        assert_eq!(
            truth_meter(Some(1000 + 50_000), Some(1000), 1, TRUTH_GRACE_MS),
            TruthMeter::Current { thru_ms: 1000 }
        );
    }

    #[test]
    fn truth_meter_blind_when_upstream_outruns_downstream() {
        // event 200_000 past summary, well beyond grace → blind SINCE the summary.
        let m = truth_meter(Some(1_000_000), Some(800_000), 7, TRUTH_GRACE_MS);
        assert_eq!(
            m,
            TruthMeter::Blind {
                since_ms: 800_000,
                events_behind: 7
            }
        );
    }

    #[test]
    fn truth_meter_blind_from_the_start_when_never_summarized() {
        // this is the v2 failure: events flowing, zero summaries — must read BLIND,
        // not a cheerful current. since_ms 0 = never had evidence.
        assert_eq!(
            truth_meter(Some(1_000), None, 981, TRUTH_GRACE_MS),
            TruthMeter::Blind {
                since_ms: 0,
                events_behind: 981
            }
        );
    }

    #[test]
    fn project_meter_blinds_if_any_session_is_behind() {
        // review finding 5: session A fresh (event 1000, summary 2000), session B
        // behind (event 3000, no summary). The project must read BLIND — A's
        // fresh summary must not mask B.
        let sessions = [
            (Some(1000u64), Some(2000u64), 0u64), // A: current
            (Some(3000u64), None, 5u64),          // B: behind
        ];
        match project_truth_meter(&sessions, TRUTH_GRACE_MS) {
            TruthMeter::Blind {
                since_ms,
                events_behind,
            } => {
                assert_eq!(since_ms, 0, "B never summarized → worst gap is since 0");
                assert_eq!(events_behind, 5);
            }
            other => panic!("expected Blind, got {other:?}"),
        }
        // all current → project current at the newest evidence.
        let all_current = [
            (Some(1000u64), Some(1000u64), 0u64),
            (Some(2000u64), Some(2500u64), 0u64),
        ];
        assert_eq!(
            project_truth_meter(&all_current, TRUTH_GRACE_MS),
            TruthMeter::Current { thru_ms: 2500 }
        );
    }

    #[test]
    fn truth_meter_current_when_no_upstream_activity() {
        // no events at all → trivially current (nothing to be behind on).
        assert_eq!(
            truth_meter(None, Some(500), 0, TRUTH_GRACE_MS),
            TruthMeter::Current { thru_ms: 500 }
        );
        assert_eq!(
            truth_meter(None, None, 0, TRUTH_GRACE_MS),
            TruthMeter::Current { thru_ms: 0 }
        );
    }

    #[test]
    fn truth_meter_line_reads_like_a_person() {
        let clock = |ms: u64| format!("T{ms}");
        assert_eq!(
            truth_meter_line(&TruthMeter::Current { thru_ms: 1432 }, clock),
            "evidence thru T1432"
        );
        assert_eq!(
            truth_meter_line(
                &TruthMeter::Blind {
                    since_ms: 1405,
                    events_behind: 4
                },
                clock
            ),
            "map is blind since T1405 (4 events behind)"
        );
        assert_eq!(
            truth_meter_line(
                &TruthMeter::Blind {
                    since_ms: 0,
                    events_behind: 981
                },
                clock
            ),
            "map is blind — no evidence yet (981 events waiting)"
        );
    }

    #[test]
    fn tap_health_flags_a_silent_all_drops_tap() {
        // 40 tool-use events, 0 rows → the format-drift alarm.
        assert_eq!(
            tap_health(40, 0, 5),
            TapHealth::Blind {
                events_seen: 40,
                rows_written: 0
            }
        );
        // some rows landing → healthy even if not 1:1 (Bash/out-of-anchor write nothing).
        assert_eq!(tap_health(40, 3, 5), TapHealth::Healthy);
        // below the alarm floor → not enough traffic to judge.
        assert_eq!(tap_health(2, 0, 5), TapHealth::Healthy);
    }

    // ---- 2. derived building ---------------------------------------------

    #[test]
    fn building_lit_only_by_declared_intent_within_48h() {
        let now = 1_000_000;
        // an alive dispatch (activity == now) → building, age 0.
        assert_eq!(
            derived_building(&[(LinkRole::Dispatch, now)], now),
            Some(BuildingState { age_secs: 0 })
        );
        // a declared link 3h ago → building, age = 3h.
        let three_h = now - 3 * 3600;
        assert_eq!(
            derived_building(&[(LinkRole::Declared, three_h)], now),
            Some(BuildingState { age_secs: 3 * 3600 })
        );
    }

    #[test]
    fn building_never_lit_by_touch_or_trail_or_stale_dispatch() {
        let now = 1_000_000;
        // a touch 1 minute ago → NEVER building (observation can't light ◔).
        assert_eq!(derived_building(&[(LinkRole::Touch, now - 60)], now), None);
        // a trail (demoted) link, fresh → still not building.
        assert_eq!(derived_building(&[(LinkRole::Trail, now - 60)], now), None);
        // a dispatch link older than 48h → aged out.
        assert_eq!(
            derived_building(
                &[(LinkRole::Dispatch, now - BUILDING_MAX_AGE_SECS - 1)],
                now
            ),
            None
        );
        // exactly 48h still counts (inclusive boundary).
        assert_eq!(
            derived_building(&[(LinkRole::Dispatch, now - BUILDING_MAX_AGE_SECS)], now),
            Some(BuildingState {
                age_secs: BUILDING_MAX_AGE_SECS
            })
        );
    }

    #[test]
    fn building_reports_the_freshest_qualifying_link() {
        let now = 1_000_000;
        // a stale-but-valid declared (10h) and a fresh dispatch (1h): badge shows 1h.
        let links = [
            (LinkRole::Declared, now - 10 * 3600),
            (LinkRole::Touch, now - 60),
            (LinkRole::Dispatch, now - 3600),
        ];
        assert_eq!(
            derived_building(&links, now),
            Some(BuildingState { age_secs: 3600 })
        );
    }

    // ---- 3. touch tap: parse ---------------------------------------------

    #[test]
    fn parse_hook_shape_edit_write_read() {
        let edit = json!({"tool_name": "Edit", "tool_input": {"file_path": "/x/strategy.py"}});
        assert_eq!(
            parse_tool_touch(&edit),
            Some(ToolTouch {
                verb: TouchVerb::Mutate,
                paths: vec!["/x/strategy.py".into()]
            })
        );
        let read = json!({"tool_name": "Read", "tool_input": {"file_path": "/x/a.rs"}});
        assert_eq!(parse_tool_touch(&read).unwrap().verb, TouchVerb::Read);
        // the full hook envelope (hook_event_name present) parses the same.
        let hooked = json!({"hook_event_name": "PreToolUse", "tool_name": "Write", "tool_input": {"file_path": "/x/new.rs"}});
        assert_eq!(parse_touched_paths(&hooked), vec!["/x/new.rs".to_string()]);
    }

    #[test]
    fn parse_grep_notebook_and_bash() {
        // Grep carries a search `path` → Search verb.
        let grep = json!({"tool_name": "Grep", "tool_input": {"pattern": "fn foo", "path": "src/", "glob": "*.rs"}});
        assert_eq!(
            parse_tool_touch(&grep),
            Some(ToolTouch {
                verb: TouchVerb::Search,
                paths: vec!["src/".into()]
            })
        );
        // NotebookEdit → notebook_path, Mutate.
        let nb =
            json!({"tool_name": "NotebookEdit", "tool_input": {"notebook_path": "/n/x.ipynb"}});
        assert_eq!(
            parse_tool_touch(&nb),
            Some(ToolTouch {
                verb: TouchVerb::Mutate,
                paths: vec!["/n/x.ipynb".into()]
            })
        );
        // Bash → Other, no paths (we don't parse command args).
        let bash = json!({"tool_name": "Bash", "tool_input": {"command": "rm -rf /x/y.rs"}});
        assert_eq!(
            parse_tool_touch(&bash),
            Some(ToolTouch {
                verb: TouchVerb::Other,
                paths: vec![]
            })
        );
        assert!(parse_touched_paths(&bash).is_empty());
    }

    #[test]
    fn parse_transcript_and_codex_shapes() {
        // claude transcript assistant tool_use block.
        let tr = json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "ignore me"},
            {"type": "tool_use", "name": "Edit", "input": {"file_path": "/t/b.rs"}}
        ]}});
        assert_eq!(
            parse_tool_touch(&tr),
            Some(ToolTouch {
                verb: TouchVerb::Mutate,
                paths: vec!["/t/b.rs".into()]
            })
        );
        // codex rollout function_call: arguments is a JSON-encoded string.
        let cx = json!({"type": "function_call", "name": "apply_patch", "arguments": "{\"file_path\": \"/c/x.py\"}"});
        assert_eq!(
            parse_tool_touch(&cx),
            Some(ToolTouch {
                verb: TouchVerb::Mutate,
                paths: vec!["/c/x.py".into()]
            })
        );
        // codex nested under payload.
        let cxp = json!({"payload": {"type": "function_call", "name": "shell", "arguments": "{\"path\": \"/c/dir\"}"}});
        assert_eq!(
            parse_tool_touch(&cxp).unwrap().paths,
            vec!["/c/dir".to_string()]
        );
    }

    #[test]
    fn parse_fails_soft_on_garbage() {
        // no known shape → None, never a panic.
        assert_eq!(parse_tool_touch(&json!({"random": "noise"})), None);
        assert_eq!(parse_tool_touch(&json!("just a string")), None);
        assert_eq!(parse_tool_touch(&serde_json::Value::Null), None);
        assert!(parse_touched_paths(&json!({"tool_name": ""})).is_empty());
        // malformed codex arguments (not JSON) → the tool is recognized, 0 paths.
        let bad = json!({"type": "function_call", "name": "apply_patch", "arguments": "not json{"});
        assert_eq!(
            parse_tool_touch(&bad),
            Some(ToolTouch {
                verb: TouchVerb::Mutate,
                paths: vec![]
            })
        );
    }

    // ---- 3. touch tap: anchor intersection + weight ----------------------

    #[test]
    fn touch_intersects_relative_anchor_against_absolute_path() {
        let anchors = vec!["crates/gui/src/**".to_string()];
        // absolute path matches the relative anchor at the component boundary.
        assert!(touch_intersects_anchor(
            "/Users/t/repo/crates/gui/src/main.rs",
            &anchors
        ));
        // relative path matches directly.
        assert!(touch_intersects_anchor("crates/gui/src/main.rs", &anchors));
        // a different subtree does NOT match.
        assert!(!touch_intersects_anchor(
            "/Users/t/repo/crates/store/src/lib.rs",
            &anchors
        ));
    }

    #[test]
    fn touch_intersects_exact_and_extension_globs() {
        assert!(touch_intersects_anchor(
            "/a/b/store.rs",
            &["**/store.rs".to_string()]
        ));
        assert!(touch_intersects_anchor(
            "src/x.py",
            &["src/*.py".to_string()]
        ));
        assert!(!touch_intersects_anchor(
            "src/x.rs",
            &["src/*.py".to_string()]
        ));
        // empty inputs never match, never panic.
        assert!(!touch_intersects_anchor("", &["**".to_string()]));
        assert!(!touch_intersects_anchor("/a/b.rs", &[]));
        assert!(!touch_intersects_anchor("/a/b.rs", &["".to_string()]));
        // a broken glob is skipped, not fatal.
        assert!(!touch_intersects_anchor("/a/b.rs", &["[".to_string()]));
    }

    #[test]
    fn touch_weight_orders_mutate_above_reads() {
        assert!(touch_weight(TouchVerb::Mutate) > touch_weight(TouchVerb::Read));
        assert!(touch_weight(TouchVerb::Read) > touch_weight(TouchVerb::Search));
        assert_eq!(touch_weight(TouchVerb::Other), 0.0);
        assert_eq!(classify_tool("MultiEdit"), TouchVerb::Mutate);
        assert_eq!(classify_tool("Glob"), TouchVerb::Search);
        assert_eq!(classify_tool("WebFetch"), TouchVerb::Other);
    }

    // ---- 4. chip epistemics ----------------------------------------------

    #[test]
    fn chip_tier_by_role_then_threshold() {
        // declared intent renders by role, ignoring touch stats.
        assert_eq!(chip_tier(LinkRole::Dispatch, 0.0, 0, 0), ChipTier::Dispatch);
        assert_eq!(chip_tier(LinkRole::Declared, 0.0, 0, 0), ChipTier::Declared);
        // a touch below all thresholds is a whisper, not a chip.
        assert_eq!(chip_tier(LinkRole::Touch, 0.5, 1, 60), ChipTier::Whisper);
        // ≥3 distinct files → hollow.
        assert_eq!(
            chip_tier(LinkRole::Touch, 0.0, 3, 0),
            ChipTier::ObservedHollow
        );
        // ≥10min in-anchor → hollow (even 1 file).
        assert_eq!(
            chip_tier(LinkRole::Touch, 0.0, 1, 600),
            ChipTier::ObservedHollow
        );
        // repeated edits to one file (weight ≥3) → hollow via the weight axis.
        assert_eq!(
            chip_tier(LinkRole::Touch, 3.0, 1, 30),
            ChipTier::ObservedHollow
        );
    }

    #[test]
    fn observed_threshold_is_an_or_over_three_axes() {
        assert!(observed_meets_threshold(0.0, 3, 0));
        assert!(observed_meets_threshold(0.0, 0, 600));
        assert!(observed_meets_threshold(3.0, 0, 0));
        assert!(!observed_meets_threshold(2.9, 2, 599));
    }

    #[test]
    fn drift_refile_needs_ratio_and_sustained_time() {
        // B 4× A's mass, 20min sustained → propose.
        assert!(drift_should_refile(1.0, 4.0, 1200));
        // same masses but only 5min → NOT yet (the 15min floor).
        assert!(!drift_should_refile(1.0, 4.0, 300));
        // 15min but only 2× → NOT yet (the 3× ratio).
        assert!(!drift_should_refile(1.0, 2.0, 900));
        // dispatched-but-idle home (0 mass): B still needs real, sustained mass.
        assert!(drift_should_refile(0.0, 5.0, 1000));
        assert!(!drift_should_refile(0.0, 2.0, 1000)); // below the absolute weight floor
    }

    // ---- 5. coalescing + expiry ------------------------------------------

    fn prop(session: &str, node: PartId, created_ms: u64, kind: ProposalKind) -> AutoProposal {
        AutoProposal {
            session: session.into(),
            node,
            created_ms,
            kind,
            note_text: format!("{session}@{created_ms}"),
        }
    }

    #[test]
    fn coalesce_keeps_one_newest_per_session_node() {
        let props = vec![
            prop("s1", 10, 100, ProposalKind::Suggests),
            prop("s1", 10, 300, ProposalKind::Suggests), // supersedes the 100 one
            prop("s1", 20, 200, ProposalKind::Suggests), // different node — kept
            prop("s2", 10, 150, ProposalKind::Suggests), // different session — kept
        ];
        let survivors = coalesce_proposals(&props);
        assert_eq!(survivors.len(), 3, "one per (session,node)");
        // the (s1,10) survivor is the NEWEST (created 300).
        let s1_10 = survivors
            .iter()
            .find(|p| p.session == "s1" && p.node == 10)
            .unwrap();
        assert_eq!(s1_10.created_ms, 300);
    }

    #[test]
    fn supersedes_is_newer_or_equal() {
        assert!(supersedes(100, 200));
        assert!(
            supersedes(100, 100),
            "an equal-ts re-run replaces in place (idempotent)"
        );
        assert!(
            !supersedes(200, 100),
            "an older proposal never supersedes a newer open card"
        );
    }

    #[test]
    fn expiry_is_asymmetric_suggests_to_note_contradicts_to_decay() {
        let now = 10 * PROPOSAL_TTL_MS;
        let props = vec![
            prop("s1", 1, now - PROPOSAL_TTL_MS - 1, ProposalKind::Suggests), // expired suggest
            prop(
                "s1",
                2,
                now - PROPOSAL_TTL_MS - 1,
                ProposalKind::Contradicts,
            ), // expired contradict
            prop("s1", 3, now - 1000, ProposalKind::Suggests),                // fresh — stays open
            prop("s1", 4, now - 1000, ProposalKind::Contradicts),             // fresh — stays open
        ];
        let (to_note, to_decay) = expire_proposals(&props, now, PROPOSAL_TTL_MS);
        assert_eq!(to_note.len(), 1);
        assert_eq!(
            to_note[0].node, 1,
            "an expired suggestion becomes a part_note"
        );
        assert_eq!(to_decay.len(), 1);
        assert_eq!(
            to_decay[0].node, 2,
            "an expired contradiction decays the node"
        );
    }

    #[test]
    fn expiry_boundary_is_inclusive_and_note_text_survives() {
        let now = PROPOSAL_TTL_MS + 5000;
        // exactly ttl old → expired.
        let exactly = vec![prop("s", 1, now - PROPOSAL_TTL_MS, ProposalKind::Suggests)];
        let (to_note, _) = expire_proposals(&exactly, now, PROPOSAL_TTL_MS);
        assert_eq!(to_note.len(), 1);
        assert_eq!(
            to_note[0].note_text,
            format!("s@{}", now - PROPOSAL_TTL_MS),
            "the session's words survive to the note"
        );
        // one ms younger than ttl → still open.
        let young = vec![prop(
            "s",
            1,
            now - PROPOSAL_TTL_MS + 1,
            ProposalKind::Suggests,
        )];
        assert!(expire_proposals(&young, now, PROPOSAL_TTL_MS).0.is_empty());
    }

    #[test]
    fn coalesce_key_is_session_and_node() {
        assert_eq!(coalesce_key("abc", 7), ("abc".to_string(), 7));
        assert_ne!(coalesce_key("abc", 7), coalesce_key("abc", 8));
    }

    // ---- 6. trigger decision ---------------------------------------------

    #[test]
    fn trigger_end_on_exit_with_new_events() {
        assert_eq!(summarize_trigger(1, 0, true), Some(Trigger::End));
        // …but a session that ended with NOTHING new does not summarize a dup.
        assert_eq!(summarize_trigger(0, 0, true), None);
    }

    #[test]
    fn trigger_delta_every_12_turns_without_waiting_for_idle() {
        assert_eq!(
            summarize_trigger(DELTA_TRIGGER_EVENTS, 0, false),
            Some(Trigger::Delta)
        );
        assert_eq!(
            summarize_trigger(DELTA_TRIGGER_EVENTS - 1, 0, false),
            None,
            "under the cadence, not idle → nothing"
        );
    }

    #[test]
    fn trigger_idle_after_5min_with_new_events() {
        assert_eq!(
            summarize_trigger(3, IDLE_TRIGGER_SECS, false),
            Some(Trigger::Idle)
        );
        assert_eq!(
            summarize_trigger(3, IDLE_TRIGGER_SECS - 1, false),
            None,
            "not idle enough yet"
        );
        assert_eq!(
            summarize_trigger(0, IDLE_TRIGGER_SECS, false),
            None,
            "idle but nothing new → nothing"
        );
    }

    // --- the cool-off that replaced the permanent dead-job blacklist ---

    const MIN: u64 = 60_000; // ms
    const HOUR: u64 = 60 * MIN;

    #[test]
    fn no_deaths_never_cools_off() {
        assert!(!cooling_off(0, 0, 1_000_000));
        assert!(!cooling_off(0, 999_999, 1_000_000), "no deaths, no skip");
        assert_eq!(cooloff_window_secs(0), 0);
    }

    #[test]
    fn one_death_cools_off_for_thirty_minutes_then_retries() {
        let now = 100 * HOUR;
        let (n, last) = recent_deaths(&[now - 5 * MIN], now);
        assert_eq!((n, last), (1, now - 5 * MIN));
        assert!(cooling_off(n, last, now), "5 min after a death: still cooling");
        assert!(
            cooling_off(1, now - 29 * MIN, now),
            "29 min: still inside the 30-min window"
        );
        assert!(
            !cooling_off(1, now - 31 * MIN, now),
            "31 min: the window EXPIRED — a one-off blip costs one cadence, not a lifetime"
        );
    }

    #[test]
    fn the_backoff_escalates_and_then_caps() {
        assert_eq!(cooloff_window_secs(1), 30 * 60);
        assert_eq!(cooloff_window_secs(2), 60 * 60);
        assert_eq!(cooloff_window_secs(3), 2 * 60 * 60);
        assert_eq!(cooloff_window_secs(4), 4 * 60 * 60);
        assert_eq!(cooloff_window_secs(5), DEATH_COOLOFF_MAX_SECS);
        // the cap holds — no overflow, no wrap, no accidental eternity.
        assert_eq!(cooloff_window_secs(6), DEATH_COOLOFF_MAX_SECS);
        assert_eq!(cooloff_window_secs(64), DEATH_COOLOFF_MAX_SECS);
        assert_eq!(cooloff_window_secs(u32::MAX), DEATH_COOLOFF_MAX_SECS);

        let now = 1_000 * HOUR;
        assert!(
            cooling_off(3, now - 90 * MIN, now),
            "3 deaths buy 2h — 90 min in, still backing off"
        );
        assert!(
            !cooling_off(3, now - 3 * HOUR, now),
            "…and 3h in, it tries again"
        );
        assert!(
            !cooling_off(u32::MAX, now - 9 * HOUR, now),
            "even a hopeless session is retried after the 8h cap — NOTHING is permanent"
        );
    }

    #[test]
    fn deaths_decay_after_a_day() {
        let now = 1_000 * HOUR;
        let old = now - (DEATH_DECAY_SECS + 60) * 1000;
        let (n, last) = recent_deaths(&[old], now);
        assert_eq!((n, last), (0, 0), "a day-old death stops counting");
        assert!(!cooling_off(n, last, now), "one quiet day clears the record");
        // mixed: only the recent ones escalate.
        let (n, last) = recent_deaths(&[old, old, now - 2 * HOUR, now - 30 * MIN], now);
        assert_eq!(n, 2, "the two stale deaths decayed out");
        assert_eq!(last, now - 30 * MIN, "anchored to the NEWEST death");
        assert!(
            cooling_off(n, last, now),
            "2 recent deaths → a 1h window, and the newest is 30 min old"
        );
        assert!(
            !cooling_off(n, now - HOUR, now),
            "the window is exclusive at its edge — an hour on, it retries"
        );
    }

    /// THE user's case, to the hour. His newest dead summary job died ~61h
    /// ago (10 sessions, 20 dead rows, Jul 5-11). Every cool-off window is
    /// capped at 8h and deaths decay after 24h, so on the next tick every one
    /// of those sessions enqueues again — with no migration and no gesture.
    #[test]
    fn the_users_two_day_old_deaths_all_revive() {
        let now = 10_000 * HOUR;
        let deaths: Vec<u64> = [61, 62, 74, 99, 128, 150, 170, 190]
            .iter()
            .map(|h| now - h * HOUR)
            .collect();
        let (n, last) = recent_deaths(&deaths, now);
        assert_eq!(n, 0, "every death is older than the 24h decay");
        assert!(!cooling_off(n, last, now), "the standup comes back by itself");
        // and even if they had NOT decayed, the 8h cap would have expired.
        assert!(!cooling_off(deaths.len() as u32, now - 61 * HOUR, now));
    }

    #[test]
    fn a_death_stamped_in_the_future_fails_open() {
        // the clock moved backwards (or a row is bogus). `now - last` would
        // saturate to 0 and cool off FOREVER — the exact permanent blacklist
        // this fn exists to abolish. Fail toward the recoverable mistake.
        let now = 1_000 * HOUR;
        assert!(
            !cooling_off(1, now + 5 * HOUR, now),
            "a future death stamp must never blacklist a session"
        );
        assert!(!cooling_off(9, now + 10_000 * HOUR, now));
        // it still COUNTS as a recent death (age 0 < decay) — the fail-open
        // decision lives in one place, not two.
        let (n, last) = recent_deaths(&[now + HOUR], now);
        assert_eq!((n, last), (1, now + HOUR));
        assert!(!cooling_off(n, last, now));
    }

    #[test]
    fn trigger_priority_end_over_delta_over_idle() {
        // ended + full delta window + idle: End wins.
        assert_eq!(summarize_trigger(20, 600, true), Some(Trigger::End));
        // not ended, full delta window + idle: Delta wins over Idle.
        assert_eq!(summarize_trigger(20, 600, false), Some(Trigger::Delta));
    }

    #[test]
    fn trigger_str_matches_the_store_enum() {
        assert_eq!(trigger_str(Trigger::End), "end");
        assert_eq!(trigger_str(Trigger::Idle), "idle");
        assert_eq!(trigger_str(Trigger::Delta), "delta");
    }

    // ---- 7. heartbeat gate -----------------------------------------------

    #[test]
    fn heartbeat_stale_after_grace_only() {
        // observed 1s ago → live; observed 10s ago → stale (link lost).
        assert!(!heartbeat_stale(9_000, 10_000, HEARTBEAT_GRACE_MS));
        assert!(heartbeat_stale(0, 10_000, HEARTBEAT_GRACE_MS));
        // exactly at grace is NOT stale (strict >).
        assert!(!heartbeat_stale(
            4_000,
            4_000 + HEARTBEAT_GRACE_MS,
            HEARTBEAT_GRACE_MS
        ));
        assert!(heartbeat_stale(
            4_000,
            4_001 + HEARTBEAT_GRACE_MS,
            HEARTBEAT_GRACE_MS
        ));
        // cold start: beat == now → live, never a launch flash.
        assert!(!heartbeat_stale(1000, 1000, HEARTBEAT_GRACE_MS));
    }

    // ---- 8. map verbs ----------------------------------------------------

    #[test]
    fn parse_here_takes_the_whole_target() {
        assert_eq!(
            parse_map_verb("map here Payments Engine"),
            Some(MapVerb::Here {
                target: "Payments Engine".into()
            })
        );
        // list-bullet / prose framing is tolerated.
        assert_eq!(
            parse_map_verb("- map here Sync"),
            Some(MapVerb::Here {
                target: "Sync".into()
            })
        );
        // no target → None.
        assert_eq!(parse_map_verb("map here"), None);
        assert_eq!(parse_map_verb("map here   "), None);
    }

    #[test]
    fn parse_done_requires_a_shipped_claim_payload() {
        assert_eq!(
            parse_map_verb("map done Touch tap — shipped the live weighted tap, 29 tests green"),
            Some(MapVerb::Done {
                target: "Touch tap".into(),
                claim: "shipped the live weighted tap, 29 tests green".into()
            })
        );
        // the `::` and `--` delimiters both work.
        assert_eq!(
            parse_map_verb("map done Auth :: landed OAuth"),
            Some(MapVerb::Done {
                target: "Auth".into(),
                claim: "landed OAuth".into()
            })
        );
        // a done with NO claim never parses — the payload is the lie guard.
        assert_eq!(parse_map_verb("map done Auth"), None);
        assert_eq!(parse_map_verb("map done Auth — "), None);
    }

    #[test]
    fn parse_note_splits_target_from_text() {
        assert_eq!(
            parse_map_verb("map note Payments :: chose stripe over adyen for the EU coverage"),
            Some(MapVerb::Note {
                target: "Payments".into(),
                text: "chose stripe over adyen for the EU coverage".into()
            })
        );
        assert_eq!(
            parse_map_verb("map note Payments"),
            None,
            "a note needs text"
        );
    }

    #[test]
    fn parse_map_verb_ignores_non_markers() {
        assert_eq!(parse_map_verb("the map here is fine"), None);
        assert_eq!(parse_map_verb("mapping the territory"), None);
        assert_eq!(parse_map_verb(""), None);
        assert_eq!(parse_map_verb("map wander somewhere"), None);
    }

    #[test]
    fn parse_doc_links_keeps_repo_paths_skips_urls() {
        let md = "See [the design](docs/019-map.md) and [substrate](docs/011.md).\nAlso [the site](https://x.com) and [dup](docs/019-map.md).";
        let links = parse_doc_links(md, 10);
        assert_eq!(links.len(), 2, "urls skipped, dup deduped");
        assert_eq!(
            links[0],
            ("the design".to_string(), "docs/019-map.md".to_string())
        );
        assert_eq!(
            links[1],
            ("substrate".to_string(), "docs/011.md".to_string())
        );
        // the cap bounds it.
        assert_eq!(parse_doc_links(md, 1).len(), 1);
        // no links → empty, never a panic on ragged brackets.
        assert!(parse_doc_links("just [text with no link", 10).is_empty());
    }

    #[test]
    fn scan_lifts_every_verb_in_order_capped() {
        let msg = "did some work\nmap here Sync\nnotes...\nmap note Sync :: chose polling\nmap done Sync — polling loop shipped\n";
        let verbs = scan_map_verbs(msg, 10);
        assert_eq!(verbs.len(), 3);
        assert_eq!(
            verbs[0],
            MapVerb::Here {
                target: "Sync".into()
            }
        );
        assert!(matches!(verbs[2], MapVerb::Done { .. }));
        // the cap bounds a pathological flood.
        assert_eq!(scan_map_verbs(msg, 1).len(), 1);
    }
}
