//! Packet rendering (docs/028 §3, §5): `handoff.md`, which the receiver reads in
//! full; `commands.md`, which it only searches; and its first message.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use jiff::tz::TimeZone;

use super::log::{clip, one_line, Event, EventKind, SessionLog, Turn};
use super::repo::RepoSnapshot;
use super::PacketRequest;

/// A message longer than this is a paste (a log, a diff): keep its two ends.
const MESSAGE_CAP: usize = 32 * 1024;
/// Commands that only look at files: they feed "files it read", not the build loop.
const READ_CMDS: [&str; 13] = ["sed", "cat", "nl", "head", "tail", "rg", "grep", "wc", "less", "ls", "find", "stat", "file"];

pub(crate) struct Ctx<'a> {
    pub log: &'a SessionLog,
    pub repo: &'a RepoSnapshot,
    pub req: &'a PacketRequest,
    pub dir: &'a Path,
    pub tz: TimeZone,
    pub now_ms: u64,
    /// One extra line when the receiver won't load the instructions the source followed.
    pub parity: Option<String>,
}

fn fmt(tz: &TimeZone, ms: u64, f: &str) -> String {
    jiff::Timestamp::from_millisecond(ms as i64)
        .map(|t| t.to_zoned(tz.clone()).strftime(f).to_string())
        .unwrap_or_else(|_| "?".to_string())
}

/// When a turn stopped: its recorded end, else its last event.
fn turn_end(t: &Turn) -> Option<u64> {
    t.ended_ms.or_else(|| t.events.last().map(|e| e.at_ms))
}

impl Ctx<'_> {
    fn when(&self, ms: u64) -> String {
        fmt(&self.tz, ms, "%Y-%m-%d %H:%M")
    }
    fn when_opt(&self, ms: Option<u64>) -> String {
        ms.map(|m| self.when(m)).unwrap_or_else(|| "?".to_string())
    }
    fn clock(&self, ms: u64) -> String {
        fmt(&self.tz, ms, "%H:%M")
    }
    fn short(&self, secs: i64) -> String {
        fmt(&self.tz, secs.max(0) as u64 * 1000, "%m-%d %H:%M")
    }

    /// The session at work, in seconds: (first line, final working turn's start,
    /// its end — or later, if a result landed after the cutoff).
    fn window(&self) -> Option<(i64, i64, i64)> {
        let t = self.log.last_working_turn()?;
        let late = t.late.iter().map(|e| e.at_ms).max();
        let end = turn_end(t).max(late)?;
        let turn_start = t.started_ms.or_else(|| t.events.first().map(|e| e.at_ms))?;
        let start = self.log.started_ms.unwrap_or(turn_start);
        Some(((start / 1000) as i64, (turn_start / 1000) as i64, (end / 1000) as i64))
    }

    /// Whose change was this? Kod runs several agents per repo (docs/028 §1).
    fn tag(&self, secs: i64, dirty: bool) -> &'static str {
        let Some((start, final_start, end)) = self.window() else {
            return "";
        };
        // A file's mtime can trail the last recorded event by a little.
        if secs > end + 120 {
            "  ← AFTER the session stopped: not its work"
        } else if secs >= final_start {
            if dirty { "  ← during the final turn (in progress)" } else { "  ← during the final turn" }
        } else if secs >= start {
            "  ← during the session"
        } else {
            "  ← before the session"
        }
    }

    /// The roots a path can be relative to: Kod's cwd for the session, and the
    /// cwd the CLI itself recorded (the same directory, unless one is a symlink).
    fn roots(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.req.cwd.as_path()).chain(self.log.cwd.as_deref().map(Path::new))
    }

    fn rel(&self, p: &str) -> String {
        self.roots()
            .find_map(|root| Path::new(p).strip_prefix(root).ok())
            .map(|r| r.display().to_string())
            .unwrap_or_else(|| p.to_string())
    }

    fn in_repo(&self, p: &str) -> bool {
        !Path::new(p).is_absolute() || self.roots().any(|root| Path::new(p).starts_with(root))
    }
}

