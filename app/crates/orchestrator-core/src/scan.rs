//! The live layer (docs/015) — builds a real `ScanSnapshot` from the user's
//! Claude session dirs, Codex rollouts, and git, doing all the
//! fs/git I/O the pure resolver must NOT do. Records facts only.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::registry::{GitFacts, ScanSnapshot, ScanSource, SourceKind};

const GIT: &str = "/usr/bin/git";

pub fn home() -> PathBuf {
    // No personal fallback in source: HOME is set on any real login session; if
    // it somehow isn't, yield an empty path rather than a user-specific one.
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

/// A store-backed project that ALREADY HAS ITS OWN DIRECTORY (#29) recorded as
/// an `Explicit` source, so it keys EXACTLY like a session source in the same
/// dir (`path:<dir>`, or the git key when the user later `git init`s it) and
/// therefore lands in the SAME resolver group — one rail row, never two. The
/// path-less `ScanSource::idea` stays for ideas with no code yet.
///
/// `display_name` carries the user's typed name so the rail keeps saying
/// "Cinematic Check-in" rather than the slugified dir basename.
/// The canonical key the REGISTRY will give this directory — the key any session
/// run there produces. A project we create (or promote) at that dir must be born
/// with THIS key, or the row the user sees and the row their data is filed under
/// drift apart on the next scan.
///
/// It mirrors `resolve()`'s `key_of` exactly: git remote → `github:<remote>`,
/// git toplevel → `path:<toplevel>`, non-git → `path:<folded dir>`. Cheap (one
/// git probe) and it is what makes adopting an EXISTING repo folder safe: the
/// project is keyed `github:…` from birth instead of a `path:` key its own
/// sessions would never produce.
pub fn key_for_dir(dir: &Path) -> String {
    let mut git = GitCache::new();
    match git.facts(dir) {
        Some(g) if g.is_git => {
            if let Some(r) = &g.remote {
                let norm = crate::registry::normalize_remote(r);
                return format!("github:{norm}").replacen("github:github:", "github:", 1);
            }
            if let Some(t) = &g.toplevel {
                return format!("path:{}", t.display());
            }
            format!("path:{}", crate::registry::fold_to_project_dir(dir).display())
        }
        _ => format!("path:{}", crate::registry::fold_to_project_dir(dir).display()),
    }
}

pub fn store_path_source(key: &str, name: &str, path: &Path) -> ScanSource {
    let mut git = GitCache::new();
    ScanSource {
        kind: SourceKind::Explicit,
        source_ref: format!("store:{key}"),
        claude_dir: None,
        launch_root: None,
        dominant_cwd: None,
        location: Some(path.to_path_buf()),
        git: git.facts(path),
        session_count: 0,
        last_activity_secs: mtime_secs(path),
        last_message: None,
        store_key: None,
        display_name: Some(name.to_string()),
    }
}

fn mtime_secs(p: &Path) -> u64 {
    fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read a Claude session file once: the dominant cwd (most-frequent `"cwd"`,
/// fast substring scan to avoid a full JSON parse on megabyte files) and the
/// last assistant message (the DID line). Returns `(dominant_cwd, last_msg)`.
fn scan_claude_session(path: &Path) -> (Option<PathBuf>, Option<String>) {
    match fs::read_to_string(path) {
        Ok(t) => {
            let (dom, last, _start) = claude_cwd_last(&t);
            (dom, last)
        }
        Err(_) => (None, None),
    }
}

/// Returns (DOMINANT cwd, last assistant text, FIRST cwd). The first cwd is the
/// session's START dir — claude's storage key for `--resume` (≠ dominant when the
/// session cd'd from where it was launched).
fn claude_cwd_last(text: &str) -> (Option<PathBuf>, Option<String>, Option<PathBuf>) {
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut first: Option<PathBuf> = None;
    for line in text.lines() {
        let mut from = 0;
        while let Some(i) = line[from..].find("\"cwd\":\"") {
            let start = from + i + 7;
            if let Some(end) = line[start..].find('"') {
                let cwd = &line[start..start + end];
                if !cwd.is_empty() {
                    *counts.entry(cwd.to_string()).or_default() += 1;
                    if first.is_none() {
                        first = Some(PathBuf::from(cwd));
                    }
                }
                from = start + end + 1;
            } else {
                break;
            }
        }
    }
    let dom = counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(c, _)| PathBuf::from(c));
    // last assistant text: scan lines in reverse for an assistant record.
    let mut last_msg = None;
    for line in text.lines().rev() {
        if line.contains("\"type\":\"assistant\"") || line.contains("\"role\":\"assistant\"") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(t) = assistant_text(&v) {
                    last_msg = Some(t);
                    break;
                }
            }
        }
    }
    (dom, last_msg, first)
}

