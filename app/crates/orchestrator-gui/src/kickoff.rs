//! kickoff — composes the prompt a dispatched session starts with (docs/011
//! §C). PURE: no I/O, no store access — the caller passes the project's part
//! tree, the node's log, and pre-shaped prior-session summaries.
//!
//! Budget: MAX_KICKOFF_CHARS. When over, later tiers drop WHOLE ENTRIES first
//! (anchors → notes/context → prior summaries → sub-parts → decisions); an
//! entry is never truncated mid-quote and never paraphrased — the decisions
//! are the user's own words. Header and scope footer always survive.

use orchestrator_store::store::NoteRow;
use orchestrator_store::{Part, PartId};

pub const MAX_KICKOFF_CHARS: usize = 8_000;

/// The stable tail of the footer — never dropped, always the last line
/// (docs/019 T11: scope is HOME BASE, not a fence). The session steers its own
/// chip: declared drift is followed, not forbidden. Tests anchor on this
/// substring; `compose` prefixes it with the dynamic "dispatched from <path>"
/// line so the rendered output still ends with FOOTER.
const FOOTER: &str = "If the work leads elsewhere, follow it and say so with `map here <node>`; the map will follow you. Claim a shipped leaf with `map done <part> — <one-line shipped claim>`, and record a decision with `map note <part> :: <text>`.";

/// Header addendum for idea projects (has_repo=false) — the session must know
/// it is thinking, not building.
const NO_REPO_LINE: &str = "This project has no repository yet — this is a thinking/spec session; produce specs, plans, and decisions rather than code.";

/// One droppable tier: heading + whole-entry bullets. When the last entry
/// drops, the heading goes with it.
struct Section {
    heading: &'static str,
    entries: Vec<String>,
}

#[cfg(test)]
pub fn compose(
    project_name: &str,
    parts: &[Part],
    part_id: PartId,
    notes: &[NoteRow],
    summaries: &[(String, String)],
    linked_docs: &[(String, String)],
    has_repo: bool,
) -> String {
    compose_with_memory(
        project_name,
        parts,
        part_id,
        notes,
        summaries,
        linked_docs,
        "",
        has_repo,
    )
}