/// Fence `body` with more backticks than it contains in a row, so a tool's
/// output that itself holds ``` can't end the block early.
fn fenced(lang: &str, body: &str) -> String {
    let mut run = 0;
    let mut longest = 0;
    for ch in body.chars() {
        run = if ch == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}{lang}\n{body}\n{fence}")
}

fn quote(text: &str) -> String {
    text.lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n")
}

/// Verbatim, unless it's a paste far past any typed message.
fn message(text: &str) -> String {
    let n = text.chars().count();
    if n <= MESSAGE_CAP {
        return text.to_string();
    }
    let head: String = text.chars().take(MESSAGE_CAP / 2).collect();
    let tail: String = text.chars().skip(n - MESSAGE_CAP / 4).collect();
    format!("{head}\n… [{} chars omitted — the full message is in the transcript] …\n{tail}", n - MESSAGE_CAP / 2 - MESSAGE_CAP / 4)
}

/// Insertion-ordered counter: ties keep first-seen order.
#[derive(Default)]
struct Tally {
    order: Vec<(String, usize)>,
    index: HashMap<String, usize>,
}

impl Tally {
    fn add(&mut self, k: String) {
        match self.index.get(&k) {
            Some(&i) => self.order[i].1 += 1,
            None => {
                self.index.insert(k.clone(), self.order.len());
                self.order.push((k, 1));
            }
        }
    }
    fn top(&self, n: usize, min: usize) -> Vec<(String, usize)> {
        let mut v: Vec<(String, usize)> = self.order.iter().filter(|(_, c)| *c >= min).cloned().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1)); // stable: ties stay in first-seen order
        v.truncate(n);
        v
    }
}

/// Split a shell line into words, honoring quotes (lenient: an open quote just runs on).
fn words(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut quote, mut any) = (Vec::new(), String::new(), None::<char>, false);
    for ch in s.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => cur.push(ch),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                any = true;
            }
            None if ch.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            None => cur.push(ch),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `name.ext`, path characters only — not a sed range (`1,75p`), a glob or a flag.
fn looks_like_file(t: &str) -> bool {
    let path_chars = t.chars().all(|c| c.is_alphanumeric() || "./_@+-".contains(c));
    let ext = t.rsplit_once('.');
    path_chars
        && ext.is_some_and(|(stem, e)| !stem.is_empty() && (1..=6).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Split a command line at unquoted `;`, `|`, `&` and newlines — `rg 'a|b' f`
/// is one command, not two.
fn segments(cmd: &str) -> Vec<String> {
    let (mut out, mut cur, mut quote) = (Vec::new(), String::new(), None::<char>);
    for ch in cmd.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => {}
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if matches!(ch, ';' | '|' | '&' | '\n') => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            None => {}
        }
        cur.push(ch);
    }
    out.push(cur);
    out
}

/// Files a read-only shell command looked at (best effort; scratch under /tmp skipped).
fn files_read(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    for seg in segments(cmd) {
        let w = words(&seg);
        let Some(first) = w.first() else { continue };
        if !READ_CMDS.contains(&first.as_str()) {
            continue;
        }
        out.extend(w[1..].iter().filter(|t| !t.starts_with('-') && !t.starts_with("/tmp/") && looks_like_file(t)).cloned());
    }
    out
}

/// A command's first line as a "build/test loop" entry, or `None` for a generic
/// read, a heredoc'd script or a git inspection.
fn workflow(cmd: &str) -> Option<String> {
    let mut first = cmd.trim().lines().next()?.trim();
    if let Some(rest) = first.strip_prefix("cd ") {
        if let Some((_, after)) = rest.split_once("&& ") {
            first = after.trim();
        }
    }
    if first.contains("<<") {
        return None;
    }
    let first = first.split(['>', '|']).next()?.trim();
    let toks: Vec<&str> = first.split_whitespace().collect();
    let head = *toks.first()?;
    if READ_CMDS.contains(&head) || (head == "git" && toks.get(1).is_some_and(|s| matches!(*s, "status" | "diff" | "log" | "show"))) {
        return None;
    }
    Some(one_line(first, 110))
}

