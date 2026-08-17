//! Transcript parsing (#9 slice 4) — derive timeline events from on-disk CLI
//! transcripts, the event source for CODEX (which has no hook channel) and the
//! backfill for resumed/crashed claude. Pure + stateless-per-line: every event
//! comes from ONE jsonl record, so there's no cross-line pairing to get wrong.
//!
//! Feeds the SAME `SessionEventKind` as the hook reducer (events.rs) — the GUI
//! never knows where an event came from.

use std::path::Path;

use crate::events::{tool_target, tool_verb, SessionEventKind, ToolVerb};

/// One parsed timeline item carrying the TRANSCRIPT's own timestamp (backfill is
/// historical, so we keep the real time rather than now()).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineItem {
    pub at_ms: u64,
    pub kind: SessionEventKind,
}

/// `2026-05-19T18:11:32.424Z` → unix milliseconds. Zero-dep civil-days (host is
/// deliberately core-free, so this duplicates core's secs-only parser + millis).
pub fn iso_to_ms(ts: &str) -> Option<u64> {
    let b = ts.as_bytes();
    if ts.len() < 19 || b[4] != b'-' || b[10] != b'T' {
        return None;
    }
    let y: i64 = ts.get(0..4)?.parse().ok()?;
    let mo: i64 = ts.get(5..7)?.parse().ok()?;
    let d: i64 = ts.get(8..10)?.parse().ok()?;
    let hh: u64 = ts.get(11..13)?.parse().ok()?;
    let mm: u64 = ts.get(14..16)?.parse().ok()?;
    let ss: u64 = ts.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let millis: u64 = if ts.len() >= 23 && b[19] == b'.' {
        ts.get(20..23).and_then(|s| s.parse().ok()).unwrap_or(0)
    } else {
        0
    };
    // days from civil (Howard Hinnant).
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = (if yy >= 0 { yy } else { yy - 399 }) / 400;
    let yoe = yy - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    if days < 0 {
        return None;
    }
    let secs = days as u64 * 86_400 + hh * 3600 + mm * 60 + ss;
    Some(secs * 1000 + millis)
}

fn base(path: &str) -> String {
    crate::util::file_name(path).to_string()
}

/// Parse ONE codex rollout jsonl line into timeline items. Codex schema:
/// `session_meta` → Started; `function_call` exec_command → Ran(cmd) (apply_patch
/// is SKIPPED so edits aren't double-counted); `patch_apply_end` → one Edited/
/// Created per changed file; `task_complete` → TurnEnd(last_agent_message).
pub fn codex_line_events(line: &str) -> Vec<TimelineItem> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    let at_ms = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(iso_to_ms)
        .unwrap_or(0);
    let one = |kind| vec![TimelineItem { at_ms, kind }];

    if v.get("type").and_then(|t| t.as_str()) == Some("session_meta") {
        return one(SessionEventKind::Started);
    }
    let payload = v.get("payload");
    let ptype = payload
        .and_then(|p| p.get("type"))
        .and_then(|t| t.as_str())
        .unwrap_or("");
    match ptype {
        "function_call" => {
            let name = payload
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            if name != "exec_command" {
                return Vec::new(); // apply_patch → patch_apply_end owns the edit rows
            }
            // `arguments` is a nested JSON STRING: {"cmd":"…","workdir":"…"}.
            let cmd = payload
                .and_then(|p| p.get("arguments"))
                .and_then(|a| a.as_str())
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .and_then(|a| a.get("cmd").and_then(|c| c.as_str()).map(String::from))
                .unwrap_or_default();
            if cmd.trim().is_empty() {
                Vec::new()
            } else {
                one(SessionEventKind::Tool {
                    verb: ToolVerb::Ran,
                    target: crate::util::truncate_to(cmd, crate::util::TARGET_MAX),
                })
            }
        }
        "patch_apply_end" => {
            let Some(changes) = payload
                .and_then(|p| p.get("changes"))
                .and_then(|c| c.as_object())
            else {
                return Vec::new();
            };
            changes
                .iter()
                .map(|(path, change)| {
                    let verb = match change.get("type").and_then(|t| t.as_str()) {
                        Some("add") => ToolVerb::Created,
                        _ => ToolVerb::Edited, // update / delete
                    };
                    TimelineItem {
                        at_ms,
                        kind: SessionEventKind::Tool {
                            verb,
                            target: base(path),
                        },
                    }
                })
                .collect()
        }
        "task_complete" => {
            let msg = payload
                .and_then(|p| p.get("last_agent_message"))
                .and_then(|m| m.as_str())
                .unwrap_or("");
            if msg.trim().is_empty() {
                Vec::new()
            } else {
                one(SessionEventKind::TurnEnd {
                    summary: crate::util::truncate_to(msg, crate::util::SUMMARY_MAX),
                })
            }
        }
        _ => Vec::new(),
    }
}