pub fn compose_with_memory(
    project_name: &str,
    parts: &[Part],
    part_id: PartId,
    notes: &[NoteRow],
    summaries: &[(String, String)],
    linked_docs: &[(String, String)],
    memory_context: &str,
    has_repo: bool,
) -> String {
    let Some(node) = parts.iter().find(|p| p.id == part_id) else {
        // Dispatch always starts from a live focused node; if the row vanished
        // mid-flight, keep the project frame + scope contract anyway.
        let mut out = format!("You are working on {project_name}.\n");
        if !has_repo {
            out.push_str(NO_REPO_LINE);
            out.push('\n');
        }
        out.push('\n');
        out.push_str(FOOTER);
        return out;
    };

    // the dynamic home-base line (docs/019 T11) precedes the stable FOOTER, so
    // the rendered output still ENDS WITH FOOTER (the tests' anchor).
    let path = ancestry_path(parts, node);
    let footer = format!("You are dispatched from {path} — home base, not a fence. {FOOTER}");

    // ---- tier 1 (always): header — name, tree path, detail_md body, lifecycle.
    let mut header = format!(
        "You are working on {} — part of {}.\n",
        node.name, project_name
    );
    header.push_str(&format!("Path: {path}\n"));
    // the FULL markdown body (docs/019: dispatch is context-rich, not 29 chars);
    // pre-migration rows have an empty detail_md and fall back to the one-liner.
    let body = if node.detail_md.trim().is_empty() {
        node.detail.as_str()
    } else {
        node.detail_md.as_str()
    };
    if !body.is_empty() {
        // the header always survives the budget loop, so an unbounded pasted
        // detail could breach MAX_KICKOFF_CHARS on its own — cap it (half the
        // budget) with an honest marker; sections are never truncated mid-entry.
        const DETAIL_CAP: usize = MAX_KICKOFF_CHARS / 2;
        if body.chars().count() > DETAIL_CAP {
            header.extend(body.chars().take(DETAIL_CAP));
            header.push_str("\n… [detail truncated for the kickoff — read the node]");
        } else {
            header.push_str(body);
        }
        header.push('\n');
    }
    header.push_str(&format!("Status: {}\n", node.lifecycle.as_str()));
    if !has_repo {
        header.push_str(NO_REPO_LINE);
        header.push('\n');
    }

    // ---- tier 2: decisions VERBATIM, newest first — the user's own words.
    let decisions = Section {
        heading: "Decisions already made (verbatim, newest first):",
        entries: kind_newest_first(notes, &["decision"])
            .iter()
            .map(|n| format!("- {}", n.text))
            .collect(),
    };
    // ---- tier 2b: linked-doc CONTENTS (docs/019 T11) — the markdown links in
    // detail_md, resolved + read by the caller. High-value context, so it sits
    // early (dropped only after the lower tiers). Each entry is one whole doc;
    // the caller caps per-doc size before passing them in.
    let docs = Section {
        heading: "Linked docs (contents):",
        entries: linked_docs
            .iter()
            .map(|(title, content)| format!("--- {title} ---\n{content}"))
            .collect(),
    };
    // ---- tier 2c: durable memory graph retrieval. This is provider-extracted,
    // evidence-verified memory, not the map tree itself.
    let memory = Section {
        heading: "Relevant durable memory:",
        entries: memory_entries(memory_context),
    };
    // ---- tier 3: children names + lifecycle.
    let mut kids: Vec<&Part> = parts
        .iter()
        .filter(|p| p.parent_id == Some(part_id))
        .collect();
    kids.sort_by(|a, b| {
        a.sort_order
            .partial_cmp(&b.sort_order)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let sub_parts = Section {
        heading: "Sub-parts of this node:",
        entries: kids
            .iter()
            .map(|k| format!("- {} ({})", k.name, k.lifecycle.as_str()))
            .collect(),
    };
    // ---- tier 4: linked prior session summaries (headline — detail).
    let prior = Section {
        heading: "Prior sessions on this node:",
        entries: summaries
            .iter()
            .map(|(h, d)| {
                if d.is_empty() {
                    format!("- {h}")
                } else {
                    format!("- {h} — {d}")
                }
            })
            .collect(),
    };
    // ---- tier 5: note/context entries verbatim (kind=session log lines are
    // the dispatch ledger, not briefing content — excluded).
    let extra = Section {
        heading: "Notes and context (verbatim, newest first):",
        entries: kind_newest_first(notes, &["note", "context"])
            .iter()
            .map(|n| format!("- [{}] {}", n.kind, n.text))
            .collect(),
    };
    // ---- tier 6: code anchors — only meaningful with a repository.
    let anchors = Section {
        heading: "Code anchors (paths/globs this node maps to):",
        entries: if has_repo {
            node.anchors.iter().map(|a| format!("- {a}")).collect()
        } else {
            Vec::new()
        },
    };

    let mut sections = [decisions, docs, memory, sub_parts, prior, extra, anchors];
    loop {
        let out = render(&header, &sections, &footer);
        if out.chars().count() <= MAX_KICKOFF_CHARS {
            return out;
        }
        // Drop WHOLE entries, later tiers first; within a tier the last entry
        // (the oldest — tiers 2/5 are newest first) goes first.
        match sections.iter_mut().rev().find(|s| !s.entries.is_empty()) {
            Some(s) => {
                s.entries.pop();
            }
            // Only header + footer left — those are emitted whole, always.
            None => return out,
        }
    }
}

fn memory_entries(memory_context: &str) -> Vec<String> {
    memory_context
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// "Path: <root> ▸ … ▸ <node>" — walk parent_id up to the root. Hop-capped so
/// a corrupt parent cycle degrades to a short path instead of hanging.
fn ancestry_path(parts: &[Part], node: &Part) -> String {
    let mut names = vec![node.name.clone()];
    let mut cur = node.parent_id;
    let mut hops = 0;
    while let Some(pid) = cur {
        hops += 1;
        if hops > parts.len() {
            break;
        }
        match parts.iter().find(|p| p.id == pid) {
            Some(p) => {
                names.push(p.name.clone());
                cur = p.parent_id;
            }
            None => break,
        }
    }
    names.reverse();
    names.join(" ▸ ")
}

/// The node's log entries of the given kinds, newest first — ties on ts_secs
/// break by id (AUTOINCREMENT ⇒ higher id is newer), same contract as
/// outlinepane::newest_first.
fn kind_newest_first<'a>(notes: &'a [NoteRow], kinds: &[&str]) -> Vec<&'a NoteRow> {
    let mut v: Vec<&NoteRow> = notes
        .iter()
        .filter(|n| kinds.contains(&n.kind.as_str()))
        .collect();
    v.sort_by(|a, b| b.ts_secs.cmp(&a.ts_secs).then(b.id.cmp(&a.id)));
    v
}

fn render(header: &str, sections: &[Section], footer: &str) -> String {
    let mut out = String::from(header);
    for s in sections {
        if s.entries.is_empty() {
            continue;
        }
        out.push('\n');
        out.push_str(s.heading);
        out.push('\n');
        out.push_str(&s.entries.join("\n"));
        out.push('\n');
    }
    out.push('\n');
    out.push_str(footer);
    out
}

#[cfg(test)]
mod tests {
    // Selective imports (crate-wide pattern; see mapview.rs's mod tests) even
    // though this file has no `use gpui::*` to leak a shadowing `#[test]`.
    use super::{compose, compose_with_memory, FOOTER, MAX_KICKOFF_CHARS, NO_REPO_LINE};
    use orchestrator_store::store::NoteRow;
    use orchestrator_store::{Lifecycle, Part};

    fn part(
        id: i64,
        parent: Option<i64>,
        name: &str,
        detail: &str,
        lc: Lifecycle,
        anchors: Vec<String>,
    ) -> Part {
        Part {
            id,
            parent_id: parent,
            name: name.into(),
            detail: detail.into(),
            lifecycle: lc,
            sort_order: id as f64,
            anchors,
            ..Part::default()
        }
    }

    fn note(id: i64, ts: u64, kind: &str, text: &str) -> NoteRow {
        NoteRow {
            id,
            part_id: 3,
            ts_secs: ts,
            kind: kind.into(),
            text: text.into(),
            source: "user".into(),
        }
    }

    /// A tree where node 3 is the dispatch target: Root ▸ Mid ▸ Leaf.
    fn tree(leaf_detail: &str, leaf_anchors: Vec<String>) -> Vec<Part> {
        vec![
            part(1, None, "Root", "", Lifecycle::Building, vec![]),
            part(2, Some(1), "Mid", "", Lifecycle::Building, vec![]),
            part(
                3,
                Some(2),
                "Leaf",
                leaf_detail,
                Lifecycle::Todo,
                leaf_anchors,
            ),
            part(4, Some(3), "Child A", "", Lifecycle::Done, vec![]),
            part(5, Some(3), "Child B", "", Lifecycle::Idea, vec![]),
        ]
    }

    #[test]
    fn oversized_detail_is_capped_so_the_budget_holds() {
        let big = "d".repeat(20_000);
        let parts = tree(&big, vec![]);
        let out = compose("alpha", &parts, 3, &[], &[], &[], true);
        assert!(out.chars().count() <= MAX_KICKOFF_CHARS);
        assert!(out.contains("[detail truncated for the kickoff"));
        assert!(out.contains(FOOTER));
    }

    #[test]
    fn header_path_detail_lifecycle_and_footer() {
        let parts = tree("the exact detail text, verbatim", vec![]);
        let out = compose("kod", &parts, 3, &[], &[], &[], true);
        assert!(out.starts_with("You are working on Leaf — part of kod.\n"));
        assert!(out.contains("Path: Root ▸ Mid ▸ Leaf"));
        assert!(out.contains("the exact detail text, verbatim"));
        assert!(out.contains("Status: todo"));
        assert!(out.ends_with(FOOTER));
        assert!(out.contains("- Child A (done)"));
        assert!(out.contains("- Child B (idea)"));
    }

    #[test]
    fn footer_is_home_base_not_a_fence() {
        let parts = tree("", vec![]);
        let out = compose("kod", &parts, 3, &[], &[], &[], true);
        // the old "Work ONLY within this node's scope" fence is gone.
        assert!(!out.contains("Work ONLY within"));
        assert!(out.contains("dispatched from Root ▸ Mid ▸ Leaf — home base"));
        assert!(
            out.contains("map here <node>"),
            "teaches the drift-declaring verb"
        );
        assert!(out.ends_with(FOOTER));
    }

    #[test]
    fn detail_md_body_wins_over_the_one_liner() {
        let mut parts = tree("first line only", vec![]);
        // node 3 gets a full markdown body; the derived `detail` is its 1st line.
        if let Some(p) = parts.iter_mut().find(|p| p.id == 3) {
            p.detail = "first line only".into();
            p.detail_md =
                "first line only\n\n## How it works\nthe full body ships in the kickoff".into();
        }
        let out = compose("kod", &parts, 3, &[], &[], &[], true);
        assert!(
            out.contains("the full body ships in the kickoff"),
            "detail_md body, not just the one-liner"
        );
    }

    #[test]
    fn linked_docs_contents_ride_the_kickoff() {
        let parts = tree("", vec![]);
        let docs = vec![(
            "docs/011".to_string(),
            "the canvas substrate lives here".to_string(),
        )];
        let out = compose("kod", &parts, 3, &[], &[], &docs, true);
        assert!(out.contains("Linked docs (contents):"));
        assert!(out.contains("--- docs/011 ---"));
        assert!(out.contains("the canvas substrate lives here"));
    }

    #[test]
    fn retrieved_memory_rides_the_kickoff() {
        let parts = tree("work on memory", vec![]);
        let memory =
            "- **Map is projection over memory graph**: The Map represents durable memory.";
        let out = compose_with_memory("kod", &parts, 3, &[], &[], &[], memory, true);

        assert!(out.contains("Relevant durable memory:"));
        assert!(out.contains("Map is projection over memory graph"));
        assert!(out.contains("The Map represents durable memory."));
    }

    #[test]
    fn decisions_verbatim_newest_first() {
        let parts = tree("", vec![]);
        let newest =
            "Use **sqlite**, not `postgres`.\n  - because: local-first\n  - trailing  spaces  kept";
        let oldest = "ship v1 claude-only";
        let notes = vec![
            note(1, 100, "decision", oldest),
            note(2, 200, "decision", newest),
        ];
        let out = compose("kod", &parts, 3, &notes, &[], &[], true);
        // exact user text survives, markdown and internal newlines included.
        assert!(out.contains(newest));
        assert!(out.contains(oldest));
        let (i_new, i_old) = (out.find(newest).unwrap(), out.find(oldest).unwrap());
        assert!(i_new < i_old, "newest decision must render first");
        assert!(out.contains("Decisions already made (verbatim, newest first):"));
    }

    #[test]
    fn summaries_and_notes_render_when_budget_allows() {
        let parts = tree("", vec!["src/kickoff.rs".into()]);
        let notes = vec![
            note(1, 10, "note", "a plain note"),
            note(2, 20, "context", "some context"),
        ];
        let summaries = vec![
            (
                "wired dispatch".to_string(),
                "spawn + link at birth".to_string(),
            ),
            ("headline only".to_string(), String::new()),
        ];
        let out = compose("kod", &parts, 3, &notes, &summaries, &[], true);
        assert!(out.contains("- wired dispatch — spawn + link at birth"));
        assert!(out.contains("- headline only\n"));
        assert!(out.contains("- [note] a plain note"));
        assert!(out.contains("- [context] some context"));
        assert!(out.contains("- src/kickoff.rs"));
        assert!(out.chars().count() <= MAX_KICKOFF_CHARS);
    }

    #[test]
    fn session_log_lines_are_not_briefing_content() {
        let parts = tree("", vec![]);
        let notes = vec![note(1, 10, "session", "▶ session started abc123")];
        let out = compose("kod", &parts, 3, &notes, &[], &[], true);
        assert!(!out.contains("session started"));
    }

    // ---- budget: drop order 6→5→4→3→2, whole entries only ----

    fn marked(tag: &str, len: usize) -> String {
        let mut s = String::from(tag);
        while s.chars().count() < len {
            s.push('x');
        }
        s
    }

    /// One oversized entry per tier; compose must shed tiers from the back.
    fn oversized_input(entry_len: usize) -> (Vec<Part>, Vec<NoteRow>, Vec<(String, String)>) {
        let mut parts = tree("", vec![marked("MARK6", entry_len)]);
        parts.push(part(
            6,
            Some(3),
            &marked("MARK3", entry_len),
            "",
            Lifecycle::Todo,
            vec![],
        ));
        let notes = vec![
            note(1, 100, "decision", &marked("MARK2", entry_len)),
            note(2, 50, "note", &marked("MARK5", entry_len)),
        ];
        let summaries = vec![(marked("MARK4", entry_len), String::new())];
        (parts, notes, summaries)
    }

    #[test]
    fn budget_drops_later_tiers_first() {
        // ~3000 chars/entry: five tiers ≈ 15k → anchors, notes, summaries drop;
        // sub-parts + decisions survive.
        let (parts, notes, summaries) = oversized_input(3000);
        let out = compose("kod", &parts, 3, &notes, &summaries, &[], true);
        assert!(out.chars().count() <= MAX_KICKOFF_CHARS);
        assert!(out.contains("MARK2") && out.contains("MARK3"));
        assert!(!out.contains("MARK4") && !out.contains("MARK5") && !out.contains("MARK6"));
        assert!(out.starts_with("You are working on Leaf"));
        assert!(out.ends_with(FOOTER));
    }

    #[test]
    fn budget_keeps_decisions_longest() {
        // ~6500 chars/entry: only the decision tier fits alongside header+footer.
        let (parts, notes, summaries) = oversized_input(6500);
        let out = compose("kod", &parts, 3, &notes, &summaries, &[], true);
        assert!(out.chars().count() <= MAX_KICKOFF_CHARS);
        assert!(out.contains("MARK2"));
        for gone in ["MARK3", "MARK4", "MARK5", "MARK6"] {
            assert!(!out.contains(gone), "{gone} should have been dropped");
        }
        assert!(out.starts_with("You are working on Leaf"));
        assert!(out.ends_with(FOOTER));
    }

    #[test]
    fn whole_entry_drops_never_truncate() {
        let parts = tree("", vec![]);
        let keep = "keep this newest decision";
        let old = format!("OLDMARK {}", "z".repeat(7_900));
        let notes = vec![
            note(1, 100, "decision", &old),
            note(2, 200, "decision", keep),
        ];
        let out = compose("kod", &parts, 3, &notes, &[], &[], true);
        assert!(out.chars().count() <= MAX_KICKOFF_CHARS);
        assert!(out.contains(keep));
        // the oversized older decision vanishes entirely — no head, no tail.
        assert!(!out.contains("OLDMARK") && !out.contains("zzzz"));
    }

    #[test]
    fn single_decision_over_whole_budget_drops_header_survives() {
        let parts = tree("", vec![]);
        let giant = "G".repeat(MAX_KICKOFF_CHARS + 1_000);
        let notes = vec![note(1, 100, "decision", &giant)];
        let out = compose("kod", &parts, 3, &notes, &[], &[], true);
        assert!(out.chars().count() <= MAX_KICKOFF_CHARS);
        assert!(!out.contains("GGGG"));
        // an emptied tier takes its heading with it.
        assert!(!out.contains("Decisions already made"));
        assert!(out.starts_with("You are working on Leaf — part of kod."));
        assert!(out.ends_with(FOOTER));
    }

    // ---- idea-project variant ----

    #[test]
    fn no_repo_variant_reframes_and_skips_anchors() {
        let parts = tree("", vec!["src/should_not_appear.rs".into()]);
        let out = compose("someday-app", &parts, 3, &[], &[], &[], false);
        assert!(out.contains(NO_REPO_LINE));
        assert!(!out.contains("Code anchors"));
        assert!(!out.contains("src/should_not_appear.rs"));
        assert!(out.ends_with(FOOTER));
    }

    #[test]
    fn repo_variant_has_anchors_and_no_reframe() {
        let parts = tree("", vec!["crates/gui/src/**".into()]);
        let out = compose("kod", &parts, 3, &[], &[], &[], true);
        assert!(!out.contains(NO_REPO_LINE));
        assert!(out.contains("- crates/gui/src/**"));
    }

    #[test]
    fn ancestry_path_is_root_first_and_cycle_safe() {
        let parts = tree("", vec![]);
        let out = compose("kod", &parts, 1, &[], &[], &[], true);
        assert!(
            out.contains("Path: Root\n"),
            "root node's path is just itself"
        );
        // corrupt cycle: 1 ↔ 2 parent each other — must terminate.
        let cyclic = vec![
            part(1, Some(2), "A", "", Lifecycle::Todo, vec![]),
            part(2, Some(1), "B", "", Lifecycle::Todo, vec![]),
        ];
        let out = compose("kod", &cyclic, 1, &[], &[], &[], true);
        assert!(out.contains("Path: "));
        assert!(out.ends_with(FOOTER));
    }
}
