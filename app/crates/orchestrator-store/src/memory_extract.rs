//! Rule-backed memory extraction primitives.
//!
//! This is the first reusable, non-oracle extraction layer. It is deliberately
//! deterministic and evidence-gated: a rule emits a memory object only when at
//! least one configured source phrase is found. Later LLM extractors should
//! produce the same `SeededMemory` / source-span shape, then run through the
//! same verifier and backend interface.

use crate::{
    MemoryBackend, MemoryObject, MemoryObjectKind, MemoryObjectState, MemoryResult, MemorySource,
    MemorySourceKind, MemorySpan,
};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MemoryDocument {
    pub id: String,
    pub kind: MemorySourceKind,
    pub uri: String,
    pub title: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SeededMemory {
    pub id: String,
    pub kind: MemoryObjectKind,
    pub title: String,
    pub body_md: String,
    pub source_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RuleBackedMemory {
    pub id: String,
    pub kind: MemoryObjectKind,
    pub title: String,
    pub body_md: String,
    pub evidence: Vec<EvidenceNeedle>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EvidenceNeedle {
    pub source_id: String,
    pub needle: String,
}

pub fn ingest_memory_documents<B: MemoryBackend>(
    backend: &mut B,
    project_key: &str,
    documents: &[MemoryDocument],
    captured_at_secs: u64,
) -> MemoryResult<()> {
    for doc in documents {
        backend.ingest_source(MemorySource {
            id: doc.id.clone(),
            project_key: project_key.to_string(),
            kind: doc.kind,
            uri: doc.uri.clone(),
            title: doc.title.clone(),
            captured_at_secs,
            content_hash: None,
            metadata: serde_json::json!({ "ingested_by": "memory_extract" }),
        })?;
    }
    Ok(())
}

pub fn upsert_seeded_memories<B: MemoryBackend>(
    backend: &mut B,
    project_key: &str,
    documents: &[MemoryDocument],
    memories: Vec<SeededMemory>,
    created_by: &str,
    now_secs: u64,
) -> MemoryResult<usize> {
    let mut count = 0;
    for memory in memories {
        upsert_memory_with_sources(
            backend,
            project_key,
            documents,
            &memory.id,
            memory.kind,
            &memory.title,
            &memory.body_md,
            &memory.source_ids,
            created_by,
            now_secs,
        )?;
        count += 1;
    }
    Ok(count)
}

pub fn extract_rule_backed_memories<B: MemoryBackend>(
    backend: &mut B,
    project_key: &str,
    documents: &[MemoryDocument],
    rules: Vec<RuleBackedMemory>,
    created_by: &str,
    now_secs: u64,
) -> MemoryResult<usize> {
    let mut count = 0;
    for rule in rules {
        let matched_sources: Vec<String> = rule
            .evidence
            .iter()
            .filter_map(|evidence| {
                let doc = documents.iter().find(|doc| doc.id == evidence.source_id)?;
                contains_loose(&doc.text, &evidence.needle).then(|| evidence.source_id.clone())
            })
            .collect();
        if matched_sources.is_empty() {
            continue;
        }
        upsert_memory_with_sources(
            backend,
            project_key,
            documents,
            &rule.id,
            rule.kind,
            &rule.title,
            &rule.body_md,
            &matched_sources,
            created_by,
            now_secs,
        )?;
        count += 1;
    }
    Ok(count)
}

fn upsert_memory_with_sources<B: MemoryBackend>(
    backend: &mut B,
    project_key: &str,
    documents: &[MemoryDocument],
    id: &str,
    kind: MemoryObjectKind,
    title: &str,
    body_md: &str,
    source_ids: &[String],
    created_by: &str,
    now_secs: u64,
) -> MemoryResult<()> {
    let mut span_ids = Vec::new();
    for (idx, source_id) in source_ids.iter().enumerate() {
        let span_id = format!("span:{id}:{idx}");
        let quote = documents
            .iter()
            .find(|doc| doc.id == *source_id)
            .and_then(|doc| best_quote(&doc.text, title));
        backend.add_span(MemorySpan {
            id: span_id.clone(),
            source_id: source_id.clone(),
            start_ref: None,
            end_ref: None,
            quote,
        })?;
        span_ids.push(span_id);
    }

    backend.upsert_object(MemoryObject {
        id: id.to_string(),
        project_key: project_key.to_string(),
        kind,
        title: title.to_string(),
        body_md: body_md.to_string(),
        state: MemoryObjectState::Active,
        confidence: 0.85,
        created_by: created_by.to_string(),
        created_at_secs: now_secs,
        updated_at_secs: now_secs,
        valid_from_secs: None,
        valid_to_secs: None,
        superseded_by: None,
        source_span_ids: span_ids,
        projection: serde_json::json!({}),
        metadata: serde_json::json!({ "backend": created_by }),
    })
}

fn best_quote(text: &str, title: &str) -> Option<String> {
    let title_terms = terms(title);
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .max_by_key(|line| {
            let line_terms = terms(line);
            title_terms
                .iter()
                .filter(|term| line_terms.contains(term))
                .count()
        })
        .map(|line| line.trim().chars().take(220).collect())
}

fn contains_loose(text: &str, needle: &str) -> bool {
    let haystack = normalize(text);
    let needle = normalize(needle);
    !needle.is_empty() && haystack.contains(&needle)
}

fn normalize(text: &str) -> String {
    terms(text).join(" ")
}

fn terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| s.len() > 2)
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryMemoryBackend;

    #[test]
    fn rule_backed_extraction_requires_evidence() {
        let mut backend = InMemoryMemoryBackend::default();
        let docs = vec![MemoryDocument {
            id: "doc-1".to_string(),
            kind: MemorySourceKind::Doc,
            uri: "doc.md".to_string(),
            title: Some("Doc".to_string()),
            text: "The Map should be a view of memory, not the memory substrate.".to_string(),
        }];
        ingest_memory_documents(&mut backend, "p", &docs, 1).unwrap();
        let inserted = extract_rule_backed_memories(
            &mut backend,
            "p",
            &docs,
            vec![
                RuleBackedMemory {
                    id: "obj-map".to_string(),
                    kind: MemoryObjectKind::Decision,
                    title: "Map is projection over memory graph".to_string(),
                    body_md: "The Map compiles from memory.".to_string(),
                    evidence: vec![EvidenceNeedle {
                        source_id: "doc-1".to_string(),
                        needle: "Map should be a view of memory".to_string(),
                    }],
                },
                RuleBackedMemory {
                    id: "obj-missing".to_string(),
                    kind: MemoryObjectKind::Decision,
                    title: "Missing".to_string(),
                    body_md: "Should not insert.".to_string(),
                    evidence: vec![EvidenceNeedle {
                        source_id: "doc-1".to_string(),
                        needle: "not present".to_string(),
                    }],
                },
            ],
            "test",
            1,
        )
        .unwrap();

        assert_eq!(inserted, 1);
        assert!(backend.object("obj-map").is_some());
        assert!(backend.object("obj-missing").is_none());
    }

    #[test]
    fn seeded_memories_keep_source_spans() {
        let mut backend = InMemoryMemoryBackend::default();
        let docs = vec![MemoryDocument {
            id: "doc-1".to_string(),
            kind: MemorySourceKind::Doc,
            uri: "doc.md".to_string(),
            title: None,
            text: "the wiki = the durable NARRATIVE LIBRARY".to_string(),
        }];
        ingest_memory_documents(&mut backend, "p", &docs, 1).unwrap();
        let inserted = upsert_seeded_memories(
            &mut backend,
            "p",
            &docs,
            vec![SeededMemory {
                id: "obj-wiki".to_string(),
                kind: MemoryObjectKind::Decision,
                title: "the wiki is a narrative library".to_string(),
                body_md: "Store owns typed memory.".to_string(),
                source_ids: vec!["doc-1".to_string()],
            }],
            "test",
            1,
        )
        .unwrap();

        let object = backend.object("obj-wiki").unwrap();
        assert_eq!(inserted, 1);
        assert_eq!(object.source_span_ids.len(), 1);
        assert!(backend.span(&object.source_span_ids[0]).is_some());
    }
}
