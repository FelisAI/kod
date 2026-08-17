//! E1 — LLM structure extraction (docs/016). Reads a project's real structure
//! (dirs, manifests, README/docs headings) into a compact digest, asks a
//! one-shot headless `claude -p` to propose the product's AREAS as a tree with
//! anchor globs, and returns `DiffOp`s. The result is a PROPOSAL — surfaced as
//! an accept-diff the user confirms/edits — never authoritative, never `done`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{OnceLock, RwLock};

// ---- decomposed child modules (see src/extract/) ----
#[allow(dead_code)]
pub mod cartographer;
/// Provider-backed memory extraction. The store crate owns the prompt schema
/// and parser; this module owns the actual Claude/Codex CLI invocation because
/// prompt provider configuration already lives here.
#[allow(dead_code)]
pub mod memory_agent;
mod breakdown;
mod living_map;
mod seed;
mod standup;

// main.rs drives these through `extract::X` — re-export so those paths resolve.
pub use breakdown::propose_breakdown;
pub use living_map::{files_touched, propose_map_updates, serialize_tree_for_llm};
pub use seed::extract_tree;
pub use standup::{standup_summarize, summarize_session};

/// Build a compact, deterministic digest of the project's structure to ground
/// the extraction (so it clusters REAL modules, not invents areas).
fn project_digest(root: &Path) -> String {
    let mut out = String::new();
    out.push_str(&format!("PROJECT ROOT: {}\n\n", root.display()));

    // top-level + one level of dirs (skip noise)
    out.push_str("DIRECTORY TREE (2 levels):\n");
    let skip = |n: &str| {
        matches!(
            n,
            ".git" | "target" | "node_modules" | ".venv" | "dist" | "build" | ".DS_Store"
        )
    };
    if let Ok(top) = std::fs::read_dir(root) {
        let mut entries: Vec<_> = top.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if skip(&name) {
                continue;
            }
            let is_dir = e.path().is_dir();
            out.push_str(&format!("  {}{}\n", name, if is_dir { "/" } else { "" }));
            if is_dir {
                if let Ok(sub) = std::fs::read_dir(e.path()) {
                    let mut subs: Vec<_> = sub
                        .flatten()
                        .map(|s| s.file_name().to_string_lossy().into_owned())
                        .filter(|n| !skip(n))
                        .collect();
                    subs.sort();
                    for s in subs.iter().take(24) {
                        out.push_str(&format!("    {s}\n"));
                    }
                }
            }
        }
    }

    // manifests + README/docs headings (the curated area names)
    for (label, rel) in [
        ("README", "README.md"),
        ("Cargo", "Cargo.toml"),
        ("package.json", "package.json"),
    ] {
        if let Ok(txt) = std::fs::read_to_string(root.join(rel)) {
            out.push_str(&format!("\n--- {label} (head) ---\n"));
            out.push_str(&txt.chars().take(1200).collect::<String>());
        }
    }
    // headings from docs/*.md (orchestrator-style roadmaps)
    let docs = root.join("docs");
    if docs.is_dir() {
        out.push_str("\n--- docs/ headings ---\n");
        if let Ok(files) = std::fs::read_dir(&docs) {
            let mut fs: Vec<_> = files
                .flatten()
                .map(|f| f.path())
                .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("md"))
                .collect();
            fs.sort();
            for f in fs.iter().take(20) {
                if let Ok(t) = std::fs::read_to_string(f) {
                    for line in t
                        .lines()
                        .filter(|l| l.starts_with("# ") || l.starts_with("## "))
                        .take(3)
                    {
                        out.push_str(&format!(
                            "  {}: {line}\n",
                            f.file_name().unwrap().to_string_lossy()
                        ));
                    }
                }
            }
        }
    }
    out.chars().take(8000).collect()
}

/// Default models for the app's isolated background `claude -p` calls — the
/// plumbing lane (summaries / map-extract) and the structural lane (the agentic
/// cartographer). EMPTY by default: the build passes NO `--model`, so each user's
/// own account/CLI default model is used and nothing assumes a model id a given
/// account can reach. Override per-lane via the env vars
/// `ORCH_PROMPT_PLUMBING_MODEL` / `ORCH_PROMPT_STRUCTURAL_MODEL`, or the stored
/// `prompt_plumbing_model` / `prompt_structural_model` settings (a model picker in
/// Settings lands with the settings-tab work).
const PLUMBING_MODEL: &str = "";
const CLAUDE_STRUCTURAL_MODEL: &str = "";

