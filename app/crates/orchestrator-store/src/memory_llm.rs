//! LLM-facing memory extraction contract.
//!
//! This module does not call an LLM. It defines the prompt/schema and parser
//! for a provider-backed extractor, then returns the same rule-backed memory
//! shape used by deterministic extraction. Verification still happens later
//! through `extract_rule_backed_memories`, which only inserts candidates whose
//! evidence needles are present in the source ledger.

use std::collections::HashSet;

use serde::Deserialize;

use crate::{
    EvidenceNeedle, MemoryDocument, MemoryError, MemoryObjectKind, MemoryResult, MemorySourceKind,
    RuleBackedMemory,
};

const DOC_EXCERPT_CHARS: usize = 3_000;

pub fn llm_memory_extraction_prompt(project_key: &str, documents: &[MemoryDocument]) -> String {
    let mut out = format!(
        "You are extracting durable project memory for orchestrator project `{project_key}`.\n\
Build a typed, evidence-backed memory graph substrate for retrieval and UI projections. \
Do not build a UI tree directly.\n\n\
Rules:\n\
- Sources such as docs, sessions, code, commits, screenshots, and design trails are evidence, not \
memory categories.\n\
- Extract durable Concepts, Areas, Tasks, Decisions, Constraints, Claims, Questions, \
SessionEvents, Learnings, and Artifacts.\n\
- Prefer product/project concepts the user would recognize over source buckets like `docs` or \
`sessions`.\n\
- Every memory MUST include at least one evidence item with `source_id` and `needle`.\n\
- `needle` MUST be a verbatim substring from that source excerpt. Short exact quotes are better.\n\
- A candidate whose evidence cannot be found will be dropped by deterministic verification.\n\
- Use stable lower-kebab ids unique within this output.\n\n\
Output ONLY minified JSON, no prose, shape:\n\
{{\"memories\":[{{\"id\":\"memory-id\",\"kind\":\"Decision|Constraint|Concept|Area|Task|Claim|Question|SessionEvent|Learning|Artifact\",\"title\":\"short title\",\"body_md\":\"one or two useful sentences\",\"evidence\":[{{\"source_id\":\"doc-1\",\"needle\":\"verbatim quote\"}}]}}]}}\n\n\
SOURCE EXCERPTS:\n"
    );

    for doc in documents {
        out.push_str(&format!(
            "\n--- source_id: {} kind: {} uri: {} title: {} ---\n{}\n",
            doc.id,
            source_kind_label(doc.kind),
            doc.uri,
            doc.title.as_deref().unwrap_or(""),
            excerpt(&doc.text, DOC_EXCERPT_CHARS)
        ));
    }
    out
}

