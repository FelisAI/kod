//! claude transcript → [`SessionLog`] (docs/028 §4).
//!
//! A claude transcript has no turn markers, so a turn is everything from one
//! typed prompt to the next. The API splits an assistant message across lines,
//! one content block each, so each line is read on its own. Tool results come
//! back as `user` lines holding `tool_result` blocks, and are paired with their
//! `tool_use` by id. Sidechain lines (a subagent's own conversation) are left
//! out: the receiver needs the subagent's result, which the parent already holds.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use serde_json::Value;

use super::codex::trim_output;
use super::log::{one_line, Event, EventKind, PlanItem, SessionLog, Turn};
use crate::session::CliKind;
use crate::transcript::iso_to_ms;

pub fn parse_transcript(path: &Path) -> io::Result<SessionLog> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut p = Parser::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        p.line(&buf);
    }
    let mut log = p.finish();
    log.source_path = Some(path.display().to_string());
    Ok(log)
}

/// The same, from text already in memory.
#[cfg(test)]
pub fn parse_text(text: &str) -> SessionLog {
    let mut p = Parser::new();
    for l in text.lines() {
        p.line(l.as_bytes());
    }
    p.finish()
}

struct Parser {
    log: SessionLog,
    open: Option<Turn>,
    /// `tool_use` id → index of its event in the open turn, until its result arrives.
    pending: HashMap<String, usize>,
}

impl Parser {
    fn new() -> Self {
        Parser { log: SessionLog::empty(CliKind::Claude), open: None, pending: HashMap::new() }
    }

    fn line(&mut self, raw: &[u8]) {
        let Ok(v) = serde_json::from_slice::<Value>(raw) else {
            return;
        };
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let at = v.get("timestamp").and_then(Value::as_str).and_then(iso_to_ms);
        self.meta(&v, at);
        match v.get("type").and_then(Value::as_str) {
            Some("user") => self.user(&v, at),
            Some("assistant") => self.assistant(&v, at),
            Some("system") if v.get("subtype").and_then(Value::as_str) == Some("compact_boundary") => {
                self.log.compactions += 1;
                self.push(at, EventKind::Compaction);
            }
            _ => {}
        }
        if let (Some(t), Some(at)) = (self.open.as_mut(), at) {
            t.ended_ms = Some(at);
        }
    }

    fn meta(&mut self, v: &Value, at: Option<u64>) {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
        if self.log.session_id.is_empty() {
            if let Some(id) = s("sessionId") {
                self.log.session_id = id;
            }
        }
        if self.log.cwd.is_none() {
            self.log.cwd = s("cwd");
        }
        if self.log.started_ms.is_none() {
            self.log.started_ms = at;
        }
        if let Some(b) = s("gitBranch").filter(|b| !b.is_empty()) {
            self.log.branch = Some(b);
        }
        if let Some(ver) = s("version") {
            self.log.cli_version = Some(ver);
        }
    }

