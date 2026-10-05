//! Session handoff (docs/028): continue a session's work in a NEW session under
//! another profile or CLI. The receiver gets a packet — the whole conversation
//! word for word, where it stopped, and the repo state attributed by time — and
//! is told to check it against the repo before acting.
//!
//! Deterministic and local: no model call, no network. The conversation is tiny
//! next to the transcript (55 KB of a real 144 MB codex rollout, 74% of which is
//! tool output), so it crosses whole; a summary could only lose things. Nor does
//! a packet depend on codex's encrypted reasoning and compaction items, which
//! another account may not accept and claude can't read at all.

mod claude;
mod codex;
mod log;
mod render;
mod repo;

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use log::{Event, EventKind, PlanItem, SessionLog, Turn};
pub use repo::{CommitRow, DirtyRow, RepoSnapshot};

use crate::session::CliKind;

/// What to hand off, and to whom.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PacketRequest {
    pub source_kind: CliKind,
    /// The source's transcript; `None` when none was found, and the packet then
    /// carries the repo state alone.
    pub transcript: Option<PathBuf>,
    /// The source session's working directory; the target starts here too.
    pub cwd: PathBuf,
    /// How the receiver and the user name the source account: "codex2", "default account".
    pub source_label: String,
    pub target_kind: CliKind,
    /// The receiver restates the state and waits for a go-ahead (docs/028 §5).
    pub confirm_first: bool,
    /// The packet the source itself began from, when it was a handoff target.
    pub earlier_packet: Option<PathBuf>,
}

/// A written packet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Packet {
    pub dir: PathBuf,
    /// The receiver's first message (also in `prompt.md`).
    pub prompt: String,
    pub turns: usize,
    pub user_messages: usize,
    pub commands: usize,
}

/// Parse a transcript into the CLI-neutral log.
pub fn read_log(kind: CliKind, path: &Path) -> io::Result<SessionLog> {
    match kind {
        CliKind::Codex => codex::parse_rollout(path),
        CliKind::Claude => claude::parse_transcript(path),
        CliKind::Shell => Err(io::Error::new(io::ErrorKind::InvalidInput, "a shell has no transcript to hand off")),
    }
}

/// A packet directory's name: sortable by time, and naming its source.
pub fn packet_dir_name(source_kind: CliKind, session_id: &str, now_ms: u64) -> String {
    let stamp = jiff::Timestamp::from_millisecond(now_ms as i64)
        .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y%m%d-%H%M%S").to_string())
        .unwrap_or_else(|_| now_ms.to_string());
    let short: String = session_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    format!("{stamp}-{}-{short}", source_kind.label())
}

/// Build the packet into `dir`, creating it. Blocking — it reads the whole
/// transcript and runs git — so call it off the UI thread.
pub fn write_packet(req: &PacketRequest, dir: &Path) -> io::Result<Packet> {
    let log = match &req.transcript {
        Some(p) => read_log(req.source_kind, p)?,
        None => SessionLog::empty(req.source_kind),
    };
    write_packet_from(req, &log, dir, jiff::tz::TimeZone::system(), crate::events::now_ms())
}

fn write_packet_from(req: &PacketRequest, log: &SessionLog, dir: &Path, tz: jiff::tz::TimeZone, now_ms: u64) -> io::Result<Packet> {
    let repo = repo::snapshot(&req.cwd, log);
    std::fs::create_dir_all(dir)?;
    let ctx = render::Ctx { log, repo: &repo, req, dir, tz, now_ms, parity: instruction_note(&req.cwd, req.source_kind, req.target_kind) };
    std::fs::write(dir.join("handoff.md"), render::handoff_md(&ctx))?;
    std::fs::write(dir.join("commands.md"), render::commands_md(&ctx))?;
    let prompt = render::prompt(&ctx);
    std::fs::write(dir.join("prompt.md"), &prompt)?;
    Ok(Packet { dir: dir.to_path_buf(), prompt, turns: log.turns.len(), user_messages: log.user_messages(), commands: log.commands() })
}