/// Extract the text of a Claude assistant record (content can be a string or an
/// array of {type:text,text}).
fn assistant_text(v: &serde_json::Value) -> Option<String> {
    let msg = v.get("message").unwrap_or(v);
    let content = msg.get("content")?;
    if let Some(s) = content.as_str() {
        return non_empty(s.trim());
    }
    if let Some(arr) = content.as_array() {
        for part in arr {
            if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    if let Some(s) = non_empty(t.trim()) {
                        return Some(s);
                    }
                }
            }
        }
    }
    None
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.chars().take(280).collect())
    }
}

/// De-encode a Claude dir name (`-Users-me-local-alpha`) to the real
/// launch path by filesystem probe: dashes are ambiguous (`/` vs `-` vs `_`),
/// so greedily build the longest path that exists on disk.
fn deencode_claude_dir(name: &str) -> Option<PathBuf> {
    let body = name.trim_start_matches('-');
    let parts: Vec<&str> = body.split('-').collect();
    let mut path = PathBuf::from("/");
    let mut i = 0;
    while i < parts.len() {
        // try joining following parts with '-' as long as the result exists
        let mut acc = parts[i].to_string();
        let mut best = i + 1;
        let mut j = i + 1;
        while j < parts.len() {
            let candidate = format!("{acc}-{}", parts[j]);
            if path.join(&candidate).exists() {
                acc = candidate;
                best = j + 1;
            }
            j += 1;
        }
        // prefer a clean component boundary when both exist
        if path.join(parts[i]).exists() && (i + 1 >= best) {
            path.push(parts[i]);
            i += 1;
        } else if path.join(&acc).exists() {
            path.push(&acc);
            i = best;
        } else {
            path.push(parts[i]);
            i += 1;
        }
    }
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

struct GitCache {
    map: HashMap<PathBuf, Option<GitFacts>>,
}
impl GitCache {
    fn new() -> Self {
        GitCache {
            map: HashMap::new(),
        }
    }
    fn facts(&mut self, dir: &Path) -> Option<GitFacts> {
        if let Some(f) = self.map.get(dir) {
            return f.clone();
        }
        let f = probe_git(dir);
        self.map.insert(dir.to_path_buf(), f.clone());
        f
    }
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new(GIT)
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    } else {
        None
    }
}

fn probe_git(dir: &Path) -> Option<GitFacts> {
    if !dir.exists() {
        return Some(GitFacts::default());
    }
    let toplevel = git_out(dir, &["rev-parse", "--show-toplevel"]);
    if toplevel.is_none() {
        return Some(GitFacts::default());
    }
    Some(GitFacts {
        is_git: true,
        toplevel: toplevel.map(PathBuf::from),
        remote: git_out(dir, &["remote", "get-url", "origin"]),
        common_dir: git_out(dir, &["rev-parse", "--git-common-dir"]).map(PathBuf::from),
    })
}

/// Find a codex session's rollout transcript by its rollout id (#9 §4). Codex
/// stores them date-foldered under `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<id>.jsonl`,
/// so we search by the id embedded in the filename.
pub fn codex_rollout_path(rollout_id: &str) -> Option<PathBuf> {
    if rollout_id.is_empty() {
        return None;
    }
    let root = PathBuf::from(std::env::var("HOME").ok()?).join(".codex/sessions");
    find_transcript(&root, rollout_id, 5)
}

/// Find a claude session's transcript by its session id (#9 §4). Claude stores
/// `~/.claude/projects/<cwd-encoded>/<session-id>.jsonl`; searching by the id in
/// the filename avoids guessing the cwd-encoding scheme.
pub fn claude_transcript_path(session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() {
        return None;
    }
    let root = PathBuf::from(std::env::var("HOME").ok()?).join(".claude/projects");
    find_transcript(&root, session_id, 3)
}

