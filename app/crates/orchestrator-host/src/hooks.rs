//! Claude hook payload parsing (docs/013 §1 claude.rs, semantic plane).
//!
//! Hooks are the WHAT layer: PermissionRequest fires when a dialog renders
//! (full tool_input — the diff card content), PreToolUse before every tool
//! call (carries `tool_use_id`, THE canonical decision-row key per 012 §1.5),
//! Stop at turn end (feeds DID), Notification on waiting states.
//!
//! Schema discipline: payloads are versioned with the CLI and grow fields
//! without notice (`effort` appeared unannounced — seen in the fixtures), so
//! parsing is LENIENT: classify on `hook_event_name`, keep everything else
//! optional, never fail on unknown fields.

use serde::{Deserialize, Serialize};

/// The raw envelope every hook event shares. Extra fields are ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RawHookPayload {
    pub session_id: Option<String>,
    pub transcript_path: Option<String>,
    pub cwd: Option<String>,
    pub permission_mode: Option<String>,
    pub hook_event_name: Option<String>,
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub permission_suggestions: Option<serde_json::Value>,
    pub message: Option<String>,
    pub notification_type: Option<String>,
    pub last_assistant_message: Option<String>,
    pub stop_hook_active: Option<bool>,
}

/// A classified hook event. Every variant keeps the raw payload — the GUI
/// and decision reducer read typed accessors; nothing is thrown away.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HookEvent {
    /// The permission dialog is rendering (or AskUserQuestion — same channel).
    PermissionRequest(RawHookPayload),
    /// A tool call is about to run; carries `tool_use_id` (the decision key).
    PreToolUse(RawHookPayload),
    /// Turn ended with `last_assistant_message` — the feed's DID.
    Stop(RawHookPayload),
    /// Waiting-state notification (`permission_prompt`, `idle_prompt`, …).
    Notification(RawHookPayload),
    /// Anything else (SessionStart, SessionEnd, future events): kept, named.
    Other(String, RawHookPayload),
}

impl HookEvent {
    pub fn payload(&self) -> &RawHookPayload {
        match self {
            HookEvent::PermissionRequest(p)
            | HookEvent::PreToolUse(p)
            | HookEvent::Stop(p)
            | HookEvent::Notification(p)
            | HookEvent::Other(_, p) => p,
        }
    }
}

/// Parse one hook stdin payload. Errors only on non-JSON.
pub fn parse_hook_payload(json: &str) -> Result<HookEvent, serde_json::Error> {
    let raw: RawHookPayload = serde_json::from_str(json)?;
    let name = raw.hook_event_name.clone().unwrap_or_default();
    Ok(match name.as_str() {
        "PermissionRequest" => HookEvent::PermissionRequest(raw),
        "PreToolUse" => HookEvent::PreToolUse(raw),
        "Stop" => HookEvent::Stop(raw),
        "Notification" => HookEvent::Notification(raw),
        _ => HookEvent::Other(name, raw),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_lines(rel: &str) -> Vec<String> {
        let path = format!("{}/../../fixtures/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(String::from)
            .collect()
    }

    #[test]
    fn permission_request_carries_the_question() {
        // recorded live: PermissionRequest fired as the Bash dialog rendered
        let lines = fixture_lines("claude/2.1.172/hooks/permreq.log");
        assert!(!lines.is_empty());
        let ev = parse_hook_payload(&lines[0]).unwrap();
        match &ev {
            HookEvent::PermissionRequest(p) => {
                assert_eq!(p.tool_name.as_deref(), Some("Bash"));
                let cmd = p.tool_input.as_ref().unwrap()["command"].as_str().unwrap();
                assert!(cmd.contains("marker.txt"), "command was: {cmd}");
                assert_eq!(p.permission_mode.as_deref(), Some("default"));
            }
            other => panic!("misclassified: {other:?}"),
        }
    }

    #[test]
    fn pretooluse_carries_tool_use_id_the_decision_key() {
        let lines = fixture_lines("claude/2.1.172/hooks/pretool.log");
        let parsed: Vec<_> =
            lines.iter().map(|l| parse_hook_payload(l).unwrap()).collect();
        let with_id = parsed
            .iter()
            .filter(|e| {
                matches!(e, HookEvent::PreToolUse(_)) && e.payload().tool_use_id.is_some()
            })
            .count();
        assert!(with_id >= 1, "no PreToolUse with tool_use_id in fixture");
    }

    #[test]
    fn stop_carries_last_assistant_message_for_did() {
        let lines = fixture_lines("claude/2.1.172/hooks/stop.log");
        let ev = parse_hook_payload(&lines[0]).unwrap();
        match &ev {
            HookEvent::Stop(p) => assert!(p.last_assistant_message.is_some()),
            other => panic!("misclassified: {other:?}"),
        }
    }

    #[test]
    fn notification_is_typed() {
        let lines = fixture_lines("claude/2.1.172/hooks/notif.log");
        let ev = parse_hook_payload(&lines[0]).unwrap();
        match &ev {
            HookEvent::Notification(p) => assert!(p.notification_type.is_some()),
            other => panic!("misclassified: {other:?}"),
        }
    }

    #[test]
    fn unknown_events_survive_leniently() {
        let ev = parse_hook_payload(
            r#"{"hook_event_name":"SomeFutureEvent","session_id":"x","brand_new_field":1}"#,
        )
        .unwrap();
        match ev {
            HookEvent::Other(name, _) => assert_eq!(name, "SomeFutureEvent"),
            other => panic!("misclassified: {other:?}"),
        }
    }
}
