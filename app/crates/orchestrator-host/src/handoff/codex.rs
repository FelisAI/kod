//! codex rollout → [`SessionLog`] (docs/028 §4).
//!
//! codex ≥ 0.15x records every finished item as `event_msg/item_completed` —
//! `UserMessage`, `AgentMessage`, `CommandExecution`, `FileChange`, … — the same
//! items its own UI shows, so those are the source. A turn with no item records
//! (an older codex) falls back to the raw Responses items it wrote instead.
//!
//! Rollouts get big (144 MB measured, 74% of it raw tool output the item records
//! repeat), so the file is streamed, and once item records are known to exist the
//! raw output lines are dropped on a byte sniff, without a JSON parse.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use serde_json::Value;

use super::log::{head_tail, one_line, Event, EventKind, SessionLog, Turn};
use crate::session::CliKind;
use crate::transcript::iso_to_ms;

/// How far into a line the type sniff looks. A rollout line opens with
/// `{"timestamp":…,"ordinal":…,"type":…,"payload":{"type":…` — both types sit in
/// the first ~120 bytes. A JSON string can't contain an unescaped `"type":"…"`,
/// so message text can't fake a match.
const SNIFF: usize = 200;
/// Lines that carry nothing a handoff renders.
const SKIP_ALWAYS: [&[u8]; 4] = [
    b"\"type\":\"token_count\"",
    b"\"type\":\"token_usage_record\"",
    b"\"type\":\"world_state\"",
    b"\"type\":\"reasoning\"", // encrypted; its plaintext summary is empty (measured)
];
/// Raw tool output: dropped once item records exist, which carry it already.
const SKIP_WITH_ITEMS: [&[u8]; 2] =
    [b"\"type\":\"custom_tool_call_output\"", b"\"type\":\"function_call_output\""];

/// Output kept per command: first 12 + last 20 lines, at most 3,000 chars.
pub(crate) fn trim_output(s: &str) -> String {
    head_tail(s, 12, 20, 3000)
}