fn find_transcript(dir: &std::path::Path, needle: &str, depth: usize) -> Option<PathBuf> {
    let mut subdirs = Vec::new();
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            subdirs.push(p);
        } else if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
            if name.ends_with(".jsonl") && name.contains(needle) {
                return Some(p);
            }
        }
    }
    if depth > 0 {
        // search newest date folders first (codex foldering is YYYY/MM/DD).
        subdirs.sort();
        for d in subdirs.into_iter().rev() {
            if let Some(found) = find_transcript(&d, needle, depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

/// A resumable session discovered on disk (the import/recovery feature). The
/// transcript survives a crash; `id` is the resume handle (claude --resume <id>
/// / codex resume <id>) and `cwd` is the RECORDED cwd resume must spawn in.
#[derive(Debug, Clone)]
pub struct RecoverableSession {
    pub id: String,
    pub is_codex: bool,
    /// the DOMINANT recorded cwd — where most of the work happened; groups the
    /// session under that project. May differ from `start_cwd` (a cd'd session).
    pub cwd: PathBuf,
    /// the cwd the session was STARTED in = where claude stored the transcript.
    /// `claude --resume` is scoped to THIS (a session started one dir up then
    /// cd'd into the project must resume from the start cwd, or claude says "No
    /// conversation found"). Equals `cwd` for codex (resumes by id, not cwd).
    pub start_cwd: PathBuf,
    pub last_active_secs: u64,
    pub last_message: Option<String>,
    /// transcript path (claude jsonl / codex rollout) — for the summary + size.
    pub path: PathBuf,
    /// first message timestamp (session start), epoch secs.
    pub started_secs: u64,
    /// last in-file message timestamp (true conversation end — more accurate
    /// than file mtime, which claude post-touches for title/mode metadata).
    pub ended_secs: u64,
    /// transcript size in bytes (a signal of session weight).
    pub bytes: u64,
    /// human prompt count (a glanceable "how big" stat).
    pub turns: u32,
}

/// Parse an ISO-8601 `YYYY-MM-DDTHH:MM:SS(.sssZ)` (always UTC in both CLIs) to
/// epoch seconds — zero-dep (civil-from-days, Howard Hinnant). Ignores millis.
fn parse_iso_epoch(ts: &str) -> Option<u64> {
    let n = |a: usize, b: usize| ts.get(a..b).and_then(|s| s.parse::<i64>().ok());
    let (y, m, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (hh, mm, ss) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    let yy = if m <= 2 { y - 1 } else { y };
    let era = (if yy >= 0 { yy } else { yy - 399 }) / 400;
    let yoe = yy - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss;
    (secs >= 0).then_some(secs as u64)
}

/// (first, last) non-null record timestamp in a transcript, in one forward pass.
/// A null `timestamp` has no quote after the colon, so the `"timestamp":"`
/// marker matches only real ISO timestamps.
fn first_last_ts(text: &str) -> (u64, u64) {
    let (mut first, mut last) = (0u64, 0u64);
    for line in text.lines() {
        if let Some(i) = line.find("\"timestamp\":\"") {
            if let Some(e) = line.get(i + 13..i + 13 + 19).and_then(parse_iso_epoch) {
                if first == 0 {
                    first = e;
                }
                if e > last {
                    last = e;
                }
            }
        }
    }
    (first, last)
}

/// Human prompt count for a claude transcript: top-level `type:"user"` records
/// whose content is a STRING (not a tool_result array) and not a meta injection.
fn claude_turns(text: &str) -> u32 {
    // A tool_result user record's TOP-level content is an array (`"content":[`),
    // but its INNER content is a string (`"content":"(Bash completed…)"`), so the
    // `"content":"` test alone matches every tool round-trip and miscounts it as a
    // human prompt. Exclude tool_result lines explicitly. isMeta records ARE
    // top-level and must be excluded too.
    text.lines()
        .filter(|l| {
            l.contains("\"type\":\"user\"")
                && !l.contains("\"isMeta\":true")
                && !l.contains("\"type\":\"tool_result\"")
                && l.contains("\"content\":\"")
        })
        .count() as u32
}

/// User-message count for a codex rollout (its `user_message` event stream).
fn codex_turns(text: &str) -> u32 {
    text.lines()
        .filter(|l| l.contains("\"user_message\""))
        .count() as u32
}

/// Our OWN headless `claude -p` calls (the recover-summary + the product-map
/// extraction) write a one-shot transcript whose single user message IS our
/// prompt — exclude them so internal tooling never appears as a recoverable
/// session. Matched right after the JSON content opener, so a real session that
/// merely QUOTES the prompt (e.g. while editing extract.rs) is NOT dropped.
fn is_internal_p_call(text: &str) -> bool {
    const SIGS: [&str; 3] = [
        "\"content\":\"Below is a digest of a coding-assistant session",
        "\"content\":\"You are mapping a software project's PRODUCT ANATOMY",
        "\"content\":\"You are summarizing a coding-assistant session for a status board",
    ];
    text.lines().any(|l| SIGS.iter().any(|s| l.contains(s)))
}

/// List resumable sessions on disk, newest first. Two-phase so age no longer
/// hides a crash: ALL candidates are stat'd cheaply (bounded only by the generous
/// `within_days`), then we read + parse just the newest `limit` VALID ones — so
/// the read cost is ~`limit` no matter how far back we look. cwd is filtered to
/// under `~/local` (skip tmp/home noise); the GUI groups them by project.
pub fn recoverable_sessions(within_days: u64, limit: usize) -> Vec<RecoverableSession> {
    let home = home();
    // EVERY project root, not just the configured one (crate::project_roots):
    // retargeting the projects folder must not empty Recover of every session
    // ever run under ~/local.
    let roots = crate::project_roots();
    let under_root = |p: &Path| roots.iter().any(|r| p.starts_with(r));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cutoff = now.saturating_sub(within_days * 86_400);

    // Phase 1 (cheap, NO content read): collect every candidate transcript as
    // (path, mtime, is_codex) by stat only. The age `cutoff` just bounds this
    // stat list — it is no longer what limits how many we actually read.
    let mut cands: Vec<(PathBuf, u64, bool)> = Vec::new();
    if let Ok(dirs) = fs::read_dir(home.join(".claude/projects")) {
        for dir in dirs.flatten() {
            if !dir.path().is_dir() {
                continue;
            }
            if let Ok(files) = fs::read_dir(dir.path()) {
                for f in files.flatten() {
                    let p = f.path();
                    if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let mt = mtime_secs(&p);
                    if mt >= cutoff {
                        cands.push((p, mt, false));
                    }
                }
            }
        }
    }
    for rollout in walk_jsonl(&home.join(".codex/sessions")) {
        let mt = mtime_secs(&rollout);
        if mt >= cutoff {
            cands.push((rollout, mt, true));
        }
    }

    // Phase 2: newest-first, read + parse ONLY until `limit` VALID sessions are
    // collected. Cost is ~`limit` reads regardless of how far back we look — so a
    // session that crashed weeks ago is recoverable, not hidden by a short window.
    cands.sort_by(|a, b| b.1.cmp(&a.1));
    let mut out: Vec<RecoverableSession> = Vec::new();
    for (p, mt, is_codex) in cands {
        if out.len() >= limit {
            break;
        }
        let md = fs::metadata(&p).ok();
        let Ok(text) = fs::read_to_string(&p) else {
            continue;
        }; // one read
        let created = md
            .as_ref()
            .and_then(|m| m.created().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let bytes = md.as_ref().map(|m| m.len()).unwrap_or(0);
        if is_codex {
            // Codex: id = line-0 payload.id (also the rollout filename uuid).
            let (cwd, last) = codex_cwd_last(&text);
            let cwd = match cwd.map(canonicalize_tmp) {
                Some(c) if under_root(&c) => c,
                _ => continue,
            };
            let Some(id) = codex_id_text(&text) else {
                continue;
            };
            let (started, ended) = first_last_ts(&text);
            out.push(RecoverableSession {
                id,
                is_codex: true,
                start_cwd: cwd.clone(), // codex resumes by id; cwd is just where to spawn
                cwd,
                last_active_secs: mt,
                last_message: last,
                path: p,
                started_secs: if started > 0 { started } else { created },
                ended_secs: if ended > 0 { ended } else { mt },
                bytes,
                turns: codex_turns(&text),
            });
        } else {
            // Claude: filename stem = the session id (the resume handle).
            if is_internal_p_call(&text) {
                continue; // our own summary/extraction -p call, not a session
            }
            let (cwd, last, start) = claude_cwd_last(&text);
            let Some(cwd) = cwd else { continue };
            if !under_root(&cwd) {
                continue; // skip tmp/home/other noise
            }
            let Some(id) = p.file_stem().and_then(|s| s.to_str()).map(String::from) else {
                continue;
            };
            // resume cwd = the session's START dir (claude's --resume storage key);
            // falls back to the dominant cwd when it never moved.
            let start_cwd = start.unwrap_or_else(|| cwd.clone());
            let (started, ended) = first_last_ts(&text);
            out.push(RecoverableSession {
                id,
                is_codex: false,
                start_cwd,
                cwd,
                last_active_secs: mt,
                last_message: last,
                path: p,
                started_secs: if started > 0 { started } else { created },
                ended_secs: if ended > 0 { ended } else { mt },
                bytes,
                turns: claude_turns(&text),
            });
        }
    }
    out
}

/// Codex session id = line-0 `session_meta.payload.id`.
pub fn codex_id(rollout: &Path) -> Option<String> {
    codex_id_text(&fs::read_to_string(rollout).ok()?)
}

fn codex_id_text(text: &str) -> Option<String> {
    let first = text.lines().next()?;
    let v: serde_json::Value = serde_json::from_str(first).ok()?;
    v.get("payload")
        .and_then(|p| p.get("id"))
        .and_then(|i| i.as_str())
        .map(String::from)
}

/// Snapshot the codex session ids already on disk for `cwd` BEFORE spawning a
/// fresh codex — so the post-spawn discovery can pick the genuinely-new rollout
/// (the one whose id is NOT in this set) rather than a sibling session in the
/// same cwd. Also excludes a resumed-in-place rollout (its id pre-exists).
/// The codex rollout root for a session: `<CODEX_HOME>/sessions` for a profiled
/// account, else the ambient `~/.codex/sessions`. A profiled codex session writes
/// its rollouts under its profile's CODEX_HOME, so birth-discovery + recovery must
/// look there, not only in HOME.
pub fn codex_sessions_root(codex_home: Option<&Path>) -> PathBuf {
    match codex_home {
        Some(h) if !h.as_os_str().is_empty() => h.join("sessions"),
        _ => home().join(".codex/sessions"),
    }
}

pub fn codex_ids_for_cwd(cwd: &Path, codex_home: Option<&Path>) -> Vec<String> {
    codex_ids_in(&codex_sessions_root(codex_home), cwd)
}

fn codex_ids_in(base: &Path, cwd: &Path) -> Vec<String> {
    let target = canonicalize_tmp(cwd.to_path_buf());
    walk_jsonl(base)
        .into_iter()
        .filter(|r| {
            scan_codex_rollout(r).0.map(canonicalize_tmp).as_deref() == Some(target.as_path())
        })
        .filter_map(|r| codex_id(&r))
        .collect()
}

/// After spawning a FRESH codex (whose uuidv7 id we can't pre-set), find the
/// rollout it just created — the newest rollout under `~/.codex/sessions` with
/// mtime >= `since_secs`, recorded cwd matching `cwd`, and id NOT in `exclude`
/// (the pre-spawn snapshot) — so two codex in the same cwd never cross-record.
pub fn newest_codex_id_for_cwd(
    cwd: &Path,
    since_secs: u64,
    exclude: &std::collections::HashSet<String>,
    codex_home: Option<&Path>,
) -> Option<String> {
    newest_codex_id_in(&codex_sessions_root(codex_home), cwd, since_secs, exclude)
}

/// Testable core of [`newest_codex_id_for_cwd`] over an explicit sessions dir.
fn newest_codex_id_in(
    base: &Path,
    cwd: &Path,
    since_secs: u64,
    exclude: &std::collections::HashSet<String>,
) -> Option<String> {
    let target = canonicalize_tmp(cwd.to_path_buf());
    let mut best: Option<(u64, String)> = None;
    for rollout in walk_jsonl(base) {
        let mt = mtime_secs(&rollout);
        if mt < since_secs {
            continue;
        }
        let (rcwd, _) = scan_codex_rollout(&rollout);
        if rcwd.map(canonicalize_tmp).as_deref() != Some(target.as_path()) {
            continue;
        }
        let Some(id) = codex_id(&rollout) else {
            continue;
        };
        if exclude.contains(&id) {
            continue;
        }
        if best.as_ref().map(|(m, _)| mt > *m).unwrap_or(true) {
            best = Some((mt, id));
        }
    }
    best.map(|(_, id)| id)
}

/// Build the live snapshot. Blocking + I/O-heavy (run off the UI thread).
pub fn build_snapshot() -> ScanSnapshot {
    let home = home();
    // the user's projects folder (Settings → Projects folder, #29) — the one
    // root the resolver trusts for tier-3 path rows AND where a new project's
    // own directory is created. Defaults to ~/local.
    let local = crate::projects_root();
    let mut git = GitCache::new();
    let mut sources: Vec<ScanSource> = Vec::new();

    // 1) Claude session dirs
    let claude_base = home.join(".claude/projects");
    if let Ok(entries) = fs::read_dir(&claude_base) {
        for e in entries.flatten() {
            let dir = e.path();
            if !dir.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let launch_root = deencode_claude_dir(&name);
            let mut jsonls: Vec<PathBuf> = Vec::new();
            if let Ok(files) = fs::read_dir(&dir) {
                for f in files.flatten() {
                    if f.path().extension().and_then(|x| x.to_str()) == Some("jsonl") {
                        jsonls.push(f.path());
                    }
                }
            }
            if jsonls.is_empty() {
                let g = launch_root.as_deref().and_then(|p| git.facts(p));
                sources.push(ScanSource {
                    kind: SourceKind::Claude,
                    source_ref: format!("claude:{name}:(no-jsonl)"),
                    claude_dir: Some(name),
                    launch_root,
                    dominant_cwd: None,
                    location: None,
                    git: g,
                    session_count: 0,
                    last_activity_secs: mtime_secs(&dir),
                    last_message: None,
                    store_key: None,
                    display_name: None,
                });
                continue;
            }
            for jf in jsonls {
                let (dom, last_msg) = scan_claude_session(&jf);
                let probe = dom.clone().or_else(|| launch_root.clone());
                let g = probe.as_deref().and_then(|p| git.facts(p));
                sources.push(ScanSource {
                    kind: SourceKind::Claude,
                    source_ref: format!("claude:{}", jf.display()),
                    claude_dir: Some(name.clone()),
                    launch_root: launch_root.clone(),
                    dominant_cwd: dom,
                    location: None,
                    git: g,
                    session_count: 1,
                    last_activity_secs: mtime_secs(&jf),
                    last_message: last_msg,
                    store_key: None,
                    display_name: None,
                });
            }
        }
    }

    // 2) Codex rollouts (one read for cwd + last message)
    for rollout in walk_jsonl(&home.join(".codex/sessions")) {
        let (cwd, last_msg) = scan_codex_rollout(&rollout);
        let cwd = cwd.map(canonicalize_tmp);
        let g = cwd.as_deref().and_then(|p| git.facts(p));
        sources.push(ScanSource {
            kind: SourceKind::Codex,
            source_ref: format!("codex:{}", rollout.display()),
            claude_dir: None,
            launch_root: None,
            dominant_cwd: cwd,
            location: None,
            git: g,
            session_count: 1,
            last_activity_secs: mtime_secs(&rollout),
            last_message: last_msg,
            store_key: None,
            display_name: None,
        });
    }

    // 3) explicit: the orchestrator's own repo (zero sessions)
    let orch = local.join("orchestrator");
    if orch.is_dir() {
        let g = git.facts(&orch);
        sources.push(ScanSource {
            kind: SourceKind::Explicit,
            source_ref: "explicit:orchestrator".into(),
            claude_dir: None,
            launch_root: None,
            dominant_cwd: None,
            location: Some(orch.clone()),
            git: g,
            session_count: 0,
            last_activity_secs: mtime_secs(&orch),
            last_message: None,
            store_key: None,
            display_name: None,
        });
    }

    ScanSnapshot {
        captured_at_secs: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        home,
        // ADDITIVE (see crate::project_roots): the configured folder AND ~/local.
        // A single root here would mean that retargeting the projects folder
        // silently dropped every non-git project under the old one from the rail.
        project_roots: crate::project_roots(),
        sources,
    }
}

fn walk_jsonl(base: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn rec(dir: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    rec(&p, out);
                } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                    out.push(p);
                }
            }
        }
    }
    rec(base, &mut out);
    out
}

/// Read a Codex rollout once: line-0 `session_meta.payload.cwd` and the last
/// `payload.type == "agent_message"` text (the DID).
fn scan_codex_rollout(rollout: &Path) -> (Option<PathBuf>, Option<String>) {
    match fs::read_to_string(rollout) {
        Ok(t) => codex_cwd_last(&t),
        Err(_) => (None, None),
    }
}

fn codex_cwd_last(text: &str) -> (Option<PathBuf>, Option<String>) {
    let cwd = text
        .lines()
        .next()
        .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .and_then(|v| {
            v.get("payload")
                .and_then(|p| p.get("cwd"))
                .and_then(|c| c.as_str())
                .map(PathBuf::from)
        });
    let mut last = None;
    for line in text.lines().rev() {
        if line.contains("\"agent_message\"") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                let payload = v.get("payload").unwrap_or(&v);
                if payload.get("type").and_then(|t| t.as_str()) == Some("agent_message") {
                    if let Some(m) = payload.get("message").and_then(|m| m.as_str()) {
                        last = non_empty(m.trim());
                        break;
                    }
                }
            }
        }
    }
    (cwd, last)
}