fn error_suffix(t: &Turn) -> String {
    t.error.as_deref().map(|e| format!(" with error: {e}")).unwrap_or_default()
}

pub(crate) fn handoff_md(c: &Ctx) -> String {
    let log = c.log;
    let kind = log.kind.label();
    let cwd = c.req.cwd.display().to_string();
    let mut s = String::new();
    let _ = writeln!(s, "# Handoff — {kind} session in {cwd}\n");

    // ---- header ----
    let mut src = format!("- Source: {} ({kind}", c.req.source_label);
    if let Some(v) = &log.cli_version {
        let _ = write!(src, " {v}");
    }
    src.push(')');
    if let Some(m) = &log.model {
        let _ = write!(src, ", model {m}");
        if let Some(e) = &log.effort {
            let _ = write!(src, " (effort {e})");
        }
    }
    if !log.session_id.is_empty() {
        let _ = write!(src, ", session `{}`", log.session_id);
    }
    let _ = writeln!(s, "{src}");
    match &log.source_path {
        Some(p) => {
            let _ = writeln!(s, "- Transcript: `{p}`");
        }
        // docs/028 §8: no transcript is no reason to refuse — the repo still is.
        None => {
            let _ = writeln!(s, "- Transcript: none found, so this packet has the repo state only");
        }
    }
    let mut wd = format!("- Working dir: `{cwd}`");
    if let Some(b) = &log.branch {
        let _ = write!(wd, " · branch `{b}`");
    }
    if let Some(sc) = c.repo.start_commit.as_deref().or(log.start_commit.as_deref()) {
        let _ = write!(wd, " · started at commit `{}`", &sc[..sc.len().min(9)]);
    }
    if let Some(ms) = log.started_ms {
        let _ = write!(wd, " on {}", c.when(ms));
    }
    let _ = writeln!(s, "{wd}");
    let last = log.last_working_turn();
    match last {
        Some(t) => match &t.error {
            Some(e) => {
                let _ = writeln!(s, "- Stopped: {} — {e}", c.when_opt(turn_end(t)));
            }
            None => {
                let _ = writeln!(s, "- Last active: {}; then handed off", c.when_opt(turn_end(t)));
            }
        },
        None => {
            let _ = writeln!(s, "- No turn did any work yet.");
        }
    }
    let later: Vec<&Turn> = log.turns.iter().filter(|t| last.is_some_and(|l| t.n > l.n)).collect();
    if let Some(newest) = later.last() {
        let failed = later.iter().filter(|t| t.error.is_some()).count();
        let _ = writeln!(
            s,
            "- Since then: {} more prompt{} (last {}), {} failed at once{}",
            later.len(),
            if later.len() == 1 { "" } else { "s" },
            c.when_opt(newest.started_ms),
            if failed == later.len() { "each" } else { "some" },
            newest.error.as_deref().map(|e| format!(" — {e}")).unwrap_or_default()
        );
    }
    let mut edited = Tally::default();
    let mut scratch = Tally::default();
    for e in log.all_events() {
        if let EventKind::Edit { paths } = &e.kind {
            for p in paths {
                if c.in_repo(p) {
                    edited.add(c.rel(p));
                } else {
                    scratch.add(p.clone());
                }
            }
        }
    }
    let _ = writeln!(
        s,
        "- Size: {} turns · {} user messages · {} commands ({} failed) · {} files edited · {} context compactions",
        log.turns.len(),
        log.user_messages(),
        log.commands(),
        log.failed_commands(),
        edited.order.len(),
        log.compactions
    );
    let _ = writeln!(s, "- Packet built {}", c.when(c.now_ms));
    s.push('\n');

    // ---- where it stopped ----
    let _ = writeln!(s, "## Where it stopped\n");
    if last.is_none() {
        let _ = writeln!(s, "Nowhere yet: no turn got as far as a reply or a tool call.\n");
    }
    if let Some(t) = last {
        let _ = writeln!(
            s,
            "The last turn that did work was turn {} (started {}, ended {}{}).\n",
            t.n,
            c.when_opt(t.started_ms),
            c.when_opt(turn_end(t)),
            error_suffix(t)
        );
        if let Some(text) = t.events.iter().find_map(|e| match &e.kind {
            EventKind::User { text } => Some(text),
            _ => None,
        }) {
            let _ = writeln!(s, "It started from this message:\n\n{}\n", quote(&clip(text, 2000)));
        }
        if let Some(text) = t.events.iter().rev().find_map(|e| match &e.kind {
            EventKind::Agent { text, .. } => Some(text),
            _ => None,
        }) {
            let _ = writeln!(s, "The agent's last message:\n\n{}\n", quote(&message(text)));
        }
        if !log.plan.is_empty() {
            let _ = writeln!(s, "Its latest plan:\n");
            for p in &log.plan {
                let _ = writeln!(s, "- [{}] {}", p.status, one_line(&p.text, 200));
            }
            s.push('\n');
        }
        let cmds: Vec<&Event> = t.events.iter().filter(|e| matches!(e.kind, EventKind::Command { .. })).collect();
        if !cmds.is_empty() {
            let _ = writeln!(s, "Its last {} commands (of {} in that turn):\n", cmds.len().min(10), cmds.len());
            for e in &cmds[cmds.len().saturating_sub(10)..] {
                if let EventKind::Command { cmd, exit, failed, .. } = &e.kind {
                    let status = match exit {
                        Some(x) => format!("exit {x}"),
                        None if *failed => "failed".to_string(),
                        None => "ok".to_string(),
                    };
                    let _ = writeln!(s, "- {} {status} · `{}`", c.clock(e.at_ms), one_line(cmd, 160));
                }
            }
            s.push('\n');
        }
        let late: Vec<&Event> = log
            .turns
            .iter()
            .filter(|x| x.n >= t.n)
            .flat_map(|x| x.late.iter())
            .filter(|e| matches!(e.kind, EventKind::Command { .. }))
            .collect();
        if !late.is_empty() {
            let _ = writeln!(s, "**Finished after the agent stopped — it never saw these results:**\n");
            for e in late {
                let EventKind::Command { cmd, exit, output, .. } = &e.kind else { continue };
                let status = exit.map(|x| format!("exit {x}")).unwrap_or_else(|| "done".to_string());
                let _ = writeln!(s, "- {} {status} · `{}`", c.when(e.at_ms), one_line(cmd, 200));
                for log_path in redirect_targets(cmd) {
                    if !Path::new(&log_path).exists() {
                        let _ = writeln!(s, "  (its log `{log_path}` no longer exists — rerun the command to see the result)");
                    }
                }
                let tail: Vec<&str> = output.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
                if !tail.is_empty() {
                    let _ = writeln!(s, "{}", fenced("", &tail.join("\n")).lines().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n"));
                }
            }
            s.push('\n');
        }
        let questions: Vec<&String> = t
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Question { text } => Some(text),
                _ => None,
            })
            .collect();
        if !questions.is_empty() {
            let _ = writeln!(s, "Questions it asked in that turn (check whether they were answered):\n");
            for q in questions {
                let _ = writeln!(s, "- {q}");
            }
            s.push('\n');
        }
        for t in &later {
            let prompt = t
                .events
                .iter()
                .filter_map(|e| match &e.kind {
                    EventKind::User { text } => Some(one_line(text, 120)),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" / ");
            let outcome = t.error.as_deref().map(|e| format!("failed at once: {e}")).unwrap_or_else(|| "got no reply".to_string());
            let _ = writeln!(s, "After that, turn {} ({}) was `{prompt}` and {outcome}\n", t.n, c.when_opt(t.started_ms));
        }
    }

    // ---- repo state ----
    let _ = writeln!(s, "## Repo state at handoff ({})\n", c.when(c.now_ms));
    if !c.repo.is_repo {
        let _ = writeln!(s, "`{cwd}` is not a git repository, so nothing here can be attributed by commit. Check the files the agent edited (below) directly.\n");
    } else {
        if let Some((start, _, end)) = c.window() {
            let _ = writeln!(
                s,
                "The session was active {} → {}. Each change below is marked by when it happened.\n",
                c.when(start as u64 * 1000),
                c.when(end as u64 * 1000)
            );
        }
        match &c.repo.start_commit {
            Some(sc) => {
                let _ = writeln!(s, "Commits since the session's start commit `{}`:\n", &sc[..sc.len().min(9)]);
            }
            None => {
                let _ = writeln!(s, "Commits since the session started:\n");
            }
        }
        let rows: Vec<String> = c
            .repo
            .commits
            .iter()
            .map(|r| format!("{} {}  {}{}", r.short, c.short(r.at_secs), r.subject, c.tag(r.at_secs, false)))
            .collect();
        let mut body = if rows.is_empty() { "(none)".to_string() } else { rows.join("\n") };
        if c.repo.more_commits {
            body.push_str("\n… older commits omitted");
        }
        let _ = writeln!(s, "{}\n", fenced("", &body));
        let _ = writeln!(s, "Uncommitted changes:\n");
        let rows: Vec<String> = c
            .repo
            .dirty
            .iter()
            .map(|r| match r.mtime_secs {
                Some(m) => format!("{} {}  (modified {}){}", r.status, r.path, c.short(m), c.tag(m, true)),
                None => format!("{} {}  (deleted)", r.status, r.path),
            })
            .collect();
        let body = if rows.is_empty() { "(clean)".to_string() } else { rows.join("\n") };
        let _ = writeln!(s, "{}\n", fenced("", &body));
        if !c.repo.diff_stat.is_empty() {
            let stat: Vec<&str> = c.repo.diff_stat.lines().collect();
            let mut body = format!("$ git diff --stat HEAD\n{}", stat.iter().take(30).copied().collect::<Vec<_>>().join("\n"));
            if stat.len() > 30 {
                let _ = write!(body, "\n… {} more lines", stat.len() - 30);
            }
            let _ = writeln!(s, "{}\n", fenced("", &body));
        }
        if !c.repo.ignored_edits.is_empty() {
            let rows: Vec<String> = c
                .repo
                .ignored_edits
                .iter()
                .map(|r| match r.mtime_secs {
                    Some(m) => format!("{}  (modified {}){}", r.path, c.short(m), c.tag(m, true)),
                    None => r.path.clone(),
                })
                .collect();
            let _ = writeln!(s, "Edited in the final turn but ignored by git, so not listed above:\n\n{}\n", fenced("", &rows.join("\n")));
        }
    }

    // ---- where to look ----
    if !edited.order.is_empty() {
        let _ = writeln!(s, "## Files the agent edited\n");
        let list: Vec<String> = edited
            .top(40, 1)
            .into_iter()
            .map(|(p, n)| if n > 1 { format!("`{p}`×{n}") } else { format!("`{p}`") })
            .collect();
        let _ = write!(s, "{}", list.join(", "));
        if edited.order.len() > 40 {
            let _ = write!(s, ", and {} more", edited.order.len() - 40);
        }
        if !scratch.order.is_empty() {
            let _ = write!(s, " (plus {} scratch files outside the repo)", scratch.order.len());
        }
        s.push_str("\n\n");
    }
    let mut read = Tally::default();
    let mut loop_cmds = Tally::default();
    for e in log.all_events() {
        match &e.kind {
            EventKind::Read { paths } => paths.iter().filter(|p| c.in_repo(p)).for_each(|p| read.add(c.rel(p))),
            EventKind::Command { cmd, .. } => {
                let reads = files_read(cmd);
                if reads.is_empty() {
                    if let Some(w) = workflow(cmd) {
                        loop_cmds.add(w);
                    }
                } else {
                    reads.into_iter().for_each(|p| read.add(c.rel(&p)));
                }
            }
            _ => {}
        }
    }
    let top_read = read.top(25, 1);
    if !top_read.is_empty() {
        let _ = writeln!(s, "## Files it read most\n");
        let list: Vec<String> = top_read.into_iter().map(|(p, n)| format!("`{p}`×{n}")).collect();
        let _ = writeln!(s, "{}\n", list.join(", "));
    }
    let top_loop = loop_cmds.top(15, 3);
    if !top_loop.is_empty() {
        let _ = writeln!(s, "## Commands it ran repeatedly (this project's build/test loop)\n");
        for (cmd, n) in top_loop {
            let _ = writeln!(s, "- ×{n} `{cmd}`");
        }
        s.push('\n');
    }
    if let Some(prev) = &c.req.earlier_packet {
        let _ = writeln!(
            s,
            "## Before this session\n\nThis session itself began from an earlier handoff: `{}`. Read it only if you need history from before this session.\n",
            prev.join("handoff.md").display()
        );
    }

    // ---- conversation ----
    let _ = writeln!(s, "## Conversation\n");
    let _ = writeln!(
        s,
        "Every message between the user and the agent, word for word, in order. Tool activity between messages is shrunk to one line; the full record is in `commands.md`.\n"
    );
    for t in &log.turns {
        let status = t.error.as_deref().map(|e| format!("ERROR: {e}")).unwrap_or_else(|| "ok".to_string());
        let _ = writeln!(s, "### Turn {} · {} → {} · {status}\n", t.n, c.when_opt(t.started_ms), c.when_opt(turn_end(t)));
        let mut acts = Activity::default();
        for e in &t.events {
            match &e.kind {
                EventKind::User { text } => {
                    acts.flush(&mut s);
                    let _ = writeln!(s, "**User:** {}\n", message(text));
                }
                EventKind::Agent { text, phase } => {
                    acts.flush(&mut s);
                    let label = phase.as_deref().map(|p| format!(" ({p})")).unwrap_or_default();
                    let _ = writeln!(s, "**Agent{label}:** {}\n", message(text));
                }
                EventKind::Question { text } => {
                    acts.flush(&mut s);
                    let _ = writeln!(s, "**Agent asked:** {text}\n");
                }
                EventKind::Command { failed, .. } => {
                    acts.commands += 1;
                    acts.failed += *failed as usize;
                }
                EventKind::Edit { paths } => {
                    for p in paths {
                        let p = c.rel(p);
                        if !acts.edited.contains(&p) {
                            acts.edited.push(p);
                        }
                    }
                }
                EventKind::Read { .. } => acts.reads += 1,
                EventKind::Image { .. } => acts.images += 1,
                EventKind::Tool { .. } => acts.tools += 1,
                EventKind::Compaction => acts.compacted = true,
            }
        }
        acts.flush(&mut s);
    }
    s
}

