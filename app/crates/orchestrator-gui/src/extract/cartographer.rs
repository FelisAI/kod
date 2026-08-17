// ---------------------------------------------------------------------------
// docs/019 slice 2 FOUNDATIONS — the agentic cartographer. `run_claude_agent`
// is the high-capability counterpart to `run_claude_p`: read-only Read/Glob/
// Grep, a per-call model, cwd = the real repo. The trust gate is no longer an
// amputated op-whitelist (as in the slice-1 proposer above) but an EVIDENCE
// verifier: every proposed node must carry {source_file, verbatim source_quote};
// a deterministic Rust check string-matches the quote against the real file.
// Unverifiable ops are FLAGGED (kept + marked, individually accepted), never
// silently dropped (docs/019 commitment 3 + ruling 4). Live runs cost quota and
// need the real CLI, so the shim is kept THIN and every ounce of logic lives in
// pure helpers here — all unit-tested; the live run is a single #[ignore] test.
//
// The whole module is dead in a plain `cargo build` until main.rs dispatches a
// seed/expand (a later slice) — the #[allow] mirrors the slice-1 proposer
// convention above.
// `pub mod` so main.rs drives seed/re-ground/expand through it; the
// #[allow(dead_code)] stays because several pure helpers are exercised only by
// the cartographer_tests + the #[ignore] live run (a bin crate computes
// dead-code from `main`, not from `pub`), so it keeps the gate at zero warnings
// whichever subset the UI wiring ends up calling.
    use super::*;
    use orchestrator_store::{DiffOp, Kind, Lifecycle, Part, PartId, PartRef};
    use std::collections::{HashMap, HashSet};

    /// The STRUCTURAL model (docs/019 model routing): seed / re-ground / expand /
    /// rework / weekly map-health are user-invoked and judgment-bearing, so they
    /// run on the configured structural model rather than the cheaper plumbing
    /// model used for summaries. Empty by default (the account's own default);
    /// override via ORCH_PROMPT_STRUCTURAL_MODEL or the stored setting.
    pub const STRUCTURAL_MODEL: &str = super::CLAUDE_STRUCTURAL_MODEL;

    /// The map-intel (structural) lane's OWN hourly cap (≈6/hr), deliberately
    /// small and SEPARATE from the plumbing lane's 20/hr `sum_job_times` ledger
    /// so a burst of session summaries can never starve a user-invoked seed.
    pub const MAP_INTEL_HOURLY_CAP: usize = 6;

    /// The structural lane's rate gate — pure, over its own ledger of run-start
    /// timestamps (distinct from the plumbing ledger). One more structural run
    /// is allowed iff fewer than `cap` ran in the trailing hour.
    pub fn map_intel_rate_allows(run_starts_secs: &[u64], now_secs: u64, cap: usize) -> bool {
        run_starts_secs
            .iter()
            .filter(|t| now_secs.saturating_sub(**t) < 3600)
            .count()
            < cap
    }

    // -- the agentic shim (thin; all logic lives in the pure helpers below) --

    /// One read-only, TIMEOUT- and TURN-bounded agentic `claude -p`. Unlike
    /// `run_claude_p`, cwd = the real project root (so Read/Glob/Grep resolve
    /// against the actual code) and the tool allowlist is passed in (the caller
    /// supplies Read/Glob/Grep — never Bash/Edit/Write). Per-call `model`
    /// (default `STRUCTURAL_MODEL`). Keeps `--no-session-persistence` so no
    /// stray transcript lands in the repo, `--strict-mcp-config` + hooks-off
    /// like the plumbing shim. The full stdout (audit transcript) is captured
    /// into `transcript_out`. Blocking + spends the map-intel ledger; run OFF
    /// the UI thread. (Live streamed progress into the card is a slice-2-UI
    /// concern, deferred — the transcript is captured whole here.)
    #[allow(clippy::too_many_arguments)]
    pub fn run_claude_agent(
        prompt_text: &str,
        cwd: &Path,
        model: &str,
        allowed_tools: &[&str],
        max_turns: u32,
        timeout_secs: u64,
        transcript_out: &mut String,
    ) -> Result<String, String> {
        super::run_agent_prompt(
            prompt_text,
            cwd,
            model,
            allowed_tools,
            max_turns,
            timeout_secs,
            transcript_out,
        )
    }

    // ------------------------ git facts (injected) -------------------------

    /// Pure formatter for the injected git-facts block: one top-level dir per
    /// stanza, each a list of recent "date subject" lines (newest first — the
    /// first line's date is the dir's last-touch). Empty in → empty out (no
    /// header for a non-repo), so the seed prompt degrades cleanly.
    pub fn format_git_facts(entries: &[(String, Vec<String>)]) -> String {
        if entries.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "GIT ACTIVITY (recent commits per top-level area; the first date is last-touch):\n",
        );
        for (dir, lines) in entries {
            out.push_str(&format!("  {dir}/\n"));
            for l in lines {
                out.push_str(&format!("    {l}\n"));
            }
        }
        out
    }

    /// Collect recent commit "date subject" lines per top-level dir by shelling
    /// `git log` (deterministic; the agent has NO shell access, so git facts
    /// are precomputed here). Non-repo / git-missing → empty (format degrades).
    pub fn collect_git_facts(root: &Path, per_dir_cap: usize) -> Vec<(String, Vec<String>)> {
        let skip = |n: &str| {
            matches!(
                n,
                ".git" | "target" | "node_modules" | ".venv" | "dist" | "build" | ".DS_Store"
            )
        };
        let mut dirs: Vec<String> = Vec::new();
        if let Ok(top) = std::fs::read_dir(root) {
            let mut es: Vec<_> = top.flatten().collect();
            es.sort_by_key(|e| e.file_name());
            for e in es {
                if e.path().is_dir() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if !skip(&n) {
                        dirs.push(n);
                    }
                }
            }
        }
        let root_s = root.to_string_lossy().into_owned();
        let mut out = Vec::new();
        for d in dirs {
            let output = Command::new("git")
                .args([
                    "-C",
                    &root_s,
                    "log",
                    &format!("-{per_dir_cap}"),
                    "--no-merges",
                    "--format=%ad %s",
                    "--date=short",
                    "--",
                    &d,
                ])
                .output();
            if let Ok(o) = output {
                if o.status.success() {
                    let lines: Vec<String> = String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .map(|l| l.chars().take(100).collect::<String>())
                        .filter(|l| !l.trim().is_empty())
                        .collect();
                    if !lines.is_empty() {
                        out.push((d, lines));
                    }
                }
            }
        }
        out
    }

    // -------------------------- prompt construction ------------------------

    /// The whole-map seed / re-ground prompt: a taxonomy GRAMMAR (never a
    /// preset), the evidence contract, and the injected git facts. The output
    /// shape is the `{"nodes":[...]}` tree `parse_agentic_nodes` consumes.
    pub fn seed_agent_prompt(digest: &str, git_facts: &str) -> String {
        format!(
            "You are mapping a software project's PRODUCT ANATOMY for a user's glanceable \
status board — the whole venture, not just its code. You have READ-ONLY tools (Read, Glob, \
Grep) and the project root as cwd: READ the real docs, README, and entry-point code before \
proposing anything.\n\
TAXONOMY GRAMMAR (docs/019 — a grammar, NEVER a preset): top level = 4-9 root AREAS named as \
noun-phrases the user would say aloud; organize by whatever principle THIS project's own \
documents exhibit; quarantine code-derived structure under exactly ONE collapsed area named \
\"from code\". 1-2 levels under each area.\n\
EVERY node MUST carry: `name`, `detail` (one line), `kind` (\"area\" | \"task\" | \"idea\"), \
`lifecycle` (\"idea\" or \"todo\"; \"done\" ONLY with a dated shipped-claim you quote, e.g. \
\"Shipped 2026-06-21\"), `anchors` (code path globs, [] when none), `rationale` (why this node, \
why here), and EVIDENCE: `source_file` (a REAL repo file path, relative to root) + `source_quote` \
(a VERBATIM span copied from that file — a deterministic verifier string-matches it, so a \
paraphrase is FLAGGED). A node with no repo evidence is still kept but WILL be flagged for \
individual review — never invent a citation, and never cite a session note (repo files only).\n\
Output ONLY minified JSON, no prose, shape: \
{{\"nodes\":[{{\"name\":\"...\",\"detail\":\"...\",\"kind\":\"area\",\"lifecycle\":\"todo\",\"anchors\":[\"...\"],\"source_file\":\"...\",\"source_quote\":\"...\",\"rationale\":\"...\",\"children\":[{{\"name\":\"...\",\"detail\":\"...\",\"kind\":\"task\",\"lifecycle\":\"todo\",\"anchors\":[],\"source_file\":\"...\",\"source_quote\":\"...\",\"rationale\":\"...\",\"children\":[]}}]}}]}}\n\n\
STRUCTURE DIGEST:\n{digest}\n\n{git_facts}"
        )
    }

    /// The RE-GROUND (delta) prompt (docs/019 seed/re-ground: "Re-ground is a
    /// permanent verb — it serializes the current tree with real ids and
    /// proposes DELTAS"). Same evidence contract + taxonomy GRAMMAR as the
    /// fresh seed, but the model is shown the EXISTING map and told to propose
    /// only genuinely NEW areas/nodes the docs support that the map is MISSING
    /// (an additive delta — the Add-only parser's v1; move/remove deltas are a
    /// later increment). Output shape is the same `{"nodes":[...]}` the parser
    /// consumes; new roots land at whole-map scope.
    pub fn reground_agent_prompt(digest: &str, git_facts: &str, existing_tree: &str) -> String {
        format!(
            "You are RE-GROUNDING a user's existing product map — it already has structure, and \
you propose what's MISSING, never a from-scratch rebuild. READ-ONLY tools (Read, Glob, Grep), the \
project root as cwd: READ the real docs, README, and entry-point code first.\n\
TAXONOMY GRAMMAR (docs/019 — a grammar, NEVER a preset): keep the map's own organizing principle; \
propose ONLY new AREAS or child nodes the documents clearly support that the CURRENT MAP below does \
NOT already cover; quarantine any code-derived structure under the ONE \"from code\" area. Do NOT \
restate a node that already exists.\n\
EVERY node MUST carry: `name`, `detail` (one line), `kind` (\"area\" | \"task\" | \"idea\"), \
`lifecycle` (\"idea\" or \"todo\"; \"done\" ONLY with a dated shipped-claim you quote), `anchors` \
(code path globs, [] when none), `rationale` (why this node, why here), and EVIDENCE: `source_file` \
(a REAL repo file path, relative to root) + `source_quote` (a VERBATIM span copied from that file — \
a paraphrase is FLAGGED). Never invent a citation; repo files only.\n\
Output ONLY minified JSON, no prose, shape: \
{{\"nodes\":[{{\"name\":\"...\",\"detail\":\"...\",\"kind\":\"area\",\"lifecycle\":\"todo\",\"anchors\":[\"...\"],\"source_file\":\"...\",\"source_quote\":\"...\",\"rationale\":\"...\",\"children\":[]}}]}}\n\n\
CURRENT MAP (do not duplicate these — propose only what's missing):\n{existing_tree}\n\n\
STRUCTURE DIGEST:\n{digest}\n\n{git_facts}"
        )
    }

    /// Pure prompt SELECTOR for the whole-map lane (docs/019: the seed's
    /// delta-vs-fresh choice, the user-attention path unit-tested here): an
    /// EMPTY (or whitespace) existing tree → the fresh `seed_agent_prompt`; a
    /// non-empty one → the additive `reground_agent_prompt` DELTA.
    pub fn seed_prompt_for(digest: &str, git_facts: &str, existing_tree: Option<&str>) -> String {
        match existing_tree {
            Some(t) if !t.trim().is_empty() => reground_agent_prompt(digest, git_facts, t),
            _ => seed_agent_prompt(digest, git_facts),
        }
    }

    /// The FENCED expand / rework prompt (docs/019 C4): everything the model
    /// proposes lands UNDER the pointed-at node; it may not touch anything
    /// outside the subtree. `taxonomy_note` (the ratified map grammar) is
    /// injected so the machine can never drift from the map's own grammar.
    pub fn expand_agent_prompt(
        node_path: &str,
        node_detail: &str,
        taxonomy_note: &str,
        git_facts: &str,
    ) -> String {
        let grammar = if taxonomy_note.trim().is_empty() {
            String::new()
        } else {
            format!("MAP GRAMMAR (do not drift from it): {taxonomy_note}\n")
        };
        format!(
            "You are fleshing out ONE area of a user's product map, FENCED to the node below — \
everything you propose lands UNDER it; you may NOT touch anything outside this subtree. Read-only \
tools (Read, Glob, Grep), project root as cwd: READ the node's linked docs and anchored code \
first.\n{grammar}\
NODE (home base): {node_path}\nDETAIL: {node_detail}\n\
Propose NEW child nodes only. EVERY node carries `name`, `detail`, `kind` (\"area\" | \"task\" | \
\"idea\"), `lifecycle` (\"idea\" or \"todo\"; \"done\" only with a dated shipped-claim you quote), \
`anchors`, `rationale`, and EVIDENCE: `source_file` (a REAL repo file) + `source_quote` (a \
VERBATIM span; a paraphrase is FLAGGED). Never invent a citation; repo files only.\n\
Output ONLY minified JSON, no prose, shape: \
{{\"nodes\":[{{\"name\":\"...\",\"detail\":\"...\",\"kind\":\"task\",\"lifecycle\":\"todo\",\"anchors\":[\"...\"],\"source_file\":\"...\",\"source_quote\":\"...\",\"rationale\":\"...\",\"children\":[]}}]}}\n\n{git_facts}"
        )
    }

    /// The FENCED RESTRUCTURE (rework) prompt (docs/019 C4 + slice 2b): unlike
    /// `expand_agent_prompt` (add-only), the op whitelist is FULL — the agent
    /// may Add / Move / Rename / SetKind / Remove INSIDE the subtree to
    /// reorganize it per the user's intent. The CURRENT subtree is serialized
    /// with REAL numeric ids (`subtree_json`, shape `{id,name,kind,one_liner,
    /// children[]}`) so move/rename/set_kind/remove reference existing nodes;
    /// new nodes coin temp ids ("new:areaX") that a move/add may name as a
    /// parent. `taxonomy_note` (the ratified grammar) is injected so the machine
    /// can't drift. Removes are held to a high bar (see RULES): move contents to
    /// an "Attic" rather than delete unless cited-contradicted.
    pub fn rework_agent_prompt(
        node_path: &str,
        node_detail: &str,
        taxonomy_note: &str,
        subtree_json: &str,
        git_facts: &str,
    ) -> String {
        let grammar = if taxonomy_note.trim().is_empty() {
            String::new()
        } else {
            format!("MAP GRAMMAR (do not drift from it): {taxonomy_note}\n")
        };
        format!(
            "You are RESTRUCTURING one subtree of a user's product map, FENCED to the node below — \
you may reorganize EVERYTHING inside this subtree but may NOT touch, reference, add under, or move \
anything OUTSIDE it. Read-only tools (Read, Glob, Grep), project root as cwd: READ the node's linked \
docs and anchored code before proposing a new shape.\n{grammar}\
NODE (home base — its subtree is your whole world): {node_path}\nGUIDANCE: {node_detail}\n\
CURRENT SUBTREE (reuse these REAL numeric ids exactly; nesting = current parent→children):\n{subtree_json}\n\
Reorganize it per the GUIDANCE and the map grammar. Output an `ops` array; each op is EXACTLY ONE of:\n\
{{\"op\":\"add\",\"id\":\"new:area1\",\"parent\":<real-id or \"new:x\">,\"name\":\"...\",\"kind\":\"area|task|idea\",\"lifecycle\":\"todo|idea\",\"source_file\":\"...\",\"source_quote\":\"...\",\"rationale\":\"...\"}}\n\
{{\"op\":\"move\",\"id\":<existing real id>,\"new_parent\":<real-id or \"new:x\">,\"rationale\":\"...\"}}\n\
{{\"op\":\"rename\",\"id\":<existing real id>,\"name\":\"...\",\"rationale\":\"...\"}}\n\
{{\"op\":\"set_kind\",\"id\":<existing real id>,\"kind\":\"area|task|idea\",\"rationale\":\"...\"}}\n\
{{\"op\":\"remove\",\"id\":<existing real id>,\"rationale\":\"...\",\"contradicting_quote\":\"...\",\"source_file\":\"...\"}}\n\
RULES:\n\
- A NEW node coins a temp id like \"new:area1\"; a later `move`/`add` may name that temp as its \
`parent`/`new_parent`. Reference EXISTING nodes ONLY by a real numeric id shown above; an id NOT in \
this subtree is out of bounds and its op is DROPPED. The fence root itself may be renamed/removed.\n\
- EVERY op carries a one-line `rationale` (why this change). An op with no rationale is FLAGGED for \
individual review.\n\
- A NEW node (`add`) also needs EVIDENCE: `source_file` (a REAL repo file, relative to root) + \
`source_quote` (a VERBATIM span copied from it — a deterministic verifier string-matches it, so a \
paraphrase is FLAGGED). `move`/`rename`/`set_kind` of EXISTING nodes assert no new fact and need only \
the rationale. Repo files only; never cite a session note.\n\
- To retire a node, PREFER moving its children under a new/other node and then removing the emptied \
husk. A `remove` of a node that STILL has children (none moved out in this same set) is FLAGGED unless \
you cite a `contradicting_quote` (+ its `source_file`) proving the node obsolete. When unsure, MOVE the \
node under a new \"Attic\" node you propose instead of removing it.\n\
- `lifecycle` is \"todo\" or \"idea\" (\"done\" only with a dated shipped-claim you quote).\n\
Output ONLY minified JSON, no prose, shape: {{\"ops\":[...]}}\n\n{git_facts}"
        )
    }

    // -------------------------- the evidence verifier ----------------------

    /// The deterministic evidence verifier (docs/019 commitment 3). Whitespace-
    /// normalize BOTH sides — collapse every run of ASCII whitespace to one
    /// space and trim — then substring-match. A paraphrase fails (→ the op is
    /// FLAGGED, never silently dropped); only a verbatim span (modulo
    /// whitespace) verifies. An empty quote never verifies.
    /// A citation must be SPECIFIC, not a substring found in every file
    /// (review: "the" / "fn " verified everything, defeating the trust gate).
    /// A real supporting quote is a phrase — require ≥ this many normalized
    /// chars. docs/019 caps quotes at ~200; the floor kills trivial tokens.
    const MIN_QUOTE_CHARS: usize = 16;
    pub fn verify_quote(file_contents: &str, quote: &str) -> bool {
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let q = norm(quote);
        // char count (not bytes) so a short multibyte quote isn't over-credited.
        if q.chars().count() < MIN_QUOTE_CHARS {
            return false;
        }
        norm(file_contents).contains(&q)
    }

    /// docs/019 seed recipe: `done` is proposable ONLY with a dated shipped-
    /// claim in the quote — a past-dated ship word. Deterministic: the quote
    /// must contain a ship word AND an ISO date (YYYY-MM-DD) that is not in the
    /// future (ISO dates compare lexically). Otherwise `done` is coerced to
    /// todo (the human asserts done themselves).
    pub fn quote_has_dated_ship_claim(quote: &str, today_iso: &str) -> bool {
        const SHIP: [&str; 8] = [
            "shipped",
            "landed",
            "released",
            "launched",
            "merged",
            "completed",
            "done",
            "fixed",
        ];
        // a NEGATED or roadmap quote is not a ship claim (review: "not done yet
        // — target 2020-01-01" must not grant `done`). Reject on any negation
        // token — heuristic, and done is already held out of Accept-all, so this
        // is defense-in-depth against a misleading badge.
        const NEGATION: [&str; 9] = [
            "not ", "n't", "yet", "todo", "target", "planned", "will ", "aim to", "pending",
        ];
        let lower = quote.to_lowercase();
        if NEGATION.iter().any(|w| lower.contains(w)) {
            return false;
        }
        if !SHIP.iter().any(|w| lower.contains(w)) {
            return false;
        }
        match find_iso_date(quote) {
            Some(d) => d.as_str() <= today_iso,
            None => false,
        }
    }

    /// First `YYYY-MM-DD` substring in `s` (no regex dep). Byte-scanned; a
    /// window that straddles a multibyte char fails the digit checks and is
    /// skipped.
    fn find_iso_date(s: &str) -> Option<String> {
        let b = s.as_bytes();
        let mut i = 0usize;
        while i + 10 <= b.len() {
            let w = &b[i..i + 10];
            let digits = w[0].is_ascii_digit()
                && w[1].is_ascii_digit()
                && w[2].is_ascii_digit()
                && w[3].is_ascii_digit()
                && w[5].is_ascii_digit()
                && w[6].is_ascii_digit()
                && w[8].is_ascii_digit()
                && w[9].is_ascii_digit();
            if digits && w[4] == b'-' && w[7] == b'-' {
                // validate calendar ranges (review: 2026-13-45 is not a date, and
                // 2026-02-30 must not slip through and grant a bogus ship date).
                let month = (w[5] - b'0') * 10 + (w[6] - b'0');
                let day = (w[8] - b'0') * 10 + (w[9] - b'0');
                let year = (w[0] - b'0') as u32 * 1000
                    + (w[1] - b'0') as u32 * 100
                    + (w[2] - b'0') as u32 * 10
                    + (w[3] - b'0') as u32;
                let max_day: u8 = match month {
                    1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
                    4 | 6 | 9 | 11 => 30,
                    2 => {
                        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
                        if leap {
                            29
                        } else {
                            28
                        }
                    }
                    _ => 0,
                };
                if (1..=12).contains(&month) && day >= 1 && day <= max_day {
                    return Some(String::from_utf8_lossy(w).into_owned());
                }
            }
            i += 1;
        }
        None
    }

    // ---------------------- parse → ops + evidence + flags ------------------

    /// A parsed agentic result: index-aligned ops / evidence / flags. `ops[i]`
    /// carries its provenance quad (verified or not) on the Add; `evidence[i]`
    /// is the review-card justification line; `flagged[i]` = the quote did NOT
    /// verify (kept + marked; blocks accept-all; must be individually accepted).
    #[derive(Default)]
    pub struct AgenticResult {
        pub ops: Vec<DiffOp>,
        pub evidence: Vec<Option<String>>,
        pub flagged: Vec<bool>,
    }

    impl AgenticResult {
        /// How many ops FLAGGED (unverifiable) — reported in Standup so output-
        /// side starvation (verifier over-flag) is observable (docs/019).
        pub fn flag_count(&self) -> usize {
            self.flagged.iter().filter(|f| **f).count()
        }
        /// How many ops verified clean.
        pub fn verified_count(&self) -> usize {
            self.flagged.iter().filter(|f| !**f).count()
        }
    }

    /// Parse an agentic `{"nodes":[...]}` tree into carded-changeset material.
    /// `fence_root` = the changeset scope: `Some(id)` parents top-level nodes
    /// under that node (expand/rework); `None` = whole-map (seed/re-ground).
    /// `read_file` reads a repo file relative to root — supplied by the caller
    /// so the parse stays pure/testable; the verifier string-matches the quote
    /// against it. `today_iso` gates `done` ship-claims. Malformed input parses
    /// cleanly to EMPTY (never a panic).
    pub fn parse_agentic_nodes(
        json: &str,
        fence_root: Option<PartId>,
        today_iso: &str,
        read_file: &dyn Fn(&str) -> Option<String>,
    ) -> AgenticResult {
        let mut out = AgenticResult::default();
        let Some(obj) = first_json_object(json) else {
            return out;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&obj) else {
            return out;
        };
        let Some(arr) = v.get("nodes").and_then(|a| a.as_array()) else {
            return out;
        };
        let top_parent = match fence_root {
            Some(id) => PartRef::Id(id),
            None => PartRef::Root,
        };
        let mut counter = 0usize;
        for node in arr {
            add_agentic_node(
                node,
                top_parent.clone(),
                today_iso,
                read_file,
                &mut out,
                &mut counter,
            );
        }
        out
    }

    fn add_agentic_node(
        v: &serde_json::Value,
        parent: PartRef,
        today_iso: &str,
        read_file: &dyn Fn(&str) -> Option<String>,
        out: &mut AgenticResult,
        counter: &mut usize,
    ) {
        let s = |k: &str| {
            v.get(k)
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let name = s("name");
        if name.is_empty() {
            return; // no name → can't Add (and no id to fence a child under)
        }
        *counter += 1;
        let temp = format!("g{counter}");
        let detail = s("detail");
        let rationale = s("rationale");
        let source_file = s("source_file");
        let source_quote = s("source_quote");
        let has_children = v
            .get("children")
            .and_then(|c| c.as_array())
            .map(|c| !c.is_empty())
            .unwrap_or(false);
        let anchors: Vec<String> = v
            .get("anchors")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        // kind: honor the model's explicit kind; absent → backfill heuristic
        // (children → area, leaf → task; docs/019).
        let kind = match v.get("kind").and_then(|k| k.as_str()) {
            Some("area") => Kind::Area,
            Some("idea") => Kind::Idea,
            Some("task") => Kind::Task,
            _ if has_children => Kind::Area,
            _ => Kind::Task,
        };
        // lifecycle: idea stays idea; done is allowed ONLY with a dated ship-
        // claim in the quote, else coerced to todo (idea for an idea-kind node).
        let lifecycle = match v.get("lifecycle").and_then(|l| l.as_str()) {
            Some("idea") => Lifecycle::Idea,
            Some("done") if quote_has_dated_ship_claim(&source_quote, today_iso) => Lifecycle::Done,
            _ if matches!(kind, Kind::Idea) => Lifecycle::Idea,
            _ => Lifecycle::Todo,
        };
        // EVIDENCE-OR-FLAG: verify the verbatim quote against the REAL repo
        // file (missing file / empty evidence → flagged). Sources are repo
        // files only — read_file resolves relative to root; a note is not a file.
        let verified = if !source_file.is_empty() && !source_quote.is_empty() {
            match read_file(&source_file) {
                Some(contents) => verify_quote(&contents, &source_quote),
                None => false,
            }
        } else {
            false
        };
        // the provenance quad rides the Add regardless — a flagged op is KEPT,
        // marked (flagged[i]=true), and must be individually accepted.
        out.ops.push(DiffOp::Add {
            temp: temp.clone(),
            parent,
            name,
            detail,
            lifecycle,
            anchors,
            kind,
            detail_md: None,
            sort_order: None,
            source_file: (!source_file.is_empty()).then(|| source_file.clone()),
            source_quote: (!source_quote.is_empty()).then(|| source_quote.clone()),
            rationale: (!rationale.is_empty()).then_some(rationale),
        });
        out.evidence
            .push((!source_quote.is_empty()).then(|| source_quote.clone()));
        out.flagged.push(!verified);
        if let Some(children) = v.get("children").and_then(|c| c.as_array()) {
            for child in children {
                add_agentic_node(
                    child,
                    PartRef::Temp(temp.clone()),
                    today_iso,
                    read_file,
                    out,
                    counter,
                );
            }
        }
    }

    // ------------------- rework: the machine RESTRUCTURE lane ---------------
    // docs/019 slice 2b. `parse_agentic_nodes` above is ADD-ONLY (seed/expand);
    // rework needs the FULL op whitelist (Add/Move/Rename/SetKind/Remove) fenced
    // to a subtree so "reorganize by product area" can reparent existing nodes,
    // not just append. The agent is given the current subtree with REAL ids and
    // emits ops referencing them; this parser maps each op → DiffOp, FENCE-checks
    // it (drop out-of-fence), and FLAGS per the trust rules (add without a
    // verified quote; a childful Remove without a cited contradiction; any op
    // with no rationale). Pure + testable; the live run is a single #[ignore].

    /// One current node of the fenced subtree, handed to the restructure parser
    /// so (a) a `rename` preserves the node's existing one-liner (the store's
    /// Rename wipes `detail` when given an empty string) and (b) a childful
    /// Remove can be detected. Mirrors what the agent is shown ({id, name, kind,
    /// one-liner, children}).
    #[derive(Clone, Debug)]
    pub struct SubtreeNode {
        pub name: String,
        pub detail: String,
        pub children: Vec<PartId>,
    }

    /// Serialize the fenced subtree for the rework prompt AND build the id-set +
    /// node-info map the parser needs — all three from one walk of `parts` so
    /// the agent's ids, the fence, and the childful/rename lookups can never
    /// disagree. The JSON nests `{id,name,kind,one_liner,children[]}` from the
    /// fence root down (children ordered by sort_order).
    pub fn serialize_subtree_for_rework(
        parts: &[Part],
        fence_root: PartId,
    ) -> (String, HashSet<PartId>, HashMap<PartId, SubtreeNode>) {
        let mut children: HashMap<PartId, Vec<&Part>> = HashMap::new();
        for p in parts {
            if let Some(pp) = p.parent_id {
                children.entry(pp).or_default().push(p);
            }
        }
        for v in children.values_mut() {
            v.sort_by(|a, b| {
                a.sort_order
                    .partial_cmp(&b.sort_order)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        let by_id: HashMap<PartId, &Part> = parts.iter().map(|p| (p.id, p)).collect();
        let mut ids: HashSet<PartId> = HashSet::new();
        let mut nodes: HashMap<PartId, SubtreeNode> = HashMap::new();

        fn build(
            id: PartId,
            by_id: &HashMap<PartId, &Part>,
            children: &HashMap<PartId, Vec<&Part>>,
            ids: &mut HashSet<PartId>,
            nodes: &mut HashMap<PartId, SubtreeNode>,
        ) -> serde_json::Value {
            let Some(p) = by_id.get(&id) else {
                return serde_json::Value::Null;
            };
            ids.insert(id);
            let kids = children.get(&id).cloned().unwrap_or_default();
            let child_ids: Vec<PartId> = kids.iter().map(|c| c.id).collect();
            nodes.insert(
                id,
                SubtreeNode {
                    name: p.name.clone(),
                    detail: p.detail.clone(),
                    children: child_ids,
                },
            );
            let child_json: Vec<serde_json::Value> = kids
                .iter()
                .map(|c| build(c.id, by_id, children, ids, nodes))
                .collect();
            serde_json::json!({
                "id": id,
                "name": p.name,
                "kind": p.kind.as_str(),
                "one_liner": p.detail,
                "children": child_json,
            })
        }

        let json = build(fence_root, &by_id, &children, &mut ids, &mut nodes);
        (json.to_string(), ids, nodes)
    }

    /// Read a JSON id field as a real `PartId` — a bare number or a numeric
    /// string (`7` or `"7"`); a non-numeric string (a coined temp) yields None.
    fn json_real_id(v: Option<&serde_json::Value>) -> Option<PartId> {
        let v = v?;
        if let Some(n) = v.as_i64() {
            return Some(n);
        }
        v.as_str().and_then(|s| s.trim().parse::<PartId>().ok())
    }

    /// Parse a rework `{"ops":[...]}` restructure into carded-changeset material
    /// (docs/019 slice 2b). Each op is one of add|move|rename|set_kind|remove.
    /// `fence_root` + `subtree_ids` (the root's subtree, root INCLUDED) are the
    /// fence: an op whose target id OR real-id destination is not in
    /// `subtree_ids ∪ fence_root ∪ this-run's coined temps` is DROPPED (loud
    /// rejection — never applied). `nodes` supplies each fenced node's current
    /// one-liner (so a Rename preserves it) and children (so a childful Remove is
    /// caught). `read_file` + `today_iso` gate add evidence exactly like the
    /// add-only parser. FLAG rules: an `add` with no verified quote; a `remove`
    /// of a still-childful node without a cited+verified contradiction; ANY op
    /// with no rationale; a `done` add without a dated ship-claim. Emission is
    /// ordered Adds → Rename/SetKind/Move → Removes so a coined temp exists
    /// before a Move names it and a husk's children are moved out before it is
    /// removed. Index-aligned ops/evidence/flags; malformed input → EMPTY.
    pub fn parse_agentic_restructure(
        json: &str,
        fence_root: PartId,
        subtree_ids: &HashSet<PartId>,
        nodes: &HashMap<PartId, SubtreeNode>,
        today_iso: &str,
        read_file: &dyn Fn(&str) -> Option<String>,
    ) -> AgenticResult {
        let mut out = AgenticResult::default();
        let Some(obj) = first_json_object(json) else {
            return out;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&obj) else {
            return out;
        };
        let Some(arr) = v.get("ops").and_then(|a| a.as_array()) else {
            return out;
        };

        // the fence set: every subtree id plus the root itself (defensive — the
        // caller's set already includes it, but a rework MUST be able to touch
        // its own root).
        let mut fence: HashSet<PartId> = subtree_ids.clone();
        fence.insert(fence_root);

        // PASS 1 — collect the temp ids the agent coins on `add` ops, so a
        // `move`/`add` whose parent references a temp defined LATER still
        // resolves. A parent temp matching no coined temp is a dangling
        // reference and its op is dropped (it would silently escape to root).
        let mut run_temps: HashSet<String> = HashSet::new();
        for item in arr {
            if item.get("op").and_then(|o| o.as_str()) == Some("add") {
                if let Some(t) = add_temp_of(item) {
                    run_temps.insert(t);
                }
            }
        }

        // resolve a `parent`/`new_parent` reference → PartRef, or None if it is
        // neither an in-fence real id nor a coined temp (→ the op is dropped).
        let resolve_parent = |val: Option<&serde_json::Value>| -> Option<PartRef> {
            let val = val?;
            if let Some(id) = json_real_id(Some(val)) {
                return fence.contains(&id).then_some(PartRef::Id(id));
            }
            let s = val.as_str()?.trim();
            run_temps.contains(s).then(|| PartRef::Temp(s.to_string()))
        };

        // staged ops carry a phase so we can order the emission without losing
        // index-alignment (evidence/flag ride each op). Removes are finalized
        // after the whole array is seen (childful check needs every move/remove).
        type StagedOp = (u8, DiffOp, Option<String>, bool);
        let mut staged: Vec<StagedOp> = Vec::new();
        let mut moved_out: HashMap<PartId, PartRef> = HashMap::new();
        let mut removed_ids: HashSet<PartId> = HashSet::new();
        struct PendingRemove {
            id: PartId,
            rationale: String,
            evidence: Option<String>,
            contradiction_ok: bool,
        }
        let mut pending_removes: Vec<PendingRemove> = Vec::new();
        let mut counter = 0usize;
        // moved nodes APPEND after a destination's existing children: a high base
        // keeps them clear of the small integer/renumbered orders siblings carry,
        // so a reparent never collides and scrambles the existing order (review).
        let mut move_order = 1_000_000.0f64;

        for item in arr {
            let s = |k: &str| {
                item.get(k)
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string()
            };
            match item.get("op").and_then(|o| o.as_str()).unwrap_or("") {
                "add" => {
                    let name = s("name");
                    if name.is_empty() {
                        continue; // no name → can't Add
                    }
                    // a coined temp id (agent's "new:x"); else a synthesized one
                    // (still a valid Add — just not referenceable as a parent).
                    let temp = match add_temp_of(item) {
                        Some(t) => t,
                        None => {
                            counter += 1;
                            format!("r{counter}")
                        }
                    };
                    // parent: absent → land under the fence root; present → must
                    // resolve in-fence (real id or coined temp), else drop.
                    let parent = match item.get("parent") {
                        None => PartRef::Id(fence_root),
                        Some(_) => match resolve_parent(item.get("parent")) {
                            Some(p) => p,
                            None => continue,
                        },
                    };
                    let detail = s("detail");
                    let rationale = s("rationale");
                    let source_file = s("source_file");
                    let source_quote = s("source_quote");
                    // a fresh rework add has no children, so no children→area
                    // heuristic; the agent's explicit kind wins, else task.
                    let kind = match item.get("kind").and_then(|k| k.as_str()) {
                        Some("area") => Kind::Area,
                        Some("idea") => Kind::Idea,
                        _ => Kind::Task,
                    };
                    let claimed_done =
                        item.get("lifecycle").and_then(|l| l.as_str()) == Some("done");
                    let dated_ship =
                        claimed_done && quote_has_dated_ship_claim(&source_quote, today_iso);
                    let lifecycle = match item.get("lifecycle").and_then(|l| l.as_str()) {
                        Some("idea") => Lifecycle::Idea,
                        Some("done") if dated_ship => Lifecycle::Done,
                        _ if matches!(kind, Kind::Idea) => Lifecycle::Idea,
                        _ => Lifecycle::Todo,
                    };
                    // evidence-or-flag: verify the verbatim quote against the real
                    // repo file (missing file / empty → unverified).
                    let verified = if !source_file.is_empty() && !source_quote.is_empty() {
                        read_file(&source_file)
                            .map(|c| verify_quote(&c, &source_quote))
                            .unwrap_or(false)
                    } else {
                        false
                    };
                    // a new node is FLAGGED when: its quote didn't verify, OR it
                    // carries no rationale, OR it claimed done without a dated
                    // ship-claim (the lifecycle is coerced to todo AND flagged so
                    // the user vouches for the demotion).
                    let flagged =
                        !verified || rationale.is_empty() || (claimed_done && !dated_ship);
                    staged.push((
                        0,
                        DiffOp::Add {
                            temp,
                            parent,
                            name,
                            detail,
                            lifecycle,
                            anchors: Vec::new(),
                            kind,
                            detail_md: None,
                            sort_order: None,
                            source_file: (!source_file.is_empty()).then(|| source_file.clone()),
                            source_quote: (!source_quote.is_empty()).then(|| source_quote.clone()),
                            rationale: (!rationale.is_empty()).then_some(rationale),
                        },
                        (!source_quote.is_empty()).then(|| source_quote.clone()),
                        flagged,
                    ));
                }
                "move" => {
                    let Some(id) = json_real_id(item.get("id")) else {
                        continue;
                    };
                    if !fence.contains(&id) {
                        continue; // target outside the fence → drop
                    }
                    let Some(parent) = resolve_parent(item.get("new_parent")) else {
                        continue;
                    };
                    if parent == PartRef::Id(id) {
                        continue; // a node cannot be its own parent
                    }
                    let rationale = s("rationale");
                    let flagged = rationale.is_empty();
                    let evidence = (!rationale.is_empty()).then(|| rationale.clone());
                    move_order += 1.0;
                    moved_out.insert(id, parent.clone());
                    staged.push((
                        1,
                        DiffOp::Move {
                            id,
                            parent,
                            sort_order: move_order,
                        },
                        evidence,
                        flagged,
                    ));
                }
                "rename" => {
                    let Some(id) = json_real_id(item.get("id")) else {
                        continue;
                    };
                    if !fence.contains(&id) {
                        continue;
                    }
                    let name = s("name");
                    if name.is_empty() {
                        continue; // rename to nothing → drop
                    }
                    if nodes.get(&id).is_some_and(|n| n.name.trim() == name.trim()) {
                        continue; // no-op rename noise from the model
                    }
                    let rationale = s("rationale");
                    let flagged = rationale.is_empty();
                    let evidence = (!rationale.is_empty()).then(|| rationale.clone());
                    // preserve the current one-liner (empty-detail Rename would
                    // wipe it in the store).
                    let detail = nodes.get(&id).map(|n| n.detail.clone()).unwrap_or_default();
                    staged.push((1, DiffOp::Rename { id, name, detail }, evidence, flagged));
                }
                "set_kind" => {
                    let Some(id) = json_real_id(item.get("id")) else {
                        continue;
                    };
                    if !fence.contains(&id) {
                        continue;
                    }
                    let kind = match s("kind").as_str() {
                        "area" => Kind::Area,
                        "idea" => Kind::Idea,
                        "task" => Kind::Task,
                        _ => continue, // unknown kind → drop
                    };
                    let rationale = s("rationale");
                    let flagged = rationale.is_empty();
                    let evidence = (!rationale.is_empty()).then(|| rationale.clone());
                    staged.push((1, DiffOp::SetKind { id, kind }, evidence, flagged));
                }
                "remove" => {
                    let Some(id) = json_real_id(item.get("id")) else {
                        continue;
                    };
                    if !fence.contains(&id) {
                        continue;
                    }
                    let rationale = s("rationale");
                    let contradicting = s("contradicting_quote");
                    let src = s("source_file");
                    // a cited contradiction only counts if it VERIFIES against a
                    // real repo file (an unverified quote can't license deletion).
                    let contradiction_ok = !contradicting.is_empty()
                        && !src.is_empty()
                        && read_file(&src)
                            .map(|c| verify_quote(&c, &contradicting))
                            .unwrap_or(false);
                    let evidence = (!contradicting.is_empty())
                        .then(|| contradicting.clone())
                        .or((!rationale.is_empty()).then(|| rationale.clone()));
                    removed_ids.insert(id);
                    pending_removes.push(PendingRemove {
                        id,
                        rationale,
                        evidence,
                        contradiction_ok,
                    });
                }
                _ => {} // unknown op kind → drop
            }
        }

        // drop any op whose Temp parent has NO surviving Add (review finding 1:
        // PASS 1 registered coined temps unconditionally, so a move/add onto a
        // temp whose Add was later dropped — out-of-fence parent, empty name —
        // survived; at accept its Temp resolves to None and the node escapes to
        // the map ROOT, out of its fence). A dangling temp-parent op is dropped.
        let surviving_temps: HashSet<String> = staged
            .iter()
            .filter_map(|(_, op, _, _)| match op {
                DiffOp::Add { temp, .. } => Some(temp.clone()),
                _ => None,
            })
            .collect();
        staged.retain(|(_, op, _, _)| match op {
            DiffOp::Add {
                parent: PartRef::Temp(t),
                ..
            }
            | DiffOp::Move {
                parent: PartRef::Temp(t),
                ..
            } => surviving_temps.contains(t),
            _ => true,
        });
        // moved_out entries whose temp destination didn't survive are stale.
        moved_out
            .retain(|_, dest| !matches!(dest, PartRef::Temp(t) if !surviving_temps.contains(t)));

        // CYCLE detection over the PROJECTED forest of real AND temp nodes
        // (review finding 3: a cycle can form THROUGH a fresh area — add T under
        // X, then move X under T). A node in a cycle vanishes from build_tree, so
        // FLAG every move whose source sits in a projected cycle — it can never
        // ride Accept-all. `TempParent` follows a temp's own Add parent.
        #[derive(Clone, PartialEq, Eq, Hash)]
        enum Node {
            Real(PartId),
            Temp(String),
        }
        let mut cur_parent: HashMap<PartId, PartId> = HashMap::new();
        for (pid, n) in nodes {
            for c in &n.children {
                cur_parent.insert(*c, *pid);
            }
        }
        // a temp's Add parent (for following a cycle through a fresh node).
        let temp_parent: HashMap<String, PartRef> = staged
            .iter()
            .filter_map(|(_, op, _, _)| match op {
                DiffOp::Add { temp, parent, .. } => Some((temp.clone(), parent.clone())),
                _ => None,
            })
            .collect();
        let proj_parent = |n: &Node| -> Option<Node> {
            match n {
                Node::Real(id) => match moved_out.get(id) {
                    Some(PartRef::Id(p)) => Some(Node::Real(*p)),
                    Some(PartRef::Temp(t)) => Some(Node::Temp(t.clone())),
                    Some(PartRef::Root) => None,
                    None => cur_parent.get(id).map(|p| Node::Real(*p)),
                },
                Node::Temp(t) => match temp_parent.get(t) {
                    Some(PartRef::Id(p)) => Some(Node::Real(*p)),
                    Some(PartRef::Temp(t2)) => Some(Node::Temp(t2.clone())),
                    _ => None,
                },
            }
        };
        let cap = nodes.len() + surviving_temps.len() + 1;
        let in_cycle = |start: &Node| -> bool {
            let mut cur = proj_parent(start);
            let mut steps = 0;
            while let Some(c) = cur {
                if c == *start {
                    return true;
                }
                steps += 1;
                if steps > cap {
                    return true;
                }
                cur = proj_parent(&c);
            }
            false
        };
        for (phase, op, _ev, flg) in staged.iter_mut() {
            if *phase == 1 {
                if let DiffOp::Move { id, .. } = op {
                    if in_cycle(&Node::Real(*id)) {
                        *flg = true;
                    }
                }
            }
        }

        // finalize removes: a Remove is FLAGGED when it has no rationale, OR it
        // still has children that were NEITHER moved out NOR removed in this same
        // changeset AND it carries no verified contradiction. An EMPTIED node
        // (all children moved/removed) with a rationale is a clean dissolve/attic
        // and is not flagged on childful grounds. The orphan-proof store promotes
        // any survivor regardless — the flag just asks the user to vouch.
        for pr in pending_removes {
            let remaining = nodes
                .get(&pr.id)
                .map(|n| {
                    n.children.iter().any(|c| {
                        let moved =
                            matches!(moved_out.get(c), Some(dest) if *dest != PartRef::Id(pr.id));
                        let removed = removed_ids.contains(c);
                        !moved && !removed
                    })
                })
                .unwrap_or(false);
            let flagged = pr.rationale.is_empty() || (remaining && !pr.contradiction_ok);
            staged.push((2, DiffOp::Remove { id: pr.id }, pr.evidence, flagged));
        }

        // emit in phases: Adds first (so a Move's coined-temp parent exists),
        // then rename/set_kind, then Moves (TOPOLOGICALLY ordered — see below),
        // then Removes last (so a husk's children are moved out before it is
        // deleted). `accept_diff_from` applies ops in the given order WITHOUT
        // reordering.
        let mut adds: Vec<StagedOp> = Vec::new();
        let mut moves: Vec<StagedOp> = Vec::new();
        let mut other1: Vec<StagedOp> = Vec::new(); // rename/set_kind
        let mut removes: Vec<StagedOp> = Vec::new();
        for st in staged {
            match (&st.0, &st.1) {
                (0, _) => adds.push(st),
                (1, DiffOp::Move { .. }) => moves.push(st),
                (2, _) => removes.push(st),
                _ => other1.push(st),
            }
        }
        let emit = |st: StagedOp, out: &mut AgenticResult| {
            out.ops.push(st.1);
            out.evidence.push(st.2);
            out.flagged.push(st.3);
        };
        // Adds: topo-order chained temps — an add parented under another add's
        // temp must follow it, else its parent resolves to None → a root orphan.
        let mut emitted: HashSet<String> = HashSet::new();
        while !adds.is_empty() {
            let before = emitted.len();
            let mut i = 0;
            while i < adds.len() {
                let ready = match &adds[i].1 {
                    DiffOp::Add {
                        parent: PartRef::Temp(t),
                        ..
                    } => emitted.contains(t),
                    _ => true,
                };
                if ready {
                    let st = adds.remove(i);
                    if let DiffOp::Add { temp, .. } = &st.1 {
                        emitted.insert(temp.clone());
                    }
                    emit(st, &mut out);
                } else {
                    i += 1;
                }
            }
            if emitted.len() == before {
                for st in adds.drain(..) {
                    emit(st, &mut out);
                }
            }
        }
        for st in other1 {
            emit(st, &mut out);
        }
        // Moves: TOPOLOGICALLY ordered so a globally-valid acyclic reorganization
        // never hits a TRANSIENT cycle at apply time (review finding 2: the store
        // skips a transiently-cyclic move, half-applying an otherwise-valid card).
        // A move of X under real P must apply AFTER P's own move settles.
        let moved_set: HashSet<PartId> = moves
            .iter()
            .filter_map(|st| match &st.1 {
                DiffOp::Move { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        let dest_dep = |st: &StagedOp| -> Option<PartId> {
            match &st.1 {
                // depends on the destination's move only if the destination is
                // itself a moved real node (a Temp dest was already added).
                DiffOp::Move {
                    parent: PartRef::Id(p),
                    ..
                } if moved_set.contains(p) => Some(*p),
                _ => None,
            }
        };
        let mut done_ids: HashSet<PartId> = HashSet::new();
        while !moves.is_empty() {
            let before = moves.len();
            let mut i = 0;
            while i < moves.len() {
                let ready = dest_dep(&moves[i])
                    .map(|p| done_ids.contains(&p))
                    .unwrap_or(true);
                if ready {
                    let st = moves.remove(i);
                    if let DiffOp::Move { id, .. } = &st.1 {
                        done_ids.insert(*id);
                    }
                    emit(st, &mut out);
                } else {
                    i += 1;
                }
            }
            if moves.len() == before {
                // a genuine move-cycle (already flagged); emit as-is — the store's
                // cycle guard skips the offending ops safely.
                for st in moves.drain(..) {
                    emit(st, &mut out);
                }
            }
        }
        for st in removes {
            emit(st, &mut out);
        }
        out
    }

    /// The temp id an `add` op coins (agent's `"new:areaX"`): its `id` (or
    /// `temp`) field, when a non-empty NON-numeric string. A numeric id on an add
    /// is a mistake (it would collide with a real row) and is ignored — the add
    /// still lands under a synthesized temp, just unreferenceable as a parent.
    fn add_temp_of(item: &serde_json::Value) -> Option<String> {
        let raw = item
            .get("id")
            .or_else(|| item.get("temp"))
            .and_then(|x| x.as_str())?
            .trim();
        if raw.is_empty() || raw.parse::<i64>().is_ok() {
            return None;
        }
        Some(raw.to_string())
    }

    // ----------------------------- the fence -------------------------------

    /// The fence (docs/019 ruling 9). `subtree` = the id-set of the fence
    /// root's subtree INCLUDING the root itself (pointing at a node grants
    /// editing the node itself — the Tech husk must be removable by a rework
    /// pointed at Tech). `whole_map` = the seed / re-ground fence
    /// (scope_part_id NULL): every existing id is in scope and Root parents are
    /// granted. New nodes (`PartRef::Temp`) are always in-fence (born inside
    /// this changeset). Returns false for any op targeting a part OUTSIDE the
    /// subtree — the caller rejects/flags it loudly rather than applying it.
    pub fn op_in_fence(op: &DiffOp, subtree: &HashSet<PartId>, whole_map: bool) -> bool {
        let parent_ok = |p: &PartRef| match p {
            PartRef::Temp(_) => true,
            PartRef::Root => whole_map,
            PartRef::Id(id) => whole_map || subtree.contains(id),
        };
        let id_ok = |id: &PartId| whole_map || subtree.contains(id);
        match op {
            DiffOp::Add { parent, .. } => parent_ok(parent),
            DiffOp::Move { id, parent, .. } => id_ok(id) && parent_ok(parent),
            DiffOp::Remove { id } => id_ok(id),
            DiffOp::Rename { id, .. } => id_ok(id),
            DiffOp::SetDetail { id, .. } => id_ok(id),
            DiffOp::SetKind { id, .. } => id_ok(id),
            DiffOp::SetStatus { id, .. } => id_ok(id),
        }
    }

    // ---------------------- misc pure helpers + live path ------------------

    /// A repo-relative path is safe to read for verification iff it is
    /// non-empty, not absolute, and has no `..`/root/prefix escape — so a
    /// model-supplied `source_file` can never read outside the project root.
    pub fn is_safe_repo_rel(rel: &str) -> bool {
        use std::path::Component;
        !rel.is_empty()
            && !rel.starts_with('/')
            && !Path::new(rel).components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
    }

    /// days-since-Unix-epoch → ISO `YYYY-MM-DD` (UTC, proleptic Gregorian —
    /// Hinnant's civil_from_days). No chrono dependency.
    pub fn days_to_iso(days: u64) -> String {
        let z = days as i64 + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097; // [0, 146096]
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
        let mp = (5 * doy + 2) / 153; // [0, 11]
        let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
        let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
        let year = if m <= 2 { y + 1 } else { y };
        format!("{year:04}-{m:02}-{d:02}")
    }

    fn today_iso() -> String {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        days_to_iso(secs / 86_400)
    }

    /// The live agentic seed / re-ground (WHOLE MAP, scope NULL). Blocking +
    /// spends the map-intel ledger; run OFF the UI thread. NOT unit-tested (real
    /// CLI + quota) — gated behind an #[ignore] live test. `existing_tree` =
    /// None/empty for a fresh seed, or the serialized current tree for a
    /// re-ground DELTA (the prompt selector picks the recipe). Everything it
    /// composes/parses is covered by the pure tests.
    pub fn seed_map_agentic(
        root: &Path,
        existing_tree: Option<&str>,
        intent: Option<&str>,
        transcript_out: &mut String,
    ) -> Result<AgenticResult, String> {
        let digest = project_digest(root);
        let git_facts = format_git_facts(&collect_git_facts(root, 5));
        let mut prompt = seed_prompt_for(&digest, &git_facts, existing_tree);
        // a whole-map re-ground can carry the user's own words (the cmd-bar
        // "reorganize by product area" path) — appended so the model does THAT,
        // never a generic re-scan.
        if let Some(t) = intent.map(str::trim).filter(|t| !t.is_empty()) {
            prompt.push_str(&format!("\n\nUSER'S INTENT (do exactly this): {t}\n"));
        }
        let stdout = run_claude_agent(
            &prompt,
            root,
            STRUCTURAL_MODEL,
            &["Read", "Glob", "Grep"],
            40,
            600,
            transcript_out,
        )?;
        let text = extract_result_text(&stdout).ok_or("no result text in agent output")?;
        let today = today_iso();
        Ok(parse_agentic_nodes(&text, None, &today, &repo_reader(root)))
    }

    /// The live agentic EXPAND / REWORK (FENCED to `fence_root`, scope =
    /// fence_root). Blocking + spends the map-intel ledger; run OFF the UI
    /// thread. The parser parents every proposed node under `fence_root` (or a
    /// new temp), and a defensive `op_in_fence` sweep FLAGS anything that would
    /// escape the subtree (never applied silently). `node_path`/`node_detail`/
    /// `taxonomy_note` ground the prompt. NOT unit-tested (real CLI + quota).
    pub fn expand_subtree_agentic(
        root: &Path,
        fence_root: PartId,
        node_path: &str,
        node_detail: &str,
        taxonomy_note: &str,
        transcript_out: &mut String,
    ) -> Result<AgenticResult, String> {
        let git_facts = format_git_facts(&collect_git_facts(root, 3));
        let prompt = expand_agent_prompt(node_path, node_detail, taxonomy_note, &git_facts);
        let stdout = run_claude_agent(
            &prompt,
            root,
            STRUCTURAL_MODEL,
            &["Read", "Glob", "Grep"],
            30,
            600,
            transcript_out,
        )?;
        let text = extract_result_text(&stdout).ok_or("no result text in agent output")?;
        let today = today_iso();
        let mut r = parse_agentic_nodes(&text, Some(fence_root), &today, &repo_reader(root));
        // the fence is structurally satisfied by the Add-only parser (every op
        // parents under fence_root or a temp), but enforce it anyway: an op that
        // somehow targets OUTSIDE the subtree is FLAGGED, never applied silently.
        let subtree: HashSet<PartId> = std::iter::once(fence_root).collect();
        for (i, op) in r.ops.iter().enumerate() {
            if !op_in_fence(op, &subtree, false) {
                r.flagged[i] = true;
            }
        }
        Ok(r)
    }

    /// The live agentic REWORK (docs/019 slice 2b — FENCED restructure). Like
    /// `expand_subtree_agentic` but the FULL op whitelist: the agent may Move /
    /// Rename / SetKind / Remove existing nodes as well as Add, to reorganize the
    /// subtree per the user's intent. `subtree_json` / `subtree_ids` / `nodes`
    /// come from `serialize_subtree_for_rework` (built under the store lock by
    /// the caller); the parser fences every op to that subtree and a defensive
    /// `op_in_fence` sweep FLAGS anything that still escapes (never applied
    /// silently). Blocking + spends the map-intel ledger; run OFF the UI thread.
    /// NOT unit-tested (real CLI + quota) — the parse/fence logic is.
    #[allow(clippy::too_many_arguments)]
    pub fn rework_subtree_agentic(
        root: &Path,
        fence_root: PartId,
        node_path: &str,
        node_detail: &str,
        taxonomy_note: &str,
        subtree_json: &str,
        subtree_ids: &HashSet<PartId>,
        nodes: &HashMap<PartId, SubtreeNode>,
        transcript_out: &mut String,
    ) -> Result<AgenticResult, String> {
        let git_facts = format_git_facts(&collect_git_facts(root, 3));
        let prompt = rework_agent_prompt(
            node_path,
            node_detail,
            taxonomy_note,
            subtree_json,
            &git_facts,
        );
        let stdout = run_claude_agent(
            &prompt,
            root,
            STRUCTURAL_MODEL,
            &["Read", "Glob", "Grep"],
            40,
            600,
            transcript_out,
        )?;
        let text = extract_result_text(&stdout).ok_or("no result text in agent output")?;
        let today = today_iso();
        let mut r = parse_agentic_restructure(
            &text,
            fence_root,
            subtree_ids,
            nodes,
            &today,
            &repo_reader(root),
        );
        // the parser already drops out-of-fence ops; enforce the fence AGAIN
        // (mirrors expand_subtree_agentic): an op that somehow targets OUTSIDE
        // the subtree is FLAGGED, never applied silently.
        let mut fence = subtree_ids.clone();
        fence.insert(fence_root);
        for (i, op) in r.ops.iter().enumerate() {
            if !op_in_fence(op, &fence, false) {
                r.flagged[i] = true;
            }
        }
        Ok(r)
    }

    /// A confined repo file reader (relative-to-root, `is_safe_repo_rel` gated)
    /// for the evidence verifier — shared by seed / re-ground / expand.
    fn repo_reader(root: &Path) -> impl Fn(&str) -> Option<String> {
        let root = root.to_path_buf();
        move |rel: &str| -> Option<String> {
            if is_safe_repo_rel(rel) {
                std::fs::read_to_string(root.join(rel)).ok()
            } else {
                None
            }
        }
    }

// ---------------------------------------------------------------------------
// docs/019 slice 2 FOUNDATIONS — cartographer pure-fn tests. Live agentic run
// isn't unit-testable (real CLI + quota), so it's a single #[ignore] test; the
// digest/prompt/verifier/parse/fence logic is all covered here.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator_store::{DiffOp, Kind, Lifecycle, Part, PartId, PartRef};
    use std::collections::{HashMap, HashSet};
    use std::path::Path;

    #[test]
    fn find_iso_date_rejects_impossible_calendar_dates() {
        // real dates are found (even embedded in surrounding text)
        assert_eq!(
            find_iso_date("shipped 2026-07-04 per the log").as_deref(),
            Some("2026-07-04")
        );
        assert_eq!(find_iso_date("2024-02-29").as_deref(), Some("2024-02-29")); // leap day
        assert_eq!(find_iso_date("2026-04-30").as_deref(), Some("2026-04-30"));
        // impossible dates are rejected — the bug was 2026-02-30 passing and
        // granting a bogus lifecycle=done via a dated ship claim.
        assert_eq!(find_iso_date("2026-02-30"), None);
        assert_eq!(find_iso_date("2026-04-31"), None); // April has 30
        assert_eq!(find_iso_date("2025-02-29"), None); // 2025 is not a leap year
        assert_eq!(find_iso_date("2026-13-01"), None); // month 13
        assert_eq!(find_iso_date("2026-00-10"), None); // month 0
        assert_eq!(find_iso_date("2026-01-00"), None); // day 0
    }

    #[test]
    fn verify_quote_matches_normalizes_and_misses() {
        // verbatim substring (≥16 normalized chars — a real citation phrase)
        assert!(verify_quote(
            "the quick brown fox jumps over",
            "quick brown fox jumps"
        ));
        // whitespace-normalized on BOTH sides (runs collapse, ends trim)
        assert!(verify_quote(
            "the  quick\n  brown\tfox jumps",
            "quick brown fox jumps"
        ));
        // a paraphrase (not a verbatim span) MUST miss → the op gets flagged
        assert!(!verify_quote(
            "the quick brown fox jumps",
            "quick red fox leaps here"
        ));
        // empty / whitespace never verifies
        assert!(!verify_quote("anything at all here now", ""));
        assert!(!verify_quote("anything here at all now", "   "));
        // TRIVIAL short quotes are NOT specific enough — flagged, not trusted
        // (review: "the"/"fn " verified every file, defeating the gate).
        assert!(!verify_quote("the codebase and the fn main here", "the"));
        assert!(!verify_quote("fn main() { let x = 1; }", "fn "));
        assert!(
            !verify_quote("owns our codebase and CI here", "codebase"),
            "8 chars < 16 floor"
        );
        // a distinctive identifier over the floor still verifies
        assert!(verify_quote(
            "calls reconcile_staleness on open",
            "reconcile_staleness"
        ));
    }

    #[test]
    fn dated_ship_claim_gate() {
        let today = "2026-07-04";
        // past-dated ship word → allowed
        assert!(quote_has_dated_ship_claim(
            "Shipped 2026-06-21 the parser",
            today
        ));
        assert!(quote_has_dated_ship_claim("landed 2026-01-02", today));
        // future date → not a past-dated claim
        assert!(!quote_has_dated_ship_claim("shipped 2099-01-01", today));
        // ship word but no date
        assert!(!quote_has_dated_ship_claim("shipped it last week", today));
        // date but no ship word
        assert!(!quote_has_dated_ship_claim(
            "2026-06-21 planning meeting",
            today
        ));
        // NEGATED / roadmap claims are not ship claims (review finding 5)
        assert!(!quote_has_dated_ship_claim(
            "not done yet — target 2020-01-01",
            today
        ));
        assert!(!quote_has_dated_ship_claim(
            "will be shipped by 2020-01-01",
            today
        ));
        assert!(!quote_has_dated_ship_claim(
            "TODO: land this by 2020-01-01",
            today
        ));
        // garbage calendar values are not dates (review finding 7)
        assert!(!quote_has_dated_ship_claim("shipped 2026-13-45", today));
        assert!(!quote_has_dated_ship_claim("completed 2026-00-00", today));
    }

    #[test]
    fn days_to_iso_known_dates() {
        assert_eq!(days_to_iso(0), "1970-01-01");
        // 1970-01-01 → 2000-01-01 is 30*365 + 7 leap days = 10957 days
        assert_eq!(days_to_iso(10957), "2000-01-01");
    }

    #[test]
    fn map_intel_ledger_is_separate_and_hourly() {
        // empty ledger → allowed; cap is the small structural cap (6)
        assert_eq!(MAP_INTEL_HOURLY_CAP, 6);
        assert!(map_intel_rate_allows(&[], 1000, MAP_INTEL_HOURLY_CAP));
        // 5 runs in the trailing hour → one more allowed; 6 → not
        assert!(map_intel_rate_allows(
            &[1000u64; 5],
            1000,
            MAP_INTEL_HOURLY_CAP
        ));
        assert!(!map_intel_rate_allows(
            &[1000u64; 6],
            1000,
            MAP_INTEL_HOURLY_CAP
        ));
        // runs older than an hour don't count
        assert!(map_intel_rate_allows(
            &[0u64; 6],
            5000,
            MAP_INTEL_HOURLY_CAP
        ));
    }

    #[test]
    fn safe_repo_rel_confines_reads() {
        assert!(is_safe_repo_rel("docs/019-map-v3-owned-brain.md"));
        assert!(is_safe_repo_rel(
            "app/crates/orchestrator-gui/src/extract.rs"
        ));
        assert!(!is_safe_repo_rel(""));
        assert!(!is_safe_repo_rel("/etc/passwd"));
        assert!(!is_safe_repo_rel("../secret"));
        assert!(!is_safe_repo_rel("app/../../etc/passwd"));
    }

    #[test]
    fn seed_prompt_is_grammar_not_preset() {
        let p = seed_agent_prompt("DIGEST-HERE", "GITFACTS-HERE");
        assert!(
            p.contains("DIGEST-HERE") && p.contains("GITFACTS-HERE"),
            "digest + git facts injected"
        );
        assert!(
            !p.contains("Product, Users, Growth"),
            "no preset taxonomy, ever"
        );
        assert!(
            p.contains("GRAMMAR") && p.contains("4-9"),
            "a grammar of 4-9 roots"
        );
        assert!(p.contains("from code"), "code quarantined under ONE area");
        assert!(p.contains("VERBATIM"), "demands a verbatim quote");
        assert!(
            p.contains("source_file") && p.contains("source_quote"),
            "evidence pair named"
        );
        assert!(p.contains("repo files only"), "notes are not evidence");
        assert!(
            p.contains("{\"nodes\":"),
            "output shape the parser consumes"
        );
    }

    #[test]
    fn seed_prompt_selector_picks_fresh_vs_delta() {
        // empty / whitespace existing tree → the FRESH seed prompt.
        let fresh = seed_prompt_for("DIG", "GF", None);
        assert!(
            fresh.contains("PRODUCT ANATOMY") && !fresh.contains("CURRENT MAP"),
            "fresh seed, no existing map"
        );
        assert_eq!(
            seed_prompt_for("DIG", "GF", Some("   ")),
            fresh,
            "whitespace tree is still fresh"
        );
        // a real serialized tree → the additive DELTA (re-ground) prompt.
        let delta = seed_prompt_for("DIG", "GF", Some("[1] Sync — the sync area (todo)\n"));
        assert!(
            delta.contains("RE-GROUNDING") && delta.contains("CURRENT MAP"),
            "delta shows the current map"
        );
        assert!(
            delta.contains("[1] Sync"),
            "the serialized tree is injected"
        );
        assert!(
            delta.contains("only what's missing") || delta.contains("MISSING"),
            "additive framing"
        );
        // both recipes keep the evidence contract + parser shape + digest.
        for p in [&fresh, &delta] {
            assert!(
                p.contains("VERBATIM") && p.contains("source_file") && p.contains("{\"nodes\":")
            );
            assert!(p.contains("DIG") && p.contains("GF"));
            assert!(
                !p.contains("Product, Users, Growth"),
                "no preset taxonomy, ever"
            );
        }
    }

    #[test]
    fn expand_prompt_fences_and_injects_taxonomy() {
        let p = expand_agent_prompt(
            "Tech > Parser",
            "the evidence-or-drop parser",
            "organize by product area",
            "GF",
        );
        assert!(
            p.contains("Tech > Parser") && p.contains("the evidence-or-drop parser"),
            "node context"
        );
        assert!(
            p.contains("organize by product area") && p.contains("MAP GRAMMAR"),
            "taxonomy_note injected"
        );
        assert!(
            p.contains("FENCED") && p.contains("UNDER it"),
            "the fence is stated loudly"
        );
        assert!(
            p.contains("VERBATIM") && p.contains("{\"nodes\":"),
            "same evidence contract + shape"
        );
        assert!(p.contains("GF"), "git facts injected");
        // no taxonomy_note → no grammar line at all
        let q = expand_agent_prompt("X", "d", "  ", "");
        assert!(!q.contains("MAP GRAMMAR"));
    }

    #[test]
    fn git_facts_formatter() {
        assert_eq!(
            format_git_facts(&[]),
            "",
            "no repo → empty block (prompt degrades)"
        );
        let f = format_git_facts(&[
            (
                "app".into(),
                vec![
                    "2026-07-03 extract: PLUMBING_MODEL const".into(),
                    "2026-07-01 earlier work".into(),
                ],
            ),
            ("docs".into(), vec!["2026-07-02 standup-as-timeline".into()]),
        ]);
        assert!(f.starts_with("GIT ACTIVITY"));
        assert!(f.contains("app/") && f.contains("2026-07-03 extract: PLUMBING_MODEL const"));
        assert!(f.contains("docs/") && f.contains("2026-07-02 standup-as-timeline"));
    }

    // a repo file-reader: only "README.md" resolves, to a fixed body.
    fn fake_reader(body: &'static str) -> impl Fn(&str) -> Option<String> {
        move |rel: &str| (rel == "README.md").then(|| body.to_string())
    }

    #[test]
    fn parse_pipeline_clean_flagged_and_done_coercion() {
        let readme = "The Sync engine keeps replicas consistent across nodes.\n\
                      Shipped 2026-06-21 conflict resolver landed clean.\n\
                      we plan to finish this cleanup soon.";
        let read = fake_reader(readme);
        let json = r#"{"nodes":[
            {"name":"Sync","detail":"the sync area","kind":"area","lifecycle":"todo","anchors":["src/sync/**"],
             "source_file":"README.md","source_quote":"keeps replicas consistent","rationale":"core area","children":[
                {"name":"Conflict resolver","detail":"","kind":"task","lifecycle":"done",
                 "source_file":"README.md","source_quote":"Shipped 2026-06-21 conflict resolver","rationale":"r"},
                {"name":"Cleanup","detail":"","kind":"task","lifecycle":"done",
                 "source_file":"README.md","source_quote":"we plan to finish this cleanup","rationale":"r2"},
                {"name":"Ghost","detail":"","kind":"task","lifecycle":"todo",
                 "source_file":"missing.rs","source_quote":"text that is in no file","rationale":"r3"}
            ]}
        ]}"#;
        let r = parse_agentic_nodes(json, None, "2026-07-04", &read);
        assert_eq!(r.ops.len(), 4, "Sync + 3 children");
        // clean vs flagged: only Ghost (file missing) is flagged
        assert_eq!(r.flagged, vec![false, false, false, true]);
        assert_eq!(r.verified_count(), 3);
        assert_eq!(r.flag_count(), 1);

        // Sync: an AREA at whole-map root, verified, provenance rides the Add
        match &r.ops[0] {
            DiffOp::Add {
                parent,
                name,
                kind,
                lifecycle,
                source_file,
                source_quote,
                rationale,
                anchors,
                ..
            } => {
                assert_eq!(*parent, PartRef::Root);
                assert_eq!(name, "Sync");
                assert_eq!(*kind, Kind::Area);
                assert_eq!(*lifecycle, Lifecycle::Todo);
                assert_eq!(source_file.as_deref(), Some("README.md"));
                assert_eq!(source_quote.as_deref(), Some("keeps replicas consistent"));
                assert_eq!(rationale.as_deref(), Some("core area"));
                assert_eq!(anchors, &vec!["src/sync/**".to_string()]);
            }
            other => panic!("expected Add, got {other:?}"),
        }
        // children are fenced under Sync's temp ("g1")
        match &r.ops[1] {
            DiffOp::Add {
                parent,
                name,
                lifecycle,
                ..
            } => {
                assert_eq!(*parent, PartRef::Temp("g1".into()));
                assert_eq!(name, "Conflict resolver");
                // done WITH a dated ship-claim in the (verified) quote → Done
                assert_eq!(*lifecycle, Lifecycle::Done);
            }
            other => panic!("expected Add, got {other:?}"),
        }
        match &r.ops[2] {
            DiffOp::Add {
                name, lifecycle, ..
            } => {
                assert_eq!(name, "Cleanup");
                // done WITHOUT a ship-claim (verified quote, but no ship word) → coerced to todo
                assert_eq!(*lifecycle, Lifecycle::Todo);
            }
            other => panic!("expected Add, got {other:?}"),
        }
        // a FLAGGED op is KEPT and still carries its (unverified) provenance
        match &r.ops[3] {
            DiffOp::Add {
                name,
                source_file,
                source_quote,
                ..
            } => {
                assert_eq!(name, "Ghost");
                assert_eq!(source_file.as_deref(), Some("missing.rs"));
                assert_eq!(source_quote.as_deref(), Some("text that is in no file"));
            }
            other => panic!("expected Add, got {other:?}"),
        }
        // evidence line is the verbatim quote (present even for the flagged op)
        assert_eq!(r.evidence[0].as_deref(), Some("keeps replicas consistent"));
        assert_eq!(r.evidence[3].as_deref(), Some("text that is in no file"));
    }

    #[test]
    fn parse_fences_top_level_under_scope_and_flags_evidenceless() {
        let read = fake_reader("");
        // fence_root Some(7): a top-level node lands UNDER id 7 (expand/rework)
        let json = r#"{"nodes":[{"name":"Child","detail":"","kind":"task","lifecycle":"todo","source_file":"","source_quote":"","rationale":""}]}"#;
        let r = parse_agentic_nodes(json, Some(7), "2026-07-04", &read);
        assert_eq!(r.ops.len(), 1);
        assert!(matches!(
            &r.ops[0],
            DiffOp::Add {
                parent: PartRef::Id(7),
                ..
            }
        ));
        // no evidence at all → flagged (evidence-or-flag), evidence line None
        assert_eq!(r.flagged, vec![true]);
        assert_eq!(r.evidence[0], None);
    }

    #[test]
    fn parse_empty_and_malformed_and_nameless() {
        let read = fake_reader("");
        assert!(
            parse_agentic_nodes(r#"{"nodes":[]}"#, None, "2026-07-04", &read)
                .ops
                .is_empty()
        );
        assert!(parse_agentic_nodes("{}", None, "2026-07-04", &read)
            .ops
            .is_empty());
        assert!(
            parse_agentic_nodes("not json at all", None, "2026-07-04", &read)
                .ops
                .is_empty()
        );
        assert!(
            parse_agentic_nodes(r#"{"nodes":"nope"}"#, None, "2026-07-04", &read)
                .ops
                .is_empty()
        );
        // a nameless node is skipped (no id to fence its children under)
        let r = parse_agentic_nodes(
            r#"{"nodes":[{"name":"","source_quote":"x"}]}"#,
            None,
            "2026-07-04",
            &read,
        );
        assert!(r.ops.is_empty());
    }

    #[test]
    fn fence_enforcement_helper() {
        // subtree rooted at 10 (root INCLUDED), children 11 + 12
        let subtree: HashSet<PartId> = [10, 11, 12].into_iter().collect();
        let add = |p: PartRef| DiffOp::Add {
            temp: "t".into(),
            parent: p,
            name: "n".into(),
            detail: String::new(),
            lifecycle: Lifecycle::Todo,
            anchors: vec![],
            kind: Kind::Task,
            detail_md: None,
            sort_order: None,
            source_file: None,
            source_quote: None,
            rationale: None,
        };
        // FENCED (whole_map = false):
        assert!(
            op_in_fence(&add(PartRef::Id(10)), &subtree, false),
            "Add under the fence root is in-fence"
        );
        assert!(
            op_in_fence(&add(PartRef::Temp("x".into())), &subtree, false),
            "a NEW node is always in-fence"
        );
        assert!(
            !op_in_fence(&add(PartRef::Id(99)), &subtree, false),
            "Add under an OUTSIDE parent is out"
        );
        assert!(
            !op_in_fence(&add(PartRef::Root), &subtree, false),
            "Root parent is out (not whole-map)"
        );
        // the fence root itself is in scope (removable / renamable)
        assert!(op_in_fence(&DiffOp::Remove { id: 10 }, &subtree, false));
        assert!(op_in_fence(
            &DiffOp::Rename {
                id: 12,
                name: "x".into(),
                detail: String::new()
            },
            &subtree,
            false
        ));
        assert!(op_in_fence(
            &DiffOp::SetStatus {
                id: 11,
                lifecycle: Lifecycle::Done,
                source: orchestrator_store::StatusSource::Agent
            },
            &subtree,
            false
        ));
        // a Move must have BOTH endpoints in-fence
        assert!(op_in_fence(
            &DiffOp::Move {
                id: 11,
                parent: PartRef::Id(10),
                sort_order: 1.0
            },
            &subtree,
            false
        ));
        assert!(
            !op_in_fence(
                &DiffOp::Move {
                    id: 11,
                    parent: PartRef::Id(99),
                    sort_order: 1.0
                },
                &subtree,
                false
            ),
            "move OUT of the fence is rejected"
        );
        // ops targeting an id OUTSIDE the subtree are rejected/flagged
        assert!(!op_in_fence(&DiffOp::Remove { id: 99 }, &subtree, false));
        assert!(!op_in_fence(
            &DiffOp::SetKind {
                id: 99,
                kind: Kind::Area
            },
            &subtree,
            false
        ));
        // whole-map (seed/re-ground): Root parents + any existing id are granted
        assert!(op_in_fence(&add(PartRef::Root), &subtree, true));
        assert!(op_in_fence(&DiffOp::Remove { id: 99 }, &subtree, true));
    }

    // ---- slice 2b: the machine RESTRUCTURE (rework) parser ----

    fn snode(name: &str, detail: &str, children: &[PartId]) -> SubtreeNode {
        SubtreeNode {
            name: name.into(),
            detail: detail.into(),
            children: children.to_vec(),
        }
    }

    // a subtree rooted at 10 with two leaf children 11, 12 (11 = "Parser").
    fn tech_subtree() -> (HashSet<PartId>, HashMap<PartId, SubtreeNode>) {
        let ids: HashSet<PartId> = [10, 11, 12].into_iter().collect();
        let mut nodes = HashMap::new();
        nodes.insert(10, snode("Tech", "the tech blob", &[11, 12]));
        nodes.insert(11, snode("Parser", "the trust gate", &[]));
        nodes.insert(12, snode("Store", "sqlite persistence", &[]));
        (ids, nodes)
    }

    #[test]
    fn rework_clean_regroup_add_new_area_plus_moves() {
        // the marquee: "reorganize by product area" → an Add(new area) + Moves of
        // the existing children under it. The `add` is emitted AFTER a move that
        // names its temp — pass 1 must still resolve it, and emission must reorder
        // the Add first so the temp exists before the Moves reference it.
        let read = fake_reader("The parser keeps replicas consistent across nodes.");
        let (ids, nodes) = tech_subtree();
        let json = r#"{"ops":[
            {"op":"move","id":11,"new_parent":"new:area1","rationale":"parser belongs under the compiler area"},
            {"op":"add","id":"new:area1","parent":10,"name":"Compiler","kind":"area","lifecycle":"todo","source_file":"README.md","source_quote":"keeps replicas consistent","rationale":"a real product area"},
            {"op":"move","id":12,"new_parent":"new:area1","rationale":"store belongs under the compiler area"}
        ]}"#;
        let r = parse_agentic_restructure(json, 10, &ids, &nodes, "2026-07-04", &read);
        assert_eq!(r.ops.len(), 3, "Add + 2 Moves");
        assert_eq!(
            r.flagged,
            vec![false, false, false],
            "verified add + moves-with-rationale are all clean"
        );
        // Add emits FIRST (phase 0) so its temp exists before the Moves.
        match &r.ops[0] {
            DiffOp::Add {
                temp,
                parent,
                name,
                kind,
                ..
            } => {
                assert_eq!(temp, "new:area1");
                assert_eq!(
                    *parent,
                    PartRef::Id(10),
                    "new area lands under the fence root"
                );
                assert_eq!(name, "Compiler");
                assert_eq!(*kind, Kind::Area);
            }
            other => panic!("expected Add, got {other:?}"),
        }
        // both Moves resolve their temp parent to PartRef::Temp, agent order kept.
        assert!(
            matches!(&r.ops[1], DiffOp::Move { id: 11, parent: PartRef::Temp(t), .. } if t == "new:area1")
        );
        assert!(
            matches!(&r.ops[2], DiffOp::Move { id: 12, parent: PartRef::Temp(t), .. } if t == "new:area1")
        );
    }

    #[test]
    fn rework_rejects_out_of_fence_ops() {
        let read = fake_reader("");
        let (ids, nodes) = tech_subtree();
        let json = r#"{"ops":[
            {"op":"move","id":99,"new_parent":10,"rationale":"drag an outside node in"},
            {"op":"move","id":11,"new_parent":99,"rationale":"push a fenced node out"},
            {"op":"rename","id":88,"name":"X","rationale":"rename an outside node"},
            {"op":"remove","id":77,"rationale":"remove an outside node"},
            {"op":"rename","id":12,"name":"Persistence","rationale":"clearer name"}
        ]}"#;
        let r = parse_agentic_restructure(json, 10, &ids, &nodes, "2026-07-04", &read);
        // every op referencing an id OUTSIDE the fence (99 target, 99 destination,
        // 88, 77) is DROPPED; only the in-fence rename of 12 survives.
        assert_eq!(r.ops.len(), 1);
        assert!(matches!(&r.ops[0], DiffOp::Rename { id: 12, name, .. } if name == "Persistence"));
    }

    #[test]
    fn rework_move_to_unknown_temp_is_dropped() {
        let read = fake_reader("");
        let (ids, nodes) = tech_subtree();
        // a move whose new_parent names a temp NO add coined would silently
        // escape to root in the store (resolve_ref → None) — it must be dropped.
        let json =
            r#"{"ops":[{"op":"move","id":11,"new_parent":"new:ghost","rationale":"nowhere"}]}"#;
        let r = parse_agentic_restructure(json, 10, &ids, &nodes, "2026-07-04", &read);
        assert!(
            r.ops.is_empty(),
            "a move to an uncoined temp is dropped, not escaped to root"
        );
    }

    #[test]
    fn rework_cyclic_move_pair_is_flagged() {
        let read = fake_reader("");
        let (ids, nodes) = tech_subtree();
        // a contradictory pair: 11 under 12 AND 12 under 11. Both pass the fence,
        // self-parent, and rationale checks; applied together they'd form a cycle
        // that VANISHES from build_tree. Both must be FLAGGED so neither rides
        // Accept-all. A lone reparent (11 under 12) stays clean — no cycle.
        let cyclic = r#"{"ops":[
            {"op":"move","id":11,"new_parent":12,"rationale":"a"},
            {"op":"move","id":12,"new_parent":11,"rationale":"b"}
        ]}"#;
        let r = parse_agentic_restructure(cyclic, 10, &ids, &nodes, "2026-07-04", &read);
        assert_eq!(r.ops.len(), 2);
        assert_eq!(
            r.flagged,
            vec![true, true],
            "a cycle-forming move pair is flagged, held out of Accept-all"
        );

        let lone = r#"{"ops":[{"op":"move","id":11,"new_parent":12,"rationale":"regroup"}]}"#;
        let r2 = parse_agentic_restructure(lone, 10, &ids, &nodes, "2026-07-04", &read);
        assert_eq!(r2.ops.len(), 1);
        assert_eq!(
            r2.flagged,
            vec![false],
            "a non-cyclic reparent with a rationale stays clean"
        );
    }

    #[test]
    fn rework_childful_remove_is_flagged() {
        let read = fake_reader("");
        let (ids, nodes) = tech_subtree();
        // remove the root husk while its children 11, 12 are NOT moved out and NOT
        // removed → still-childful → FLAGGED (user must vouch).
        let json = r#"{"ops":[{"op":"remove","id":10,"rationale":"obsolete husk"}]}"#;
        let r = parse_agentic_restructure(json, 10, &ids, &nodes, "2026-07-04", &read);
        assert_eq!(r.ops.len(), 1);
        assert!(matches!(&r.ops[0], DiffOp::Remove { id: 10 }));
        assert_eq!(
            r.flagged,
            vec![true],
            "a childful Remove with no cited contradiction is flagged"
        );
    }

    #[test]
    fn rework_emptied_remove_is_clean() {
        let read = fake_reader("");
        // deeper subtree: 10 → {11 → {13,14}, 12}. Move 13+14 out under 12, then
        // remove the emptied husk 11 → NOT flagged (a clean dissolve).
        let ids: HashSet<PartId> = [10, 11, 12, 13, 14].into_iter().collect();
        let mut nodes = HashMap::new();
        nodes.insert(10, snode("Root", "", &[11, 12]));
        nodes.insert(11, snode("Husk", "", &[13, 14]));
        nodes.insert(12, snode("Keep", "", &[]));
        nodes.insert(13, snode("A", "", &[]));
        nodes.insert(14, snode("B", "", &[]));
        let json = r#"{"ops":[
            {"op":"move","id":13,"new_parent":12,"rationale":"regroup"},
            {"op":"move","id":14,"new_parent":12,"rationale":"regroup"},
            {"op":"remove","id":11,"rationale":"husk emptied — its children moved out"}
        ]}"#;
        let r = parse_agentic_restructure(json, 10, &ids, &nodes, "2026-07-04", &read);
        assert_eq!(r.ops.len(), 3);
        // Removes emit LAST so survivors move out before the husk is deleted.
        assert!(matches!(&r.ops[2], DiffOp::Remove { id: 11 }));
        assert_eq!(
            r.flagged,
            vec![false, false, false],
            "emptied dissolve is clean"
        );
    }

    #[test]
    fn rework_childful_remove_with_verified_contradiction_is_clean() {
        // a childful Remove is allowed (unflagged) when it cites a CONTRADICTING
        // quote that verifies against a real repo file.
        let read = fake_reader("The legacy aspect migration was reverted and deleted for good.");
        let (ids, nodes) = tech_subtree();
        let json = r#"{"ops":[{"op":"remove","id":10,"rationale":"obsolete","contradicting_quote":"migration was reverted and deleted","source_file":"README.md"}]}"#;
        let r = parse_agentic_restructure(json, 10, &ids, &nodes, "2026-07-04", &read);
        assert_eq!(r.ops.len(), 1);
        assert_eq!(
            r.flagged,
            vec![false],
            "a cited+verified contradiction licenses a childful remove"
        );
        assert_eq!(
            r.evidence[0].as_deref(),
            Some("migration was reverted and deleted")
        );
    }

    #[test]
    fn rework_ops_without_rationale_are_flagged() {
        let read = fake_reader("");
        let (ids, nodes) = tech_subtree();
        let json = r#"{"ops":[
            {"op":"rename","id":11,"name":"NewName"},
            {"op":"move","id":12,"new_parent":11},
            {"op":"set_kind","id":11,"kind":"area"}
        ]}"#;
        let r = parse_agentic_restructure(json, 10, &ids, &nodes, "2026-07-04", &read);
        assert_eq!(r.ops.len(), 3);
        // rename + set_kind stay phase 1 alongside the move; every one lacks a
        // rationale, so every one is flagged.
        assert!(
            r.flagged.iter().all(|f| *f),
            "a restructure op with no rationale is flagged"
        );
        assert!(
            r.evidence.iter().all(|e| e.is_none()),
            "no rationale → no evidence line"
        );
    }

    #[test]
    fn rework_add_without_verified_quote_is_flagged() {
        let read = fake_reader("The real README body, entirely unrelated to the citation.");
        let (ids, nodes) = tech_subtree();
        let json = r#"{"ops":[
            {"op":"add","id":"new:g","parent":10,"name":"Ghost","kind":"task","lifecycle":"todo","source_file":"missing.rs","source_quote":"text that is in no file","rationale":"r"},
            {"op":"add","id":"new:h","parent":10,"name":"NoCite","kind":"task","lifecycle":"todo","rationale":"r"}
        ]}"#;
        let r = parse_agentic_restructure(json, 10, &ids, &nodes, "2026-07-04", &read);
        assert_eq!(r.ops.len(), 2);
        // both adds carry a rationale but neither verifies → both flagged.
        assert_eq!(r.flagged, vec![true, true]);
        // the flagged op is KEPT with its provenance quad riding the Add.
        assert!(
            matches!(&r.ops[0], DiffOp::Add { name, source_file: Some(sf), .. } if name == "Ghost" && sf == "missing.rs")
        );
    }

    #[test]
    fn rework_rename_preserves_current_one_liner() {
        // the store's empty-detail Rename WIPES detail; the parser must supply the
        // node's current one-liner so a pure rename keeps it.
        let read = fake_reader("");
        let (ids, nodes) = tech_subtree();
        let json = r#"{"ops":[{"op":"rename","id":11,"name":"Gate","rationale":"clearer name"}]}"#;
        let r = parse_agentic_restructure(json, 10, &ids, &nodes, "2026-07-04", &read);
        assert_eq!(
            r.ops[0],
            DiffOp::Rename {
                id: 11,
                name: "Gate".into(),
                detail: "the trust gate".into()
            }
        );
        assert_eq!(r.flagged, vec![false]);
    }

    #[test]
    fn rework_same_name_rename_is_dropped_as_noop() {
        let read = fake_reader("");
        let (ids, nodes) = tech_subtree();
        let json = r#"{"ops":[{"op":"rename","id":11,"name":"Parser","rationale":"same label"}]}"#;
        let r = parse_agentic_restructure(json, 10, &ids, &nodes, "2026-07-04", &read);
        assert!(
            r.ops.is_empty(),
            "same-name rename suggestions are review noise"
        );
    }

    #[test]
    fn rework_done_add_without_ship_claim_is_coerced_and_flagged() {
        let read = fake_reader("we plan to finish this cleanup soon in the store layer");
        let (ids, nodes) = tech_subtree();
        let json = r#"{"ops":[
            {"op":"add","id":"new:d","parent":10,"name":"Cleanup","kind":"task","lifecycle":"done","source_file":"README.md","source_quote":"we plan to finish this cleanup","rationale":"r"}
        ]}"#;
        let r = parse_agentic_restructure(json, 10, &ids, &nodes, "2026-07-04", &read);
        // verified quote, but no dated ship-claim → lifecycle coerced to todo AND
        // flagged (user vouches for the demotion).
        assert!(matches!(
            &r.ops[0],
            DiffOp::Add {
                lifecycle: Lifecycle::Todo,
                ..
            }
        ));
        assert_eq!(r.flagged, vec![true]);
    }

    #[test]
    fn rework_chained_temp_adds_emit_parent_before_child() {
        // the agent lists a CHILD area (new:b, parent new:a) BEFORE its parent
        // area (new:a). accept_diff applies in-order, so the parser must reorder
        // new:a ahead of new:b or new:b lands at root.
        let read = fake_reader("");
        let (ids, nodes) = tech_subtree();
        let json = r#"{"ops":[
            {"op":"add","id":"new:b","parent":"new:a","name":"Child","kind":"area","lifecycle":"todo","rationale":"under the parent area"},
            {"op":"add","id":"new:a","parent":10,"name":"Parent","kind":"area","lifecycle":"todo","rationale":"a top area"}
        ]}"#;
        let r = parse_agentic_restructure(json, 10, &ids, &nodes, "2026-07-04", &read);
        assert_eq!(r.ops.len(), 2);
        // new:a (Parent) MUST be emitted before new:b (Child).
        assert!(
            matches!(&r.ops[0], DiffOp::Add { temp, parent: PartRef::Id(10), .. } if temp == "new:a")
        );
        assert!(
            matches!(&r.ops[1], DiffOp::Add { temp, parent: PartRef::Temp(t), .. } if temp == "new:b" && t == "new:a")
        );
    }

    #[test]
    fn rework_empty_and_malformed_parse_cleanly() {
        let read = fake_reader("");
        let (ids, nodes) = tech_subtree();
        let p = |j: &str| parse_agentic_restructure(j, 10, &ids, &nodes, "2026-07-04", &read);
        assert!(p(r#"{"ops":[]}"#).ops.is_empty());
        assert!(p("{}").ops.is_empty());
        assert!(p("not json at all").ops.is_empty());
        assert!(p(r#"{"ops":"nope"}"#).ops.is_empty());
        // a nameless add + an unknown op kind are both dropped.
        assert!(p(r#"{"ops":[{"op":"add","id":"new:x","parent":10,"name":"","rationale":"r"},{"op":"frobnicate","id":11}]}"#).ops.is_empty());
    }

    fn spart(
        id: PartId,
        parent: Option<PartId>,
        name: &str,
        detail: &str,
        kind: Kind,
        order: f64,
    ) -> Part {
        Part {
            id,
            parent_id: parent,
            name: name.into(),
            detail: detail.into(),
            kind,
            sort_order: order,
            ..Part::default()
        }
    }

    #[test]
    fn serialize_subtree_confines_to_the_fence_and_orders_children() {
        // tree: 10 "Tech" {12 "Store"(order2), 11 "Parser"(order1)} + a SIBLING
        // root 20 "Growth" {21} that must NOT bleed into the fenced serialization.
        let parts = vec![
            spart(10, None, "Tech", "the tech blob", Kind::Area, 1.0),
            spart(11, Some(10), "Parser", "trust gate", Kind::Task, 1.0),
            spart(12, Some(10), "Store", "sqlite", Kind::Task, 2.0),
            spart(20, None, "Growth", "", Kind::Area, 2.0),
            spart(21, Some(20), "Launch", "", Kind::Task, 1.0),
        ];
        let (json, ids, nodes) = serialize_subtree_for_rework(&parts, 10);
        assert_eq!(
            ids,
            [10, 11, 12].into_iter().collect::<HashSet<PartId>>(),
            "only the fenced subtree"
        );
        assert!(
            !ids.contains(&20) && !ids.contains(&21),
            "a sibling root never bleeds in"
        );
        // children are ordered by sort_order (Parser 1.0 before Store 2.0).
        assert_eq!(nodes[&10].children, vec![11, 12]);
        assert_eq!(
            nodes[&11].detail, "trust gate",
            "one-liner captured for rename-preserve"
        );
        // the JSON carries real ids + kind + one_liner the agent reuses.
        assert!(
            json.contains("\"id\":10") && json.contains("\"id\":11") && json.contains("\"id\":12")
        );
        assert!(
            json.contains("\"kind\":\"area\"") && json.contains("\"one_liner\":\"trust gate\"")
        );
        assert!(!json.contains("Growth"), "out-of-fence names are absent");
    }

    #[test]
    fn rework_parse_round_trips_serialized_ids() {
        // end-to-end (no CLI): serialize a real tree, then parse a restructure
        // that references its serialized ids → a fenced changeset of Add + Moves.
        let parts = vec![
            spart(10, None, "Tech", "the tech blob", Kind::Area, 1.0),
            spart(11, Some(10), "Parser", "trust gate", Kind::Task, 1.0),
            spart(12, Some(10), "Store", "sqlite", Kind::Task, 2.0),
        ];
        let (_json, ids, nodes) = serialize_subtree_for_rework(&parts, 10);
        let read = fake_reader("the parser is the trust gate for every proposal");
        let ops = r#"{"ops":[
            {"op":"add","id":"new:eng","parent":10,"name":"Engine","kind":"area","lifecycle":"todo","source_file":"README.md","source_quote":"the trust gate for every proposal","rationale":"the runtime area"},
            {"op":"move","id":11,"new_parent":"new:eng","rationale":"parser is engine"},
            {"op":"move","id":12,"new_parent":"new:eng","rationale":"store is engine"}
        ]}"#;
        let r = parse_agentic_restructure(ops, 10, &ids, &nodes, "2026-07-04", &read);
        assert_eq!(r.ops.len(), 3);
        assert_eq!(r.flag_count(), 0, "all cited/rationaled → clean");
        assert!(matches!(
            &r.ops[0],
            DiffOp::Add {
                parent: PartRef::Id(10),
                ..
            }
        ));
        assert!(
            matches!(&r.ops[1], DiffOp::Move { id: 11, parent: PartRef::Temp(t), .. } if t == "new:eng")
        );
    }

    #[test]
    fn rework_move_onto_dropped_add_temp_is_not_emitted() {
        // review finding 1: an add is DROPPED (out-of-fence parent), but a move
        // referenced its coined temp — the move must NOT survive (it would
        // resolve to None at accept → escape to the map root, out of fence).
        let read = fake_reader("");
        let (ids, nodes) = tech_subtree();
        let json = r#"{"ops":[
            {"op":"add","id":"new:ghost","parent":99,"name":"Ghost","kind":"area","rationale":"x"},
            {"op":"move","id":11,"new_parent":"new:ghost","rationale":"onto the dropped add"}
        ]}"#;
        let r = parse_agentic_restructure(json, 10, &ids, &nodes, "2026-07-04", &read);
        // the add drops (parent 99 out of fence); the orphaning move drops too.
        assert!(
            r.ops.is_empty(),
            "dangling-temp move dropped, node never escapes to root"
        );
    }

    #[test]
    fn rework_cycle_through_a_fresh_area_is_flagged() {
        // review finding 3: add T under 11, then move 11 under T → 11 is its own
        // projected ancestor. The temp-destination cycle must be FLAGGED.
        let read = fake_reader("");
        let (ids, nodes) = tech_subtree();
        let json = r#"{"ops":[
            {"op":"add","id":"new:t","parent":11,"name":"Sub","kind":"area","rationale":"under parser"},
            {"op":"move","id":11,"new_parent":"new:t","rationale":"parser under its own child"}
        ]}"#;
        let r = parse_agentic_restructure(json, 10, &ids, &nodes, "2026-07-04", &read);
        let move_flagged = r
            .ops
            .iter()
            .zip(r.flagged.iter())
            .any(|(op, f)| matches!(op, DiffOp::Move { id: 11, .. }) && *f);
        assert!(
            move_flagged,
            "a cycle formed via a fresh area is flagged, never rides Accept-all"
        );
    }

    #[test]
    fn rework_moves_are_topologically_ordered() {
        // review finding 2: a valid acyclic reorg with a transient cycle in agent
        // order (move A→B, then B→R giving R→B→A) must EMIT in an order the store
        // applies fully — the dependent move (11 under 12) after its destination's
        // own move (12 under root).
        let read = fake_reader("");
        // subtree: root 10 → 12 → 11 (11 under 12 under 10).
        let ids: HashSet<PartId> = [10, 11, 12].into_iter().collect();
        let mut nodes = HashMap::new();
        nodes.insert(10, snode("Root", "", &[12]));
        nodes.insert(12, snode("Mid", "", &[11]));
        nodes.insert(11, snode("Leaf", "", &[]));
        // agent lists the dependent move FIRST: move 12 under 11 (transient
        // cycle), then move 11 under 10. Valid end state: 10→11, 10→12? No — the
        // agent wants 10→11→12: move 12 under 11, move 11 under 10.
        let json = r#"{"ops":[
            {"op":"move","id":12,"new_parent":11,"rationale":"mid under leaf"},
            {"op":"move","id":11,"new_parent":10,"rationale":"leaf to root"}
        ]}"#;
        let r = parse_agentic_restructure(json, 10, &ids, &nodes, "2026-07-04", &read);
        // neither is a genuine cycle → neither flagged; and the move of 11 (whose
        // destination 10 isn't moved) emits BEFORE the move of 12 (dest 11 IS
        // moved), so sequential apply never hits a transient cycle.
        assert_eq!(r.flag_count(), 0, "acyclic reorg, nothing flagged");
        let order: Vec<PartId> = r
            .ops
            .iter()
            .filter_map(|o| match o {
                DiffOp::Move { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(
            order,
            vec![11, 12],
            "the move whose destination is also moved emits last"
        );
    }

    // LIVE: one real agentic seed of the orchestrator repo (structural model, quota):
    //   cargo test -p orchestrator-gui --bin orchestrator -- --ignored seed_agentic_live
    #[test]
    #[ignore]
    fn seed_agentic_live() {
        let mut transcript = String::new();
        let r = seed_map_agentic(
            Path::new("/Users/me/local/orchestrator"),
            None,
            None,
            &mut transcript,
        )
        .expect("agentic seed");
        println!(
            "seeded {} ops ({} verified, {} flagged)",
            r.ops.len(),
            r.verified_count(),
            r.flag_count()
        );
        for (op, ev) in r.ops.iter().zip(r.evidence.iter()) {
            if let DiffOp::Add {
                name, source_file, ..
            } = op
            {
                println!("  + {name}  src={source_file:?}  ev={ev:?}");
            }
        }
        assert!(r.ops.len() >= 4, "expected several evidence-cited nodes");
        assert!(!transcript.is_empty(), "the audit transcript is captured");
    }

    // LIVE: one real agentic REWORK of a hand-built subtree (structural model, quota). Uses
    // the orchestrator repo as cwd so the agent's Read/Glob/Grep resolve, but a
    // synthetic fenced subtree so it has real ids to Move/Rename/Remove:
    //   cargo test -p orchestrator-gui --bin orchestrator -- --ignored rework_agentic_live
    #[test]
    #[ignore]
    fn rework_agentic_live() {
        let (ids, nodes) = tech_subtree();
        let (json, _, _) = {
            // reuse the real serializer over a synthetic tree so the prompt shape
            // matches production exactly.
            let parts = vec![
                spart(10, None, "Tech", "the code blob", Kind::Area, 1.0),
                spart(11, Some(10), "Parser", "the trust gate", Kind::Task, 1.0),
                spart(12, Some(10), "Store", "sqlite persistence", Kind::Task, 2.0),
            ];
            serialize_subtree_for_rework(&parts, 10)
        };
        let mut transcript = String::new();
        let r = rework_subtree_agentic(
            Path::new("/Users/me/local/orchestrator"),
            10,
            "Tech",
            "User's intent: reorganize by product area",
            "organized by top-level product areas",
            &json,
            &ids,
            &nodes,
            &mut transcript,
        )
        .expect("agentic rework");
        println!(
            "reworked {} ops ({} verified, {} flagged)",
            r.ops.len(),
            r.verified_count(),
            r.flag_count()
        );
        for op in &r.ops {
            println!("  {op:?}");
        }
        assert!(!transcript.is_empty(), "the audit transcript is captured");
    }
}
