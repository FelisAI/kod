//! The CLI-neutral session log a handoff renders (docs/028 §4). Every CLI's
//! transcript parses into this; the renderer never sees a CLI's own schema, so a
//! new direction (claude → codex, codex → claude, …) never needs a new renderer.

use serde::{Deserialize, Serialize};

use crate::session::CliKind;

/// One session's work, in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionLog {
    pub kind: CliKind,
    pub session_id: String,
    pub cli_version: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    /// HEAD when the session started. codex records it; claude doesn't, so the repo
    /// snapshot looks it up by the session's start time.
    pub start_commit: Option<String>,
    pub started_ms: Option<u64>,
    pub turns: Vec<Turn>,
    pub compactions: u32,
    /// The agent's latest todo list (claude's `TodoWrite`); empty when it kept none.
    pub plan: Vec<PlanItem>,
    /// The transcript this was read from, for "full text at …" pointers.
    pub source_path: Option<String>,
}

/// One user→agent exchange: a prompt and everything the agent did with it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub n: usize,
    pub started_ms: Option<u64>,
    pub ended_ms: Option<u64>,
    /// Why the turn ended badly, e.g. "Your workspace is out of credits. Add credits
    /// to continue. (usage_limit_exceeded)".
    pub error: Option<String>,
    pub events: Vec<Event>,
    /// Items that completed AFTER the turn had ended — results the agent never saw
    /// (a test suite still running when the account ran out, measured).
    pub late: Vec<Event>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub at_ms: u64,
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EventKind {
    User { text: String },
    /// `phase` is codex's label for the message ("commentary", "final_answer").
    Agent { text: String, phase: Option<String> },
    /// The agent asked the user something (codex's `request_user_input`).
    Question { text: String },
    /// `output` is already trimmed to head + tail; the full text stays in the
    /// transcript. `exit` is None when the CLI doesn't report one (claude).
    Command { cmd: String, cwd: Option<String>, exit: Option<i64>, failed: bool, output: String },
    Edit { paths: Vec<String> },
    Read { paths: Vec<String> },
    Image { path: String },
    /// Anything else the agent used: an MCP tool, a web search, a subagent, …
    Tool { name: String, detail: String, failed: bool },
    Compaction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanItem {
    pub text: String,
    pub status: String,
}

impl SessionLog {
    pub fn empty(kind: CliKind) -> Self {
        SessionLog {
            kind,
            session_id: String::new(),
            cli_version: None,
            model: None,
            effort: None,
            cwd: None,
            branch: None,
            start_commit: None,
            started_ms: None,
            turns: Vec::new(),
            compactions: 0,
            plan: Vec::new(),
            source_path: None,
        }
    }

    /// The last turn that did any work — NOT simply the last turn. A "continue"
    /// typed into a dead account is the newest turn and fails at once; anchoring
    /// there would hide where the work actually stopped (docs/028 §1, measured).
    pub fn last_working_turn(&self) -> Option<&Turn> {
        self.turns.iter().rev().find(|t| t.did_work())
    }

    pub fn user_messages(&self) -> usize {
        self.all_events().filter(|e| matches!(e.kind, EventKind::User { .. })).count()
    }

    pub fn commands(&self) -> usize {
        self.all_events().filter(|e| matches!(e.kind, EventKind::Command { .. })).count()
    }

    pub fn failed_commands(&self) -> usize {
        self.all_events()
            .filter(|e| matches!(e.kind, EventKind::Command { failed: true, .. }))
            .count()
    }

    /// Every event, late ones included, in turn order.
    pub fn all_events(&self) -> impl Iterator<Item = &Event> {
        self.turns.iter().flat_map(|t| t.events.iter().chain(t.late.iter()))
    }
}

impl Turn {
    pub fn new(n: usize, started_ms: Option<u64>) -> Self {
        Turn { n, started_ms, ended_ms: None, error: None, events: Vec::new(), late: Vec::new() }
    }

    /// Whether the agent did anything in this turn (said, ran, edited, looked).
    pub fn did_work(&self) -> bool {
        self.events.iter().any(|e| !matches!(e.kind, EventKind::User { .. } | EventKind::Compaction))
    }
}

/// Keep the first `head` and last `tail` lines, then cap at `cap` chars. A tool's
/// output is 74% of a real rollout's bytes; head + tail keeps what a reader checks
/// (the command's banner, its verdict) at a bounded size.
pub(crate) fn head_tail(s: &str, head: usize, tail: usize, cap: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let kept = if lines.len() > head + tail {
        let mut v: Vec<String> = lines[..head].iter().map(|l| l.to_string()).collect();
        v.push(format!("… [{} lines omitted] …", lines.len() - head - tail));
        v.extend(lines[lines.len() - tail..].iter().map(|l| l.to_string()));
        v.join("\n")
    } else {
        lines.join("\n")
    };
    clip(&kept, cap)
}

/// Char-safe cap with a note of what was cut.
pub(crate) fn clip(s: &str, cap: usize) -> String {
    match s.char_indices().nth(cap) {
        None => s.to_string(),
        Some((at, _)) => format!("{} … [+{} chars]", &s[..at], s[at..].chars().count()),
    }
}

/// Collapse whitespace to single spaces, then cap.
pub(crate) fn one_line(s: &str, cap: usize) -> String {
    clip(&s.split_whitespace().collect::<Vec<_>>().join(" "), cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_tail_keeps_both_ends_and_says_what_it_dropped() {
        let s: String = (1..=50).map(|i| format!("line {i}\n")).collect();
        let t = head_tail(&s, 2, 3, 10_000);
        assert!(t.starts_with("line 1\nline 2\n… [45 lines omitted] …\nline 48"), "{t}");
        assert!(t.ends_with("line 50"));
        assert_eq!(head_tail("a\nb", 2, 3, 100), "a\nb");
    }

    #[test]
    fn clip_is_char_safe() {
        assert_eq!(clip("héllo", 2), "hé … [+3 chars]");
        assert_eq!(clip("hi", 2), "hi");
    }

    #[test]
    fn the_last_working_turn_skips_a_failed_retry() {
        let mut log = SessionLog::empty(CliKind::Codex);
        let mut work = Turn::new(1, Some(1));
        work.events.push(Event { at_ms: 1, kind: EventKind::User { text: "build it".into() } });
        work.events.push(Event { at_ms: 2, kind: EventKind::Agent { text: "on it".into(), phase: None } });
        let mut retry = Turn::new(2, Some(9));
        retry.events.push(Event { at_ms: 9, kind: EventKind::User { text: "continue".into() } });
        retry.error = Some("out of credits".into());
        log.turns = vec![work, retry];
        assert_eq!(log.last_working_turn().map(|t| t.n), Some(1));
    }
}