/// Tool activity since the last message, shrunk to one italic line.
#[derive(Default)]
struct Activity {
    commands: usize,
    failed: usize,
    edited: Vec<String>,
    reads: usize,
    images: usize,
    tools: usize,
    compacted: bool,
}

impl Activity {
    fn flush(&mut self, s: &mut String) {
        let mut bits: Vec<String> = Vec::new();
        if self.commands > 0 {
            let failed = if self.failed > 0 { format!(" ({} failed)", self.failed) } else { String::new() };
            bits.push(format!("{} command{}{failed}", self.commands, if self.commands == 1 { "" } else { "s" }));
        }
        if !self.edited.is_empty() {
            let more = if self.edited.len() > 8 { "…" } else { "" };
            bits.push(format!("edited {}{more}", self.edited.iter().take(8).cloned().collect::<Vec<_>>().join(", ")));
        }
        if self.reads > 0 {
            bits.push(format!("read {} file{}", self.reads, if self.reads == 1 { "" } else { "s" }));
        }
        if self.images > 0 {
            bits.push(format!("viewed {} image{}", self.images, if self.images == 1 { "" } else { "s" }));
        }
        if self.tools > 0 {
            bits.push(format!("{} other tool call{}", self.tools, if self.tools == 1 { "" } else { "s" }));
        }
        if self.compacted {
            bits.push("context compacted".to_string());
        }
        if !bits.is_empty() {
            let _ = writeln!(s, "_… {}_\n", bits.join(" · "));
        }
        *self = Activity::default();
    }
}