/// Parse a whole codex rollout file into timeline items, in file order.
pub fn codex_rollout_events(text: &str) -> Vec<TimelineItem> {
    text.lines().flat_map(codex_line_events).collect()
}

/// One rate-limit window from codex's `token_count` telemetry. Codex reports two:
/// `primary` (the rolling ~5h window, `window_minutes:300`) and `secondary` (the
/// weekly window, `window_minutes:10080`). `used_percent` reaches 100.0 on a
/// limit; the reset instant is either an explicit `resets_at` unix epoch OR
/// `resets_in_seconds` measured from the observation time.
#[derive(Debug, Clone, PartialEq)]
pub struct RateWindow {
    pub used_percent: f64,
    pub window_minutes: u32,
    pub resets_at: Option<i64>,
    pub resets_in_seconds: Option<i64>,
}

/// The `rate_limits{primary,secondary}` block from the LAST `token_count` event
/// in a codex rollout, plus the observation time (ms) that anchors a
/// `resets_in_seconds` offset. This is codex's STRUCTURED limit signal — read in
/// place of scraping the terminal grid (docs/019 codex limit).
#[derive(Debug, Clone, PartialEq)]
pub struct CodexRateLimits {
    pub observed_ms: u64,
    pub primary: Option<RateWindow>,
    pub secondary: Option<RateWindow>,
}

/// Lift codex's STRUCTURED rate-limit telemetry from a rollout jsonl blob: the
/// LAST `token_count` event's `rate_limits{primary,secondary}`. Returns `None`
/// when no such event is present, so it can NEVER manufacture a false "cleared" —
/// a rollout with no telemetry leaves any stored limit untouched.
///
/// PURE string parsing, no subprocess (RULE ZERO). The `type`/`rate_limits` keys
/// nest exactly like `codex_line_events` (~78-83): they may sit at the top level
/// or under `payload`, so both shapes are probed.
pub fn codex_rate_limits(text: &str) -> Option<CodexRateLimits> {
    let mut last: Option<CodexRateLimits> = None;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let payload = v.get("payload");
        // type: top-level (older) or under payload (event_msg wrapper).
        let ptype = v
            .get("type")
            .and_then(|t| t.as_str())
            .filter(|t| *t == "token_count")
            .or_else(|| {
                payload
                    .and_then(|p| p.get("type"))
                    .and_then(|t| t.as_str())
                    .filter(|t| *t == "token_count")
            });
        if ptype.is_none() {
            continue;
        }
        // rate_limits sits next to the type — top-level or under payload. A JSON
        // `null` is `Some(&Value::Null)`, NOT `None` (F3): filter it out so
        // `rate_limits: null` is treated as "no telemetry", not an empty block.
        let Some(rl) = v
            .get("rate_limits")
            .filter(|rl| !rl.is_null())
            .or_else(|| payload.and_then(|p| p.get("rate_limits")).filter(|rl| !rl.is_null()))
        else {
            continue;
        };
        let primary = rate_window(rl.get("primary"));
        let secondary = rate_window(rl.get("secondary"));
        // NEVER manufacture a false clear (F3): a token_count whose rate_limits
        // carries NO usable window (both parse to None — e.g. `rate_limits: {}`)
        // must not become a `Some{primary:None,secondary:None}` that clears a
        // real stored hit. Only advance `last` when a real window is present.
        if primary.is_none() && secondary.is_none() {
            continue;
        }
        // observation time anchors a resets_in_seconds offset (mirror ~68-72).
        let observed_ms = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(iso_to_ms)
            .unwrap_or(0);
        last = Some(CodexRateLimits {
            observed_ms,
            primary,
            secondary,
        });
    }
    last
}