    fn user(&mut self, v: &Value, at: Option<u64>) {
        // Injected context and the summary a compaction writes are not prompts.
        if v.get("isMeta").and_then(Value::as_bool) == Some(true)
            || v.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
        {
            return;
        }
        let content = v.get("message").and_then(|m| m.get("content"));
        let mut texts: Vec<String> = Vec::new();
        let mut results = 0;
        match content {
            Some(Value::String(s)) => texts.push(s.clone()),
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("tool_result") => {
                            results += 1;
                            self.result(b);
                        }
                        Some("text") => texts.extend(b.get("text").and_then(Value::as_str).map(String::from)),
                        Some("image") => texts.push("[image]".to_string()),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        if results > 0 && texts.is_empty() {
            return; // a tool result's carrier, not a prompt
        }
        let Some(text) = prompt_text(&texts.join("\n")) else {
            return;
        };
        self.start_turn(at);
        self.push(at, EventKind::User { text });
    }

    fn assistant(&mut self, v: &Value, at: Option<u64>) {
        let msg = v.get("message");
        // A refusal — rate limit, overload — is written as an assistant line.
        if v.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) {
            let text = content_text(msg.and_then(|m| m.get("content")));
            let code = v.get("error").and_then(Value::as_str);
            let err = match (text.is_empty(), code) {
                (false, Some(c)) => format!("{text} ({c})"),
                (false, None) => text,
                (true, Some(c)) => c.to_string(),
                (true, None) => "API error".to_string(),
            };
            self.ensure_open(at).error = Some(err);
            return;
        }
        if let Some(m) = msg.and_then(|m| m.get("model")).and_then(Value::as_str) {
            if m != "<synthetic>" {
                self.log.model = Some(m.to_string());
            }
        }
        let Some(blocks) = msg.and_then(|m| m.get("content")).and_then(Value::as_array) else {
            return;
        };
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = b.get("text").and_then(Value::as_str).unwrap_or("").trim();
                    if !text.is_empty() {
                        self.push(at, EventKind::Agent { text: text.to_string(), phase: None });
                    }
                }
                Some("tool_use") => self.tool_use(b, at),
                _ => {} // thinking: signed and, in the transcript, empty
            }
        }
    }

    fn tool_use(&mut self, b: &Value, at: Option<u64>) {
        let name = b.get("name").and_then(Value::as_str).unwrap_or("");
        let input = b.get("input").unwrap_or(&Value::Null);
        let field = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let kind = match name {
            "Bash" => EventKind::Command { cmd: field("command"), cwd: None, exit: None, failed: false, output: String::new() },
            "Edit" | "Write" | "MultiEdit" => EventKind::Edit { paths: vec![field("file_path")] },
            "NotebookEdit" => EventKind::Edit { paths: vec![field("notebook_path")] },
            "Read" => EventKind::Read { paths: vec![field("file_path")] },
            "TodoWrite" => {
                self.log.plan = input
                    .get("todos")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|t| PlanItem {
                        text: t.get("content").and_then(Value::as_str).unwrap_or("").to_string(),
                        status: t.get("status").and_then(Value::as_str).unwrap_or("").to_string(),
                    })
                    .collect();
                return;
            }
            "AskUserQuestion" => EventKind::Question { text: ask_text(input) },
            "Task" | "Agent" => EventKind::Tool { name: "subagent".to_string(), detail: one_line(&field("description"), 200), failed: false },
            "Grep" | "Glob" => EventKind::Tool { name: name.to_string(), detail: one_line(&field("pattern"), 200), failed: false },
            other => EventKind::Tool { name: other.to_string(), detail: one_line(&input.to_string(), 200), failed: false },
        };
        let id = b.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let i = self.push(at, kind);
        if !id.is_empty() {
            self.pending.insert(id, i);
        }
    }

    /// Pair a `tool_result` with its `tool_use`: a command gets its output, a
    /// question gets its answer, and anything flagged `is_error` is marked failed.
    fn result(&mut self, b: &Value) {
        let Some(id) = b.get("tool_use_id").and_then(Value::as_str) else {
            return;
        };
        let Some(i) = self.pending.remove(id) else {
            return;
        };
        let is_error = b.get("is_error").and_then(Value::as_bool) == Some(true);
        let text = content_text(b.get("content"));
        let Some(ev) = self.open.as_mut().and_then(|t| t.events.get_mut(i)) else {
            return;
        };
        match &mut ev.kind {
            EventKind::Command { output, failed, .. } => {
                *output = trim_output(&text);
                *failed = is_error;
            }
            EventKind::Question { text: q } if !text.is_empty() => {
                q.push_str(&format!(" → {}", one_line(&text, 300)));
            }
            EventKind::Tool { failed, .. } => *failed = is_error,
            _ => {}
        }
    }

    fn start_turn(&mut self, at: Option<u64>) {
        self.close_turn();
        self.open = Some(Turn::new(self.log.turns.len() + 1, at));
    }

    fn ensure_open(&mut self, at: Option<u64>) -> &mut Turn {
        if self.open.is_none() {
            self.start_turn(at);
        }
        self.open.as_mut().expect("just opened")
    }

    /// Push into the open turn; returns the event's index there.
    fn push(&mut self, at: Option<u64>, kind: EventKind) -> usize {
        let t = self.ensure_open(at);
        t.events.push(Event { at_ms: at.unwrap_or(0), kind });
        t.events.len() - 1
    }

    fn close_turn(&mut self) {
        self.pending.clear();
        if let Some(t) = self.open.take() {
            self.log.turns.push(t);
        }
    }

    fn finish(mut self) -> SessionLog {
        self.close_turn();
        self.log
    }
}

/// What the user typed, or `None` for what only looks like a prompt. A slash
/// command is kept as `/name args`; its local output and caveats are not.
fn prompt_text(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t.starts_with("<local-command-") || t.starts_with("Caveat:") {
        return None;
    }
    if t.contains("<command-name>") {
        let tag = |name: &str| {
            let open = format!("<{name}>");
            let close = format!("</{name}>");
            let start = t.find(&open)? + open.len();
            let end = t[start..].find(&close)? + start;
            Some(t[start..end].trim().to_string())
        };
        let name = tag("command-name")?;
        let args = tag("command-args").unwrap_or_default();
        return Some(if args.is_empty() { name } else { format!("{name} {args}") });
    }
    Some(t.to_string())
}

