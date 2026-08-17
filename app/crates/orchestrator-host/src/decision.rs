//! The decision model (docs/014) — a Claude permission/question prompt turned
//! into a typed, readable card.
//!
//! INFORMATIONAL ONLY: the card shows WHAT is being asked; it never answers it.
//! The app has no answer channel — the user reads the card, then types into the
//! real dialog in the terminal, which is the only place the full, correct set of
//! choices exists.
//!
//! Pure: `extract` maps a recorded `HookEvent` to a `PendingDecision`. Tested
//! against recorded PermissionRequest payloads — never spawns claude.

use serde::{Deserialize, Serialize};

use crate::hooks::HookEvent;

/// What the agent wants to do — the card's body. The split is exactly what the
/// `010` mock renders (red/green diff for edits, mono command for Bash).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionView {
    Edit { path: String, old: String, new: String },
    Write { path: String, content: String },
    Bash { command: String, description: String },
    Question { header: String, prompt: String, options: Vec<String> },
    Other { tool: String, summary: String },
}

impl DecisionView {
    /// A one-line headline for the strip / collapsed card.
    pub fn summary(&self) -> String {
        match self {
            DecisionView::Edit { path, .. } => format!("edit {}", base(path)),
            DecisionView::Write { path, .. } => format!("create {}", base(path)),
            DecisionView::Bash { command, .. } => command.clone(),
            DecisionView::Question { prompt, .. } => prompt.clone(),
            DecisionView::Other { tool, summary } => {
                if summary.is_empty() { tool.clone() } else { format!("{tool}: {summary}") }
            }
        }
    }

}

/// A live decision the session is blocked on (the agent is paused at its
/// dialog). `tool_use_id` is the join key (docs/012 §1.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingDecision {
    pub tool_use_id: Option<String>,
    pub tool_name: String,
    pub view: DecisionView,
}