pub fn parse_rollout(path: &Path) -> io::Result<SessionLog> {
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

/// A turn being read. Raw-Responses events wait in `fallback` and are used only
/// if the turn turns out to have no item records.
struct Open {
    turn: Turn,
    fallback: Vec<Event>,
    has_items: bool,
    /// Fallback commands awaiting their `function_call_output`, by call_id.
    pending: HashMap<String, usize>,
}

struct Parser {
    log: SessionLog,
    seen_items: bool,
    seen_tasks: bool,
    open: Option<Open>,
}

impl Parser {
    fn new() -> Self {
        Parser { log: SessionLog::empty(CliKind::Codex), seen_items: false, seen_tasks: false, open: None }
    }

    fn line(&mut self, raw: &[u8]) {
        let head = &raw[..raw.len().min(SNIFF)];
        if has(head, b"\"type\":\"compacted\"") {
            self.log.compactions += 1;
            return;
        }
        if SKIP_ALWAYS.iter().any(|p| has(head, p))
            || (self.seen_items && SKIP_WITH_ITEMS.iter().any(|p| has(head, p)))
        {
            return;
        }
        let Ok(v) = serde_json::from_slice::<Value>(raw) else {
            return;
        };
        let at = v.get("timestamp").and_then(Value::as_str).and_then(iso_to_ms);
        let payload = v.get("payload").unwrap_or(&Value::Null);
        match str_of(&v, "type") {
            "session_meta" => self.meta(payload, at),
            "turn_context" => {
                if let Some(m) = payload.get("model").and_then(Value::as_str) {
                    self.log.model = Some(m.to_string());
                }
                if let Some(e) = payload.get("effort").and_then(Value::as_str) {
                    self.log.effort = Some(e.to_string());
                }
            }
            "event_msg" => match str_of(payload, "type") {
                "task_started" => {
                    self.seen_tasks = true;
                    self.start_turn(at);
                }
                "task_complete" => {
                    let o = self.ensure_open(at);
                    o.turn.ended_ms = at.or(o.turn.ended_ms);
                    o.turn.error = payload.get("error").and_then(error_text);
                }
                "item_completed" => {
                    self.seen_items = true;
                    if let Some(kind) = payload.get("item").and_then(item_kind) {
                        let ev = Event { at_ms: at.unwrap_or(0), kind };
                        let o = self.ensure_open(at);
                        o.has_items = true;
                        // After task_complete: a result the agent never saw.
                        if o.turn.ended_ms.is_some() {
                            o.turn.late.push(ev);
                        } else {
                            o.turn.events.push(ev);
                        }
                    }
                }
                "patch_apply_end" if !self.seen_items => {
                    let paths = change_paths(payload.get("changes"));
                    if !paths.is_empty() {
                        let at_ms = at.unwrap_or(0);
                        self.ensure_open(at).fallback.push(Event { at_ms, kind: EventKind::Edit { paths } });
                    }
                }
                _ => {}
            },
            "response_item" => self.response_item(payload, at),
            _ => {}
        }
    }

    fn meta(&mut self, p: &Value, at: Option<u64>) {
        let s = |k: &str| p.get(k).and_then(Value::as_str).map(String::from);
        // The first meta is this session's own; a later one (a resume) only
        // updates which CLI version last wrote the file.
        if self.log.session_id.is_empty() {
            self.log.session_id = s("id").unwrap_or_default();
            self.log.cwd = s("cwd");
            self.log.started_ms = s("timestamp").as_deref().and_then(iso_to_ms).or(at);
            if let Some(g) = p.get("git") {
                self.log.start_commit = g.get("commit_hash").and_then(Value::as_str).map(String::from);
                self.log.branch = g.get("branch").and_then(Value::as_str).map(String::from);
            }
        }
        if let Some(v) = s("cli_version") {
            self.log.cli_version = Some(v);
        }
    }

    fn response_item(&mut self, p: &Value, at: Option<u64>) {
        let at_ms = at.unwrap_or(0);
        match str_of(p, "type") {
            "function_call" => {
                let name = str_of(p, "name");
                let args = p
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .unwrap_or(Value::Null);
                // Item records don't carry the agent's questions; this does, in
                // either mode.
                if name.starts_with("request_user_input") {
                    for text in questions(&args) {
                        self.ensure_open(at).turn.events.push(Event { at_ms, kind: EventKind::Question { text } });
                    }
                    return;
                }
                if self.seen_items || !matches!(name, "exec_command" | "shell" | "local_shell" | "container.exec") {
                    return;
                }
                let cmd = args
                    .get("cmd")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or_else(|| args.get("command").map(command_text))
                    .unwrap_or_default();
                if cmd.trim().is_empty() {
                    return;
                }
                let cwd = args.get("workdir").and_then(Value::as_str).map(String::from);
                let call_id = str_of(p, "call_id").to_string();
                let o = self.ensure_open(at);
                o.fallback.push(Event {
                    at_ms,
                    kind: EventKind::Command { cmd, cwd, exit: None, failed: false, output: String::new() },
                });
                o.pending.insert(call_id, o.fallback.len() - 1);
            }
            "function_call_output" if !self.seen_items => {
                let call_id = str_of(p, "call_id").to_string();
                let o = self.ensure_open(at);
                let Some(&i) = o.pending.get(&call_id) else {
                    return;
                };
                let (text, code) = call_output(p.get("output"));
                if let Some(Event { kind: EventKind::Command { output, exit, failed, .. }, .. }) =
                    o.fallback.get_mut(i)
                {
                    *output = trim_output(&text);
                    *exit = code;
                    *failed = code.is_some_and(|c| c != 0);
                }
            }
            "message" if !self.seen_items => {
                let text = content_text(p.get("content"));
                if text.is_empty() {
                    return;
                }
                let kind = match str_of(p, "role") {
                    "user" if !injected(&text) => EventKind::User { text },
                    "assistant" => EventKind::Agent { text, phase: p.get("phase").and_then(Value::as_str).map(String::from) },
                    _ => return, // developer/system context, AGENTS.md, environment
                };
                // A codex too old to mark turns: each prompt after some work
                // opens the next one.
                if !self.seen_tasks
                    && matches!(kind, EventKind::User { .. })
                    && self.open.as_ref().is_some_and(|o| o.fallback.iter().any(|e| !matches!(e.kind, EventKind::User { .. })))
                {
                    self.start_turn(at);
                }
                self.ensure_open(at).fallback.push(Event { at_ms, kind });
            }
            _ => {}
        }
    }

    fn start_turn(&mut self, at: Option<u64>) {
        self.close_turn();
        let n = self.log.turns.len() + 1;
        self.open = Some(Open { turn: Turn::new(n, at), fallback: Vec::new(), has_items: false, pending: HashMap::new() });
    }

    fn ensure_open(&mut self, at: Option<u64>) -> &mut Open {
        if self.open.is_none() {
            self.start_turn(at);
        }
        self.open.as_mut().expect("just opened")
    }

    fn close_turn(&mut self) {
        let Some(mut o) = self.open.take() else {
            return;
        };
        if !o.has_items && !o.fallback.is_empty() {
            // Questions were pushed straight into `events`; merge them in by time.
            let questions = std::mem::take(&mut o.turn.events);
            o.turn.events = o.fallback;
            o.turn.events.extend(questions);
            o.turn.events.sort_by_key(|e| e.at_ms);
        }
        if o.turn.events.is_empty() && o.turn.late.is_empty() && o.turn.error.is_none() {
            return; // an empty task_started/complete pair says nothing
        }
        o.turn.n = self.log.turns.len() + 1;
        self.log.turns.push(o.turn);
    }

    fn finish(mut self) -> SessionLog {
        self.close_turn();
        self.log
    }
}

fn has(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

/// One `item_completed` item → an event; `None` for what a reader doesn't need
/// (reasoning is encrypted, and its summary was empty in every measured item).
fn item_kind(it: &Value) -> Option<EventKind> {
    let s = |k: &str| it.get(k).and_then(Value::as_str);
    Some(match s("type")? {
        "UserMessage" => EventKind::User { text: nonempty(content_text(it.get("content")))? },
        "AgentMessage" => EventKind::Agent {
            text: nonempty(content_text(it.get("content")))?,
            phase: s("phase").map(String::from),
        },
        "CommandExecution" => {
            let exit = it.get("exit_code").and_then(Value::as_i64);
            EventKind::Command {
                cmd: it.get("command").map(command_text).unwrap_or_default(),
                cwd: s("cwd").map(String::from),
                exit,
                failed: exit.map_or(s("status") == Some("failed"), |c| c != 0),
                output: trim_output(s("aggregated_output").unwrap_or("")),
            }
        }
        "FileChange" => EventKind::Edit { paths: change_paths(it.get("changes")) },
        "ImageView" => EventKind::Image { path: s("path").unwrap_or("").trim_start_matches("file://").to_string() },
        "McpToolCall" => EventKind::Tool {
            name: format!("mcp {}.{}", s("server").unwrap_or("?"), s("tool").unwrap_or("?")),
            detail: it.get("arguments").map(|a| one_line(&a.to_string(), 200)).unwrap_or_default(),
            failed: s("status") == Some("failed"),
        },
        "Extension" => EventKind::Tool {
            name: s("kind").unwrap_or("extension").to_string(),
            detail: one_line(s("query").unwrap_or(""), 200),
            failed: false,
        },
        "ContextCompaction" => EventKind::Compaction,
        "Reasoning" => return None,
        other => EventKind::Tool { name: other.to_string(), detail: String::new(), failed: false },
    })
}

fn nonempty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

/// `["/bin/zsh","-lc","<script>"]` → the script; a plain string as is.
fn command_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => {
            let parts: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
            if parts.len() >= 3 && matches!(parts[1], "-lc" | "-c") {
                parts[2..].join(" ")
            } else {
                parts.join(" ")
            }
        }
        _ => String::new(),
    }
}

