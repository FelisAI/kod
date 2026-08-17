use super::*;
use orchestrator_store::{DiffOp, Kind, Lifecycle, Part, PartRef, StatusSource};
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Slice 2 (#10): the LIVING map — session summaries propose map updates as
// evidence-bearing DiffOps. The PARSER is the trust gate (evidence-or-drop):
// an LLM asked for updates always invents some, so every op must carry a
// verbatim quote and reference an id that exists; everything else is dropped.
// Empty ops is the EXPECTED common output. Allowed ops ONLY: SetStatus
// (source Agent), Add (todo/idea under an EXISTING parent), detail-append
// (a Rename that keeps the name). NEVER Remove, NEVER Move.
// ---------------------------------------------------------------------------

// NOTE: the `allow(dead_code)`s below exist because this is a bin crate and
// the GUI proposer worker (next slice, main.rs) hasn't wired these entry
// points yet — remove them when it does.

/// A proposed map update: each op paired with the verbatim evidence quote that
/// justifies it (rendered on the per-op review card).
pub struct MapProposal {
    pub ops: Vec<(DiffOp, String)>,
}

/// Serialize the CURRENT map for the proposer prompt — one line per node:
/// `[<id>] <Aspect> > <Node> — <detail> (<lifecycle>)`. Paths come from
/// `parent_id`; sibling order is the stable `sort_order` (via `build_tree`).
/// These printed ids are the ONLY ids the parser will accept back.
pub fn serialize_tree_for_llm(parts: &[Part]) -> String {
    fn walk(nodes: &[orchestrator_store::TreeNode], prefix: &str, out: &mut String) {
        for n in nodes {
            let path = if prefix.is_empty() {
                n.part.name.clone()
            } else {
                format!("{prefix} > {}", n.part.name)
            };
            let detail = n.part.detail.trim();
            if detail.is_empty() {
                out.push_str(&format!(
                    "[{}] {} ({})\n",
                    n.part.id,
                    path,
                    n.part.lifecycle.as_str()
                ));
            } else {
                out.push_str(&format!(
                    "[{}] {} — {} ({})\n",
                    n.part.id,
                    path,
                    detail,
                    n.part.lifecycle.as_str()
                ));
            }
            walk(&n.children, &path, out);
        }
    }
    let mut out = String::new();
    walk(&orchestrator_store::build_tree(parts), "", &mut out);
    out
}

/// The strict-JSON proposer contract. Grounding = current map + every NEW
/// summary since the last proposal + files touched (paths beat prose).
/// `dispatched` = (session id8, part_id) pairs for sessions explicitly
/// dispatched onto a node — a hint (weigh evidence toward that node first),
/// never authority (the trust gate is unchanged).
fn map_proposal_prompt(
    tree_txt: &str,
    summaries_txt: &str,
    files_txt: &str,
    dispatched: &[(String, i64)],
) -> String {
    let mut p = format!(
        "You maintain a product map for a solo user's status board. Below: the CURRENT MAP \
(one node per line, numeric id in [brackets]), the NEW SESSION SUMMARIES since the map was last \
reviewed, and the FILES TOUCHED in those sessions (file paths are stronger evidence than prose). \
Propose map updates ONLY where the evidence clearly shows one. Output ONLY minified JSON, no \
prose, shape: {{\"ops\":[...]}} where each op is one of:\n\
{{\"kind\":\"set_status\",\"id\":N,\"lifecycle\":\"building|done|todo\",\"evidence\":\"...\"}}\n\
{{\"kind\":\"add\",\"parent_id\":N,\"name\":\"...\",\"detail\":\"...\",\"lifecycle\":\"todo|idea\",\"evidence\":\"...\"}}\n\
{{\"kind\":\"detail\",\"id\":N,\"append\":\"...\",\"evidence\":\"...\"}}\n\
RULES: (1) If nothing on the map clearly changed, output EXACTLY {{\"ops\":[]}} — that is the \
NORMAL result; never stretch thin evidence into an op. (2) `evidence` MUST be a VERBATIM quote \
copied from the summaries or a touched file path — ops whose evidence is not verbatim are \
discarded. Never invent. (3) `id`/`parent_id` MUST be a numeric id shown in [brackets] on the \
CURRENT MAP. (4) Only mark a node done when the evidence explicitly names THAT work as finished. \
(5) Never remove, move, or rename nodes; `add` only for genuinely new todo/idea children.\n\n\
CURRENT MAP:\n{tree_txt}\n\
NEW SESSION SUMMARIES:\n{summaries_txt}\n\
FILES TOUCHED:\n{files_txt}"
    );
    if !dispatched.is_empty() {
        p.push_str("\nDISPATCHED SESSIONS:\n");
        for (id8, part_id) in dispatched {
            p.push_str(&format!("session {id8} was dispatched to [{part_id}] — weigh its summary evidence toward that node first.\n"));
        }
    }
    p
}

/// The trust gate. Parses the proposer's JSON into accepted ops, DROPPING:
/// ops without non-empty evidence, ops whose id/parent_id is not in
/// `valid_ids`, and any kind outside set_status/add/detail (Remove/Move can
/// never enter the system here). Malformed JSON or a missing `ops` array
/// parses cleanly to EMPTY (the expected common output must never error).
/// `current` maps id → (name, detail) so detail-append becomes a
/// name-preserving `Rename` with `<old detail> — <append>`.
#[allow(dead_code)]
pub fn parse_map_proposal(
    json: &str,
    valid_ids: &HashSet<i64>,
    current: &HashMap<i64, (String, String)>,
) -> MapProposal {
    let mut ops: Vec<(DiffOp, String)> = Vec::new();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return MapProposal { ops };
    };
    let Some(arr) = v.get("ops").and_then(|o| o.as_array()) else {
        return MapProposal { ops };
    };
    let mut counter = 0usize;
    for item in arr {
        let s = |k: &str| {
            item.get(k)
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let evidence = s("evidence");
        if evidence.is_empty() {
            continue; // EVIDENCE-OR-DROP: no verbatim quote, no op
        }
        match s("kind").as_str() {
            "set_status" => {
                let Some(id) = item.get("id").and_then(|x| x.as_i64()) else {
                    continue;
                };
                if !valid_ids.contains(&id) {
                    continue; // id not on the serialized map
                }
                let lifecycle = match s("lifecycle").as_str() {
                    "todo" => Lifecycle::Todo,
                    "building" => Lifecycle::Building,
                    "done" => Lifecycle::Done,
                    _ => continue, // idea/unknown is not a status an agent may assert
                };
                ops.push((
                    DiffOp::SetStatus {
                        id,
                        lifecycle,
                        source: StatusSource::Agent,
                    },
                    evidence,
                ));
            }
            "add" => {
                let Some(pid) = item.get("parent_id").and_then(|x| x.as_i64()) else {
                    continue;
                };
                if !valid_ids.contains(&pid) {
                    continue; // parent must EXIST — no add-under-invented-parent
                }
                let name = s("name");
                if name.is_empty() {
                    continue;
                }
                // whitelist: idea or todo ONLY — an added node is never an assertion
                let lifecycle = if s("lifecycle") == "idea" {
                    Lifecycle::Idea
                } else {
                    Lifecycle::Todo
                };
                counter += 1;
                ops.push((
                    DiffOp::Add {
                        temp: format!("m{counter}"),
                        parent: PartRef::Id(pid),
                        name,
                        detail: s("detail"),
                        lifecycle,
                        anchors: Vec::new(),
                        kind: Kind::Task,
                        detail_md: None,
                        sort_order: None,
                        source_file: None,
                        source_quote: None,
                        rationale: None,
                    },
                    evidence,
                ));
            }
            "detail" => {
                let Some(id) = item.get("id").and_then(|x| x.as_i64()) else {
                    continue;
                };
                if !valid_ids.contains(&id) {
                    continue;
                }
                let Some((name, cur)) = current.get(&id) else {
                    continue;
                };
                let append = s("append");
                if append.is_empty() {
                    continue;
                }
                let detail = if cur.trim().is_empty() {
                    append
                } else {
                    format!("{cur} — {append}")
                };
                ops.push((
                    DiffOp::Rename {
                        id,
                        name: name.clone(),
                        detail,
                    },
                    evidence,
                ));
            }
            _ => {} // remove/move/anything else: NEVER accepted from an LLM
        }
    }
    MapProposal { ops }
}

/// Mine the file paths the session actually touched from the transcript tail —
/// tool_use inputs are STRONGER evidence than summary prose. Claude JSONL:
/// assistant messages, content[].type=="tool_use", input.file_path/.path.
/// Codex rollouts: best-effort on function_call arguments (empty when the
/// shape doesn't match). Returns up to `cap` distinct paths, newest first.
pub fn files_touched(transcript_path: &Path, is_codex: bool, cap: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(transcript_path) else {
        return Vec::new();
    };
    files_touched_text(&text, is_codex, cap)
}

fn files_touched_text(text: &str, is_codex: bool, cap: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let push = |out: &mut Vec<String>, p: &str| {
        let p = p.trim();
        if !p.is_empty() && out.len() < cap && !out.iter().any(|x| x == p) {
            out.push(p.to_string());
        }
    };
    for line in text.lines().rev() {
        if out.len() >= cap {
            break;
        }
        if is_codex {
            if !line.contains("\"function_call\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let p = v.get("payload").unwrap_or(&v);
            if p.get("type").and_then(|t| t.as_str()) != Some("function_call") {
                continue;
            }
            // arguments is a JSON-encoded string; best-effort top-level path keys.
            let Some(args) = p.get("arguments").and_then(|a| a.as_str()) else {
                continue;
            };
            let Ok(a) = serde_json::from_str::<serde_json::Value>(args) else {
                continue;
            };
            for k in ["file_path", "path"] {
                if let Some(f) = a.get(k).and_then(|x| x.as_str()) {
                    push(&mut out, f);
                }
            }
        } else {
            if !line.contains("\"type\":\"assistant\"") || !line.contains("\"tool_use\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(content) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            else {
                continue;
            };
            for part in content {
                if part.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                    continue;
                }
                let Some(input) = part.get("input") else {
                    continue;
                };
                for k in ["file_path", "path", "notebook_path"] {
                    if let Some(f) = input.get(k).and_then(|x| x.as_str()) {
                        push(&mut out, f);
                    }
                }
            }
        }
    }
    out
}

/// One proposer call (slice 2). Pure inputs → `MapProposal`: NO store access —
/// the GUI worker gathers tree/summaries/files and persists the result.
/// Blocking + spends plan quota; run OFF the UI thread. temp-dir cwd like
/// `summarize_session` (never a project dir a stray transcript could land in).
pub fn propose_map_updates(
    tree_txt: &str,
    summaries_txt: &str,
    files_txt: &str,
    dispatched: &[(String, i64)],
    valid_ids: &HashSet<i64>,
    current: &HashMap<i64, (String, String)>,
) -> Result<MapProposal, String> {
    let stdout = run_claude_p(
        &map_proposal_prompt(tree_txt, summaries_txt, files_txt, dispatched),
        &std::env::temp_dir(),
        90,
    )?;
    let text = extract_result_text(&stdout).ok_or("no result text in claude output")?;
    let json = first_json_object(&text).ok_or("no JSON object in proposal output")?;
    Ok(parse_map_proposal(&json, valid_ids, current))
}

#[cfg(test)]
mod tests {
    use super::*;
    // ---- slice 2: the living-map proposal pipeline (pure fns, no CLI) ----

    fn mk_part(
        id: i64,
        parent: Option<i64>,
        name: &str,
        detail: &str,
        lc: Lifecycle,
        order: f64,
    ) -> Part {
        Part {
            id,
            parent_id: parent,
            name: name.into(),
            detail: detail.into(),
            lifecycle: lc,
            status_source: StatusSource::Seed,
            sort_order: order,
            ..Part::default()
        }
    }

    #[test]
    fn serialize_tree_paths_order_and_lifecycle() {
        let parts = vec![
            mk_part(1, None, "Tech", "the codebase", Lifecycle::Building, 2.0),
            mk_part(2, None, "Growth", "", Lifecycle::Todo, 1.0),
            mk_part(3, Some(1), "Parser", "trust gate", Lifecycle::Todo, 1.0),
            mk_part(4, Some(3), "Lexer", "", Lifecycle::Idea, 1.0),
        ];
        let txt = serialize_tree_for_llm(&parts);
        let lines: Vec<&str> = txt.lines().collect();
        // sort_order: Growth (1.0) before Tech (2.0); children follow parents
        assert_eq!(lines[0], "[2] Growth (todo)");
        assert_eq!(lines[1], "[1] Tech — the codebase (building)");
        assert_eq!(lines[2], "[3] Tech > Parser — trust gate (todo)");
        assert_eq!(lines[3], "[4] Tech > Parser > Lexer (idea)");
        assert_eq!(lines.len(), 4);
    }

    fn ids(v: &[i64]) -> HashSet<i64> {
        v.iter().copied().collect()
    }

    fn cur(v: &[(i64, &str, &str)]) -> HashMap<i64, (String, String)> {
        v.iter()
            .map(|(id, n, d)| (*id, (n.to_string(), d.to_string())))
            .collect()
    }

    #[test]
    fn proposal_parses_all_allowed_kinds() {
        let json = r#"{"ops":[
            {"kind":"set_status","id":3,"lifecycle":"done","evidence":"parser fixed, 12 tests green"},
            {"kind":"add","parent_id":1,"name":"Retry queue","detail":"resume drops","lifecycle":"idea","evidence":"we should add a retry queue"},
            {"kind":"detail","id":3,"append":"now streaming","evidence":"switched the parser to streaming"}
        ]}"#;
        let p = parse_map_proposal(
            json,
            &ids(&[1, 3]),
            &cur(&[(1, "Tech", "code"), (3, "Parser", "trust gate")]),
        );
        assert_eq!(p.ops.len(), 3);
        assert_eq!(
            p.ops[0].0,
            DiffOp::SetStatus {
                id: 3,
                lifecycle: Lifecycle::Done,
                source: StatusSource::Agent
            }
        );
        assert_eq!(p.ops[0].1, "parser fixed, 12 tests green");
        match &p.ops[1].0 {
            DiffOp::Add {
                parent,
                name,
                lifecycle,
                anchors,
                ..
            } => {
                assert_eq!(*parent, PartRef::Id(1));
                assert_eq!(name, "Retry queue");
                assert_eq!(*lifecycle, Lifecycle::Idea);
                assert!(anchors.is_empty());
            }
            other => panic!("expected Add, got {other:?}"),
        }
        // detail-append = name-preserving Rename with " — " joined detail
        assert_eq!(
            p.ops[2].0,
            DiffOp::Rename {
                id: 3,
                name: "Parser".into(),
                detail: "trust gate — now streaming".into()
            }
        );
    }

    #[test]
    fn proposal_drops_ops_without_evidence() {
        let json = r#"{"ops":[
            {"kind":"set_status","id":1,"lifecycle":"done"},
            {"kind":"set_status","id":1,"lifecycle":"done","evidence":"  "},
            {"kind":"add","parent_id":1,"name":"X","lifecycle":"todo","evidence":""},
            {"kind":"detail","id":1,"append":"x"}
        ]}"#;
        let p = parse_map_proposal(json, &ids(&[1]), &cur(&[(1, "Tech", "code")]));
        assert!(p.ops.is_empty(), "every evidence-free op must be dropped");
    }

    #[test]
    fn proposal_drops_unknown_ids_and_parents() {
        let json = r#"{"ops":[
            {"kind":"set_status","id":99,"lifecycle":"done","evidence":"q"},
            {"kind":"set_status","lifecycle":"done","evidence":"q"},
            {"kind":"add","parent_id":99,"name":"X","lifecycle":"todo","evidence":"q"},
            {"kind":"add","name":"X","lifecycle":"todo","evidence":"q"},
            {"kind":"detail","id":99,"append":"x","evidence":"q"}
        ]}"#;
        let p = parse_map_proposal(json, &ids(&[1]), &cur(&[(1, "Tech", "code")]));
        assert!(
            p.ops.is_empty(),
            "ids not on the serialized map must be dropped"
        );
    }

    #[test]
    fn proposal_drops_disallowed_kinds_and_lifecycles() {
        let json = r#"{"ops":[
            {"kind":"remove","id":1,"evidence":"this area is obsolete"},
            {"kind":"move","id":1,"parent_id":1,"evidence":"restructure"},
            {"kind":"rename","id":1,"name":"Y","evidence":"q"},
            {"kind":"set_status","id":1,"lifecycle":"idea","evidence":"q"},
            {"kind":"set_status","id":1,"lifecycle":"finished","evidence":"q"},
            {"kind":"add","parent_id":1,"name":"","lifecycle":"todo","evidence":"q"},
            {"kind":"detail","id":1,"append":"","evidence":"q"}
        ]}"#;
        let p = parse_map_proposal(json, &ids(&[1]), &cur(&[(1, "Tech", "code")]));
        assert!(
            p.ops.is_empty(),
            "Remove/Move/bad-lifecycle/empty payload ops must all be dropped"
        );
    }

    #[test]
    fn proposal_add_lifecycle_never_asserts() {
        // "done"/"building" on an add coerce to todo — an Add is never an assertion
        let json = r#"{"ops":[{"kind":"add","parent_id":1,"name":"X","lifecycle":"done","evidence":"q"}]}"#;
        let p = parse_map_proposal(json, &ids(&[1]), &cur(&[(1, "Tech", "code")]));
        assert!(matches!(
            &p.ops[0].0,
            DiffOp::Add {
                lifecycle: Lifecycle::Todo,
                ..
            }
        ));
    }

    #[test]
    fn proposal_empty_and_malformed_parse_cleanly() {
        let none = ids(&[]);
        let cmap = cur(&[]);
        // the EXPECTED common output
        assert!(parse_map_proposal(r#"{"ops":[]}"#, &none, &cmap)
            .ops
            .is_empty());
        // missing ops / wrong type / garbage — all EMPTY, never a panic or error
        assert!(parse_map_proposal("{}", &none, &cmap).ops.is_empty());
        assert!(parse_map_proposal(r#"{"ops":"nope"}"#, &none, &cmap)
            .ops
            .is_empty());
        assert!(parse_map_proposal("not json at all", &none, &cmap)
            .ops
            .is_empty());
    }

    #[test]
    fn proposal_detail_append_onto_empty_detail() {
        let json = r#"{"ops":[{"kind":"detail","id":1,"append":"now has tests","evidence":"added tests"}]}"#;
        let p = parse_map_proposal(json, &ids(&[1]), &cur(&[(1, "Tech", "")]));
        // no dangling " — " prefix when the current detail is empty
        assert_eq!(
            p.ops[0].0,
            DiffOp::Rename {
                id: 1,
                name: "Tech".into(),
                detail: "now has tests".into()
            }
        );
    }

    #[test]
    fn proposal_prompt_carries_grounding_and_contract() {
        let p = map_proposal_prompt("[1] Tech (todo)\n", "S1: fixed parser\n", "/a/b.rs\n", &[]);
        assert!(p.contains("[1] Tech (todo)"));
        assert!(p.contains("S1: fixed parser"));
        assert!(p.contains("/a/b.rs"));
        assert!(p.contains("VERBATIM"), "must demand verbatim evidence");
        assert!(
            p.contains(r#"{"ops":[]}"#),
            "must name the empty-ops normal case"
        );
        assert!(p.contains("set_status") && p.contains("parent_id") && p.contains("append"));
        // no dispatched sessions → no hint block at all
        assert!(!p.contains("DISPATCHED SESSIONS"));
    }

    #[test]
    fn proposal_prompt_renders_dispatch_hints() {
        let dispatched = vec![
            ("a1b2c3d4".to_string(), 7i64),
            ("e5f6a7b8".to_string(), 12i64),
        ];
        let p = map_proposal_prompt("[7] Tech (todo)\n", "s\n", "f\n", &dispatched);
        assert!(p.contains("DISPATCHED SESSIONS:"));
        assert!(p.contains("session a1b2c3d4 was dispatched to [7] — weigh its summary evidence toward that node first."));
        assert!(p.contains("session e5f6a7b8 was dispatched to [12] — weigh its summary evidence toward that node first."));
    }

    #[test]
    fn files_touched_mines_claude_tool_use_tail() {
        let t = concat!(
            r#"{"type":"user","message":{"content":"go"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/p/old.rs"}}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"editing"},{"type":"tool_use","name":"Edit","input":{"file_path":"/p/a.rs","old_string":"x","new_string":"y"}}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Grep","input":{"pattern":"z","path":"/p/dir"}}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/p/a.rs"}}]}}"#,
            "\n",
        );
        let f = files_touched_text(t, false, 10);
        // newest first, deduped: a.rs (twice) once, then dir, then old.rs
        assert_eq!(
            f,
            vec![
                "/p/a.rs".to_string(),
                "/p/dir".to_string(),
                "/p/old.rs".to_string()
            ]
        );
        // cap is respected
        assert_eq!(files_touched_text(t, false, 1), vec!["/p/a.rs".to_string()]);
        // a claude transcript scanned as codex yields nothing (best-effort)
        assert!(files_touched_text(t, true, 10).is_empty());
        // codex function_call best-effort shape
        let c = concat!(
            r#"{"payload":{"type":"function_call","name":"edit","arguments":"{\"file_path\":\"/c/x.py\"}"}}"#,
            "\n",
            r#"{"payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"ls\"]}"}}"#,
            "\n",
        );
        assert_eq!(files_touched_text(c, true, 10), vec!["/c/x.py".to_string()]);
    }
}
