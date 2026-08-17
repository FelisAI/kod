use super::*;

/// On-demand one-line summary of a recoverable session (the user's ask).
/// Blocking + uses plan quota — run OFF the UI thread. Reads a head+tail digest
/// of the transcript (first user goal + last assistant message + turn count) and
/// asks `claude -p` for a single sentence. Returns the sentence or an error
/// string. Spends NO quota when the transcript yields no goal/outcome.
pub fn summarize_session(path: &Path, is_codex: bool) -> Result<String, String> {
    let digest = build_digest(path, is_codex).ok_or("transcript is empty or unreadable")?;
    // temp-dir cwd: even a future claude that ignored --no-session-persistence
    // would land outside the recoverable scan's ~/local filter.
    let stdout = run_claude_p(&summary_prompt(&digest), &std::env::temp_dir(), 90)?;
    let text = extract_result_text(&stdout).ok_or("no result text in claude output")?;
    let line = text.trim().lines().next().unwrap_or("").trim().to_string();
    if line.is_empty() {
        return Err("empty summary".into());
    }
    Ok(line)
}

/// The Standup summary of one session (task #16): what happened, and what the
/// human should do next — the first durable memory record. Blocking + spends
/// plan quota; run ONLY from the summarizer worker (budgeted, opt-in).
pub struct SessSummary {
    pub goal: String,
    pub headline: String,
    pub next_action: String,
    pub detail: Vec<String>,
}

pub fn standup_summarize(
    path: &Path,
    is_codex: bool,
    prev_headline: Option<&str>,
) -> Result<SessSummary, String> {
    // `transcript:` is a NOT-READY stage, not a defect — a codex rollout that
    // has no user/agent message yet is a job to DEFER, not an attempt to spend
    // (the worker classifies on this prefix). Everything below is a real
    // generation attempt.
    let (digest, goal) =
        build_standup_digest(path, is_codex).ok_or("transcript: empty or unreadable")?;
    let stdout = run_claude_p(
        &standup_prompt(&digest, prev_headline),
        &std::env::temp_dir(),
        90,
    )?;
    let text = extract_result_text(&stdout).ok_or("parse: no result text in CLI output")?;
    // keep the model's ACTUAL words on a parse failure — a prose reply used to
    // leave a static string behind, so "the model didn't answer in JSON" and
    // "the model said it was rate limited" were indistinguishable in the store.
    let json = first_json_object(&text)
        .ok_or_else(|| format!("parse: no JSON in summary output: {}", tail(&text, 300)))?;
    parse_standup_json(&json, goal)
}

fn standup_prompt(digest: &str, prev_headline: Option<&str>) -> String {
    // delta framing (docs/012 §4): the timeline reads headlines as "what was
    // achieved since I last looked", so the model gets the previous status and
    // is told to report the DELTA — outcomes, never a play-by-play.
    let prev = match prev_headline.filter(|p| !p.is_empty()) {
        Some(p) => format!("PREVIOUS STATUS (already on the board): \"{p}\"\n"),
        None => String::new(),
    };
    format!(
        "You are summarizing a coding-assistant session for a status board a solo user \
glances at. Below: the session GOAL and the most recent exchange. Output ONLY minified JSON \
(no prose, no markdown fence), shape: \
{{\"headline\":\"...\",\"next_action\":\"...\",\"detail\":[\"...\",\"...\"]}}\n\
RULES: headline = ONE line (max ~80 chars) stating what was ACCOMPLISHED — outcomes (features \
landed, bugs fixed, decisions made, results learned), with exact names/numbers; never narrate \
activity (\"working on X\", \"exploring Y\"). {prev}If a previous status is given, report only \
what changed SINCE it; if nothing meaningful changed, say so in five words. next_action = what \
the HUMAN should do next (max ~90 chars) ONLY if something genuinely awaits them (a question, \
a result to review, a decision); otherwise EXACTLY the empty string \"\". detail = 2-3 short \
factual bullets. Never invent facts not in the digest.\n\nSESSION DIGEST:\n{digest}"
    )
}