/// One `rate_limits.{primary,secondary}` object → a `RateWindow`. `None` when the
/// object is absent or carries no `used_percent`.
fn rate_window(v: Option<&serde_json::Value>) -> Option<RateWindow> {
    let v = v?;
    let used_percent = v.get("used_percent").and_then(|x| x.as_f64())?;
    Some(RateWindow {
        used_percent,
        window_minutes: v.get("window_minutes").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
        resets_at: v.get("resets_at").and_then(|x| x.as_i64()),
        resets_in_seconds: v.get("resets_in_seconds").and_then(|x| x.as_i64()),
    })
}

/// Read the last `cap` bytes of a rollout file as UTF-8 (lossy), dropping a
/// leading partial line — enough to catch the LAST `token_count` without
/// slurping a multi-MB rollout on every poll. Returns "" on any I/O error (the
/// caller's `codex_rate_limits` then yields `None`, so a stored limit is left
/// untouched). Lives in host (not the GUI) so the DAEMON sweep can self-poll
/// codex limits with no client attached (docs/019 codex limit).
pub fn read_rollout_tail(path: &Path, cap: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(len) = f.metadata().map(|m| m.len()) else {
        return String::new();
    };
    let start = len.saturating_sub(cap);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        // the first line is almost certainly truncated — drop it.
        if let Some(nl) = text.find('\n') {
            return text[nl + 1..].to_string();
        }
    }
    text
}

// ── auto-continue held-prompt SOURCE (docs/019 rework, adversarial safety
// review): the GROUND-TRUTH prompt to replay at a usage-limit reset is the LAST
// user-submitted message in the CLI's OWN session transcript — never a keystroke
// reconstruction (the review proved keystroke capture replays stale/wrong/
// truncated prompts). Pure string parsing, unit-tested on fixture blobs with NO
// subprocess (RULE ZERO). Mirrors the app's standup extractors (gui
// `extract/standup.rs`), whose logic lives in a crate the host can't import. ──

/// The LAST real user-submitted message in a CLAUDE `.jsonl` transcript, or
/// `None` when the transcript carries no genuine user turn (only tool results /
/// meta injections / slash-command noise). Reverse-scans so the NEWEST qualifying
/// turn wins — that is the prompt whose turn the usage limit interrupted.
pub fn claude_last_user_message(text: &str) -> Option<String> {
    for line in text.lines().rev() {
        // cheap pre-filter before the JSON parse (mirrors standup.rs): a real user
        // turn, never a meta/sidechain injection.
        if !line.contains("\"type\":\"user\"") || line.contains("\"isMeta\":true") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(t) = claude_user_text(&v) {
            if !is_goal_noise(&t) {
                return Some(t);
            }
        }
    }
    None
}