/// Which CLI should service the app's isolated background prompts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptProvider {
    Claude,
    Codex,
}

impl PromptProvider {
    pub const ALL: [PromptProvider; 2] = [PromptProvider::Claude, PromptProvider::Codex];

    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "codex" | "codex-api" | "api" | "openai" | "codex-oauth" | "oauth" | "chatgpt"
            | "codex_chatgpt" => Self::Codex,
            _ => Self::Claude,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
        }
    }

    pub fn note(self) -> &'static str {
        match self {
            Self::Claude => "uses claude -p with isolated hooks/MCP disabled",
            Self::Codex => "uses codex exec with the CLI's current auth",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptConfig {
    pub provider: PromptProvider,
    pub plumbing_model: String,
    pub structural_model: String,
}

impl PromptConfig {
    pub fn from_settings(
        provider: Option<&str>,
        plumbing_model: Option<&str>,
        structural_model: Option<&str>,
    ) -> Self {
        let provider = std::env::var("ORCH_PROMPT_PROVIDER")
            .ok()
            .as_deref()
            .map(PromptProvider::parse)
            .unwrap_or_else(|| {
                provider
                    .map(PromptProvider::parse)
                    .unwrap_or(PromptProvider::Claude)
            });
        let default_plumbing = match provider {
            PromptProvider::Claude => PLUMBING_MODEL,
            PromptProvider::Codex => "",
        };
        let default_structural = match provider {
            PromptProvider::Claude => CLAUDE_STRUCTURAL_MODEL,
            PromptProvider::Codex => "",
        };
        Self {
            provider,
            plumbing_model: std::env::var("ORCH_PROMPT_PLUMBING_MODEL")
                .ok()
                .or_else(|| plumbing_model.map(str::to_string))
                .unwrap_or_else(|| default_plumbing.to_string()),
            structural_model: std::env::var("ORCH_PROMPT_STRUCTURAL_MODEL")
                .ok()
                .or_else(|| structural_model.map(str::to_string))
                .unwrap_or_else(|| default_structural.to_string()),
        }
    }
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self::from_settings(None, None, None)
    }
}

fn prompt_config_cell() -> &'static RwLock<PromptConfig> {
    static CELL: OnceLock<RwLock<PromptConfig>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(PromptConfig::default()))
}

pub fn set_prompt_config(config: PromptConfig) {
    if let Ok(mut current) = prompt_config_cell().write() {
        *current = config;
    }
}

fn active_prompt_config() -> PromptConfig {
    prompt_config_cell()
        .read()
        .map(|c| c.clone())
        .unwrap_or_default()
}

fn run_claude_p(prompt_text: &str, cwd: &Path, timeout_secs: u64) -> Result<String, String> {
    let config = active_prompt_config();
    match config.provider {
        PromptProvider::Claude => {
            run_claude_p_impl(prompt_text, cwd, timeout_secs, &config.plumbing_model)
        }
        PromptProvider::Codex => {
            run_codex_exec(prompt_text, cwd, timeout_secs, &config.plumbing_model, None)
        }
    }
}

