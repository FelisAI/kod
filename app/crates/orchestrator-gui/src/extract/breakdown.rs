use super::*;
use orchestrator_store::{DiffOp, Kind, Lifecycle, PartId, PartRef};

// ---------------------------------------------------------------------------
// Slice 3 (docs/011 §C): '◇ break down' — one isolated call proposes 3-7
// concrete sub-parts of ONE node, grounded ONLY in that node's own detail +
// decision log. Same trust gate philosophy as the living-map proposer: Add
// ops ONLY, parent FORCED to the node (whatever the model said), evidence-or-
// drop, zero ops is a fine answer. Worst case is a dismissed proposal card,
// never a corrupted tree.
// ---------------------------------------------------------------------------

/// The break-down contract: sub-parts of THIS node, each grounded in a
/// verbatim quote from the node's detail/decision log — nothing invented.
pub fn breakdown_prompt(
    node_line: &str,
    detail: &str,
    decisions: &[String],
    children: &[String],
) -> String {
    let decisions_txt = if decisions.is_empty() {
        "(none)".to_string()
    } else {
        decisions.join("\n")
    };
    let children_txt = if children.is_empty() {
        "(none)".to_string()
    } else {
        children.join("\n")
    };
    format!(
        "You are breaking ONE node of a solo user's product map into concrete sub-parts. \
Below: the NODE (one line), its DETAIL, its DECISION LOG, and its EXISTING CHILDREN. Propose 3-7 \
concrete sub-parts of THIS node. Output ONLY minified JSON, no prose, shape: \
{{\"ops\":[{{\"add\":{{\"name\":\"...\",\"detail\":\"...\"}},\"evidence\":\"...\"}}]}}\n\
RULES: (1) `evidence` MUST be a VERBATIM quote copied from the DETAIL or DECISION LOG below — ops \
whose evidence is not verbatim are discarded. (2) Propose NOTHING not grounded in that text; \
FEWER ops — or zero, EXACTLY {{\"ops\":[]}} — is fine; never pad to 3. (3) NEVER restate, rename, \
or duplicate an EXISTING CHILD.\n\n\
NODE: {node_line}\n\
DETAIL:\n{detail}\n\
DECISION LOG:\n{decisions_txt}\n\
EXISTING CHILDREN:\n{children_txt}"
    )
}