/// The LAST real user-submitted message in a CODEX rollout, or `None`. Codex user
/// turns are `payload.type == "user_message"` with a string `payload.message`
/// (the payload may sit at the top level in older rollouts).
pub fn codex_last_user_message(text: &str) -> Option<String> {
    for line in text.lines().rev() {
        if !line.contains("\"user_message\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let p = v.get("payload").unwrap_or(&v);
        if p.get("type").and_then(|t| t.as_str()) != Some("user_message") {
            continue;
        }
        let msg = p
            .get("message")
            .and_then(|m| m.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(m) = msg {
            if !is_goal_noise(&m) {
                return Some(m);
            }
        }
    }
    None
}

/// Extract a human prompt from a claude `{type:"user"}` jsonl value — a bare
/// string content, else the first `text` part, but NEVER a `tool_result` turn (a
/// tool output fed back, not a prompt). Mirrors the app's `claude_user_text`.
fn claude_user_text(v: &serde_json::Value) -> Option<String> {
    let content = v.get("message")?.get("content")?;
    if let Some(s) = content.as_str() {
        let t = s.trim();
        return (!t.is_empty()).then(|| t.to_string());
    }
    if let Some(arr) = content.as_array() {
        if arr
            .iter()
            .any(|p| p.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
        {
            return None; // a tool_result fed back as a user turn — not a human prompt
        }
        for p in arr {
            if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(s) = p.get("text").and_then(|t| t.as_str()) {
                    let t = s.trim();
                    if !t.is_empty() {
                        return Some(t.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Slash-command / caveat / injected-preamble noise that masquerades as a user
/// prompt — must never be replayed (mirrors the app's `is_goal_noise`).
fn is_goal_noise(t: &str) -> bool {
    let head: String = t.chars().take(60).collect();
    t.starts_with("<command-name>")
        || t.starts_with("<local-command")
        || t.starts_with("<command-message>")
        || t.starts_with("You are ") // injected agent/system preamble (codex)
        || head.contains("Caveat:")
}

/// Parse ONE claude transcript jsonl line. Claude schema: assistant `tool_use`
/// content → a Tool row; assistant `end_turn` text → TurnEnd. (Used for
/// BACKFILL only — live claude is owned by hooks; the caller applies the
/// pre-`started_at` cutoff so there's no double-count.)
pub fn claude_line_events(line: &str) -> Vec<TimelineItem> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return Vec::new();
    }
    let at_ms = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(iso_to_ms)
        .unwrap_or(0);
    let msg = v.get("message");
    let content = msg
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array());
    let Some(content) = content else {
        return Vec::new();
    };
    let stop = msg
        .and_then(|m| m.get("stop_reason"))
        .and_then(|s| s.as_str())
        .unwrap_or("");

    let mut out = Vec::new();
    for item in content {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("tool_use") => {
                let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let input = item
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                out.push(TimelineItem {
                    at_ms,
                    kind: SessionEventKind::Tool {
                        verb: tool_verb(name),
                        target: tool_target(name, &input),
                    },
                });
            }
            Some("text") if stop == "end_turn" => {
                let t = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if !t.trim().is_empty() {
                    out.push(TimelineItem {
                        at_ms,
                        kind: SessionEventKind::TurnEnd {
                            summary: crate::util::truncate_to(t, crate::util::SUMMARY_MAX),
                        },
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// Parse a whole claude transcript file into timeline items, in file order.
pub fn claude_transcript_events(text: &str) -> Vec<TimelineItem> {
    text.lines().flat_map(claude_line_events).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_last_user_message_picks_newest_real_prompt() {
        let t = r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<injected system reminder>"}}
{"type":"user","message":{"role":"user","content":"the first real ask"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"working on it"}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":"cmd output"}]}}
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"the LATEST ask"}]}}"#;
        // the newest genuine user turn wins; the tool_result + isMeta turns skip.
        assert_eq!(claude_last_user_message(t).as_deref(), Some("the LATEST ask"));

        // a lone tool_result "user" turn is NOT a prompt → None.
        assert!(claude_last_user_message(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"x"}]}}"#
        )
        .is_none());
        // no user turn at all → None (caller surfaces "no recoverable prompt").
        assert!(claude_last_user_message(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#
        )
        .is_none());
        // slash-command noise is not a replayable prompt.
        assert!(claude_last_user_message(
            r#"{"type":"user","message":{"role":"user","content":"<command-name>/clear</command-name>"}}"#
        )
        .is_none());
    }

    #[test]
    fn codex_last_user_message_picks_newest_and_skips_noise() {
        let t = r#"{"type":"session_meta","payload":{}}
{"payload":{"type":"user_message","message":"the first codex ask"}}
{"payload":{"type":"agent_message","message":"a reply"}}
{"payload":{"type":"user_message","message":"the LATEST codex ask"}}"#;
        assert_eq!(
            codex_last_user_message(t).as_deref(),
            Some("the LATEST codex ask")
        );
        // an injected "You are ..." preamble is noise, not a user prompt.
        assert!(codex_last_user_message(
            r#"{"payload":{"type":"user_message","message":"You are a helpful agent"}}"#
        )
        .is_none());
        // no user_message present → None.
        assert!(
            codex_last_user_message(r#"{"payload":{"type":"agent_message","message":"hi"}}"#)
                .is_none()
        );
    }

    #[test]
    fn iso_to_ms_parses_utc_with_millis() {
        assert_eq!(iso_to_ms("1970-01-01T00:00:00.000Z"), Some(0));
        let a = iso_to_ms("2026-05-19T18:11:32.424Z").unwrap();
        let b = iso_to_ms("2026-05-19T18:11:32.425Z").unwrap();
        assert_eq!(b - a, 1, "the millis field is parsed");
        assert_eq!(iso_to_ms("nope"), None);
    }

    #[test]
    fn codex_rollout_maps_meta_exec_patch_and_complete() {
        // a minimal rollout matching the REAL schema (from the recon).
        let roll = r#"{"timestamp":"2026-05-19T18:11:00.000Z","type":"session_meta","payload":{"cwd":"/x"}}
{"timestamp":"2026-05-19T18:11:32.424Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"npm install\",\"workdir\":\"/x\"}","call_id":"c1"}}
{"timestamp":"2026-05-19T18:11:40.000Z","type":"response_item","payload":{"type":"function_call","name":"apply_patch","arguments":"{}","call_id":"c2"}}
{"timestamp":"2026-05-20T20:39:53.950Z","type":"event_msg","payload":{"type":"patch_apply_end","changes":{"/Users/x/foo.rs":{"type":"add"},"/Users/x/bar.rs":{"type":"update"}},"success":true}}
{"timestamp":"2026-05-20T20:40:00.000Z","type":"event_msg","payload":{"type":"task_complete","last_agent_message":"All done."}}"#;
        let evs = codex_rollout_events(roll);
        // Started + Ran + (Created foo, Edited bar) + TurnEnd — apply_patch SKIPPED.
        assert!(evs.iter().any(|e| e.kind == SessionEventKind::Started));
        assert!(evs.iter().any(|e| e.kind
            == SessionEventKind::Tool {
                verb: ToolVerb::Ran,
                target: "npm install".into()
            }));
        assert!(evs.iter().any(|e| e.kind
            == SessionEventKind::Tool {
                verb: ToolVerb::Created,
                target: "foo.rs".into()
            }));
        assert!(evs.iter().any(|e| e.kind
            == SessionEventKind::Tool {
                verb: ToolVerb::Edited,
                target: "bar.rs".into()
            }));
        assert!(evs.iter().any(|e| e.kind
            == SessionEventKind::TurnEnd {
                summary: "All done.".into()
            }));
        // exactly 5 — the apply_patch function_call produced NO row (no double-count).
        assert_eq!(evs.len(), 5);
        // historical timestamps are preserved + ascending in file order.
        assert!(evs.first().unwrap().at_ms < evs.last().unwrap().at_ms);
    }

    #[test]
    fn codex_rate_limits_reads_last_token_count() {
        // real-shaped rollout: token_count `event_msg`s carrying a rate_limits
        // block under `payload` (newer codex nests it there). Two token_count
        // records — the LAST one wins.
        let roll = r#"{"timestamp":"2026-05-19T18:11:00.000Z","type":"session_meta","payload":{"cwd":"/x"}}
{"timestamp":"2026-05-19T18:11:30.000Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":42.0,"window_minutes":300,"resets_at":1747000000},"secondary":{"used_percent":10.0,"window_minutes":10080,"resets_in_seconds":600}}}}
{"timestamp":"2026-05-19T18:40:00.000Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":100.0,"window_minutes":300,"resets_at":1747003600},"secondary":{"used_percent":73.0,"window_minutes":10080,"resets_in_seconds":720}}}}"#;
        let rl = codex_rate_limits(roll).expect("a token_count with rate_limits must parse");
        assert_eq!(rl.observed_ms, iso_to_ms("2026-05-19T18:40:00.000Z").unwrap());
        let p = rl.primary.expect("primary window present");
        assert_eq!(p.used_percent, 100.0);
        assert_eq!(p.window_minutes, 300);
        assert_eq!(p.resets_at, Some(1747003600));
        let s = rl.secondary.expect("secondary window present");
        assert_eq!(s.used_percent, 73.0);
        assert_eq!(s.resets_in_seconds, Some(720));

        // the TOP-LEVEL nesting variant (type + rate_limits not under payload).
        let flat = r#"{"timestamp":"2026-05-19T19:00:00.000Z","type":"token_count","rate_limits":{"primary":{"used_percent":55.5,"window_minutes":300,"resets_in_seconds":900}}}"#;
        let rl2 = codex_rate_limits(flat).expect("top-level token_count must parse");
        assert_eq!(rl2.primary.unwrap().used_percent, 55.5);
        assert!(rl2.secondary.is_none());

        // a rollout with NO token_count NEVER yields a (false) cleared signal.
        assert!(codex_rate_limits(
            r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"hi"}}"#
        )
        .is_none());
        assert!(codex_rate_limits("").is_none());

        // F3: a token_count whose rate_limits is JSON `null` is Some(&Value::Null),
        // NOT None — it must be treated as "no telemetry", never a false clear.
        assert!(codex_rate_limits(
            r#"{"timestamp":"2026-05-19T18:40:00.000Z","type":"token_count","rate_limits":null}"#
        )
        .is_none());
        // F3: an EMPTY rate_limits block (no windows) likewise can't manufacture
        // a Some{primary:None,secondary:None} that would clear a real stored hit.
        assert!(codex_rate_limits(
            r#"{"timestamp":"2026-05-19T18:40:00.000Z","type":"token_count","rate_limits":{}}"#
        )
        .is_none());
        // F3: a LATER null/empty token_count must NOT clobber an earlier real
        // window — the last USABLE window wins, so the reading survives.
        let then_null = r#"{"timestamp":"2026-05-19T18:11:30.000Z","type":"token_count","rate_limits":{"primary":{"used_percent":88.0,"window_minutes":300,"resets_in_seconds":600}}}
{"timestamp":"2026-05-19T18:40:00.000Z","type":"token_count","rate_limits":null}"#;
        let rl3 = codex_rate_limits(then_null).expect("earlier real window survives a later null");
        assert_eq!(rl3.primary.unwrap().used_percent, 88.0);
    }

    #[test]
    fn claude_transcript_maps_tool_use_and_end_turn() {
        let tx = r#"{"type":"user","timestamp":"2026-06-05T23:39:28.520Z","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}
{"type":"assistant","timestamp":"2026-06-05T23:39:31.680Z","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la"}}]}}
{"type":"assistant","timestamp":"2026-06-05T23:39:40.000Z","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"/a/b/strategy.py"}}]}}
{"type":"assistant","timestamp":"2026-06-05T23:45:26.796Z","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Done."}]}}"#;
        let evs = claude_transcript_events(tx);
        assert!(evs.iter().any(|e| e.kind
            == SessionEventKind::Tool {
                verb: ToolVerb::Ran,
                target: "ls -la".into()
            }));
        assert!(evs.iter().any(|e| e.kind
            == SessionEventKind::Tool {
                verb: ToolVerb::Edited,
                target: "/a/b/strategy.py".into()
            }));
        assert!(evs.iter().any(|e| e.kind
            == SessionEventKind::TurnEnd {
                summary: "Done.".into()
            }));
        // the user line + the tool_use lines' non-end_turn text yield no TurnEnd noise.
        assert_eq!(
            evs.iter()
                .filter(|e| matches!(e.kind, SessionEventKind::TurnEnd { .. }))
                .count(),
            1
        );
    }

}