fn parse_standup_json(json: &str, goal: String) -> Result<SessSummary, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("parse: bad summary JSON: {e}"))?;
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let headline = s("headline");
    if headline.is_empty() {
        return Err("parse: summary missing headline".into());
    }
    let detail = v
        .get("detail")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|t| t.trim().to_string()))
                .filter(|t| !t.is_empty())
                .take(3)
                .collect()
        })
        .unwrap_or_default();
    Ok(SessSummary {
        goal,
        headline: headline.chars().take(120).collect(),
        next_action: s("next_action").chars().take(120).collect(),
        detail,
    })
}

/// GOAL + the last few substantive user/assistant messages (NOT a byte-tail:
/// one tool_result line can exceed 10k chars and post-turn metadata would
/// drown the text — design critique #16). Returns (digest, goal).
fn build_standup_digest(path: &Path, is_codex: bool) -> Option<(String, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let (goal, _outcome, turns) = if is_codex {
        codex_digest(&text)
    } else {
        claude_digest(&text)
    };
    let recent = last_messages(&text, is_codex, 6);
    if goal.is_none() && recent.is_empty() {
        return None;
    }
    let goal = goal.unwrap_or_default();
    let mut d = String::new();
    if !goal.is_empty() {
        d.push_str("GOAL (first user request):\n");
        d.push_str(&goal.chars().take(1200).collect::<String>());
        d.push_str("\n\n");
    }
    d.push_str(&format!(
        "TOTAL TURNS: {turns}\n\nMOST RECENT EXCHANGE (oldest→newest):\n"
    ));
    for (role, t) in &recent {
        d.push_str(&format!(
            "[{role}] {}\n\n",
            t.chars().take(900).collect::<String>()
        ));
    }
    Some((
        d.chars().take(10_000).collect(),
        goal.chars().take(300).collect(),
    ))
}

/// The last `n` substantive user/assistant texts, oldest→newest.
fn last_messages(text: &str, is_codex: bool, n: usize) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = Vec::new();
    for line in text.lines().rev() {
        if out.len() >= n {
            break;
        }
        let got: Option<(&'static str, String)> = if is_codex {
            if line.contains("\"user_message\"") || line.contains("\"agent_message\"") {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|v| {
                        let p = v.get("payload").cloned().unwrap_or(v);
                        let ty = p.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        let msg = p
                            .get("message")
                            .and_then(|m| m.as_str())
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty());
                        match (ty, msg) {
                            ("user_message", Some(m)) if !is_goal_noise(&m) => Some(("user", m)),
                            ("agent_message", Some(m)) => Some(("assistant", m)),
                            _ => None,
                        }
                    })
            } else {
                None
            }
        } else if line.contains("\"type\":\"user\"") && !line.contains("\"isMeta\":true") {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| claude_user_text(&v))
                .filter(|t| !is_goal_noise(t))
                .map(|t| ("user", t))
        } else if line.contains("\"type\":\"assistant\"") {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| claude_assistant_text(&v))
                .map(|t| ("assistant", t))
        } else {
            None
        };
        if let Some(x) = got {
            out.push(x);
        }
    }
    out.reverse();
    out
}

fn summary_prompt(digest: &str) -> String {
    format!(
        "Below is a digest of a coding-assistant session: the user's first request (the GOAL), the \
assistant's final message (the OUTCOME), and the turn count. Write ONE plain sentence (max ~22 words) \
describing WHAT this session worked on and WHERE it ended up (e.g. shipped / mid-debug / blocked / \
abandoned). Output ONLY that sentence — no preamble, no quotes, no markdown.\n\nSESSION DIGEST:\n{digest}"
    )
}