/// The text of a message's content: a string, or a list of text/image parts
/// (`Text`/`text`/`input_text`/`output_text` all carry `text`).
fn content_text(c: Option<&Value>) -> String {
    let Some(c) = c else {
        return String::new();
    };
    if let Some(s) = c.as_str() {
        return s.trim().to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    for b in c.as_array().into_iter().flatten() {
        if let Some(t) = b.get("text").and_then(Value::as_str) {
            parts.push(t.to_string());
        } else if str_of(b, "type").to_ascii_lowercase().contains("image") {
            parts.push(match b.get("path").and_then(Value::as_str) {
                Some(p) => format!("[image: {p}]"),
                None => "[image]".to_string(),
            });
        }
    }
    parts.join("\n").trim().to_string()
}

/// `{"path": {"type": "update", …}}` or `[{"path": …}]` → the paths.
fn change_paths(c: Option<&Value>) -> Vec<String> {
    match c {
        Some(Value::Object(m)) => m.keys().cloned().collect(),
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.get("path").and_then(Value::as_str).map(String::from)).collect(),
        _ => Vec::new(),
    }
}

/// Context codex writes as a `user` message that the user never typed.
fn injected(text: &str) -> bool {
    const PREFIXES: [&str; 6] = [
        "<environment_context>",
        "<user_instructions>",
        "# AGENTS.md instructions",
        "<turn_aborted>",
        "<user_shell_command>",
        "<permissions instructions>",
    ];
    let t = text.trim_start();
    PREFIXES.iter().any(|p| t.starts_with(p))
}