fn run_claude_p_impl(
    prompt_text: &str,
    cwd: &Path,
    timeout_secs: u64,
    model: &str,
) -> Result<String, String> {
    let mut args = vec![
        "-p".to_string(),
        prompt_text.to_string(),
        "--no-session-persistence".to_string(),
        "--output-format".to_string(),
        "json".to_string(),
    ];
    if !model.trim().is_empty() {
        args.extend(["--model".to_string(), model.to_string()]);
    }
    args.extend([
        "--strict-mcp-config".to_string(),
        "--settings".to_string(),
        "{\"disableAllHooks\":true}".to_string(),
    ]);
    let mut child = spawn_grouped(
        Command::new("claude")
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .map_err(|e| format!("spawn: couldn't run claude: {e}"))?;
    wait_output(&mut child, "claude -p", timeout_secs)
}

#[allow(clippy::too_many_arguments)]
fn run_agent_prompt(
    prompt_text: &str,
    cwd: &Path,
    claude_model: &str,
    allowed_tools: &[&str],
    max_turns: u32,
    timeout_secs: u64,
    transcript_out: &mut String,
) -> Result<String, String> {
    let config = active_prompt_config();
    match config.provider {
        PromptProvider::Claude => {
            let model = if config.structural_model.trim().is_empty() {
                claude_model
            } else {
                &config.structural_model
            };
            run_claude_agent_impl(
                prompt_text,
                cwd,
                model,
                allowed_tools,
                max_turns,
                timeout_secs,
                transcript_out,
            )
        }
        PromptProvider::Codex => run_codex_exec(
            prompt_text,
            cwd,
            timeout_secs,
            &config.structural_model,
            Some(transcript_out),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_claude_agent_impl(
    prompt_text: &str,
    cwd: &Path,
    model: &str,
    allowed_tools: &[&str],
    max_turns: u32,
    timeout_secs: u64,
    transcript_out: &mut String,
) -> Result<String, String> {
    let tools = allowed_tools.join(",");
    let turns = max_turns.to_string();
    // OSS: omit --model when empty (use the account's default), mirroring
    // run_claude_p_impl / codex_exec_args. With the empty structural default,
    // passing `--model ""` would make the claude CLI reject the call (API 400).
    let mut args: Vec<&str> = vec![
        "-p",
        prompt_text,
        "--no-session-persistence",
        "--output-format",
        "json",
    ];
    if !model.trim().is_empty() {
        args.push("--model");
        args.push(model);
    }
    args.extend([
        "--allowedTools",
        tools.as_str(),
        "--max-turns",
        turns.as_str(),
        "--strict-mcp-config",
        "--settings",
        "{\"disableAllHooks\":true}",
    ]);
    let mut child = spawn_grouped(
        Command::new("claude")
            .args(&args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .map_err(|e| format!("spawn: couldn't run claude agent: {e}"))?;
    let out = wait_output(&mut child, "claude agent", timeout_secs)?;
    transcript_out.clear();
    transcript_out.push_str(&out);
    Ok(out)
}

fn run_codex_exec(
    prompt_text: &str,
    cwd: &Path,
    timeout_secs: u64,
    model: &str,
    mut transcript_out: Option<&mut String>,
) -> Result<String, String> {
    let output_path = temp_prompt_path("codex-last");
    let args = codex_exec_args(cwd, &output_path, model);
    let mut child = spawn_grouped(
        Command::new("codex")
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .map_err(|e| format!("spawn: couldn't run codex exec: {e}"))?;

    // Feed the prompt on a thread and drop stdin (EOF) when done, so a large
    // prompt can't block the write while codex is blocked writing stdout that
    // wait_output hasn't started draining yet (the stdin/stdout counterpart of
    // the pipe-buffer deadlock). A write error just yields a codex failure,
    // which wait_output surfaces.
    if let Some(mut stdin) = child.stdin.take() {
        let prompt = prompt_text.to_string();
        std::thread::spawn(move || {
            let _ = stdin.write_all(prompt.as_bytes());
        });
    }

    let stdout = wait_output(&mut child, "codex exec", timeout_secs);
    let last = std::fs::read_to_string(&output_path).ok();
    let _ = std::fs::remove_file(&output_path);
    // On failure, `--output-last-message` may still hold what the model DID
    // say before codex bailed. The old `stdout?` dropped it on the floor along
    // with the file we just deleted — carry it into the error instead.
    let stdout = match stdout {
        Ok(s) => s,
        Err(e) => {
            return Err(
                match last.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
                    Some(l) => format!("{e} | last-message: {}", tail(l, 200)),
                    None => e,
                },
            )
        }
    };
    if let Some(t) = transcript_out.as_mut() {
        t.clear();
        t.push_str(&stdout);
        if let Some(last) = &last {
            if !last.trim().is_empty() && !stdout.contains(last.trim()) {
                t.push_str("\n\nLAST MESSAGE:\n");
                t.push_str(last);
            }
        }
    }
    match last.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        Some(last) => Ok(last),
        None => Ok(stdout),
    }
}

fn codex_exec_args(cwd: &Path, output_path: &Path, model: &str) -> Vec<String> {
    let mut args = vec![
        "--ask-for-approval".to_string(),
        "never".to_string(),
        "exec".to_string(),
        "-c".to_string(),
        "check_for_update_on_startup=false".to_string(),
        "--skip-git-repo-check".to_string(),
        "--ephemeral".to_string(),
        "--ignore-user-config".to_string(),
        "--ignore-rules".to_string(),
        "--sandbox".to_string(),
        "read-only".to_string(),
        "--color".to_string(),
        "never".to_string(),
        "-C".to_string(),
        cwd.to_string_lossy().into_owned(),
        "--output-last-message".to_string(),
        output_path.to_string_lossy().into_owned(),
    ];
    if !model.trim().is_empty() {
        args.extend(["-m".to_string(), model.to_string()]);
    }
    args.push("-".to_string());
    args
}

fn temp_prompt_path(prefix: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "orchestrator-{prefix}-{}-{nanos}.txt",
        std::process::id()
    ))
}

/// The LAST `n` chars of `s`, elided at the front. Errors land at the END of a
/// stream: a CLI prints its banner, echoes its input, and only then says why it
/// failed. Taking the HEAD stored 500 chars of codex preamble on every failure
/// and truncated the reason away — 20 dead jobs in the user's store whose
/// `last_error` is byte-identical boilerplate, and a `is_rate_limited` check
/// that could never once have fired because the word "limit" never survived
/// the truncation. Tail, always.
pub(crate) fn tail(s: &str, n: usize) -> String {
    let t = s.trim();
    let count = t.chars().count();
    if count <= n {
        return t.to_string();
    }
    format!("…{}", t.chars().skip(count - n).collect::<String>())
}

/// Spawn a CLI as the leader of ITS OWN PROCESS GROUP.
///
/// Load-bearing, and measured: `which codex` on this machine is a NODE LAUNCHER
/// that `spawn`s the real binary as a GRANDCHILD with `stdio: "inherit"` — so
/// the grandchild holds the write ends of OUR pipes. `child.kill()` reaps only
/// the launcher; the grandchild lives on with the pipes open, `read_to_string`
/// never sees EOF, and anything that waits for the readers waits forever. Its
/// own group makes the whole tree killable with one signal (`kill_group`), which
/// closes those pipes and lets the readers EOF naturally.
///
/// EVERY `wait_output` caller must spawn through here: `kill_group` signals
/// `-pid`, which is only the child's own group because of `process_group(0)`.
fn spawn_grouped(cmd: &mut Command) -> std::io::Result<Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
}

/// Kill the child AND everything it spawned. See `spawn_grouped`.
fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    {
        // SAFETY: `child` is our own live, unreaped child, and `spawn_grouped`
        // made it a process-group LEADER — so its pgid is its pid and this can
        // only ever signal the tree we started. (Were it not a leader the pgid
        // wouldn't exist and killpg would simply return ESRCH.)
        unsafe {
            libc::killpg(child.id() as i32, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

/// How long we will wait for a pipe to hit EOF once we no longer need the child.
/// The reader drains continuously, so at this point the bytes are already in our
/// buffer or in a ≤64KB pipe — EOF is microseconds away in every sane case. The
/// bound exists for the INSANE case: a grandchild that outlives the kill and
/// holds the pipe open forever. Five seconds, then we take what we have and go.
const READER_EOF_GRACE_SECS: u64 = 5;

/// A child pipe being drained on its own thread into a buffer we can read at any
/// moment — as opposed to a `JoinHandle<String>`, which can only be read by
/// BLOCKING until EOF. That distinction is the whole point: joining a reader
/// whose pipe a stray grandchild holds open never returns, and the summarizer
/// thread that joined it would wedge with `sum_running` latched — no summary
/// would ever run again. The buffer is bytes, decoded once at the end, so a read
/// that stops mid-character can't corrupt one.
struct Drain {
    buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    eof: std::sync::mpsc::Receiver<()>,
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> Drain {
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (tx, eof) = std::sync::mpsc::channel();
    if let Some(mut pipe) = pipe {
        let sink = std::sync::Arc::clone(&buf);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut b) = sink.lock() {
                            b.extend_from_slice(&chunk[..n]);
                        }
                    }
                }
            }
            let _ = tx.send(());
        });
    }
    // No pipe → `tx` is dropped here, so the wait below returns at once.
    Drain { buf, eof }
}