/// head (first real user goal) + tail (last assistant message) + turn count.
/// Reads the WHOLE transcript into memory (can be tens of MB — fine on the
/// background summary thread), then caps the DIGEST we send the LLM to ~5k chars
/// so we never ship a 30MB transcript to `claude -p`.
fn build_digest(path: &Path, is_codex: bool) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let (goal, outcome, turns) = if is_codex {
        codex_digest(&text)
    } else {
        claude_digest(&text)
    };
    if goal.is_none() && outcome.is_none() {
        return None;
    }
    let mut d = String::new();
    if let Some(g) = goal {
        d.push_str("GOAL (first user request):\n");
        d.push_str(&g.chars().take(1500).collect::<String>());
        d.push_str("\n\n");
    }
    if let Some(o) = outcome {
        d.push_str("LAST UPDATE (final assistant message):\n");
        d.push_str(&o.chars().take(2000).collect::<String>());
        d.push_str("\n\n");
    }
    d.push_str(&format!("TOTAL TURNS: {turns}"));
    Some(d.chars().take(5000).collect())
}

/// Slash-command / caveat / system-injection noise that masquerades as a user
/// prompt — must not become the digest GOAL (applies to BOTH clis).
fn is_goal_noise(t: &str) -> bool {
    let head: String = t.chars().take(60).collect();
    t.starts_with("<command-name>")
        || t.starts_with("<local-command")
        || t.starts_with("<command-message>")
        || t.starts_with("You are ") // injected agent/system preamble (codex)
        || head.contains("Caveat:")
}

fn claude_digest(text: &str) -> (Option<String>, Option<String>, u32) {
    let mut goal = None;
    let mut turns = 0u32;
    for line in text.lines() {
        if !line.contains("\"type\":\"user\"") || line.contains("\"isMeta\":true") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(t) = claude_user_text(&v) else {
            continue;
        };
        if is_goal_noise(&t) {
            continue;
        }
        turns += 1;
        if goal.is_none() {
            goal = Some(t);
        }
    }
    let outcome = last_match(
        text,
        &["\"type\":\"assistant\"", "\"role\":\"assistant\""],
        |v| claude_assistant_text(v),
    );
    (goal, outcome, turns)
}