/// The receiver loads its own CLI's instruction file, not the source's: claude
/// reads CLAUDE.md, codex reads AGENTS.md. One line when the repo's instructions
/// would be lost in the crossing (docs/028 §5).
fn instruction_note(cwd: &Path, source: CliKind, target: CliKind) -> Option<String> {
    if source == target {
        return None;
    }
    let read = |name: &str| std::fs::read_to_string(cwd.join(name)).ok();
    match target {
        CliKind::Claude => {
            read("AGENTS.md")?;
            let imported = read("CLAUDE.md").is_some_and(|c| c.contains("@AGENTS.md"));
            (!imported).then(|| {
                "The previous agent followed this repo's AGENTS.md, which your CLAUDE.md doesn't import. Read AGENTS.md before you start.".to_string()
            })
        }
        CliKind::Codex => {
            let claude_md = read("CLAUDE.md")?;
            // A CLAUDE.md that imports AGENTS.md keeps its instructions there,
            // where codex already looks.
            (!claude_md.contains("@AGENTS.md")).then(|| {
                "The previous agent followed this repo's CLAUDE.md, which codex doesn't load. Read CLAUDE.md before you start.".to_string()
            })
        }
        CliKind::Shell => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path(rel: &str) -> PathBuf {
        PathBuf::from(format!("{}/../../fixtures/{rel}", env!("CARGO_MANIFEST_DIR")))
    }

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("orch-handoff-{tag}-{}-{}", std::process::id(), crate::events::now_ms()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn req(cwd: &Path, target: CliKind, confirm_first: bool) -> PacketRequest {
        PacketRequest {
            source_kind: CliKind::Codex,
            transcript: Some(fixture_path("codex/0.155.1/rollout/handoff_limit.jsonl")),
            cwd: cwd.to_path_buf(),
            source_label: "codex2".to_string(),
            target_kind: target,
            confirm_first,
            earlier_packet: None,
        }
    }

    // The fixture is shaped on the live session docs/028 measured: a turn cut off
    // by an out-of-credits workspace, a selftest that finished after the cutoff,
    // and a hand-typed `continue` that failed at once.
    #[test]
    fn the_packet_anchors_on_the_last_working_turn_and_flags_late_results() {
        let cwd = temp("cwd");
        let out = temp("pkt");
        let r = req(&cwd, CliKind::Claude, true);
        let log = read_log(CliKind::Codex, r.transcript.as_ref().unwrap()).unwrap();
        let p = write_packet_from(&r, &log, &out, jiff::tz::TimeZone::UTC, 1_791_300_000_000).unwrap();
        assert_eq!((p.turns, p.user_messages, p.commands), (2, 2, 2));
        let md = std::fs::read_to_string(out.join("handoff.md")).unwrap();
        // Stopped at the end of the turn that worked, not at the failed retry.
        assert!(md.contains("- Stopped: 2026-10-01 00:45 — Your workspace is out of credits. Add credits to continue. (usage_limit_exceeded)"), "{md}");
        assert!(md.contains("- Since then: 1 more prompt (last 2026-10-05 17:17), each failed at once"), "{md}");
        assert!(md.contains("It started from this message:\n\n> Make the loading message say where it's loading from"), "{md}");
        assert!(md.contains("**Finished after the agent stopped — it never saw these results:**"), "{md}");
        assert!(md.contains("`python3 tools/selftest.py > /tmp/garden-after.log 2>&1`"), "{md}");
        assert!(md.contains("(its log `/tmp/garden-after.log` no longer exists"), "{md}");
        assert!(md.contains("Questions it asked in that turn"), "{md}");
        assert!(md.contains("After that, turn 2 (2026-10-05 17:17) was `continue` and failed at once"), "{md}");
        // Not a repo: said so, not guessed.
        assert!(md.contains("is not a git repository"), "{md}");
        // Every message verbatim, activity shrunk between them.
        assert!(md.contains("**User:** Make the loading message say where it's loading from"));
        assert!(md.contains("**Agent (commentary):** Checks pass. Installing on your iPhone and running the final checks."));
        assert!(md.contains("_… 1 command · edited App/ContentView.swift"), "{md}");
        let cmds = std::fs::read_to_string(out.join("commands.md")).unwrap();
        assert!(cmds.contains("swift test 2>&1 | tail -40") && cmds.contains("(after the agent stopped)"), "{cmds}");
        let prompt = std::fs::read_to_string(out.join("prompt.md")).unwrap();
        assert_eq!(prompt, p.prompt);
        assert!(prompt.contains(&format!("Read this first, in full: {}", out.join("handoff.md").display())));
        assert!(prompt.contains("on the account \"codex2\""), "{prompt}");
        assert!(prompt.contains("Then wait for my go-ahead."));
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn the_receiver_is_told_about_instructions_it_wont_load() {
        let cwd = temp("parity");
        std::fs::write(cwd.join("AGENTS.md"), "# rules").unwrap();
        assert!(instruction_note(&cwd, CliKind::Codex, CliKind::Claude).unwrap().contains("Read AGENTS.md"));
        assert_eq!(instruction_note(&cwd, CliKind::Codex, CliKind::Codex), None, "same CLI, same instructions");
        std::fs::write(cwd.join("CLAUDE.md"), "@AGENTS.md\n").unwrap();
        assert_eq!(instruction_note(&cwd, CliKind::Codex, CliKind::Claude), None, "CLAUDE.md imports AGENTS.md");
        assert_eq!(instruction_note(&cwd, CliKind::Claude, CliKind::Codex), None, "its instructions live in AGENTS.md");
        std::fs::write(cwd.join("CLAUDE.md"), "Always run the linter.\n").unwrap();
        assert!(instruction_note(&cwd, CliKind::Claude, CliKind::Codex).unwrap().contains("Read CLAUDE.md"));
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn without_confirmation_the_receiver_carries_on() {
        let cwd = temp("noconfirm");
        let out = temp("pkt2");
        let r = req(&cwd, CliKind::Codex, false);
        let p = write_packet(&r, &out).unwrap();
        assert!(p.prompt.contains("Then carry on with the work."));
        assert!(!p.prompt.contains("wait for my go-ahead"));
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&out);
    }

    // A session that never wrote a transcript still hands off: the packet says so
    // and carries the repo state (driven end to end in the dev sandbox, where a
    // fresh claude with no prompt is exactly this).
    #[test]
    fn no_transcript_still_makes_a_packet_that_says_so() {
        let cwd = temp("empty-cwd");
        let out = temp("empty-pkt");
        let r = PacketRequest { transcript: None, source_kind: CliKind::Claude, ..req(&cwd, CliKind::Codex, true) };
        let p = write_packet(&r, &out).unwrap();
        assert_eq!((p.turns, p.user_messages, p.commands), (0, 0, 0));
        let md = std::fs::read_to_string(out.join("handoff.md")).unwrap();
        assert!(md.contains("- Transcript: none found, so this packet has the repo state only"), "{md}");
        assert!(md.contains("Nowhere yet: no turn got as far"), "{md}");
        assert!(!md.contains(" on ?"), "an unknown start time is left out, not shown as ?: {md}");
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn packet_dirs_sort_by_time_and_name_their_source() {
        let name = packet_dir_name(CliKind::Codex, "01a0f38a-7599-7013", 1_791_300_000_000);
        assert!(name.ends_with("-codex-01a0f38a"), "{name}");
        assert_eq!(name.len(), "20261006-123456-codex-01a0f38a".len());
    }
}