/// Build a `PendingDecision` from a PermissionRequest hook (the only event that
/// raises one). Returns `None` for other events.
pub fn extract(event: &HookEvent) -> Option<PendingDecision> {
    let p = match event {
        HookEvent::PermissionRequest(p) => p,
        _ => return None,
    };
    let tool = p.tool_name.clone().unwrap_or_default();
    let input = p.tool_input.clone().unwrap_or(serde_json::Value::Null);
    let s = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();

    let view = match tool.as_str() {
        "Edit" => DecisionView::Edit {
            path: s("file_path"),
            old: s("old_string"),
            new: s("new_string"),
        },
        // MultiEdit's hunks live in an `edits[]` array, NOT top-level — reading
        // old_string/new_string directly would render a BLANK card and the user
        // would approve an invisible edit. Concatenate the hunks into the diff.
        "MultiEdit" => {
            let mut old = String::new();
            let mut new = String::new();
            if let Some(edits) = input.get("edits").and_then(|v| v.as_array()) {
                for (i, e) in edits.iter().enumerate() {
                    if i > 0 {
                        old.push('\n');
                        new.push('\n');
                    }
                    old.push_str(e.get("old_string").and_then(|v| v.as_str()).unwrap_or(""));
                    new.push_str(e.get("new_string").and_then(|v| v.as_str()).unwrap_or(""));
                }
            }
            DecisionView::Edit { path: s("file_path"), old, new }
        }
        "Write" => DecisionView::Write { path: s("file_path"), content: s("content") },
        "Bash" => DecisionView::Bash { command: s("command"), description: s("description") },
        // LOSSY BY CONSTRUCTION: `questions` is an array (we mirror only the
        // first) and its `multiSelect` flag is dropped — the payload can never
        // reconstruct the list the TUI actually draws. The view is TEXT the user
        // reads, never a control they answer.
        "AskUserQuestion" => {
            let q = input
                .get("questions")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first());
            let header = q.and_then(|q| q.get("header")).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let prompt = q.and_then(|q| q.get("question")).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let options = q
                .and_then(|q| q.get("options"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|o| o.get("label").and_then(|v| v.as_str()).map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            DecisionView::Question { header, prompt, options }
        }
        _ => DecisionView::Other { tool: tool.clone(), summary: other_summary(&input) },
    };

    Some(PendingDecision { tool_use_id: p.tool_use_id.clone(), tool_name: tool, view })
}

/// The body of a tool we don't model — `mcp__*`, WebFetch, WebSearch,
/// NotebookEdit, ExitPlanMode. The card is the one place the user can READ what
/// is being asked before answering it in the terminal, so this must not come
/// back blank while the payload holds anything. Try the keys that name the
/// target across those tools, then fall back to a bounded dump of the raw
/// input. Blank ONLY when the payload is.
fn other_summary(input: &serde_json::Value) -> String {
    const KEYS: [&str; 9] = [
        "command",
        "file_path",
        "url",
        "path",
        "notebook_path",
        "pattern",
        "query",
        "description",
        "plan", // ExitPlanMode — the plan is the body; it must READ
    ];
    if let Some(s) = KEYS
        .iter()
        .find_map(|k| input.get(*k).and_then(|v| v.as_str()))
        .filter(|s| !s.trim().is_empty())
    {
        return s.to_string();
    }
    // No key we know: show the payload itself rather than nothing. `Null` (no
    // `tool_input` at all) and `{}` carry nothing to consent to — leave those
    // empty so the card falls back to the tool NAME instead of printing "null".
    match input {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Object(m) if m.is_empty() => String::new(),
        _ => {
            let dump = serde_json::to_string(input).unwrap_or_default();
            // truncate by CHARS — a byte-index `truncate` would panic mid-UTF-8.
            if dump.chars().count() > DUMP_CHARS {
                dump.chars().take(DUMP_CHARS - 1).chain(['…']).collect()
            } else {
                dump
            }
        }
    }
}

/// How much of an unmodelled tool's raw input the card body shows.
const DUMP_CHARS: usize = 200;

fn base(path: &str) -> &str {
    crate::util::file_name(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::parse_hook_payload;

    fn perm(json: &str) -> PendingDecision {
        extract(&parse_hook_payload(json).unwrap()).expect("should raise a decision")
    }

    #[test]
    fn bash_permission_from_real_fixture() {
        let line = std::fs::read_to_string(format!(
            "{}/../../fixtures/claude/2.1.172/hooks/permreq.log",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let first = line.lines().next().unwrap();
        let d = perm(first);
        assert_eq!(d.tool_name, "Bash");
        match d.view {
            DecisionView::Bash { command, .. } => assert!(command.contains("marker.txt")),
            other => panic!("expected Bash view, got {other:?}"),
        }
    }

    #[test]
    fn edit_yields_red_green_diff() {
        let d = perm(
            r#"{"hook_event_name":"PermissionRequest","tool_name":"Edit","tool_use_id":"toolu_1",
                "tool_input":{"file_path":"/p/src/x.rs","old_string":"a=1","new_string":"a=2"}}"#,
        );
        assert_eq!(d.tool_use_id.as_deref(), Some("toolu_1"));
        match d.view {
            DecisionView::Edit { path, old, new } => {
                assert_eq!(path, "/p/src/x.rs");
                assert_eq!(old, "a=1");
                assert_eq!(new, "a=2");
            }
            other => panic!("expected Edit, got {other:?}"),
        }
        assert_eq!(perm_summary("Edit", "/p/src/x.rs"), "edit x.rs");
    }

    fn perm_summary(_tool: &str, path: &str) -> String {
        DecisionView::Edit { path: path.into(), old: String::new(), new: String::new() }.summary()
    }

    #[test]
    fn multiedit_concatenates_hunks_not_blank() {
        // regression: MultiEdit hunks are in edits[], not top-level — must not
        // render an empty diff (the blind-approval bug).
        let d = perm(
            r#"{"hook_event_name":"PermissionRequest","tool_name":"MultiEdit",
                "tool_input":{"file_path":"/p/x.rs","edits":[
                  {"old_string":"a=1","new_string":"a=2"},
                  {"old_string":"b=3","new_string":"b=4"}]}}"#,
        );
        match d.view {
            DecisionView::Edit { old, new, .. } => {
                assert_eq!(old, "a=1\nb=3");
                assert_eq!(new, "a=2\nb=4");
                assert!(!old.is_empty() && !new.is_empty(), "MultiEdit card must not be blank");
            }
            other => panic!("expected Edit, got {other:?}"),
        }
    }

    #[test]
    fn write_is_a_new_file() {
        let d = perm(
            r#"{"hook_event_name":"PermissionRequest","tool_name":"Write",
                "tool_input":{"file_path":"/p/new.txt","content":"hi"}}"#,
        );
        match d.view {
            DecisionView::Write { path, content } => {
                assert_eq!(path, "/p/new.txt");
                assert_eq!(content, "hi");
            }
            other => panic!("expected Write, got {other:?}"),
        }
    }

    #[test]
    fn ask_user_question_is_text_the_user_reads_not_a_control() {
        // shape matches the live S1 AskUserQuestion payload
        let d = perm(
            r#"{"hook_event_name":"PermissionRequest","tool_name":"AskUserQuestion",
                "tool_input":{"questions":[{"header":"Choice","question":"A or B?",
                "options":[{"label":"A","description":"x"},{"label":"B","description":"y"}]}]}}"#,
        );
        match &d.view {
            DecisionView::Question { header, prompt, options } => {
                assert_eq!(header, "Choice");
                assert_eq!(prompt, "A or B?");
                assert_eq!(options, &vec!["A".to_string(), "B".to_string()]);
            }
            other => panic!("expected Question, got {other:?}"),
        }
        // the options are a PREFIX of the TUI's own list (which appends synthetic
        // entries) — text to READ, never a keypad.
        assert_eq!(d.tool_name, "AskUserQuestion");
    }

    #[test]
    fn multi_select_and_extra_questions_are_dropped_so_the_terminal_owns_it() {
        // the payload is an ARRAY and carries `multiSelect`; the view keeps
        // neither — proof that mirroring it as buttons could only ever guess.
        let d = perm(
            r#"{"hook_event_name":"PermissionRequest","tool_name":"AskUserQuestion",
                "tool_input":{"questions":[
                  {"header":"Pick","question":"Which?","multiSelect":true,
                   "options":[{"label":"A"},{"label":"B"}]},
                  {"header":"Second","question":"Dropped?","options":[{"label":"C"}]}]}}"#,
        );
        match &d.view {
            DecisionView::Question { prompt, options, .. } => {
                assert_eq!(prompt, "Which?"); // the 2nd question never surfaces
                assert_eq!(options.len(), 2);
            }
            other => panic!("expected Question, got {other:?}"),
        }
    }

    #[test]
    fn exit_plan_mode_still_renders_its_plan() {
        // ExitPlanMode's body is the plan itself — the card is where the user
        // READS it before answering the 3-way dialog in the terminal.
        let d = perm(
            r#"{"hook_event_name":"PermissionRequest","tool_name":"ExitPlanMode",
                "tool_input":{"plan":"Plan:\n1. do it"}}"#,
        );
        match &d.view {
            DecisionView::Other { tool, summary } => {
                assert_eq!(tool, "ExitPlanMode");
                assert!(summary.contains("do it"), "body should show the plan, got {summary:?}");
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn unmodelled_tools_show_what_they_do() {
        // REGRESSION (#27 fallout): the catch-all only ever read
        // `command`/`file_path`, so every MCP tool, WebFetch, WebSearch and
        // NotebookEdit rendered an EMPTY card — nothing to read before walking to
        // the terminal. The PermissionRequest hook is installed with no matcher,
        // so user MCP servers land here for real.
        let cases = [
            (
                r#"{"hook_event_name":"PermissionRequest","tool_name":"mcp__qc-mcp__read_project",
                    "tool_input":{"projectId":123}}"#,
                "123", // no known key → bounded dump of the payload
            ),
            (
                r#"{"hook_event_name":"PermissionRequest","tool_name":"WebFetch",
                    "tool_input":{"url":"https://example.com/x","prompt":"summarize"}}"#,
                "https://example.com/x",
            ),
            (
                r#"{"hook_event_name":"PermissionRequest","tool_name":"WebSearch",
                    "tool_input":{"query":"gpui focus handle"}}"#,
                "gpui focus handle",
            ),
            (
                r#"{"hook_event_name":"PermissionRequest","tool_name":"NotebookEdit",
                    "tool_input":{"notebook_path":"/p/a.ipynb","new_source":"x=1"}}"#,
                "/p/a.ipynb",
            ),
        ];
        for (json, expect) in cases {
            let d = perm(json);
            match &d.view {
                DecisionView::Other { summary, .. } => {
                    assert!(
                        !summary.trim().is_empty(),
                        "{} must not render a BLANK card — you can't consent to nothing",
                        d.tool_name
                    );
                    assert!(
                        summary.contains(expect),
                        "{} body should show {expect:?}, got {summary:?}",
                        d.tool_name
                    );
                }
                other => panic!("expected Other, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_payloadless_tool_renders_no_body_rather_than_the_word_null() {
        // `tool_input` absent → serde would happily dump "null"; an empty object
        // would dump "{}". Both are noise, so the summary stays empty and the UI
        // falls back to the tool NAME.
        for json in [
            r#"{"hook_event_name":"PermissionRequest","tool_name":"SomeTool"}"#,
            r#"{"hook_event_name":"PermissionRequest","tool_name":"SomeTool","tool_input":{}}"#,
        ] {
            let d = perm(json);
            match &d.view {
                DecisionView::Other { tool, summary } => {
                    assert_eq!(tool, "SomeTool");
                    assert_eq!(summary, "", "empty payload must not print 'null'/'{{}}'");
                    // the strip headline falls back to the tool name, never blank.
                    assert_eq!(d.view.summary(), "SomeTool");
                }
                other => panic!("expected Other, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_huge_unmodelled_payload_is_truncated_without_splitting_a_char() {
        // the dump is bounded so one giant MCP argument can't become the card.
        // A byte-index `truncate` would PANIC mid-UTF-8; assert the multi-byte
        // case explicitly.
        let big = "é".repeat(500);
        let d = perm(&format!(
            r#"{{"hook_event_name":"PermissionRequest","tool_name":"mcp__x__y",
                "tool_input":{{"blob":"{big}"}}}}"#
        ));
        match &d.view {
            DecisionView::Other { summary, .. } => {
                assert_eq!(summary.chars().count(), 200);
                assert!(summary.ends_with('…'));
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn codex_pendings_render_a_readable_body() {
        // codex raises its OWN pendings (session.rs `codex_pending_from_title` /
        // `codex_pending_from_notify`) — it has no PermissionRequest hook, so it
        // never goes through `extract`. These fixtures MIRROR those two
        // constructors: whatever raised the card, it must carry something to READ,
        // because the card is all the user gets before they walk to the terminal.
        let title = PendingDecision {
            tool_use_id: None,
            tool_name: "Codex".into(),
            view: DecisionView::Other { tool: "Codex".into(), summary: "Codex needs your approval".into() },
        };
        let notify_cmd = PendingDecision {
            tool_use_id: None,
            tool_name: "Codex".into(),
            view: DecisionView::Bash { command: "rm -rf build".into(), description: String::new() },
        };
        let notify_edit = PendingDecision {
            tool_use_id: None,
            tool_name: "Codex".into(),
            view: DecisionView::Other { tool: "Codex".into(), summary: "codex wants to edit main.rs".into() },
        };
        for d in [&title, &notify_cmd, &notify_edit] {
            assert!(!d.view.summary().trim().is_empty(), "codex card must not be blank: {:?}", d.view);
        }
    }

    #[test]
    fn non_permission_events_raise_nothing() {
        let stop = parse_hook_payload(r#"{"hook_event_name":"Stop","last_assistant_message":"done"}"#).unwrap();
        assert!(extract(&stop).is_none());
    }
}
