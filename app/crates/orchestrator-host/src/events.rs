//! Per-session EVENT TIMELINE (#9 rich) — a source-agnostic SINK: typed events
//! that power the Sessions curated stream ("edited X · ran Y · said: …").
//!
//! The SOURCE is decoupled from the SINK. Slice 1 derives events from the four
//! already-armed hooks (`reduce`); later sources (PostToolUse results, codex
//! transcript tailing) feed the SAME enum without reshaping storage/wire/render.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::hooks::HookEvent;

/// One thing that happened in a session — appended newest-last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEvent {
    /// per-session monotonic cursor (the delta key — NEVER a slice index, so a
    /// ring eviction can't corrupt which events stream).
    pub seq: u64,
    /// wall-clock ms, for display/order.
    pub at_ms: u64,
    pub kind: SessionEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionEventKind {
    /// the session started (host-synthesized at spawn — NOT the blocking
    /// SessionStart hook).
    Started,
    /// a tool ran (PreToolUse) — the verb + its target (file path / command).
    Tool { verb: ToolVerb, target: String },
    /// a permission breadcrumb (PermissionRequest) — the CARD stays separate;
    /// this is the timeline's "paused → needs you" history.
    Awaiting { tool: String },
    /// the turn ended (Stop) — the assistant's last message = the DID/divider.
    TurnEnd { summary: String },
    /// a waiting-state notice (Notification).
    Notice { text: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolVerb {
    Edited,
    Created,
    Ran,
    Read,
    Used,
}

/// Tool name → verb (Edit/MultiEdit→Edited, Write→Created, Bash→Ran,
/// Read→Read, else→Used with the tool name carried as the target).
pub fn tool_verb(name: &str) -> ToolVerb {
    match name {
        "Edit" | "MultiEdit" => ToolVerb::Edited,
        "Write" => ToolVerb::Created,
        "Bash" => ToolVerb::Ran,
        "Read" => ToolVerb::Read,
        _ => ToolVerb::Used,
    }
}

/// The human-meaningful target of a tool call: the file path for
/// edits/writes/reads, the command (or description) for Bash, else the tool
/// name. Truncated to keep the row a single line.
pub fn tool_target(name: &str, input: &serde_json::Value) -> String {
    let get = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let raw = match name {
        "Edit" | "MultiEdit" | "Write" | "Read" => get("file_path"),
        "Bash" => {
            let c = get("command");
            if c.is_empty() { get("description") } else { c }
        }
        _ => "",
    };
    let raw = if raw.is_empty() { name } else { raw };
    crate::util::truncate_to(raw, crate::util::TARGET_MAX)
}

/// Reduce an armed hook event to a timeline event KIND (slice 1's source). The
/// caller stamps `seq`/`at_ms`. `None` = doesn't belong on the timeline. The
/// consent path (`decision::extract`) is left untouched — this is parallel.
pub fn reduce(event: &HookEvent) -> Option<SessionEventKind> {
    match event {
        HookEvent::PreToolUse(p) => {
            let name = p.tool_name.clone().unwrap_or_default();
            if name.is_empty() {
                return None;
            }
            let input = p.tool_input.clone().unwrap_or(serde_json::Value::Null);
            Some(SessionEventKind::Tool { verb: tool_verb(&name), target: tool_target(&name, &input) })
        }
        HookEvent::PermissionRequest(p) => {
            Some(SessionEventKind::Awaiting { tool: p.tool_name.clone().unwrap_or_default() })
        }
        HookEvent::Stop(p) => {
            let m = p.last_assistant_message.clone().unwrap_or_default();
            (!m.trim().is_empty()).then(|| SessionEventKind::TurnEnd { summary: crate::util::truncate_to(&m, crate::util::SUMMARY_MAX) })
        }
        HookEvent::Notification(p) => {
            let t = p.message.clone().or_else(|| p.notification_type.clone()).unwrap_or_default();
            (!t.trim().is_empty()).then(|| SessionEventKind::Notice { text: t })
        }
        HookEvent::Other(..) => None,
    }
}

/// Wall-clock milliseconds (for `SessionEvent.at_ms`).
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Semantic tone for a timeline row — the GUI maps it to a palette color, so the
/// event→display mapping stays pure (and testable outside the gpui bin crate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventTone {
    Muted,
    Active,
    Attention,
}

/// A timeline row ready to render: a glyph, a one-line label, a tone. Plain
/// data — the GUI just colors the glyph/text by `tone`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventLine {
    pub glyph: &'static str,
    pub text: String,
    pub tone: EventTone,
}