pub fn parse_llm_memory_candidates(raw: &str) -> MemoryResult<Vec<RuleBackedMemory>> {
    let json = first_json_object(raw).ok_or_else(|| MemoryError::new("no JSON object"))?;
    let payload: LlmMemoryPayload = serde_json::from_str(&json)
        .map_err(|e| MemoryError::new(format!("bad memory candidate JSON: {e}")))?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut saw_memory = false;
    for memory in payload.memories {
        saw_memory = true;
        let id = memory.id.trim();
        let title = memory.title.trim();
        let body_md = memory.body_md.trim();
        let Some(kind) = parse_memory_object_kind(&memory.kind) else {
            continue;
        };
        if id.is_empty() || title.is_empty() || body_md.is_empty() || !seen.insert(id.to_string()) {
            continue;
        }
        let evidence: Vec<EvidenceNeedle> = memory
            .evidence
            .into_iter()
            .filter_map(|evidence| {
                let source_id = evidence.source_id.trim();
                let needle = evidence.needle.trim();
                (!source_id.is_empty() && !needle.is_empty()).then(|| EvidenceNeedle {
                    source_id: source_id.to_string(),
                    needle: needle.to_string(),
                })
            })
            .collect();
        if evidence.is_empty() {
            continue;
        }
        out.push(RuleBackedMemory {
            id: id.to_string(),
            kind,
            title: title.to_string(),
            body_md: body_md.to_string(),
            evidence,
        });
    }
    if out.is_empty() && saw_memory {
        return Err(MemoryError::new(
            "memory candidate output contained no valid memories",
        ));
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct LlmMemoryPayload {
    #[serde(default)]
    memories: Vec<LlmMemory>,
}

#[derive(Debug, Deserialize)]
struct LlmMemory {
    #[serde(default)]
    id: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body_md: String,
    #[serde(default)]
    evidence: Vec<LlmEvidence>,
}

#[derive(Debug, Deserialize)]
struct LlmEvidence {
    #[serde(default)]
    source_id: String,
    #[serde(default)]
    needle: String,
}

fn parse_memory_object_kind(raw: &str) -> Option<MemoryObjectKind> {
    match normalize_label(raw).as_str() {
        "concept" => Some(MemoryObjectKind::Concept),
        "area" => Some(MemoryObjectKind::Area),
        "task" => Some(MemoryObjectKind::Task),
        "decision" => Some(MemoryObjectKind::Decision),
        "constraint" => Some(MemoryObjectKind::Constraint),
        "claim" => Some(MemoryObjectKind::Claim),
        "question" | "openquestion" => Some(MemoryObjectKind::Question),
        "sessionevent" => Some(MemoryObjectKind::SessionEvent),
        "learning" => Some(MemoryObjectKind::Learning),
        "artifact" => Some(MemoryObjectKind::Artifact),
        _ => None,
    }
}

fn normalize_label(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn source_kind_label(kind: MemorySourceKind) -> &'static str {
    match kind {
        MemorySourceKind::Doc => "doc",
        MemorySourceKind::Code => "code",
        MemorySourceKind::Session => "session",
        MemorySourceKind::MapPart => "map_part",
        MemorySourceKind::Git => "git",
        MemorySourceKind::UserCapture => "user_capture",
        MemorySourceKind::Artifact => "artifact",
    }
}

fn excerpt(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn first_json_object(s: &str) -> Option<String> {
    let mut start = None;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape = false;
    for (idx, ch) in s.char_indices() {
        if start.is_none() {
            if ch == '{' {
                start = Some(idx);
                depth = 1;
            }
            continue;
        }

        if in_string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let start = start?;
                    return Some(s[start..idx + ch.len_utf8()].to_string());
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
    use crate::{extract_rule_backed_memories, ingest_memory_documents, InMemoryMemoryBackend};

    #[test]
    fn prompt_names_sources_and_evidence_contract() {
        let prompt = llm_memory_extraction_prompt(
            "orchestrator",
            &[MemoryDocument {
                id: "doc-020".to_string(),
                kind: MemorySourceKind::Doc,
                uri: "docs/020.md".to_string(),
                title: Some("Memory design".to_string()),
                text: "The Map should be a view of memory, not the memory substrate.".to_string(),
            }],
        );

        assert!(prompt.contains("source_id: doc-020"));
        assert!(prompt.contains("Sources such as docs"));
        assert!(prompt.contains("evidence, not memory categories"));
        assert!(prompt.contains("Decision|Constraint|Concept"));
    }

    #[test]
    fn parser_accepts_wrapped_json_and_kind_aliases() {
        let raw = r#"Here is JSON:
{"memories":[{"id":"session-kickoff","kind":"Session Event","title":"session kickoff uses memory","body_md":"Kickoff should include scoped memory.","evidence":[{"source_id":"doc-020","needle":"dispatch_to_part(part_id)"}]}]}"#;

        let memories = parse_llm_memory_candidates(raw).unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].kind, MemoryObjectKind::SessionEvent);
        assert_eq!(memories[0].evidence[0].source_id, "doc-020");
    }

    #[test]
    fn parser_skips_invalid_candidates() {
        let raw = r#"{"memories":[
{"id":"no-evidence","kind":"Decision","title":"x","body_md":"y","evidence":[]},
{"id":"unknown-kind","kind":"Bucket","title":"x","body_md":"y","evidence":[{"source_id":"doc","needle":"n"}]},
{"id":"valid","kind":"Constraint","title":"source types are not categories","body_md":"Docs are evidence.","evidence":[{"source_id":"doc","needle":"Source types are not categories"}]}
]}"#;

        let memories = parse_llm_memory_candidates(raw).unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].id, "valid");
    }

    #[test]
    fn parser_accepts_empty_memory_set() {
        let memories = parse_llm_memory_candidates(r#"{"memories":[]}"#).unwrap();
        assert!(memories.is_empty());
    }

    #[test]
    fn parser_handles_braces_inside_strings() {
        let raw = r#"noise {"memories":[{"id":"shape","kind":"Concept","title":"json shape","body_md":"Shape uses { braces }.","evidence":[{"source_id":"doc","needle":"Output {JSON}"}]}]} trailing {"other":true}"#;
        let memories = parse_llm_memory_candidates(raw).unwrap();
        assert_eq!(memories[0].body_md, "Shape uses { braces }.");
    }

    #[test]
    fn parsed_candidates_still_require_evidence_verification() {
        let docs = vec![MemoryDocument {
            id: "doc-020".to_string(),
            kind: MemorySourceKind::Doc,
            uri: "docs/020.md".to_string(),
            title: None,
            text: "The Map should be a view of memory, not the memory substrate.".to_string(),
        }];
        let raw = r#"{"memories":[
{"id":"map-projection","kind":"Decision","title":"Map is projection over memory graph","body_md":"The Map is compiled from memory.","evidence":[{"source_id":"doc-020","needle":"Map should be a view of memory"}]},
{"id":"hallucinated","kind":"Decision","title":"Hallucinated","body_md":"Should not insert.","evidence":[{"source_id":"doc-020","needle":"not present in source"}]}
]}"#;
        let candidates = parse_llm_memory_candidates(raw).unwrap();
        let mut backend = InMemoryMemoryBackend::default();
        ingest_memory_documents(&mut backend, "orchestrator", &docs, 1).unwrap();
        let inserted = extract_rule_backed_memories(
            &mut backend,
            "orchestrator",
            &docs,
            candidates,
            "llm_test",
            1,
        )
        .unwrap();

        assert_eq!(inserted, 1);
        assert!(backend.object("map-projection").is_some());
        assert!(backend.object("hallucinated").is_none());
    }
}
