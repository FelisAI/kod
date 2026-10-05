//! The repo half of a packet (docs/028 §3): what is committed and dirty NOW,
//! with times, so each change can be attributed to the session or not. Kod runs
//! several agents per repo; "everything since the start commit" alone would hand
//! the receiver other sessions' work as this one's.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use super::log::{EventKind, SessionLog};

/// Commits listed at most; more are summarized as a count.
const MAX_COMMITS: usize = 40;
const MAX_DIRTY: usize = 60;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RepoSnapshot {
    pub is_repo: bool,
    /// The session's start commit, as recorded or looked up by time.
    pub start_commit: Option<String>,
    /// Newest first.
    pub commits: Vec<CommitRow>,
    pub more_commits: bool,
    pub dirty: Vec<DirtyRow>,
    /// `git diff --stat HEAD`.
    pub diff_stat: String,
    /// Files the final turn edited that git ignores, so `git status` can't show
    /// them (a local task queue, measured) — listed from the agent's edit records.
    pub ignored_edits: Vec<DirtyRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommitRow {
    pub at_secs: i64,
    pub short: String,
    pub subject: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirtyRow {
    /// Porcelain status, e.g. " M", "??"; empty for an ignored edit.
    pub status: String,
    pub path: String,
    pub mtime_secs: Option<i64>,
}

/// Run git in `cwd`. `GIT_OPTIONAL_LOCKS=0` keeps `status` from refreshing the
/// index — never take `index.lock` from under an agent working in this tree.
fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn mtime_secs(p: &Path) -> Option<i64> {
    let m = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(m.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64)
}

/// `secs` → git's own date format, which every git parses exactly.
fn git_date(secs: i64) -> String {
    jiff::Timestamp::from_second(secs)
        .map(|t| t.strftime("%Y-%m-%d %H:%M:%S +0000").to_string())
        .unwrap_or_default()
}

pub fn snapshot(cwd: &Path, log: &SessionLog) -> RepoSnapshot {
    let mut snap = RepoSnapshot::default();
    if git(cwd, &["rev-parse", "--is-inside-work-tree"]).as_deref().map(str::trim) != Some("true") {
        return snap;
    }
    snap.is_repo = true;
    // git answers with symlinks resolved (/var → /private/var on macOS), so every
    // path compared against the top is canonicalized too.
    let top = git(cwd, &["rev-parse", "--show-toplevel"])
        .map(|s| PathBuf::from(s.trim()))
        .unwrap_or_else(|| cwd.to_path_buf());
    let top = top.canonicalize().unwrap_or(top);
    let started = log.started_ms.map(|ms| (ms / 1000) as i64);

    // The start commit: codex records it. claude doesn't, so take the last commit
    // made before the session's first line. A recorded commit that a rewrite
    // removed is treated as unknown.
    snap.start_commit = log
        .start_commit
        .clone()
        .filter(|c| git(cwd, &["cat-file", "-e", &format!("{c}^{{commit}}")]).is_some())
        .or_else(|| {
            let before = format!("--before={}", git_date(started?));
            git(cwd, &["rev-list", "-1", &before, "HEAD"]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
        });

    let limit = format!("-n{}", MAX_COMMITS + 1);
    let log_out = match (&snap.start_commit, started) {
        (Some(c), _) => git(cwd, &["log", "--format=%at%x09%h%x09%s", &limit, &format!("{c}..HEAD")]),
        (None, Some(s)) => git(cwd, &["log", "--format=%at%x09%h%x09%s", &limit, &format!("--since={}", git_date(s))]),
        (None, None) => None,
    };
    for line in log_out.unwrap_or_default().lines() {
        let mut f = line.splitn(3, '\t');
        let (Some(at), Some(short), Some(subject)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        snap.commits.push(CommitRow { at_secs: at.parse().unwrap_or(0), short: short.into(), subject: subject.into() });
    }
    if snap.commits.len() > MAX_COMMITS {
        snap.commits.truncate(MAX_COMMITS);
        snap.more_commits = true;
    }

    // `-z`: paths verbatim (no quoting); a rename is "XY new\0old\0".
    let status = git(cwd, &["status", "--porcelain=v1", "-z"]).unwrap_or_default();
    let mut recs = status.split('\0').filter(|r| !r.is_empty());
    while let Some(rec) = recs.next() {
        if rec.len() < 4 {
            continue;
        }
        let (code, path) = (&rec[..2], &rec[3..]);
        if code.contains('R') || code.contains('C') {
            recs.next(); // the old path
        }
        if snap.dirty.len() < MAX_DIRTY {
            snap.dirty.push(DirtyRow { status: code.to_string(), path: path.to_string(), mtime_secs: mtime_secs(&top.join(path)) });
        }
    }
    snap.diff_stat = git(cwd, &["diff", "--stat", "HEAD"]).unwrap_or_default().trim_end().to_string();

    // git can't see an ignored file, but the final turn may have been editing one.
    if let Some(turn) = log.last_working_turn() {
        let mut seen: Vec<String> = Vec::new();
        for e in turn.events.iter().chain(turn.late.iter()) {
            let EventKind::Edit { paths } = &e.kind else { continue };
            for p in paths {
                let abs = if Path::new(p).is_absolute() { PathBuf::from(p) } else { cwd.join(p) };
                let Ok(abs) = abs.canonicalize() else { continue }; // gone since
                let Ok(rel) = abs.strip_prefix(&top) else { continue }; // outside the repo: scratch
                let rel = rel.display().to_string();
                if seen.contains(&rel) {
                    continue;
                }
                seen.push(rel.clone());
                if git(&top, &["check-ignore", "-q", "--", &rel]).is_some() {
                    snap.ignored_edits.push(DirtyRow { status: String::new(), path: rel, mtime_secs: mtime_secs(&abs) });
                }
            }
        }
    }
    snap
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff::log::{Event, Turn};
    use crate::session::CliKind;

    /// A throwaway repo under the temp dir (store test convention: never a real path).
    fn repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orch-handoff-{tag}-{}-{}", std::process::id(), crate::events::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str], date: &str| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        run(&["init", "-q", "-b", "main"], "2026-09-30T10:00:00Z");
        std::fs::write(dir.join(".gitignore"), "QUEUE.md\n").unwrap();
        std::fs::write(dir.join("a.txt"), "1").unwrap();
        run(&["add", "."], "2026-09-30T10:00:00Z");
        run(&["commit", "-qm", "before the session"], "2026-09-30T10:00:00Z");
        std::fs::write(dir.join("a.txt"), "2").unwrap();
        run(&["commit", "-qam", "during the session"], "2026-09-30T12:00:00Z");
        std::fs::write(dir.join("a.txt"), "3").unwrap(); // dirty
        std::fs::write(dir.join("QUEUE.md"), "next").unwrap(); // ignored
        dir
    }

    fn log_for(dir: &Path) -> SessionLog {
        let mut log = SessionLog::empty(CliKind::Claude);
        log.started_ms = Some(1_790_766_000_000); // 2026-09-30T11:00:00Z — no start commit recorded
        let mut t = Turn::new(1, log.started_ms);
        t.events.push(Event { at_ms: 1, kind: EventKind::Agent { text: "editing".into(), phase: None } });
        t.events.push(Event { at_ms: 2, kind: EventKind::Edit { paths: vec![dir.join("QUEUE.md").display().to_string(), "/tmp/scratch.js".into()] } });
        log.turns.push(t);
        log
    }

    #[test]
    fn commits_dirty_files_and_ignored_edits_are_all_seen() {
        let dir = repo("snap");
        let snap = snapshot(&dir, &log_for(&dir));
        assert!(snap.is_repo);
        // claude recorded no start commit: the last commit before 11:00 stands in.
        let first = git(&dir, &["rev-list", "--max-parents=0", "HEAD"]).unwrap();
        assert_eq!(snap.start_commit.as_deref(), Some(first.trim()));
        assert_eq!(snap.commits.len(), 1);
        assert_eq!(snap.commits[0].subject, "during the session");
        assert_eq!(snap.commits[0].at_secs, 1_790_769_600); // 12:00Z
        assert_eq!(snap.dirty.len(), 1);
        assert_eq!((snap.dirty[0].status.as_str(), snap.dirty[0].path.as_str()), (" M", "a.txt"));
        assert!(snap.dirty[0].mtime_secs.is_some());
        assert!(snap.diff_stat.contains("a.txt"));
        // QUEUE.md is ignored, so only the edit record shows it; /tmp is outside the repo.
        assert_eq!(snap.ignored_edits.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(), ["QUEUE.md"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plain_directory_is_not_a_repo() {
        let dir = std::env::temp_dir().join(format!("orch-handoff-norepo-{}-{}", std::process::id(), crate::events::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let snap = snapshot(&dir, &SessionLog::empty(CliKind::Codex));
        assert!(!snap.is_repo);
        assert!(snap.commits.is_empty() && snap.dirty.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
