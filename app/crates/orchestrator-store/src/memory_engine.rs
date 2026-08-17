//! Native memory engine v0.
//!
//! This is the first engine layer above raw extraction. It does not call an
//! LLM; it takes already-extracted candidates and decides whether they should
//! become memory, be skipped as duplicates/no-ops, or supersede older memory.

use crate::{
    extract_rule_backed_memories, ingest_memory_documents, EvidenceNeedle, MemoryBackend,
    MemoryDocument, MemoryObject, MemoryObjectState, MemoryResult, RetrievalIntent, RetrievalQuery,
    RuleBackedMemory,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeMemoryCandidate {
    pub candidate: RuleBackedMemory,
    pub intent: NativeMemoryIntent,
}

impl From<RuleBackedMemory> for NativeMemoryCandidate {
    fn from(candidate: RuleBackedMemory) -> Self {
        Self {
            candidate,
            intent: NativeMemoryIntent::Upsert,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeMemoryIntent {
    Upsert,
    Rename {
        target_id: Option<String>,
        before_title: String,
        after_title: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeMemoryDecisionKind {
    Insert,
    Duplicate,
    NoOp,
    Supersedes,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeMemoryDecision {
    pub candidate_id: String,
    pub kind: NativeMemoryDecisionKind,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub existing_object_id: Option<String>,
    #[serde(default)]
    pub verified_source_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_evidence: Vec<EvidenceNeedle>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeMemoryEngineReport {
    pub inserted: usize,
    pub decisions: Vec<NativeMemoryDecision>,
}

impl NativeMemoryEngineReport {
    pub fn count(&self, kind: NativeMemoryDecisionKind) -> usize {
        self.decisions
            .iter()
            .filter(|decision| decision.kind == kind)
            .count()
    }
}

pub fn evaluate_native_memory_candidates<B: MemoryBackend>(
    backend: &B,
    project_key: &str,
    documents: &[MemoryDocument],
    candidates: &[NativeMemoryCandidate],
) -> MemoryResult<Vec<NativeMemoryDecision>> {
    candidates
        .iter()
        .map(|candidate| evaluate_candidate(backend, project_key, documents, candidate))
        .collect()
}

pub fn apply_native_memory_candidates<B: MemoryBackend>(
    backend: &mut B,
    project_key: &str,
    documents: &[MemoryDocument],
    candidates: &[NativeMemoryCandidate],
    created_by: &str,
    now_secs: u64,
) -> MemoryResult<NativeMemoryEngineReport> {
    ingest_memory_documents(backend, project_key, documents, now_secs)?;
    let mut inserted = 0usize;
    let mut decisions = Vec::new();

    for candidate in candidates {
        let decision = evaluate_candidate(backend, project_key, documents, candidate)?;
        match decision.kind {
            NativeMemoryDecisionKind::Insert | NativeMemoryDecisionKind::Supersedes => {
                if decision.kind == NativeMemoryDecisionKind::Supersedes {
                    if let Some(existing_id) = &decision.existing_object_id {
                        if existing_id != &candidate.candidate.id {
                            if let Some(mut existing) = backend.load_object(existing_id)? {
                                existing.state = MemoryObjectState::Superseded;
                                existing.superseded_by = Some(candidate.candidate.id.clone());
                                existing.updated_at_secs = now_secs;
                                backend.upsert_object(existing)?;
                            }
                        }
                    }
                }
                inserted += extract_rule_backed_memories(
                    backend,
                    project_key,
                    documents,
                    vec![candidate.candidate.clone()],
                    created_by,
                    now_secs,
                )?;
            }
            NativeMemoryDecisionKind::Duplicate
            | NativeMemoryDecisionKind::NoOp
            | NativeMemoryDecisionKind::Unsupported => {}
        }
        decisions.push(decision);
    }

    Ok(NativeMemoryEngineReport {
        inserted,
        decisions,
    })
}

fn evaluate_candidate<B: MemoryBackend>(
    backend: &B,
    project_key: &str,
    documents: &[MemoryDocument],
    input: &NativeMemoryCandidate,
) -> MemoryResult<NativeMemoryDecision> {
    if let Some(reason) = no_op_reason(backend, &input.intent)? {
        return Ok(decision(
            &input.candidate,
            NativeMemoryDecisionKind::NoOp,
            reason,
            None,
            Vec::new(),
            Vec::new(),
        ));
    }

    let evidence = verify_evidence(documents, &input.candidate);
    if evidence.verified_source_ids.is_empty() {
        return Ok(decision(
            &input.candidate,
            NativeMemoryDecisionKind::Unsupported,
            "no evidence needles matched the source ledger".to_string(),
            None,
            evidence.verified_source_ids,
            evidence.missing_evidence,
        ));
    }

    if let Some(existing) = nearest_existing_memory(backend, project_key, &input.candidate)? {
        if is_duplicate_memory(&input.candidate, &existing) {
            return Ok(decision(
                &input.candidate,
                NativeMemoryDecisionKind::Duplicate,
                "same kind, title, and body as existing memory".to_string(),
                Some(existing.id),
                evidence.verified_source_ids,
                evidence.missing_evidence,
            ));
        }
        if existing.id != input.candidate.id && same_label(&existing.title, &input.candidate.title)
        {
            return Ok(decision(
                &input.candidate,
                NativeMemoryDecisionKind::Supersedes,
                "same title with materially different body; treat as newer revision".to_string(),
                Some(existing.id),
                evidence.verified_source_ids,
                evidence.missing_evidence,
            ));
        }
    }

    Ok(decision(
        &input.candidate,
        NativeMemoryDecisionKind::Insert,
        "supported new memory".to_string(),
        None,
        evidence.verified_source_ids,
        evidence.missing_evidence,
    ))
}

fn decision(
    candidate: &RuleBackedMemory,
    kind: NativeMemoryDecisionKind,
    reason: String,
    existing_object_id: Option<String>,
    verified_source_ids: Vec<String>,
    missing_evidence: Vec<EvidenceNeedle>,
) -> NativeMemoryDecision {
    NativeMemoryDecision {
        candidate_id: candidate.id.clone(),
        kind,
        reason,
        existing_object_id,
        verified_source_ids,
        missing_evidence,
    }
}

fn no_op_reason<B: MemoryBackend>(
    backend: &B,
    intent: &NativeMemoryIntent,
) -> MemoryResult<Option<String>> {
    let NativeMemoryIntent::Rename {
        target_id,
        before_title,
        after_title,
    } = intent
    else {
        return Ok(None);
    };

    if same_label(before_title, after_title) {
        return Ok(Some(
            "rename before and after titles are identical".to_string(),
        ));
    }
    if let Some(target_id) = target_id {
        if let Some(target) = backend.load_object(target_id)? {
            if same_label(&target.title, after_title) {
                return Ok(Some(
                    "rename after title is unchanged from target memory".to_string(),
                ));
            }
        }
    }
    Ok(None)
}

fn nearest_existing_memory<B: MemoryBackend>(
    backend: &B,
    project_key: &str,
    candidate: &RuleBackedMemory,
) -> MemoryResult<Option<MemoryObject>> {
    let retrieval = backend.retrieve(RetrievalQuery {
        project_key: project_key.to_string(),
        intent: RetrievalIntent::ExpandMemory,
        text: format!("{} {}", candidate.title, candidate.body_md),
        scope_memory_id: None,
        since_secs: None,
        limit: 5,
    })?;
    for item in retrieval.items {
        let Some(object) = backend.load_object(&item.object_id)? else {
            continue;
        };
        if object.project_key != project_key || object.state == MemoryObjectState::Rejected {
            continue;
        }
        if object.kind == candidate.kind
            && (same_label(&object.title, &candidate.title)
                || body_similarity(&object.body_md, &candidate.body_md) >= 0.88)
        {
            return Ok(Some(object));
        }
    }
    Ok(None)
}

fn is_duplicate_memory(candidate: &RuleBackedMemory, existing: &MemoryObject) -> bool {
    existing.kind == candidate.kind
        && same_label(&existing.title, &candidate.title)
        && body_similarity(&existing.body_md, &candidate.body_md) >= 0.92
}

struct EvidenceVerification {
    verified_source_ids: Vec<String>,
    missing_evidence: Vec<EvidenceNeedle>,
}

fn verify_evidence(
    documents: &[MemoryDocument],
    candidate: &RuleBackedMemory,
) -> EvidenceVerification {
    let mut verified_source_ids = Vec::new();
    let mut missing_evidence = Vec::new();
    for evidence in &candidate.evidence {
        let matched = documents
            .iter()
            .find(|doc| doc.id == evidence.source_id)
            .is_some_and(|doc| contains_loose(&doc.text, &evidence.needle));
        if matched {
            if !verified_source_ids.contains(&evidence.source_id) {
                verified_source_ids.push(evidence.source_id.clone());
            }
        } else {
            missing_evidence.push(evidence.clone());
        }
    }
    EvidenceVerification {
        verified_source_ids,
        missing_evidence,
    }
}

fn same_label(left: &str, right: &str) -> bool {
    normalize(left) == normalize(right)
}

fn body_similarity(left: &str, right: &str) -> f32 {
    let left_terms = terms(left);
    let right_terms = terms(right);
    if left_terms.is_empty() || right_terms.is_empty() {
        return 0.0;
    }
    let shared = left_terms
        .iter()
        .filter(|term| right_terms.contains(term))
        .count();
    shared as f32 / left_terms.len().max(right_terms.len()) as f32
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
    use crate::{InMemoryMemoryBackend, MemoryObjectKind, MemorySourceKind};

    fn doc(text: &str) -> Vec<MemoryDocument> {
        vec![MemoryDocument {
            id: "doc-1".to_string(),
            kind: MemorySourceKind::Doc,
            uri: "docs/memory.md".to_string(),
            title: Some("Memory".to_string()),
            text: text.to_string(),
        }]
    }

    fn memory(id: &str, title: &str, body: &str, needle: &str) -> NativeMemoryCandidate {
        RuleBackedMemory {
            id: id.to_string(),
            kind: MemoryObjectKind::Decision,
            title: title.to_string(),
            body_md: body.to_string(),
            evidence: vec![EvidenceNeedle {
                source_id: "doc-1".to_string(),
                needle: needle.to_string(),
            }],
        }
        .into()
    }

    #[test]
    fn native_engine_inserts_supported_candidate() {
        let mut backend = InMemoryMemoryBackend::default();
        let docs = doc("The Map should be a view of memory, not the memory substrate.");
        let report = apply_native_memory_candidates(
            &mut backend,
            "orchestrator",
            &docs,
            &[memory(
                "map-projection",
                "Map is projection over memory graph",
                "The Map compiles from typed memory.",
                "Map should be a view of memory",
            )],
            "native_engine:test",
            1,
        )
        .unwrap();

        assert_eq!(report.inserted, 1);
        assert_eq!(report.decisions[0].kind, NativeMemoryDecisionKind::Insert);
        assert!(backend.object("map-projection").is_some());
    }

    #[test]
    fn native_engine_rejects_unsupported_candidate() {
        let mut backend = InMemoryMemoryBackend::default();
        let docs = doc("Only this sentence exists.");
        let report = apply_native_memory_candidates(
            &mut backend,
            "orchestrator",
            &docs,
            &[memory(
                "missing",
                "Missing evidence",
                "Should not insert.",
                "not present",
            )],
            "native_engine:test",
            1,
        )
        .unwrap();

        assert_eq!(report.inserted, 0);
        assert_eq!(report.count(NativeMemoryDecisionKind::Unsupported), 1);
        assert!(backend.object("missing").is_none());
    }

    #[test]
    fn native_engine_skips_duplicate_candidate() {
        let mut backend = InMemoryMemoryBackend::default();
        let docs = doc("The source ledger keeps evidence for memory.");
        let candidate = memory(
            "source-ledger",
            "Source ledger keeps evidence",
            "The source ledger keeps evidence for memory.",
            "source ledger keeps evidence",
        );
        let first = apply_native_memory_candidates(
            &mut backend,
            "orchestrator",
            &docs,
            &[candidate.clone()],
            "native_engine:test",
            1,
        )
        .unwrap();
        let second = apply_native_memory_candidates(
            &mut backend,
            "orchestrator",
            &docs,
            &[candidate],
            "native_engine:test",
            2,
        )
        .unwrap();

        assert_eq!(first.inserted, 1);
        assert_eq!(second.inserted, 0);
        assert_eq!(second.count(NativeMemoryDecisionKind::Duplicate), 1);
    }

    #[test]
    fn native_engine_skips_duplicate_within_same_batch() {
        let mut backend = InMemoryMemoryBackend::default();
        let docs = doc("The source ledger keeps evidence for memory.");
        let first = memory(
            "source-ledger-v1",
            "Source ledger keeps evidence",
            "The source ledger keeps evidence for memory.",
            "source ledger keeps evidence",
        );
        let second = memory(
            "source-ledger-v2",
            "Source ledger keeps evidence",
            "The source ledger keeps evidence for memory.",
            "source ledger keeps evidence",
        );

        let report = apply_native_memory_candidates(
            &mut backend,
            "orchestrator",
            &docs,
            &[first, second],
            "native_engine:test",
            1,
        )
        .unwrap();

        assert_eq!(report.inserted, 1);
        assert_eq!(report.count(NativeMemoryDecisionKind::Insert), 1);
        assert_eq!(report.count(NativeMemoryDecisionKind::Duplicate), 1);
        assert!(backend.object("source-ledger-v1").is_some());
        assert!(backend.object("source-ledger-v2").is_none());
    }

    #[test]
    fn native_engine_rejects_noop_rename() {
        let mut backend = InMemoryMemoryBackend::default();
        let docs = doc("Rename proposals are invalid when old and new labels are the same.");
        let mut candidate = memory(
            "same-rename",
            "Same rename is invalid",
            "Rename proposals are invalid when labels are unchanged.",
            "Rename proposals are invalid",
        );
        candidate.intent = NativeMemoryIntent::Rename {
            target_id: None,
            before_title: "Map projection".to_string(),
            after_title: "Map projection".to_string(),
        };

        let report = apply_native_memory_candidates(
            &mut backend,
            "orchestrator",
            &docs,
            &[candidate],
            "native_engine:test",
            1,
        )
        .unwrap();

        assert_eq!(report.inserted, 0);
        assert_eq!(report.count(NativeMemoryDecisionKind::NoOp), 1);
        assert!(backend.object("same-rename").is_none());
    }

    #[test]
    fn native_engine_supersedes_changed_memory() {
        let mut backend = InMemoryMemoryBackend::default();
        let docs = doc(
            "Use source_memory baseline first. Use local_extract instead for typed memory now.",
        );
        let first = memory(
            "memory-choice-v1",
            "Memory engine choice",
            "Use source_memory baseline first.",
            "Use source_memory baseline",
        );
        let second = memory(
            "memory-choice-v2",
            "Memory engine choice",
            "Use local_extract instead for typed memory now.",
            "Use local_extract instead",
        );

        apply_native_memory_candidates(
            &mut backend,
            "orchestrator",
            &docs,
            &[first],
            "native_engine:test",
            1,
        )
        .unwrap();
        let report = apply_native_memory_candidates(
            &mut backend,
            "orchestrator",
            &docs,
            &[second],
            "native_engine:test",
            2,
        )
        .unwrap();

        assert_eq!(report.inserted, 1);
        assert_eq!(report.count(NativeMemoryDecisionKind::Supersedes), 1);
        assert_eq!(
            backend.object("memory-choice-v1").unwrap().state,
            MemoryObjectState::Superseded
        );
        assert_eq!(
            backend
                .object("memory-choice-v1")
                .unwrap()
                .superseded_by
                .as_deref(),
            Some("memory-choice-v2")
        );
        assert_eq!(
            backend.object("memory-choice-v2").unwrap().state,
            MemoryObjectState::Active
        );
    }
}