/// `… > /tmp/x.log 2>&1` → `/tmp/x.log`: where a command's result went, if it
/// went to a file that can vanish.
fn redirect_targets(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = cmd;
    while let Some(i) = rest.find('>') {
        let after = rest[i + 1..].trim_start_matches('>').trim_start();
        let target: String = after.chars().take_while(|c| !c.is_whitespace() && *c != ';' && *c != '&' && *c != '|').collect();
        if target.starts_with("/tmp/") || target.starts_with("/private/tmp/") {
            out.push(target);
        }
        rest = &rest[i + 1..];
    }
    out
}

pub(crate) fn commands_md(c: &Ctx) -> String {
    let log = c.log;
    let mut s = String::new();
    let _ = writeln!(s, "# Every tool action — {} session {}\n", log.kind.label(), log.session_id);
    let _ = writeln!(s, "Search this file; don't read it top to bottom. Each output keeps its first 12 and last 20 lines.\n");
    for t in &log.turns {
        let _ = writeln!(s, "## Turn {} · {}\n", t.n, c.when_opt(t.started_ms));
        for (events, suffix) in [(&t.events, ""), (&t.late, " (after the agent stopped)")] {
            for e in events.iter() {
                let at = c.when(e.at_ms);
                match &e.kind {
                    EventKind::Command { cmd, cwd, exit, failed, output } => {
                        let status = match exit {
                            Some(x) => format!("exit {x}"),
                            None if *failed => "failed".to_string(),
                            None => "ok".to_string(),
                        };
                        let place = cwd.as_deref().filter(|d| Path::new(d) != c.req.cwd).map(|d| format!(" · in {d}")).unwrap_or_default();
                        let _ = writeln!(s, "### {at} · {status}{suffix}{place}\n\n{}\n", fenced("sh", &clip(cmd, 2000)));
                        if !output.trim().is_empty() {
                            let _ = writeln!(s, "{}\n", fenced("", output));
                        }
                    }
                    EventKind::Edit { paths } => {
                        let list: Vec<String> = paths.iter().map(|p| c.rel(p)).collect();
                        let _ = writeln!(s, "### {at} · edited{suffix}: {}\n", list.join(", "));
                    }
                    EventKind::Read { paths } => {
                        let list: Vec<String> = paths.iter().map(|p| c.rel(p)).collect();
                        let _ = writeln!(s, "### {at} · read{suffix}: {}\n", list.join(", "));
                    }
                    EventKind::Image { path } => {
                        let _ = writeln!(s, "### {at} · viewed image{suffix}: {path}\n");
                    }
                    EventKind::Tool { name, detail, failed } => {
                        let f = if *failed { " (failed)" } else { "" };
                        let _ = writeln!(s, "### {at} · {name}{f}{suffix}: {detail}\n");
                    }
                    EventKind::Question { text } => {
                        let _ = writeln!(s, "### {at} · asked{suffix}: {text}\n");
                    }
                    EventKind::User { .. } | EventKind::Agent { .. } | EventKind::Compaction => {}
                }
            }
        }
    }
    s
}