fn canonicalize_tmp(p: PathBuf) -> PathBuf {
    if let Ok(s) = p.strip_prefix("/tmp") {
        return Path::new("/private/tmp").join(s);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_p_call_excluded_but_quoting_session_kept() {
        // our own -p calls: the prompt IS the user message → excluded.
        assert!(is_internal_p_call(
            "{\"type\":\"user\",\"message\":{\"content\":\"Below is a digest of a coding-assistant session: GOAL...\"}}\n"
        ));
        assert!(is_internal_p_call(
            "{\"type\":\"user\",\"message\":{\"content\":\"You are mapping a software project's PRODUCT ANATOMY for a board.\"}}\n"
        ));
        // a REAL session that merely QUOTES the prompt deeper in content (e.g. while
        // editing extract.rs) — the sig is behind an escaped quote, not right after
        // the content opener → KEPT (not a false positive).
        assert!(!is_internal_p_call(
            "{\"type\":\"user\",\"message\":{\"content\":\"fix this: let p = \\\"Below is a digest of a coding-assistant session\\\";\"}}\n"
        ));
        assert!(!is_internal_p_call(
            "{\"type\":\"user\",\"message\":{\"content\":\"normal request\"}}\n"
        ));
    }

    #[test]
    fn dominant_cwd_picks_most_frequent() {
        let dir = std::env::temp_dir().join(format!("orch-scan-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let f = dir.join("s.jsonl");
        fs::write(
            &f,
            "{\"cwd\":\"/a/x\"}\n{\"cwd\":\"/a/y\"}\n{\"cwd\":\"/a/y\"}\n",
        )
        .unwrap();
        assert_eq!(scan_claude_session(&f).0, Some(PathBuf::from("/a/y")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tmp_canonicalizes() {
        assert_eq!(
            canonicalize_tmp("/tmp/x/y".into()),
            PathBuf::from("/private/tmp/x/y")
        );
    }

    #[test]
    fn deencode_descends_to_existing_path() {
        // machine-independent: encode the real HOME to its Claude dir form
        // (every '/' → '-') and confirm the fs-probe reconstructs it.
        let home = std::env::var("HOME").unwrap();
        let encoded = home.replace('/', "-");
        assert_eq!(deencode_claude_dir(&encoded), Some(PathBuf::from(&home)));
    }

    #[test]
    fn iso_epoch_parses_utc() {
        // 2026-06-05T23:39:28.520Z → known epoch (UTC)
        assert_eq!(
            parse_iso_epoch("2026-06-05T23:39:28.520Z"),
            Some(1_780_702_768)
        );
        // millis/Z optional; bad input → None
        assert_eq!(parse_iso_epoch("2026-06-05T23:39:28"), Some(1_780_702_768));
        assert_eq!(parse_iso_epoch("not-a-date"), None);
        // epoch 0
        assert_eq!(parse_iso_epoch("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn first_last_ts_and_turns() {
        let text = "{\"type\":\"queue-operation\",\"timestamp\":null}\n\
                    {\"type\":\"user\",\"timestamp\":\"2026-06-05T10:00:00.000Z\",\"message\":{\"content\":\"hello\"}}\n\
                    {\"type\":\"user\",\"timestamp\":\"2026-06-05T10:00:05.000Z\",\"message\":{\"content\":[{\"type\":\"tool_result\"}]}}\n\
                    {\"type\":\"assistant\",\"timestamp\":\"2026-06-05T10:05:00.000Z\"}\n";
        let (first, last) = first_last_ts(text);
        assert_eq!(first, parse_iso_epoch("2026-06-05T10:00:00Z").unwrap());
        assert_eq!(last, parse_iso_epoch("2026-06-05T10:05:00Z").unwrap());
        // one human turn (string content); the tool_result user line is excluded.
        assert_eq!(claude_turns(text), 1);
        // a STRING prompt that merely MENTIONS tool_result must still count (the
        // old `!contains("tool_result")` substring exclusion false-dropped it).
        let m = "{\"type\":\"user\",\"timestamp\":\"2026-06-05T10:01:00.000Z\",\"message\":{\"content\":\"how do I parse a tool_result block?\"}}\n";
        assert_eq!(claude_turns(m), 1);
        // an isMeta system injection (string content) is still excluded.
        let meta =
            "{\"type\":\"user\",\"isMeta\":true,\"message\":{\"content\":\"Caveat: ...\"}}\n";
        assert_eq!(claude_turns(meta), 0);
    }

    #[test]
    fn newest_codex_id_filters_by_cwd_and_since() {
        let base = std::env::temp_dir().join(format!("orch-codex-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let _ = fs::create_dir_all(&base);
        let meta = |id: &str, cwd: &str| {
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{cwd}\"}}}}\n"
            )
        };
        // a rollout for the target cwd, and one for a DIFFERENT cwd (excluded).
        fs::write(
            base.join("rollout-match.jsonl"),
            meta("id-match", "/Users/me/proj"),
        )
        .unwrap();
        fs::write(
            base.join("rollout-other.jsonl"),
            meta("id-other", "/Users/me/elsewhere"),
        )
        .unwrap();

        let none = std::collections::HashSet::new();
        // since=0 includes the just-written files → returns the cwd-matching id.
        assert_eq!(
            newest_codex_id_in(&base, Path::new("/Users/me/proj"), 0, &none),
            Some("id-match".into())
        );
        // a different cwd with no rollout → None.
        assert_eq!(
            newest_codex_id_in(&base, Path::new("/Users/me/nope"), 0, &none),
            None
        );
        // a `since` in the far future excludes everything (mtime cutoff).
        let far_future = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 10_000;
        assert_eq!(
            newest_codex_id_in(&base, Path::new("/Users/me/proj"), far_future, &none),
            None
        );
        // EXCLUDE the only matching id (a pre-existing sibling) → None, never a cross-record.
        let exclude: std::collections::HashSet<String> =
            ["id-match".to_string()].into_iter().collect();
        assert_eq!(
            newest_codex_id_in(&base, Path::new("/Users/me/proj"), 0, &exclude),
            None
        );
        // codex_ids_in snapshots the cwd's ids (for the pre-spawn exclude set).
        assert_eq!(
            codex_ids_in(&base, Path::new("/Users/me/proj")),
            vec!["id-match".to_string()]
        );
        let _ = fs::remove_dir_all(&base);
    }

}