fn content_text(c: Option<&Value>) -> String {
    match c {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string(),
        _ => String::new(),
    }
}

/// `AskUserQuestion` input → "question — options: A; B" per question.
fn ask_text(input: &Value) -> String {
    input
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|q| {
            let title = q.get("question").and_then(Value::as_str)?;
            let opts: Vec<&str> = q
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|o| o.get("label").and_then(Value::as_str))
                .collect();
            Some(if opts.is_empty() { title.to_string() } else { format!("{title} — options: {}", opts.join("; ")) })
        })
        .collect::<Vec<_>>()
        .join(" / ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(rel: &str) -> String {
        let path = format!("{}/../../fixtures/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(path).unwrap()
    }

    // Recorded live (2.1.172): a typed prompt, three tool calls refused by a
    // hook (`is_error: true`), then a closing message.
    #[test]
    fn a_recorded_transcript_becomes_one_turn() {
        let log = parse_text(&fixture("claude/2.1.172/transcripts/e6bc7cae-8dff-45be-8833-6be1eb98dc96.jsonl"));
        assert_eq!(log.kind, CliKind::Claude);
        assert_eq!(log.turns.len(), 1);
        let t = &log.turns[0];
        assert!(matches!(&t.events[0].kind, EventKind::User { text } if text == "create a file marker.txt containing ok"));
        assert!(matches!(&t.events[1].kind, EventKind::Edit { paths } if paths == &["/private/tmp/orchspike-s1/proj/marker.txt"]));
        assert_eq!(log.commands(), 2);
        assert_eq!(log.failed_commands(), 2, "both Bash calls came back is_error");
        assert!(matches!(&t.events.last().unwrap().kind, EventKind::Agent { text, .. } if text.starts_with("I'm encountering a system-level error")));
        assert!(t.ended_ms >= t.started_ms);
    }

    // Recorded live: an AskUserQuestion and the user's pick come back as one line.
    #[test]
    fn a_question_carries_its_answer() {
        let log = parse_text(&fixture("claude/2.1.172/transcripts/4674bdc3-c5fe-44e8-82cd-23d11252ee4f.jsonl"));
        let q = log.all_events().find_map(|e| match &e.kind {
            EventKind::Question { text } => Some(text.clone()),
            _ => None,
        });
        let q = q.expect("the AskUserQuestion is kept");
        assert!(q.starts_with("Which would you prefer to use? — options: A; B → "), "{q}");
        assert!(q.contains("=\"A\""), "{q}");
    }

    #[test]
    fn a_limit_refusal_is_the_turns_error_and_todo_lists_are_kept() {
        let text = [
            r#"{"type":"user","timestamp":"2026-10-01T00:00:00.000Z","sessionId":"s1","cwd":"/w","message":{"role":"user","content":"ship it"}}"#,
            r#"{"type":"assistant","timestamp":"2026-10-01T00:00:01.000Z","message":{"model":"claude-opus-5-5","content":[{"type":"tool_use","id":"t1","name":"TodoWrite","input":{"todos":[{"content":"write tests","status":"completed"},{"content":"release","status":"in_progress"}]}}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-10-01T00:00:02.000Z","message":{"model":"claude-opus-5-5","content":[{"type":"text","text":"Tests are green; releasing."}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-10-01T00:00:03.000Z","isApiErrorMessage":true,"error":"rate_limit","message":{"model":"<synthetic>","content":[{"type":"text","text":"You've hit your limit · resets 4pm"}]}}"#,
            r#"{"type":"user","timestamp":"2026-10-01T00:00:04.000Z","isMeta":true,"message":{"role":"user","content":"<system-reminder>x</system-reminder>"}}"#,
            r#"{"type":"user","timestamp":"2026-10-01T00:00:05.000Z","message":{"role":"user","content":"<command-name>/model</command-name><command-args>sonnet</command-args>"}}"#,
            r#"{"type":"user","timestamp":"2026-10-01T00:00:06.000Z","isSidechain":true,"message":{"role":"user","content":"subagent prompt"}}"#,
        ]
        .join("\n");
        let log = parse_text(&text);
        assert_eq!(log.session_id, "s1");
        assert_eq!(log.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(log.turns.len(), 2, "meta and sidechain lines open no turn");
        assert_eq!(log.turns[0].error.as_deref(), Some("You've hit your limit · resets 4pm (rate_limit)"));
        assert_eq!(log.plan.len(), 2);
        assert_eq!(log.plan[1], PlanItem { text: "release".into(), status: "in_progress".into() });
        assert!(matches!(&log.turns[1].events[0].kind, EventKind::User { text } if text == "/model sonnet"));
        assert_eq!(log.last_working_turn().map(|t| t.n), Some(1));
    }
}