/// Map a `SessionEvent` to its display row (#9) — pure, so it's unit-tested
/// here rather than in the gpui bin crate (whose render tree overflows the
/// test-compile recursion limit).
pub fn event_line(e: &SessionEvent) -> EventLine {
    let base = |p: &str| crate::util::file_name(p).to_string();
    match &e.kind {
        SessionEventKind::Started => EventLine { glyph: "⏺", text: "session started".into(), tone: EventTone::Muted },
        SessionEventKind::Tool { verb, target } => {
            let verb_word = match verb {
                ToolVerb::Edited => "edited",
                ToolVerb::Created => "created",
                ToolVerb::Ran => "ran",
                ToolVerb::Read => "read",
                ToolVerb::Used => "used",
            };
            // file verbs show the basename; ran/used show the command/tool whole.
            let what = match verb {
                ToolVerb::Ran | ToolVerb::Used => target.clone(),
                _ => base(target),
            };
            EventLine { glyph: "⏺", text: format!("{verb_word} {what}"), tone: EventTone::Muted }
        }
        SessionEventKind::TurnEnd { summary } => EventLine { glyph: "✳", text: format!("said: {summary}"), tone: EventTone::Active },
        SessionEventKind::Awaiting { tool } => EventLine { glyph: "◆", text: format!("paused — needs you ({tool})"), tone: EventTone::Attention },
        SessionEventKind::Notice { text } => EventLine { glyph: "·", text: text.clone(), tone: EventTone::Muted },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::parse_hook_payload;

    fn ev(json: &str) -> HookEvent {
        parse_hook_payload(json).unwrap()
    }

    #[test]
    fn reduce_maps_the_four_armed_hooks() {
        // PreToolUse Bash → ran <command> (real pretool.log shape).
        let pre = ev(r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"touch /tmp/marker.txt","description":"Create marker file"},"tool_use_id":"toolu_1"}"#);
        assert_eq!(reduce(&pre), Some(SessionEventKind::Tool { verb: ToolVerb::Ran, target: "touch /tmp/marker.txt".into() }));
        // Edit → edited <file>.
        let edit = ev(r#"{"hook_event_name":"PreToolUse","tool_name":"Edit","tool_input":{"file_path":"/x/strategy.py"}}"#);
        assert_eq!(reduce(&edit), Some(SessionEventKind::Tool { verb: ToolVerb::Edited, target: "/x/strategy.py".into() }));
        // Stop → the assistant's last message (the DID divider).
        let stop = ev(r#"{"hook_event_name":"Stop","last_assistant_message":"Done."}"#);
        assert_eq!(reduce(&stop), Some(SessionEventKind::TurnEnd { summary: "Done.".into() }));
        // an empty Stop is not a timeline event.
        assert_eq!(reduce(&ev(r#"{"hook_event_name":"Stop","last_assistant_message":""}"#)), None);
        // Notification → notice.
        let notif = ev(r#"{"hook_event_name":"Notification","message":"Claude is waiting for your input","notification_type":"idle_prompt"}"#);
        assert_eq!(reduce(&notif), Some(SessionEventKind::Notice { text: "Claude is waiting for your input".into() }));
        // PermissionRequest → an Awaiting breadcrumb (the card stays separate).
        let perm = ev(r#"{"hook_event_name":"PermissionRequest","tool_name":"Edit","tool_input":{"file_path":"/x/y.rs"}}"#);
        assert_eq!(reduce(&perm), Some(SessionEventKind::Awaiting { tool: "Edit".into() }));
    }

    #[test]
    fn event_line_maps_each_kind() {
        let line = |kind| event_line(&SessionEvent { seq: 1, at_ms: 0, kind });
        // Bash shows the whole command; Edit/Write show the basename.
        assert_eq!(line(SessionEventKind::Tool { verb: ToolVerb::Ran, target: "pytest -q".into() }),
            EventLine { glyph: "⏺", text: "ran pytest -q".into(), tone: EventTone::Muted });
        assert_eq!(line(SessionEventKind::Tool { verb: ToolVerb::Edited, target: "/a/b/strategy.py".into() }).text, "edited strategy.py");
        assert_eq!(line(SessionEventKind::Tool { verb: ToolVerb::Created, target: "/x/new.rs".into() }).text, "created new.rs");
        // the turn end is the DID divider, in the active tone.
        let said = line(SessionEventKind::TurnEnd { summary: "did it".into() });
        assert_eq!((said.glyph, said.text.as_str(), said.tone), ("✳", "said: did it", EventTone::Active));
        // a permission breadcrumb draws attention.
        assert_eq!(line(SessionEventKind::Awaiting { tool: "Edit".into() }).tone, EventTone::Attention);
        assert_eq!(line(SessionEventKind::Started).text, "session started");
    }

    #[test]
    fn tool_verb_and_target() {
        assert_eq!(tool_verb("Write"), ToolVerb::Created);
        assert_eq!(tool_verb("Read"), ToolVerb::Read);
        assert_eq!(tool_verb("Grep"), ToolVerb::Used);
        let v = serde_json::json!({"file_path": "/a/b.rs"});
        assert_eq!(tool_target("Write", &v), "/a/b.rs");
        // an unknown tool carries its name as the target.
        assert_eq!(tool_target("Grep", &serde_json::Value::Null), "Grep");
    }
}