/// `request_user_input` arguments → one line per question, with its options.
fn questions(args: &Value) -> Vec<String> {
    args.get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|q| {
            let title = q.get("title").or_else(|| q.get("question")).and_then(Value::as_str)?;
            let opts: Vec<String> = q
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|o| o.as_str().or_else(|| o.get("label").and_then(Value::as_str)).map(|o| one_line(o, 80)))
                .collect();
            let title = one_line(title, 400);
            Some(if opts.is_empty() { title } else { format!("{title} — options: {}", opts.join("; ")) })
        })
        .collect()
}

/// A task's error → "message (code)". codex writes
/// `{"message": "Your workspace is out of credits…", "codex_error_info": "usage_limit_exceeded"}`.
fn error_text(e: &Value) -> Option<String> {
    match e {
        Value::Null => None,
        Value::String(s) => nonempty(s.trim().to_string()),
        Value::Object(o) => {
            let msg = o.get("message").and_then(Value::as_str).unwrap_or("").trim();
            let msg = if msg.is_empty() { e.to_string() } else { msg.to_string() };
            let code = match o.get("codex_error_info") {
                Some(Value::String(c)) => Some(c.clone()),
                Some(Value::Object(m)) => m.keys().next().cloned(),
                _ => None,
            };
            Some(match code {
                Some(c) => format!("{msg} ({c})"),
                None => msg,
            })
        }
        other => Some(other.to_string()),
    }
}