pub(crate) fn prompt(c: &Ctx) -> String {
    let log = c.log;
    let model = log.model.as_deref().map(|m| format!(" ({m})")).unwrap_or_default();
    let stop = match log.last_working_turn() {
        Some(t) => match &t.error {
            Some(e) => format!(" and stopped at {}: {e}", c.when_opt(turn_end(t))),
            None => format!(" until {}, when I handed it off to you", c.when_opt(turn_end(t))),
        },
        None => String::new(),
    };
    let handoff = c.dir.join("handoff.md");
    let commands = c.dir.join("commands.md");
    let parity = c.parity.as_deref().map(|p| format!("\n{p}\n")).unwrap_or_default();
    let ask = if c.req.confirm_first {
        "Before changing anything, reply with: (a) the goal in one line, (b) what's done, (c) what was in progress when it stopped, (d) your next step. Then wait for my go-ahead."
    } else {
        "Start by stating in a few lines: (a) the goal, (b) what's done, (c) what was in progress when it stopped, (d) your next step. Then carry on with the work."
    };
    format!(
        "You're taking over a coding session from another agent. A {kind} session{model} on the account \"{label}\" was working in this repo{stop}. Continue the same work.

Read this first, in full: {handoff}
It has every message the previous agent and I exchanged, word for word, plus where it stopped and the repo state at handoff time.
For anything else, search {commands} rather than reading it top to bottom. It has every command the agent ran, with exit codes and trimmed output.
{parity}
Rules:
1. The handoff is a lead, not the truth. Check its claims against the repo before relying on them.
2. The agent may have been cut off mid-turn. Anything listed as finishing after it stopped is a result it never saw, so check those before redoing anything.
3. Don't redo or revert work that's already finished.
4. The previous session is closed, so you're the only agent on this task.

{ask}
",
        kind = log.kind.label(),
        label = c.req.source_label,
        handoff = handoff.display(),
        commands = commands.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_read_takes_paths_not_ranges_or_flags() {
        assert_eq!(files_read("sed -n '1,240p' pedon/ios/ar_app.py"), ["pedon/ios/ar_app.py"]);
        assert_eq!(files_read("rg -n 'BZ1|BY1' pedon/QUEUE.md pedon/BACKLOG.md | tail -55"), ["pedon/QUEUE.md", "pedon/BACKLOG.md"]);
        assert!(files_read("cat /tmp/x.log").is_empty());
        assert!(files_read("python3 tools/selftest.py").is_empty());
    }

    #[test]
    fn workflow_keeps_the_build_loop_and_drops_reads_and_scripts() {
        assert_eq!(workflow("python3 tools/selftest.py > /tmp/a.log 2>&1").as_deref(), Some("python3 tools/selftest.py"));
        assert_eq!(workflow("cd pedon && swift test | tail").as_deref(), Some("swift test"));
        assert_eq!(workflow("python3 - <<'PY'\nprint(1)\nPY"), None);
        assert_eq!(workflow("git status --short"), None);
        assert_eq!(workflow("sed -n '1,9p' a.rs"), None);
    }

    #[test]
    fn a_fence_outlasts_the_backticks_inside() {
        assert_eq!(fenced("", "plain"), "```\nplain\n```");
        assert_eq!(fenced("sh", "echo ```"), "````sh\necho ```\n````");
    }

    #[test]
    fn redirects_to_tmp_are_found() {
        assert_eq!(redirect_targets("python3 tools/selftest.py > /tmp/after.log 2>&1"), ["/tmp/after.log"]);
        assert!(redirect_targets("make >build.log").is_empty());
    }

    #[test]
    fn a_huge_paste_keeps_its_ends() {
        let paste = format!("HEAD{}TAIL", "x".repeat(MESSAGE_CAP * 2));
        let m = message(&paste);
        assert!(m.starts_with("HEAD") && m.ends_with("TAIL") && m.contains("chars omitted"));
        assert!(m.chars().count() < MESSAGE_CAP);
        assert_eq!(message("short"), "short");
    }
}