impl Drain {
    /// Wait a BOUNDED time for EOF, then take whatever has arrived. Never blocks
    /// forever, and never throws away bytes we already hold.
    fn take(self) -> String {
        let _ = self
            .eof
            .recv_timeout(std::time::Duration::from_secs(READER_EOF_GRACE_SECS));
        let bytes = self.buf.lock().map(|b| b.clone()).unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Stage prefix on every summarizer error, so one query classifies every death:
///   SELECT substr(last_error,1,instr(last_error,':')), count(*)
///     FROM summary_job WHERE state='dead' GROUP BY 1;
/// `spawn:` the CLI wouldn't launch · `cli:` it ran and failed/timed out ·
/// `parse:` it answered but not with JSON · `transcript:`/`write:` ours.
///
/// Only `cli:` is ever classified as a rate limit (`main::is_rate_limited`): the
/// stdout tail below can carry the MODEL's words, and the model's opinion is not
/// the provider's verdict.
fn wait_output(child: &mut Child, label: &str, timeout_secs: u64) -> Result<String, String> {
    // Drain stdout+stderr on separate threads WHILE the child runs. If we only
    // read after exit (as the loop learns of it via try_wait), a child that
    // writes more than the OS pipe buffer (~64KB on macOS) blocks on a full
    // pipe, never exits, and we spin to the timeout and kill it as a spurious
    // "timed out" — a pipe-buffer deadlock. Concurrent readers keep the pipes
    // empty so the child can always make progress.
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed().as_secs() >= timeout_secs {
                    // Kill the GROUP, not just the child: a launcher's grandchild
                    // holds our pipes and would keep them open forever. Then take
                    // the output on a BOUNDED wait — a timeout must explain itself
                    // (a hung tool call? a half-written answer?), but recovering
                    // the explanation must never cost us the worker.
                    kill_group(child);
                    let _ = child.wait();
                    let (err, out) = (err.take(), out.take());
                    return Err(format!(
                        "cli: {label} timed out after {timeout_secs}s | stderr({}B) tail: {} | stdout({}B) tail: {}",
                        err.len(),
                        tail(&err, 400),
                        out.len(),
                        tail(&out, 200),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            Err(e) => return Err(format!("spawn: {label} wait failed: {e}")),
        }
    };
    let out = out.take();
    if !status.success() {
        let err = err.take();
        // exit code + the TAIL of both streams. stdout is included because a
        // CLI that fails on stdout-reported errors (or writes nothing to stderr
        // at all) would otherwise store an empty reason.
        return Err(format!(
            "cli: {label} exit={} | stderr({}B) tail: {} | stdout({}B) tail: {}",
            status
                .code()
                .map_or_else(|| "signal".to_string(), |c| c.to_string()),
            err.len(),
            tail(&err, 600),
            out.len(),
            tail(&out, 200),
        ));
    }
    Ok(out)
}

/// `claude -p --output-format json` emits an object with a `result` field
/// holding the assistant's text. Fall back to the raw stdout.
fn extract_result_text(stdout: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout) {
        if let Some(r) = v.get("result").and_then(|r| r.as_str()) {
            return Some(r.to_string());
        }
    }
    Some(stdout.to_string())
}

/// The first brace-balanced JSON object in `s` (a markdown fence or a preamble
/// around it is fine — we scan for `{`).
///
/// STRING-AWARE, because the old brace counter was not: a headline that merely
/// CONTAINED a `}` — entirely plausible for a session about JSON, templates, or
/// this very function — closed the object early, and the truncated fragment
/// failed to parse. That failed deterministically on every retry (same
/// transcript, same digest), so it burned all 3 attempts and killed the session.
/// Quotes and backslash escapes are now honoured, so braces inside a string are
/// just characters.
fn first_json_object(s: &str) -> Option<String> {
    let start = s.find('{')?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, c) in s[start..].char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(s[start..start + i + c.len_utf8()].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The diagnostic that wasn't. codex writes a ~270-char banner AND echoes
    /// the prompt to stderr, then prints the real reason LAST — so the old
    /// `err.chars().take(500)` stored nothing but preamble. Every one of the
    /// user's 20 dead jobs has a 519-char `last_error` of pure boilerplate,
    /// and `is_rate_limited` (which greps for "limit"/"429") could never once
    /// have matched, so the 900s rate-limit backoff never fired and 3 attempts
    /// burned inside one transient window. Tail, not head.
    #[test]
    fn tail_keeps_the_end_where_the_error_is() {
        let banner = "OpenAI Codex v0.144.1\nworkdir: /x\nmodel: gpt-5.6-sol\n".repeat(20);
        let stderr = format!(
            "{banner}ERROR: {{\"type\":\"error\",\"status\":429,\"message\":\"usage limit reached\"}}"
        );
        let kept = tail(&stderr, 120);
        assert!(kept.starts_with('…'), "elided at the FRONT");
        assert!(kept.contains("429"), "the reason survives");
        assert!(kept.contains("usage limit reached"));
        assert!(
            crate::is_rate_limited(&format!("cli: codex exec exit=1 | stderr tail: {kept}")),
            "…which is what lets the rate-limit backoff fire at all (and it is a \
             `cli:` error — the only stage whose text may be so classified)"
        );
        assert!(!kept.contains("OpenAI Codex v0.144.1"), "the banner does not");
        // short input is returned whole, un-elided.
        assert_eq!(tail("boom", 120), "boom");
        assert_eq!(tail("  boom\n", 120), "boom");
        // multi-byte chars must not panic or split (we count chars, not bytes).
        assert_eq!(tail("héllo wörld", 5), "…wörld");
    }

    /// A `}` inside a STRING used to close the object early: brace-counting
    /// without string-awareness truncated the JSON, the fragment failed to
    /// parse, and — because the digest is stable — it failed the SAME way on
    /// all 3 retries. A deterministic trapdoor straight to a dead job.
    #[test]
    fn first_json_object_is_string_aware() {
        let out = r#"{"headline":"fixed the `}` brace bug","next_action":"","detail":["a {b} c"]}"#;
        let got = first_json_object(out).expect("a balanced object");
        assert_eq!(got, out, "braces inside strings are just characters");
        let v: serde_json::Value = serde_json::from_str(&got).expect("valid JSON");
        assert_eq!(v["headline"], "fixed the `}` brace bug");
        // an escaped quote must not end the string early.
        let esc = r#"{"headline":"he said \"}\" loudly","detail":[]}"#;
        assert_eq!(first_json_object(esc).as_deref(), Some(esc));
        // a markdown fence / preamble around it is still fine.
        let fenced = "here you go:\n```json\n{\"headline\":\"ok\"}\n```";
        assert_eq!(
            first_json_object(fenced).as_deref(),
            Some("{\"headline\":\"ok\"}")
        );
        assert_eq!(first_json_object("no braces here"), None);
    }

    #[test]
    fn codex_exec_approval_flag_is_top_level() {
        let args = codex_exec_args(Path::new("/tmp/project"), Path::new("/tmp/out.txt"), "");
        let exec = args
            .iter()
            .position(|a| a == "exec")
            .expect("exec subcommand");
        let approval = args
            .iter()
            .position(|a| a == "--ask-for-approval")
            .expect("approval arg");
        assert!(
            approval < exec,
            "approval mode is a top-level codex option, not an exec option"
        );
        assert_eq!(args.get(approval + 1).map(String::as_str), Some("never"));
        assert!(
            args[exec + 1..].iter().all(|a| a != "--ask-for-approval"),
            "codex exec itself rejects --ask-for-approval"
        );
    }

    #[test]
    fn digest_captures_crate_structure() {
        // A TREE THIS TEST BUILDS, not the repo it happens to live in. The old
        // version walked up from CARGO_MANIFEST_DIR and asserted the digest
        // contained "docs" — true in this repo, FALSE in the published one, where
        // scripts/oss-publish.sh deliberately excludes docs/. So every stranger
        // who cloned the public source and ran `cargo test` failed on their first
        // command, and the assertion was really about our directory layout rather
        // than about project_digest. A fixture puts the test back on the function.
        let root = std::env::temp_dir().join(format!("kod-digest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["app/crates/orchestrator-gui", "notes", "target/debug"] {
            std::fs::create_dir_all(root.join(d)).expect("fixture tree");
        }
        std::fs::write(root.join("README.md"), "# fixture\n").expect("fixture file");

        let d = project_digest(&root);
        let _ = std::fs::remove_dir_all(&root);

        assert!(d.contains("app"), "digest should list top-level dirs:\n{d}");
        assert!(d.contains("notes"), "digest should list EVERY top-level dir:\n{d}");
        assert!(d.contains("crates"), "digest should descend one level:\n{d}");
        // `target` is on project_digest's own skip list: the digest briefs an
        // agent, and build output is noise that would crowd out the source.
        assert!(!d.contains("target"), "build dirs must be skipped:\n{d}");
    }
}