fn codex_digest(text: &str) -> (Option<String>, Option<String>, u32) {
    let mut goal = None;
    let mut turns = 0u32;
    for line in text.lines() {
        if !line.contains("\"user_message\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let p = v.get("payload").unwrap_or(&v);
        if p.get("type").and_then(|t| t.as_str()) != Some("user_message") {
            continue;
        }
        let Some(t) = p
            .get("message")
            .and_then(|m| m.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        // same noise filter as claude (slash-commands, caveats, injected preamble).
        if is_goal_noise(&t) {
            continue;
        }
        turns += 1;
        if goal.is_none() {
            goal = Some(t);
        }
    }
    let outcome = last_match(text, &["\"agent_message\""], |v| {
        let p = v.get("payload").unwrap_or(v);
        (p.get("type").and_then(|t| t.as_str()) == Some("agent_message"))
            .then(|| {
                p.get("message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.trim().to_string())
            })
            .flatten()
            .filter(|s| !s.is_empty())
    });
    (goal, outcome, turns)
}

/// reverse-scan for the last line matching any `needles`, returning the first
/// `extract` hit (the final assistant/agent message).
fn last_match(
    text: &str,
    needles: &[&str],
    extract: impl Fn(&serde_json::Value) -> Option<String>,
) -> Option<String> {
    for line in text.lines().rev() {
        if needles.iter().any(|n| line.contains(n)) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(t) = extract(&v) {
                    return Some(t);
                }
            }
        }
    }
    None
}

fn claude_user_text(v: &serde_json::Value) -> Option<String> {
    let content = v.get("message")?.get("content")?;
    if let Some(s) = content.as_str() {
        return non_empty_trim(s);
    }
    if let Some(arr) = content.as_array() {
        if arr
            .iter()
            .any(|p| p.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
        {
            return None; // a tool_result fed back as a user turn — not a human prompt
        }
        for p in arr {
            if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(s) = p
                    .get("text")
                    .and_then(|t| t.as_str())
                    .and_then(non_empty_trim)
                {
                    return Some(s);
                }
            }
        }
    }
    None
}

fn claude_assistant_text(v: &serde_json::Value) -> Option<String> {
    let content = v.get("message").unwrap_or(v).get("content")?;
    if let Some(s) = content.as_str() {
        return non_empty_trim(s);
    }
    content.as_array()?.iter().find_map(|p| {
        (p.get("type").and_then(|t| t.as_str()) == Some("text"))
            .then(|| {
                p.get("text")
                    .and_then(|t| t.as_str())
                    .and_then(non_empty_trim)
            })
            .flatten()
    })
}

fn non_empty_trim(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn standup_json_parses_and_caps() {
        let s = parse_standup_json(
            r#"{"headline":"Fixed the resume double-spawn guard","next_action":"review the diff","detail":["added in-flight set","134 tests green"]}"#,
            "fix resume".into(),
        )
        .unwrap();
        assert_eq!(s.headline, "Fixed the resume double-spawn guard");
        assert_eq!(s.next_action, "review the diff");
        assert_eq!(s.detail.len(), 2);
        assert_eq!(s.goal, "fix resume");
        // empty next_action stays empty (the "nothing awaits you" contract)
        let s2 = parse_standup_json(
            r#"{"headline":"h","next_action":"","detail":[]}"#,
            String::new(),
        )
        .unwrap();
        assert!(s2.next_action.is_empty());
        // missing headline is an error, not a fabricated row
        assert!(parse_standup_json(r#"{"next_action":"x"}"#, String::new()).is_err());
    }

    #[test]
    fn last_messages_extracts_substantive_tail() {
        let t = concat!(
            r#"{"type":"user","message":{"content":"do the thing"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working on it"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"BIGBLOB"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done, 3 tests green"}]}}"#,
            "\n",
        );
        let msgs = last_messages(t, false, 6);
        // tool_result line is NOT a message; order is oldest→newest
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0], ("user", "do the thing".to_string()));
        assert_eq!(msgs[2].1, "done, 3 tests green");
        // codex shape
        let c = concat!(
            r#"{"payload":{"type":"user_message","message":"fix the parser"}}"#,
            "\n",
            r#"{"payload":{"type":"agent_message","message":"parser fixed"}}"#,
            "\n",
        );
        let m = last_messages(c, true, 6);
        assert_eq!(m.len(), 2);
        assert_eq!(m[1], ("assistant", "parser fixed".to_string()));
    }

    // LIVE: summarize the smallest real recoverable session (uses quota).
    //   cargo test -p orchestrator-gui --bin orchestrator -- --ignored summarize_live
    #[test]
    #[ignore]
    fn summarize_live() {
        let mut sessions = orchestrator_core::scan::recoverable_sessions(7, 40);
        sessions.sort_by_key(|s| s.bytes);
        let pick = sessions
            .iter()
            .find(|s| s.turns >= 12 && s.turns <= 60 && !s.is_codex)
            .expect("a real work session");
        println!(
            "summarizing {} ({} msgs, {} bytes): {}",
            pick.id,
            pick.turns,
            pick.bytes,
            pick.path.display()
        );
        let s = summarize_session(&pick.path, pick.is_codex).expect("summary");
        println!("SUMMARY → {s}");
        assert!(!s.is_empty());
        assert!(s.len() < 400, "should be one sentence");
    }
}

#[cfg(test)]
mod standup_live_tests {
    use super::*;

    // LIVE: one real standup summary (uses quota, sonnet):
    //   cargo test -p orchestrator-gui --bin orchestrator -- --ignored standup_live
    #[test]
    #[ignore]
    fn standup_live() {
        let mut sessions = orchestrator_core::scan::recoverable_sessions(7, 40);
        sessions.sort_by_key(|s| s.bytes);
        let pick = sessions
            .iter()
            .find(|s| s.turns >= 12 && s.turns <= 80 && !s.is_codex)
            .expect("a real work session");
        println!(
            "standup-summarizing {} ({} msgs): {}",
            pick.id,
            pick.turns,
            pick.path.display()
        );
        let s = standup_summarize(&pick.path, pick.is_codex, None).expect("summary");
        println!("HEADLINE  → {}", s.headline);
        println!("NEXT      → {}", s.next_action);
        println!("DETAIL    → {:?}", s.detail);
        assert!(!s.headline.is_empty());
    }
}
