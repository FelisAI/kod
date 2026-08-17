use super::*;
use orchestrator_store::{DiffOp, Kind, Lifecycle, PartRef};

fn prompt(digest: &str) -> String {
    format!(
        "You are mapping a software project's PRODUCT ANATOMY for a glanceable status board — the \
whole product as a venture, not just its code. NO preset taxonomy (docs/019): top level = 4–9 \
areas named as noun-phrases the user would say aloud, organized by whatever principle THIS \
project's own documents exhibit. Quarantine code-derived structure under exactly ONE collapsed \
area named \"from code\" — code layout must never become the organizing principle of the map. 1–2 \
levels under each area. For each node: a short human name, a one-line detail, `anchors` = the code \
path globs it maps to (relative to the root, e.g. \"crates/foo/**\" or \"src/audio/**\" — [] when no \
real path applies), and `lifecycle`. \
RULES: every child must be grounded in EVIDENCE from the digest — an area with no evidence gets an \
EMPTY children list (do NOT invent children). Do NOT judge completion — `lifecycle` is only \
\"todo\" (concrete planned/underway work) or \"idea\" (speculative direction); never anything else \
(the human asserts done themselves). \
Output ONLY minified JSON, no prose, shape: \
{{\"areas\":[{{\"name\":\"...\",\"detail\":\"...\",\"anchors\":[\"...\"],\"lifecycle\":\"todo\",\"children\":[{{\"name\":\"...\",\"detail\":\"...\",\"anchors\":[\"...\"],\"lifecycle\":\"idea\"}}]}}]}}\n\n\
STRUCTURE DIGEST:\n{digest}"
    )
}

/// Run the one-shot extraction. Blocking + uses plan quota — run off the UI
/// thread. Returns `Add` DiffOps (a proposal), or an error string.
pub fn extract_tree(root: &Path) -> Result<Vec<DiffOp>, String> {
    let digest = project_digest(root);
    let stdout = run_claude_p(&prompt(&digest), root, 120)?;
    // claude -p --output-format json wraps the result; pull the assistant text.
    let text = extract_result_text(&stdout).ok_or("no result text in claude output")?;
    let json = first_json_object(&text).ok_or("no JSON object in extraction")?;
    parse_areas(&json)
}

fn parse_areas(json: &str) -> Result<Vec<DiffOp>, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("bad JSON: {e}"))?;
    let areas = v
        .get("areas")
        .and_then(|a| a.as_array())
        .ok_or("missing 'areas' array")?;
    let mut ops = Vec::new();
    let mut counter = 0usize;
    for area in areas {
        add_area(area, PartRef::Root, &mut ops, &mut counter);
    }
    if ops.is_empty() {
        return Err("extraction produced no areas".into());
    }
    Ok(ops)
}

fn add_area(v: &serde_json::Value, parent: PartRef, ops: &mut Vec<DiffOp>, counter: &mut usize) {
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() {
        return;
    }
    *counter += 1;
    let temp = format!("e{counter}");
    let detail = v
        .get("detail")
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .to_string();
    let anchors = v
        .get("anchors")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    // seed lifecycle whitelist: idea or todo ONLY — a seed can never assert
    // done/building, whatever the model outputs (docs/016).
    let lifecycle = match v.get("lifecycle").and_then(|l| l.as_str()) {
        Some("idea") => Lifecycle::Idea,
        _ => Lifecycle::Todo,
    };
    // docs/019 (review): a seeded node with children is an AREA — rollup,
    // not asserted lifecycle. Leaves are tasks. Without this, post-backfill
    // installs (gate already set) would seed all-task trees forever.
    let kind = if v
        .get("children")
        .and_then(|c| c.as_array())
        .map(|c| !c.is_empty())
        .unwrap_or(false)
    {
        Kind::Area
    } else {
        Kind::Task
    };
    ops.push(DiffOp::Add {
        temp: temp.clone(),
        parent,
        name,
        detail,
        lifecycle,
        anchors,
        kind,
        detail_md: None,
        sort_order: None,
        source_file: None,
        source_quote: None,
        rationale: None,
    });
    if let Some(children) = v.get("children").and_then(|c| c.as_array()) {
        for child in children {
            add_area(child, PartRef::Temp(temp.clone()), ops, counter);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn seed_prompt_has_no_preset_taxonomy() {
        // docs/019 C2: the "Product, Users, Growth, Tech" preset was one of
        // the two hardcodings that built the unexplainable Tech blob.
        let p = prompt("DIGEST");
        assert!(
            !p.contains("Product, Users, Growth"),
            "no preset taxonomy, ever"
        );
        assert!(
            p.contains("NO preset taxonomy"),
            "the prompt states the rule"
        );
        assert!(
            p.contains("from code"),
            "code structure quarantines into ONE area"
        );
        assert!(
            p.contains("\"idea\"") && p.contains("\"todo\""),
            "lifecycle whitelist named"
        );
        assert!(p.contains("EMPTY children"), "no-evidence areas stay empty");
        assert!(
            p.contains("{\"areas\":"),
            "output shape unchanged (plumbing intact)"
        );
    }

    #[test]
    fn seed_parse_honors_todo_idea_whitelist() {
        let json = r#"{"areas":[
            {"name":"Growth","detail":"","anchors":[],"lifecycle":"idea","children":[
                {"name":"Launch post","detail":"","anchors":[],"lifecycle":"done"}]}
        ]}"#;
        let ops = parse_areas(json).unwrap();
        assert!(matches!(
            &ops[0],
            DiffOp::Add {
                lifecycle: Lifecycle::Idea,
                ..
            }
        ));
        // "done" from the seed can never assert — coerced to todo
        assert!(matches!(
            &ops[1],
            DiffOp::Add {
                lifecycle: Lifecycle::Todo,
                ..
            }
        ));
    }

    // LIVE: runs `claude -p` against the real orchestrator project (uses quota).
    //   cargo test -p orchestrator-gui --bin orchestrator -- --ignored extract_live
    #[test]
    #[ignore]
    fn extract_live() {
        let ops = extract_tree(Path::new("/Users/me/local/orchestrator")).expect("extraction");
        println!("extracted {} ops:", ops.len());
        for op in &ops {
            if let DiffOp::Add {
                name,
                parent,
                anchors,
                ..
            } = op
            {
                println!("  + {name}  parent={parent:?}  anchors={anchors:?}");
            }
        }
        assert!(ops.len() >= 4, "expected several areas");
    }
}
