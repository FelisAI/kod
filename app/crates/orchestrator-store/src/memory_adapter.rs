//! Adapter-facing memory engine contract.
//!
//! `MemoryBackend` is the product-facing store contract. This module is the
//! eval/compiler-facing boundary: every candidate engine must explain what it
//! returned, with enough provenance for scorecards and UI correction.

use crate::memory::{
    MemoryBackend, MemoryId, MemoryResult, MemorySourceKind, RetrievalItem, RetrievalQuery,
    RetrievalResult,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryEngineMode {
    Evaluation,
    Production,
}

impl MemoryEngineMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Evaluation => "evaluation",
            Self::Production => "production",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEngineCapabilities {
    pub stable_ids: bool,
    pub source_provenance: bool,
    pub typed_objects: bool,
    pub graph_edges: bool,
    pub temporal_validity: bool,
    pub correction_feedback: bool,
    pub projection_support: bool,
}

impl MemoryEngineCapabilities {
    pub fn native_store() -> Self {
        Self {
            stable_ids: true,
            source_provenance: true,
            typed_objects: true,
            graph_edges: true,
            temporal_validity: true,
            correction_feedback: true,
            projection_support: true,
        }
    }

    pub fn raw_source() -> Self {
        Self {
            stable_ids: true,
            source_provenance: true,
            typed_objects: false,
            graph_edges: false,
            temporal_validity: false,
            correction_feedback: false,
            projection_support: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineCandidate {
    pub engine: String,
    pub candidate_id: MemoryId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_id: Option<MemoryId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<MemoryId>,
    pub score: f32,
    pub reason: String,
    #[serde(default)]
    pub source_span_ids: Vec<MemoryId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(default)]
    pub engine_trace: serde_json::Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalTrace {
    pub engine: String,
    pub mode: MemoryEngineMode,
    pub capabilities: MemoryEngineCapabilities,
    pub candidate_count: usize,
    pub candidates: Vec<EngineCandidate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineRetrievalResult {
    pub retrieval: RetrievalResult,
    pub trace: RetrievalTrace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryEngineRecommendation {
    PrimaryCandidate,
    AuxiliaryCandidate,
    BenchmarkOnly,
    Rejected,
}

impl MemoryEngineRecommendation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PrimaryCandidate => "primary_candidate",
            Self::AuxiliaryCandidate => "auxiliary_candidate",
            Self::BenchmarkOnly => "benchmark_only",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryEngineTraceScorecard {
    pub engine: String,
    pub mode: MemoryEngineMode,
    pub candidate_count: usize,
    pub candidates_with_provenance: usize,
    pub candidates_with_typed_objects: usize,
    pub limitation_count: usize,
    pub capability_score_pct: f32,
    pub provenance_score_pct: f32,
    pub typed_memory_score_pct: f32,
    pub recommendation: MemoryEngineRecommendation,
}

impl MemoryEngineTraceScorecard {
    pub fn from_trace(trace: &RetrievalTrace) -> Self {
        let candidate_count = trace.candidates.len();
        let candidates_with_provenance = trace
            .candidates
            .iter()
            .filter(|candidate| {
                candidate.source_id.is_some() || !candidate.source_span_ids.is_empty()
            })
            .count();
        let candidates_with_typed_objects = trace
            .candidates
            .iter()
            .filter(|candidate| candidate.object_id.is_some())
            .count();
        let limitation_count = trace
            .candidates
            .iter()
            .map(|candidate| candidate.limitations.len())
            .sum();
        let capability_score_pct = capability_score_pct(&trace.capabilities);
        let provenance_score_pct = pct(candidates_with_provenance, candidate_count);
        let typed_memory_score_pct = pct(candidates_with_typed_objects, candidate_count);
        let recommendation = recommendation_for_trace(
            trace,
            candidate_count,
            provenance_score_pct,
            typed_memory_score_pct,
        );

        Self {
            engine: trace.engine.clone(),
            mode: trace.mode,
            candidate_count,
            candidates_with_provenance,
            candidates_with_typed_objects,
            limitation_count,
            capability_score_pct,
            provenance_score_pct,
            typed_memory_score_pct,
            recommendation,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalPolicy {
    pub name: String,
    pub min_candidates: usize,
    pub require_source_provenance: bool,
    pub require_typed_objects: bool,
    pub require_projection_support: bool,
}

impl RetrievalPolicy {
    pub fn evaluation() -> Self {
        Self {
            name: "evaluation".to_string(),
            min_candidates: 1,
            require_source_provenance: true,
            require_typed_objects: false,
            require_projection_support: false,
        }
    }

    pub fn production_primary() -> Self {
        Self {
            name: "production_primary".to_string(),
            min_candidates: 1,
            require_source_provenance: true,
            require_typed_objects: true,
            require_projection_support: true,
        }
    }

    pub fn evaluate(&self, trace: &RetrievalTrace) -> RetrievalPolicyEvaluation {
        let scorecard = MemoryEngineTraceScorecard::from_trace(trace);
        let mut failures = Vec::new();
        if scorecard.candidate_count < self.min_candidates {
            failures.push(format!(
                "candidate_count {} below required {}",
                scorecard.candidate_count, self.min_candidates
            ));
        }
        if self.require_source_provenance
            && (!trace.capabilities.source_provenance || scorecard.provenance_score_pct < 100.0)
        {
            failures.push("source provenance requirement not satisfied".to_string());
        }
        if self.require_typed_objects
            && (!trace.capabilities.typed_objects || scorecard.typed_memory_score_pct < 100.0)
        {
            failures.push("typed object requirement not satisfied".to_string());
        }
        if self.require_projection_support && !trace.capabilities.projection_support {
            failures.push("projection support requirement not satisfied".to_string());
        }

        RetrievalPolicyEvaluation {
            policy: self.name.clone(),
            passed: failures.is_empty(),
            failures,
            scorecard,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalPolicyEvaluation {
    pub policy: String,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<String>,
    pub scorecard: MemoryEngineTraceScorecard,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyRetrievalResult {
    pub engine_result: EngineRetrievalResult,
    pub policy: RetrievalPolicyEvaluation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineSelectionScore {
    pub engine: String,
    pub selected: bool,
    pub policy_passed: bool,
    pub score: f32,
    pub recommendation: MemoryEngineRecommendation,
    pub candidate_count: usize,
    pub provenance_score_pct: f32,
    pub typed_memory_score_pct: f32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineSelection {
    pub selected_engine: Option<String>,
    pub selected_index: Option<usize>,
    pub reason: String,
    pub scores: Vec<EngineSelectionScore>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiEngineRetrievalResult {
    pub query: RetrievalQuery,
    pub selection: EngineSelection,
    pub engine_results: Vec<PolicyRetrievalResult>,
}

impl MultiEngineRetrievalResult {
    pub fn selected_result(&self) -> Option<&PolicyRetrievalResult> {
        self.selection
            .selected_index
            .and_then(|index| self.engine_results.get(index))
    }
}

pub trait MemoryEngineAdapter {
    fn name(&self) -> &str;
    fn mode(&self) -> MemoryEngineMode;
    fn capabilities(&self) -> MemoryEngineCapabilities;
    fn retrieve(&self, query: RetrievalQuery) -> MemoryResult<EngineRetrievalResult>;
}

pub fn retrieve_with_policy<A: MemoryEngineAdapter + ?Sized>(
    adapter: &A,
    query: RetrievalQuery,
    policy: &RetrievalPolicy,
) -> MemoryResult<PolicyRetrievalResult> {
    let engine_result = adapter.retrieve(query)?;
    let policy = policy.evaluate(&engine_result.trace);
    Ok(PolicyRetrievalResult {
        engine_result,
        policy,
    })
}

pub fn retrieve_with_policy_selection(
    adapters: &[&dyn MemoryEngineAdapter],
    query: RetrievalQuery,
    policy: &RetrievalPolicy,
) -> MemoryResult<MultiEngineRetrievalResult> {
    let mut engine_results = Vec::new();
    for adapter in adapters {
        engine_results.push(retrieve_with_policy(*adapter, query.clone(), policy)?);
    }

    let selected_index = select_engine_index(&engine_results);
    let mut scores = engine_results
        .iter()
        .enumerate()
        .map(|(index, result)| selection_score(index, result, selected_index))
        .collect::<Vec<_>>();
    scores.sort_by(|a, b| a.engine.cmp(&b.engine));
    let selected_engine = selected_index
        .and_then(|index| engine_results.get(index))
        .map(|result| result.engine_result.trace.engine.clone());
    let reason = match selected_engine.as_deref() {
        Some(engine) => format!(
            "selected {engine} by policy pass, recommendation, provenance, typed-memory score, capability score, candidate count, and limitations"
        ),
        None => "no adapters produced a selectable result".to_string(),
    };

    Ok(MultiEngineRetrievalResult {
        query,
        selection: EngineSelection {
            selected_engine,
            selected_index,
            reason,
            scores,
        },
        engine_results,
    })
}

pub struct NativeStoreMemoryAdapter<'a, B: MemoryBackend + ?Sized> {
    name: String,
    mode: MemoryEngineMode,
    backend: &'a B,
}

impl<'a, B: MemoryBackend + ?Sized> NativeStoreMemoryAdapter<'a, B> {
    pub fn new(name: impl Into<String>, backend: &'a B) -> Self {
        Self {
            name: name.into(),
            mode: MemoryEngineMode::Evaluation,
            backend,
        }
    }

    pub fn with_mode(mut self, mode: MemoryEngineMode) -> Self {
        self.mode = mode;
        self
    }
}

impl<B: MemoryBackend + ?Sized> MemoryEngineAdapter for NativeStoreMemoryAdapter<'_, B> {
    fn name(&self) -> &str {
        &self.name
    }

    fn mode(&self) -> MemoryEngineMode {
        self.mode
    }

    fn capabilities(&self) -> MemoryEngineCapabilities {
        MemoryEngineCapabilities::native_store()
    }

    fn retrieve(&self, query: RetrievalQuery) -> MemoryResult<EngineRetrievalResult> {
        let retrieval = self.backend.retrieve(query)?;
        let candidates = retrieval
            .items
            .iter()
            .map(|item| {
                let mut limitations = Vec::new();
                if item.source_span_ids.is_empty() {
                    limitations.push("retrieval item has no source spans".to_string());
                }
                EngineCandidate {
                    engine: self.name.clone(),
                    candidate_id: item.object_id.clone(),
                    object_id: Some(item.object_id.clone()),
                    source_id: None,
                    score: item.score,
                    reason: item.reason.clone(),
                    source_span_ids: item.source_span_ids.clone(),
                    snippet: None,
                    engine_trace: json!({
                        "normalized_from": "RetrievalItem",
                        "reason": item.reason,
                    }),
                    limitations,
                }
            })
            .collect::<Vec<_>>();

        Ok(EngineRetrievalResult {
            trace: RetrievalTrace {
                engine: self.name.clone(),
                mode: self.mode,
                capabilities: self.capabilities(),
                candidate_count: candidates.len(),
                candidates,
                notes: vec![
                    "native_store adapter wraps MemoryBackend::retrieve without reranking"
                        .to_string(),
                ],
            },
            retrieval,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawSourceDocument {
    pub id: MemoryId,
    pub project_key: String,
    pub kind: MemorySourceKind,
    pub uri: String,
    pub title: Option<String>,
    pub text: String,
}

pub struct RawSourceMemoryAdapter {
    name: String,
    mode: MemoryEngineMode,
    documents: Vec<RawSourceDocument>,
}

impl RawSourceMemoryAdapter {
    pub fn new(name: impl Into<String>, documents: Vec<RawSourceDocument>) -> Self {
        Self {
            name: name.into(),
            mode: MemoryEngineMode::Evaluation,
            documents,
        }
    }

    pub fn with_mode(mut self, mode: MemoryEngineMode) -> Self {
        self.mode = mode;
        self
    }
}

impl MemoryEngineAdapter for RawSourceMemoryAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn mode(&self) -> MemoryEngineMode {
        self.mode
    }

    fn capabilities(&self) -> MemoryEngineCapabilities {
        MemoryEngineCapabilities::raw_source()
    }

    fn retrieve(&self, query: RetrievalQuery) -> MemoryResult<EngineRetrievalResult> {
        let query_terms = terms(&query.text);
        let mut candidates = self
            .documents
            .iter()
            .filter(|document| document.project_key == query.project_key)
            .filter_map(|document| {
                let score =
                    overlap_score(&query_terms, &format!("{} {}", document.uri, document.text));
                (score > 0.0).then(|| EngineCandidate {
                    engine: self.name.clone(),
                    candidate_id: document.id.clone(),
                    object_id: None,
                    source_id: Some(document.id.clone()),
                    score,
                    reason: "raw-source keyword-overlap baseline".to_string(),
                    source_span_ids: Vec::new(),
                    snippet: snippet_for(&document.text, &query_terms),
                    engine_trace: json!({
                        "normalized_from": "RawSourceDocument",
                        "source_kind": document.kind,
                        "uri": document.uri,
                    }),
                    limitations: vec![
                        "raw source retrieval has no typed object, graph edge, or source span"
                            .to_string(),
                    ],
                })
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.candidate_id.cmp(&b.candidate_id))
        });
        candidates.truncate(query.limit.max(1));

        let items = candidates
            .iter()
            .map(|candidate| RetrievalItem {
                object_id: candidate.candidate_id.clone(),
                score: candidate.score,
                reason: candidate.reason.clone(),
                source_span_ids: Vec::new(),
            })
            .collect::<Vec<_>>();
        let context_md = candidates
            .iter()
            .filter_map(|candidate| {
                candidate
                    .snippet
                    .as_ref()
                    .map(|snippet| format!("- **{}**: {}", candidate.candidate_id, snippet))
            })
            .collect::<Vec<_>>()
            .join("\n");

        Ok(EngineRetrievalResult {
            retrieval: RetrievalResult {
                query,
                items,
                context_md,
            },
            trace: RetrievalTrace {
                engine: self.name.clone(),
                mode: self.mode,
                capabilities: self.capabilities(),
                candidate_count: candidates.len(),
                candidates,
                notes: vec![
                    "raw_source adapter is a retrieval baseline, not an operational memory store"
                        .to_string(),
                ],
            },
        })
    }
}

fn terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| s.len() > 2)
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

fn overlap_score(query_terms: &[String], text: &str) -> f32 {
    if query_terms.is_empty() {
        return 0.0;
    }
    let haystack = terms(text);
    let hits = query_terms
        .iter()
        .filter(|term| haystack.contains(term))
        .count();
    hits as f32 / query_terms.len() as f32
}

fn capability_score_pct(capabilities: &MemoryEngineCapabilities) -> f32 {
    let count = [
        capabilities.stable_ids,
        capabilities.source_provenance,
        capabilities.typed_objects,
        capabilities.graph_edges,
        capabilities.temporal_validity,
        capabilities.correction_feedback,
        capabilities.projection_support,
    ]
    .into_iter()
    .filter(|enabled| *enabled)
    .count();
    pct(count, 7)
}

fn recommendation_for_trace(
    trace: &RetrievalTrace,
    candidate_count: usize,
    provenance_score_pct: f32,
    typed_memory_score_pct: f32,
) -> MemoryEngineRecommendation {
    if candidate_count == 0 {
        return MemoryEngineRecommendation::Rejected;
    }
    if trace.capabilities.typed_objects
        && trace.capabilities.source_provenance
        && trace.capabilities.projection_support
        && provenance_score_pct >= 80.0
        && typed_memory_score_pct >= 80.0
    {
        return MemoryEngineRecommendation::PrimaryCandidate;
    }
    if trace.capabilities.source_provenance && provenance_score_pct >= 80.0 {
        return MemoryEngineRecommendation::AuxiliaryCandidate;
    }
    MemoryEngineRecommendation::BenchmarkOnly
}

fn select_engine_index(results: &[PolicyRetrievalResult]) -> Option<usize> {
    results
        .iter()
        .enumerate()
        .filter(|(_, result)| result.policy.scorecard.candidate_count > 0)
        .max_by(|(left_index, left), (right_index, right)| {
            selection_rank(left)
                .partial_cmp(&selection_rank(right))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    let left_engine = &left.engine_result.trace.engine;
                    let right_engine = &right.engine_result.trace.engine;
                    right_engine.cmp(left_engine)
                })
                .then_with(|| right_index.cmp(left_index))
        })
        .map(|(index, _)| index)
}

fn selection_score(
    index: usize,
    result: &PolicyRetrievalResult,
    selected_index: Option<usize>,
) -> EngineSelectionScore {
    let scorecard = &result.policy.scorecard;
    EngineSelectionScore {
        engine: scorecard.engine.clone(),
        selected: selected_index == Some(index),
        policy_passed: result.policy.passed,
        score: selection_rank(result),
        recommendation: scorecard.recommendation,
        candidate_count: scorecard.candidate_count,
        provenance_score_pct: scorecard.provenance_score_pct,
        typed_memory_score_pct: scorecard.typed_memory_score_pct,
        failures: result.policy.failures.clone(),
    }
}

fn selection_rank(result: &PolicyRetrievalResult) -> f32 {
    let scorecard = &result.policy.scorecard;
    let pass_weight = if result.policy.passed {
        1_000.0
    } else {
        -1_000.0
    };
    let recommendation_weight = match scorecard.recommendation {
        MemoryEngineRecommendation::PrimaryCandidate => 500.0,
        MemoryEngineRecommendation::AuxiliaryCandidate => 250.0,
        MemoryEngineRecommendation::BenchmarkOnly => 50.0,
        MemoryEngineRecommendation::Rejected => -500.0,
    };
    pass_weight
        + recommendation_weight
        + engine_reliability_prior(&scorecard.engine)
        + scorecard.provenance_score_pct * 2.0
        + scorecard.typed_memory_score_pct * 1.5
        + scorecard.capability_score_pct
        + scorecard.candidate_count.min(10) as f32 * 5.0
        - scorecard.limitation_count as f32 * 2.0
}

fn engine_reliability_prior(engine: &str) -> f32 {
    match engine {
        "local_reference" => 120.0,
        "local_extract" => 100.0,
        "native_store" => 80.0,
        "source_memory" => 20.0,
        "keyword_source" | "raw_source" => 0.0,
        _ => 40.0,
    }
}

fn pct(passed: usize, total: usize) -> f32 {
    if total == 0 {
        0.0
    } else {
        (passed as f32 / total as f32) * 100.0
    }
}

fn snippet_for(text: &str, query_terms: &[String]) -> Option<String> {
    let clean = text
        .lines()
        .map(str::trim)
        .find(|line| {
            let line_terms = terms(line);
            query_terms
                .iter()
                .any(|query_term| line_terms.contains(query_term))
        })
        .or_else(|| text.lines().map(str::trim).find(|line| !line.is_empty()))?;
    Some(clean.chars().take(240).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{
        InMemoryMemoryBackend, MemoryObject, MemoryObjectKind, MemoryObjectState, MemorySource,
        MemorySpan, RetrievalIntent,
    };

    #[test]
    fn native_store_adapter_preserves_retrieval_and_adds_trace() {
        let mut backend = InMemoryMemoryBackend::default();
        backend
            .ingest_source(MemorySource {
                id: "source-1".to_string(),
                project_key: "orchestrator".to_string(),
                kind: MemorySourceKind::Doc,
                uri: "docs/memory.md".to_string(),
                title: Some("Memory".to_string()),
                captured_at_secs: 1,
                content_hash: None,
                metadata: serde_json::json!({}),
            })
            .unwrap();
        backend
            .add_span(MemorySpan {
                id: "span-1".to_string(),
                source_id: "source-1".to_string(),
                start_ref: None,
                end_ref: None,
                quote: Some("corrections propagate".to_string()),
            })
            .unwrap();
        backend
            .upsert_object(MemoryObject {
                id: "obj-1".to_string(),
                project_key: "orchestrator".to_string(),
                kind: MemoryObjectKind::Constraint,
                title: "Corrections propagate".to_string(),
                body_md: "Rejected proposals must affect later retrieval.".to_string(),
                state: MemoryObjectState::Active,
                confidence: 1.0,
                created_by: "test".to_string(),
                created_at_secs: 1,
                updated_at_secs: 1,
                valid_from_secs: None,
                valid_to_secs: None,
                superseded_by: None,
                source_span_ids: vec!["span-1".to_string()],
                projection: serde_json::json!({}),
                metadata: serde_json::json!({}),
            })
            .unwrap();

        let adapter = NativeStoreMemoryAdapter::new("native_store", &backend);
        let result = adapter
            .retrieve(RetrievalQuery {
                project_key: "orchestrator".to_string(),
                intent: RetrievalIntent::MidSession,
                text: "How do corrections affect retrieval?".to_string(),
                scope_memory_id: None,
                since_secs: None,
                limit: 5,
            })
            .unwrap();

        assert_eq!(result.retrieval.items[0].object_id, "obj-1");
        assert_eq!(result.trace.engine, "native_store");
        assert_eq!(
            result.trace.candidates[0].object_id.as_deref(),
            Some("obj-1")
        );
        assert_eq!(result.trace.candidates[0].source_span_ids, vec!["span-1"]);
    }

    #[test]
    fn raw_source_adapter_returns_source_candidates() {
        let adapter = RawSourceMemoryAdapter::new(
            "raw_source",
            vec![RawSourceDocument {
                id: "doc-1".to_string(),
                project_key: "orchestrator".to_string(),
                kind: MemorySourceKind::Doc,
                uri: "docs/memory.md".to_string(),
                title: Some("Memory".to_string()),
                text: "The Map is a projection over memory, not the memory substrate.".to_string(),
            }],
        );

        let result = adapter
            .retrieve(RetrievalQuery {
                project_key: "orchestrator".to_string(),
                intent: RetrievalIntent::Kickoff,
                text: "memory projection".to_string(),
                scope_memory_id: None,
                since_secs: None,
                limit: 5,
            })
            .unwrap();

        assert_eq!(
            result.trace.candidates[0].source_id.as_deref(),
            Some("doc-1")
        );
        assert!(!result.trace.capabilities.typed_objects);
        assert!(result.retrieval.context_md.contains("Map is a projection"));
    }

    #[test]
    fn trace_scorecard_classifies_raw_source_as_auxiliary() {
        let adapter = RawSourceMemoryAdapter::new(
            "raw_source",
            vec![RawSourceDocument {
                id: "doc-1".to_string(),
                project_key: "orchestrator".to_string(),
                kind: MemorySourceKind::Doc,
                uri: "docs/memory.md".to_string(),
                title: None,
                text: "The source ledger keeps provenance for memory.".to_string(),
            }],
        );
        let result = adapter
            .retrieve(RetrievalQuery {
                project_key: "orchestrator".to_string(),
                intent: RetrievalIntent::Kickoff,
                text: "source provenance memory".to_string(),
                scope_memory_id: None,
                since_secs: None,
                limit: 5,
            })
            .unwrap();

        let scorecard = MemoryEngineTraceScorecard::from_trace(&result.trace);

        assert_eq!(
            scorecard.recommendation,
            MemoryEngineRecommendation::AuxiliaryCandidate
        );
        assert_eq!(scorecard.provenance_score_pct, 100.0);
        assert_eq!(scorecard.typed_memory_score_pct, 0.0);
    }

    #[test]
    fn production_primary_policy_rejects_raw_source_adapter() {
        let adapter = RawSourceMemoryAdapter::new(
            "raw_source",
            vec![RawSourceDocument {
                id: "doc-1".to_string(),
                project_key: "orchestrator".to_string(),
                kind: MemorySourceKind::Doc,
                uri: "docs/memory.md".to_string(),
                title: None,
                text: "The Map is compiled from typed memory objects.".to_string(),
            }],
        );
        let result = retrieve_with_policy(
            &adapter,
            RetrievalQuery {
                project_key: "orchestrator".to_string(),
                intent: RetrievalIntent::Kickoff,
                text: "typed memory objects".to_string(),
                scope_memory_id: None,
                since_secs: None,
                limit: 5,
            },
            &RetrievalPolicy::production_primary(),
        )
        .unwrap();

        assert!(!result.policy.passed);
        assert!(result
            .policy
            .failures
            .iter()
            .any(|failure| failure.contains("typed object")));
        assert!(result
            .policy
            .failures
            .iter()
            .any(|failure| failure.contains("projection support")));
    }

    #[test]
    fn policy_selection_prefers_native_store_for_production_primary() {
        let mut backend = InMemoryMemoryBackend::default();
        backend
            .ingest_source(MemorySource {
                id: "source-1".to_string(),
                project_key: "orchestrator".to_string(),
                kind: MemorySourceKind::Doc,
                uri: "docs/memory.md".to_string(),
                title: Some("Memory".to_string()),
                captured_at_secs: 1,
                content_hash: None,
                metadata: serde_json::json!({}),
            })
            .unwrap();
        backend
            .add_span(MemorySpan {
                id: "span-1".to_string(),
                source_id: "source-1".to_string(),
                start_ref: None,
                end_ref: None,
                quote: Some("Map projection memory".to_string()),
            })
            .unwrap();
        backend
            .upsert_object(MemoryObject {
                id: "obj-1".to_string(),
                project_key: "orchestrator".to_string(),
                kind: MemoryObjectKind::Decision,
                title: "Map projection memory".to_string(),
                body_md: "The Map is compiled from typed memory objects.".to_string(),
                state: MemoryObjectState::Active,
                confidence: 1.0,
                created_by: "test".to_string(),
                created_at_secs: 1,
                updated_at_secs: 1,
                valid_from_secs: None,
                valid_to_secs: None,
                superseded_by: None,
                source_span_ids: vec!["span-1".to_string()],
                projection: serde_json::json!({}),
                metadata: serde_json::json!({}),
            })
            .unwrap();
        let native = NativeStoreMemoryAdapter::new("native_store", &backend);
        let raw = RawSourceMemoryAdapter::new(
            "raw_source",
            vec![RawSourceDocument {
                id: "source-1".to_string(),
                project_key: "orchestrator".to_string(),
                kind: MemorySourceKind::Doc,
                uri: "docs/memory.md".to_string(),
                title: Some("Memory".to_string()),
                text: "The Map is compiled from typed memory objects.".to_string(),
            }],
        );

        let result = retrieve_with_policy_selection(
            &[&raw, &native],
            RetrievalQuery {
                project_key: "orchestrator".to_string(),
                intent: RetrievalIntent::Kickoff,
                text: "typed memory objects".to_string(),
                scope_memory_id: None,
                since_secs: None,
                limit: 5,
            },
            &RetrievalPolicy::production_primary(),
        )
        .unwrap();

        assert_eq!(
            result.selection.selected_engine.as_deref(),
            Some("native_store")
        );
        assert!(result
            .selection
            .scores
            .iter()
            .any(|score| score.engine == "raw_source" && !score.policy_passed));
    }

    #[test]
    fn policy_selection_falls_back_to_raw_source_when_native_is_empty() {
        let backend = InMemoryMemoryBackend::default();
        let native = NativeStoreMemoryAdapter::new("native_store", &backend);
        let raw = RawSourceMemoryAdapter::new(
            "raw_source",
            vec![RawSourceDocument {
                id: "doc-1".to_string(),
                project_key: "orchestrator".to_string(),
                kind: MemorySourceKind::Doc,
                uri: "docs/memory.md".to_string(),
                title: None,
                text: "The source ledger keeps memory provenance.".to_string(),
            }],
        );

        let result = retrieve_with_policy_selection(
            &[&native, &raw],
            RetrievalQuery {
                project_key: "orchestrator".to_string(),
                intent: RetrievalIntent::Kickoff,
                text: "source memory provenance".to_string(),
                scope_memory_id: None,
                since_secs: None,
                limit: 5,
            },
            &RetrievalPolicy::evaluation(),
        )
        .unwrap();

        assert_eq!(
            result.selection.selected_engine.as_deref(),
            Some("raw_source")
        );
        assert_eq!(
            result
                .selected_result()
                .expect("selected result")
                .engine_result
                .trace
                .candidates[0]
                .source_id
                .as_deref(),
            Some("doc-1")
        );
    }

    #[test]
    fn policy_selection_uses_engine_reliability_prior_for_equivalent_native_results() {
        let mut backend = InMemoryMemoryBackend::default();
        backend
            .ingest_source(MemorySource {
                id: "source-1".to_string(),
                project_key: "orchestrator".to_string(),
                kind: MemorySourceKind::Doc,
                uri: "docs/memory.md".to_string(),
                title: Some("Memory".to_string()),
                captured_at_secs: 1,
                content_hash: None,
                metadata: serde_json::json!({}),
            })
            .unwrap();
        backend
            .add_span(MemorySpan {
                id: "span-1".to_string(),
                source_id: "source-1".to_string(),
                start_ref: None,
                end_ref: None,
                quote: Some("source bucket leakage".to_string()),
            })
            .unwrap();
        backend
            .upsert_object(MemoryObject {
                id: "obj-1".to_string(),
                project_key: "orchestrator".to_string(),
                kind: MemoryObjectKind::Constraint,
                title: "Source bucket leakage".to_string(),
                body_md: "Docs should not become Map roots.".to_string(),
                state: MemoryObjectState::Active,
                confidence: 1.0,
                created_by: "test".to_string(),
                created_at_secs: 1,
                updated_at_secs: 1,
                valid_from_secs: None,
                valid_to_secs: None,
                superseded_by: None,
                source_span_ids: vec!["span-1".to_string()],
                projection: serde_json::json!({}),
                metadata: serde_json::json!({}),
            })
            .unwrap();
        let source_memory = NativeStoreMemoryAdapter::new("source_memory", &backend);
        let local_extract = NativeStoreMemoryAdapter::new("local_extract", &backend);

        let result = retrieve_with_policy_selection(
            &[&source_memory, &local_extract],
            RetrievalQuery {
                project_key: "orchestrator".to_string(),
                intent: RetrievalIntent::Kickoff,
                text: "source bucket leakage".to_string(),
                scope_memory_id: None,
                since_secs: None,
                limit: 5,
            },
            &RetrievalPolicy::evaluation(),
        )
        .unwrap();

        assert_eq!(
            result.selection.selected_engine.as_deref(),
            Some("local_extract")
        );
    }
}