/// An older codex's `function_call_output`: a string that is often itself JSON,
/// `{"output": "…", "metadata": {"exit_code": 0}}`.
fn call_output(o: Option<&Value>) -> (String, Option<i64>) {
    let parsed = match o {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).unwrap_or_else(|_| Value::String(s.clone())),
        Some(v) => v.clone(),
        None => return (String::new(), None),
    };
    match &parsed {
        Value::String(s) => (s.clone(), None),
        v => (
            v.get("output").and_then(Value::as_str).unwrap_or("").to_string(),
            v.get("metadata").and_then(|m| m.get("exit_code")).and_then(Value::as_i64),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(rel: &str) -> String {
        let path = format!("{}/../../fixtures/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(path).unwrap()
    }

    // Shaped line for line on the 0.155.1 rollout measured for docs/028 (the
    // live landscaping session), with the content replaced.
    #[test]
    fn item_records_become_the_log() {
        let log = parse_text(&fixture("codex/0.155.1/rollout/handoff_limit.jsonl"));
        assert_eq!(log.session_id, "01a0f38a-0000-7000-a000-000000000001");
        assert_eq!(log.cwd.as_deref(), Some("/work/garden"));
        assert_eq!(log.start_commit.as_deref(), Some("b3edec722e32d2fa4239e7d2afbf2d520f304545"));
        assert_eq!(log.model.as_deref(), Some("gpt-6"));
        assert_eq!(log.effort.as_deref(), Some("xhigh"));
        assert_eq!(log.compactions, 1);
        assert_eq!(log.turns.len(), 2);
        assert_eq!(log.user_messages(), 2);
        // One command in the turn, one that finished after it ended.
        assert_eq!(log.commands(), 2);
        assert_eq!(log.failed_commands(), 0);

        let t1 = &log.turns[0];
        assert_eq!(t1.error.as_deref(), Some("Your workspace is out of credits. Add credits to continue. (usage_limit_exceeded)"));
        match &t1.events[0].kind {
            EventKind::User { text } => assert_eq!(text, "Make the loading message say where it's loading from"),
            other => panic!("first event should be the prompt: {other:?}"),
        }
        assert!(t1.events.iter().any(|e| matches!(&e.kind, EventKind::Edit { paths } if paths == &["/work/garden/App/ContentView.swift"])));
        assert!(t1.events.iter().any(|e| matches!(&e.kind, EventKind::Image { path } if path == "/tmp/before.png")));
        assert!(t1.events.iter().any(|e| matches!(&e.kind, EventKind::Tool { name, failed: true, .. } if name == "mcp yardeye.composition")));
        assert!(t1.events.iter().any(|e| matches!(&e.kind, EventKind::Question { text } if text.starts_with("Which size should the design assume? — options: Natural spread"))));
        assert!(t1.events.iter().any(|e| matches!(e.kind, EventKind::Compaction)));
        // The argv's script, not the shell wrapper; output trimmed to head + tail.
        match &t1.events.iter().find(|e| matches!(e.kind, EventKind::Command { .. })).unwrap().kind {
            EventKind::Command { cmd, exit, output, .. } => {
                assert_eq!(cmd, "swift test 2>&1 | tail -40");
                assert_eq!(*exit, Some(0));
                assert!(output.contains("lines omitted") && output.ends_with("all 40 tests passed"), "{output}");
            }
            _ => unreachable!(),
        }
        match &t1.late[..] {
            [Event { kind: EventKind::Command { cmd, exit: Some(0), .. }, .. }] => {
                assert_eq!(cmd, "python3 tools/selftest.py > /tmp/garden-after.log 2>&1")
            }
            other => panic!("the selftest finished after the cutoff: {other:?}"),
        }
        assert_eq!(log.last_working_turn().map(|t| t.n), Some(1));
        assert!(!log.turns[1].did_work(), "the failed `continue` did no work");
    }

    // An older codex wrote no item records: messages and commands come from the
    // raw Responses items, and injected context is not a user message.
    #[test]
    fn a_rollout_without_item_records_falls_back_to_raw_items() {
        let log = parse_text(&fixture("codex/0.132.0/rollout/handoff_legacy.jsonl"));
        assert_eq!(log.turns.len(), 1);
        let kinds: Vec<&EventKind> = log.turns[0].events.iter().map(|e| &e.kind).collect();
        assert!(matches!(kinds[0], EventKind::User { text } if text == "fix the failing test"), "{kinds:?}");
        assert!(kinds.iter().all(|k| !matches!(k, EventKind::User { text } if text.contains("environment_context"))));
        assert!(kinds.iter().any(|k| matches!(k, EventKind::Command { cmd, exit: Some(1), failed: true, output, .. } if cmd == "cargo test" && output == "1 failed")));
        assert!(kinds.iter().any(|k| matches!(k, EventKind::Edit { paths } if paths == &["src/lib.rs"])));
        assert!(matches!(kinds.last(), Some(EventKind::Agent { text, .. }) if text == "Fixed: the test passes now."));
    }
}