/// The break-down trust gate. Accepts Add ops ONLY; the parent is FORCED to
/// `parent` no matter what the model said. Drops entries with empty/missing
/// evidence or name, dedups names case-insensitively, caps at 7. New parts are
/// always `todo` (a proposal never asserts). Malformed input parses cleanly
/// to EMPTY — same contract as `parse_map_proposal`.
pub fn parse_breakdown(raw: &str, parent: PartId) -> Vec<(DiffOp, String)> {
    let mut ops: Vec<(DiffOp, String)> = Vec::new();
    let Some(json) = first_json_object(raw) else {
        return ops;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) else {
        return ops;
    };
    let Some(arr) = v.get("ops").and_then(|o| o.as_array()) else {
        return ops;
    };
    let mut seen: Vec<String> = Vec::new();
    for item in arr {
        if ops.len() >= 7 {
            break;
        }
        let evidence = item
            .get("evidence")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if evidence.is_empty() {
            continue; // EVIDENCE-OR-DROP: no verbatim quote, no op
        }
        let Some(add) = item.get("add") else { continue }; // Add ops ONLY — anything else never enters
        let s = |k: &str| {
            add.get(k)
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let name = s("name");
        if name.is_empty() {
            continue;
        }
        let lower = name.to_lowercase();
        if seen.contains(&lower) {
            continue;
        }
        seen.push(lower);
        ops.push((
            DiffOp::Add {
                temp: format!("b{}", ops.len() + 1),
                parent: PartRef::Id(parent),
                name,
                detail: s("detail"),
                lifecycle: Lifecycle::Todo,
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
    ops
}

/// One break-down call (slice 3). Pure inputs → accepted (op, evidence) pairs;
/// NO store access — the GUI worker gathers node context and persists the
/// result as pending kind='breakdown:<part_id>'. Blocking + spends plan
/// quota; run OFF the UI thread. Same shim/model/timeout as
/// `propose_map_updates`; errors feed the GUI's red line.
pub fn propose_breakdown(
    node_line: &str,
    detail: &str,
    decisions: &[String],
    children: &[String],
    parent: PartId,
) -> Result<Vec<(DiffOp, String)>, String> {
    let stdout = run_claude_p(
        &breakdown_prompt(node_line, detail, decisions, children),
        &std::env::temp_dir(),
        90,
    )?;
    let text = extract_result_text(&stdout).ok_or("no result text in claude output")?;
    if first_json_object(&text).is_none() {
        return Err("no JSON object in breakdown output".into());
    }
    Ok(parse_breakdown(&text, parent))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    // ---- slice 3: '◇ break down' proposer (pure fns, no CLI) ----

    #[test]
    fn breakdown_prompt_carries_context_and_contract() {
        let p = breakdown_prompt(
            "[3] Tech > Parser — trust gate (todo)",
            "the evidence-or-drop parser",
            &["decided: streaming".to_string()],
            &["Lexer".to_string(), "Tokenizer".to_string()],
        );
        assert!(p.contains("[3] Tech > Parser — trust gate (todo)"));
        assert!(p.contains("the evidence-or-drop parser"));
        assert!(p.contains("decided: streaming"));
        // existing children listed so the model never restates them
        assert!(p.contains("Lexer") && p.contains("Tokenizer"));
        assert!(p.contains("EXISTING CHILD"));
        assert!(p.contains("VERBATIM"), "must demand verbatim evidence");
        assert!(
            p.contains(r#"{"ops":[]}"#),
            "zero ops must be named as fine"
        );
        assert!(
            p.contains(r#"{"add":"#),
            "must name the add-only output shape"
        );
        // empty decisions/children render a placeholder, not a dangling header
        let q = breakdown_prompt("[1] X (todo)", "d", &[], &[]);
        assert!(q.contains("(none)"));
    }

    #[test]
    fn breakdown_parses_valid_fixture() {
        let raw = r#"{"ops":[
            {"add":{"name":"Lexer","detail":"tokenize input"},"evidence":"decided: streaming"},
            {"add":{"name":"AST builder","detail":""},"evidence":"the evidence-or-drop parser"}
        ]}"#;
        let ops = parse_breakdown(raw, 3);
        assert_eq!(ops.len(), 2);
        match &ops[0].0 {
            DiffOp::Add {
                parent,
                name,
                detail,
                lifecycle,
                anchors,
                ..
            } => {
                assert_eq!(*parent, PartRef::Id(3));
                assert_eq!(name, "Lexer");
                assert_eq!(detail, "tokenize input");
                assert_eq!(*lifecycle, Lifecycle::Todo);
                assert!(anchors.is_empty());
            }
            other => panic!("expected Add, got {other:?}"),
        }
        assert_eq!(ops[0].1, "decided: streaming");
        assert_eq!(ops[1].1, "the evidence-or-drop parser");
        // parser accepts prose-wrapped JSON (raw claude text, not pre-extracted)
        let wrapped = format!("Here you go:\n{raw}");
        assert_eq!(parse_breakdown(&wrapped, 3).len(), 2);
    }

    #[test]
    fn breakdown_gate_add_only_and_parent_forced() {
        let raw = r#"{"ops":[
            {"remove":{"id":1},"evidence":"q"},
            {"kind":"set_status","id":1,"lifecycle":"done","evidence":"q"},
            {"move":{"id":1,"parent_id":9},"evidence":"q"},
            {"add":{"name":"Real","detail":"","parent_id":99},"evidence":"q"}
        ]}"#;
        let ops = parse_breakdown(raw, 5);
        // non-Add shapes never enter; the model's parent_id is IGNORED
        assert_eq!(ops.len(), 1);
        assert!(matches!(
            &ops[0].0,
            DiffOp::Add {
                parent: PartRef::Id(5),
                ..
            }
        ));
    }

    #[test]
    fn breakdown_gate_drops_evidence_free_and_nameless() {
        let raw = r#"{"ops":[
            {"add":{"name":"NoEvidence","detail":"x"}},
            {"add":{"name":"Blank","detail":"x"},"evidence":"  "},
            {"add":{"name":"","detail":"x"},"evidence":"q"},
            {"add":{"detail":"x"},"evidence":"q"}
        ]}"#;
        assert!(
            parse_breakdown(raw, 1).is_empty(),
            "evidence-free and nameless ops must all be dropped"
        );
    }

    #[test]
    fn breakdown_gate_dedups_names_case_insensitively() {
        let raw = r#"{"ops":[
            {"add":{"name":"Lexer","detail":"a"},"evidence":"q"},
            {"add":{"name":"lexer","detail":"b"},"evidence":"q"},
            {"add":{"name":"LEXER ","detail":"c"},"evidence":"q"},
            {"add":{"name":"Parser","detail":"d"},"evidence":"q"}
        ]}"#;
        let ops = parse_breakdown(raw, 1);
        assert_eq!(ops.len(), 2);
        assert!(matches!(&ops[0].0, DiffOp::Add { name, .. } if name == "Lexer"));
        assert!(matches!(&ops[1].0, DiffOp::Add { name, .. } if name == "Parser"));
    }

    #[test]
    fn breakdown_gate_caps_at_seven() {
        let items: Vec<String> = (1..=9)
            .map(|i| format!(r#"{{"add":{{"name":"Part {i}","detail":""}},"evidence":"q"}}"#))
            .collect();
        let raw = format!(r#"{{"ops":[{}]}}"#, items.join(","));
        let ops = parse_breakdown(&raw, 1);
        assert_eq!(ops.len(), 7);
        // temp ids stay unique so accept_diff can thread parents
        let temps: HashSet<String> = ops
            .iter()
            .map(|(op, _)| match op {
                DiffOp::Add { temp, .. } => temp.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(temps.len(), 7);
    }

    #[test]
    fn breakdown_empty_and_malformed_parse_cleanly() {
        assert!(parse_breakdown(r#"{"ops":[]}"#, 1).is_empty());
        assert!(parse_breakdown("{}", 1).is_empty());
        assert!(parse_breakdown(r#"{"ops":"nope"}"#, 1).is_empty());
        assert!(parse_breakdown("not json at all", 1).is_empty());
    }
}
