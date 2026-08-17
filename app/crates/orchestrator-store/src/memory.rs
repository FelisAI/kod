//! Product memory substrate contract (docs/020-021).
//!
//! This module is intentionally interface-first. The first implementation can
//! be native SQLite, Mem0, Graphiti, Memora-like, or a hybrid, but the rest of
//! orchestrator should depend on this product-facing shape:
//!
//! Source -> MemoryObject -> MemoryEdge -> RetrievalResult -> Projection.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::fmt;

pub type MemoryId = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryError {
    pub message: String,
}

impl MemoryError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for MemoryError {}

pub type MemoryResult<T> = Result<T, MemoryError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemorySourceKind {
    Doc,
    Code,
    Session,
    MapPart,
    Git,
    UserCapture,
    Artifact,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySource {
    pub id: MemoryId,
    pub project_key: String,
    pub kind: MemorySourceKind,
    pub uri: String,
    pub title: Option<String>,
    pub captured_at_secs: u64,
    pub content_hash: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySpan {
    pub id: MemoryId,
    pub source_id: MemoryId,
    pub start_ref: Option<String>,
    pub end_ref: Option<String>,
    pub quote: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryObjectKind {
    Concept,
    Area,
    Task,
    Decision,
    Constraint,
    Claim,
    Question,
    SessionEvent,
    Learning,
    Artifact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryObjectState {
    Candidate,
    Active,
    Rejected,
    Superseded,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryObject {
    pub id: MemoryId,
    pub project_key: String,
    pub kind: MemoryObjectKind,
    pub title: String,
    #[serde(default)]
    pub body_md: String,
    pub state: MemoryObjectState,
    pub confidence: f32,
    pub created_by: String,
    pub created_at_secs: u64,
    pub updated_at_secs: u64,
    pub valid_from_secs: Option<u64>,
    pub valid_to_secs: Option<u64>,
    pub superseded_by: Option<MemoryId>,
    #[serde(default)]
    pub source_span_ids: Vec<MemoryId>,
    #[serde(default)]
    pub projection: serde_json::Value,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryEdgeKind {
    DerivedFrom,
    Mentions,
    Supports,
    Contradicts,
    Supersedes,
    DependsOn,
    Blocks,
    Implements,
    BelongsTo,
    TouchesFile,
    SameAs,
    RelatedTo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryEdge {
    pub id: MemoryId,
    pub project_key: String,
    pub src_id: MemoryId,
    pub dst_id: MemoryId,
    pub kind: MemoryEdgeKind,
    pub confidence: f32,
    pub created_at_secs: u64,
    pub source_span_id: Option<MemoryId>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetrievalIntent {
    Kickoff,
    MidSession,
    ExplainNode,
    ProjectStatus,
    WhatChanged,
    ExpandMemory,
    ReprojectMap,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalQuery {
    pub project_key: String,
    pub intent: RetrievalIntent,
    pub text: String,
    pub scope_memory_id: Option<MemoryId>,
    pub since_secs: Option<u64>,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalItem {
    pub object_id: MemoryId,
    pub score: f32,
    pub reason: String,
    #[serde(default)]
    pub source_span_ids: Vec<MemoryId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalResult {
    pub query: RetrievalQuery,
    pub items: Vec<RetrievalItem>,
    pub context_md: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectionKind {
    Map,
    Evidence,
    Decision,
    Timeline,
    OpenQuestions,
    Brain,
    KickoffContext,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionRequest {
    pub project_key: String,
    pub kind: ProjectionKind,
    pub scope_memory_id: Option<MemoryId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectionTrust {
    HumanAccepted,
    AgentCandidate,
    Verified,
    Stale,
    Contradicted,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectionItem {
    pub view_id: String,
    pub memory_id: MemoryId,
    pub label: String,
    pub role: String,
    pub trust: ProjectionTrust,
    #[serde(default)]
    pub actions: Vec<String>,
    #[serde(default)]
    pub explanation_span_ids: Vec<MemoryId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Projection {
    pub request: ProjectionRequest,
    pub items: Vec<ProjectionItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HumanCorrection {
    pub project_key: String,
    pub target_id: MemoryId,
    pub action: String,
    pub note: String,
    pub corrected_at_secs: u64,
}

/// Product-facing backend contract. Keep this synchronous until the first real
/// async caller forces the issue; sync keeps SQLite and eval harnesses simple.
pub trait MemoryBackend {
    fn ingest_source(&mut self, source: MemorySource) -> MemoryResult<()>;
    fn add_span(&mut self, span: MemorySpan) -> MemoryResult<()>;
    fn upsert_object(&mut self, object: MemoryObject) -> MemoryResult<()>;
    fn upsert_edge(&mut self, edge: MemoryEdge) -> MemoryResult<()>;
    fn load_object(&self, id: &str) -> MemoryResult<Option<MemoryObject>>;
    fn retrieve(&self, query: RetrievalQuery) -> MemoryResult<RetrievalResult>;
    fn project(&self, request: ProjectionRequest) -> MemoryResult<Projection>;
    fn record_correction(&mut self, correction: HumanCorrection) -> MemoryResult<()>;
}

#[derive(Debug, Default)]
pub struct InMemoryMemoryBackend {
    sources: HashMap<MemoryId, MemorySource>,
    spans: HashMap<MemoryId, MemorySpan>,
    objects: HashMap<MemoryId, MemoryObject>,
    edges: HashMap<MemoryId, MemoryEdge>,
    corrections: Vec<HumanCorrection>,
}

impl InMemoryMemoryBackend {
    pub fn object(&self, id: &str) -> Option<&MemoryObject> {
        self.objects.get(id)
    }

    pub fn span(&self, id: &str) -> Option<&MemorySpan> {
        self.spans.get(id)
    }

    pub fn source(&self, id: &str) -> Option<&MemorySource> {
        self.sources.get(id)
    }

    pub fn corrections(&self) -> &[HumanCorrection] {
        &self.corrections
    }
}

impl MemoryBackend for InMemoryMemoryBackend {
    fn ingest_source(&mut self, source: MemorySource) -> MemoryResult<()> {
        self.sources.insert(source.id.clone(), source);
        Ok(())
    }

    fn add_span(&mut self, span: MemorySpan) -> MemoryResult<()> {
        if !self.sources.contains_key(&span.source_id) {
            return Err(MemoryError::new(format!(
                "unknown source {}",
                span.source_id
            )));
        }
        self.spans.insert(span.id.clone(), span);
        Ok(())
    }

    fn upsert_object(&mut self, object: MemoryObject) -> MemoryResult<()> {
        self.objects.insert(object.id.clone(), object);
        Ok(())
    }

    fn upsert_edge(&mut self, edge: MemoryEdge) -> MemoryResult<()> {
        self.edges.insert(edge.id.clone(), edge);
        Ok(())
    }

    fn load_object(&self, id: &str) -> MemoryResult<Option<MemoryObject>> {
        Ok(self.objects.get(id).cloned())
    }

    fn retrieve(&self, query: RetrievalQuery) -> MemoryResult<RetrievalResult> {
        let query_terms = terms(&query.text);
        let mut items: Vec<RetrievalItem> = self
            .objects
            .values()
            .filter(|object| object.project_key == query.project_key)
            .filter(|object| {
                !matches!(
                    object.state,
                    MemoryObjectState::Rejected | MemoryObjectState::Superseded
                )
            })
            .filter_map(|object| {
                let title_score = overlap_score(&query_terms, &object.title);
                let body_score = overlap_score(&query_terms, &object.body_md);
                let score = title_score * 2.0 + body_score;
                (score > 0.0).then(|| RetrievalItem {
                    object_id: object.id.clone(),
                    score,
                    reason: "keyword-overlap baseline".to_string(),
                    source_span_ids: object.source_span_ids.clone(),
                })
            })
            .collect();
        items.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.object_id.cmp(&b.object_id))
        });
        items.truncate(query.limit.max(1));

        let context_md = items
            .iter()
            .filter_map(|item| self.objects.get(&item.object_id))
            .map(|object| format!("- **{}**: {}", object.title, object.body_md))
            .collect::<Vec<_>>()
            .join("\n");

        Ok(RetrievalResult {
            query,
            items,
            context_md,
        })
    }

    fn project(&self, request: ProjectionRequest) -> MemoryResult<Projection> {
        let mut items: Vec<ProjectionItem> = self
            .objects
            .values()
            .filter(|object| object.project_key == request.project_key)
            .filter(|object| match request.kind {
                ProjectionKind::Map => matches!(
                    object.kind,
                    MemoryObjectKind::Area | MemoryObjectKind::Task | MemoryObjectKind::Concept
                ),
                ProjectionKind::Decision => object.kind == MemoryObjectKind::Decision,
                ProjectionKind::OpenQuestions => object.kind == MemoryObjectKind::Question,
                _ => true,
            })
            .map(|object| ProjectionItem {
                view_id: format!("{:?}:{}", request.kind, object.id),
                memory_id: object.id.clone(),
                label: object.title.clone(),
                role: format!("{:?}", object.kind),
                trust: trust_for(object),
                actions: vec!["show_evidence".to_string(), "edit".to_string()],
                explanation_span_ids: object.source_span_ids.clone(),
            })
            .collect();
        items.sort_by(|a, b| a.label.cmp(&b.label));
        Ok(Projection { request, items })
    }

    fn record_correction(&mut self, correction: HumanCorrection) -> MemoryResult<()> {
        self.corrections.push(correction);
        Ok(())
    }
}

fn trust_for(object: &MemoryObject) -> ProjectionTrust {
    match object.state {
        MemoryObjectState::Candidate => ProjectionTrust::AgentCandidate,
        MemoryObjectState::Active if object.source_span_ids.is_empty() => {
            ProjectionTrust::Unsupported
        }
        MemoryObjectState::Active => ProjectionTrust::Verified,
        MemoryObjectState::Rejected => ProjectionTrust::Unsupported,
        MemoryObjectState::Superseded => ProjectionTrust::Contradicted,
        MemoryObjectState::Stale => ProjectionTrust::Stale,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn active_constraint(id: &str, title: &str, body: &str) -> MemoryObject {
        MemoryObject {
            id: id.to_string(),
            project_key: "orchestrator".to_string(),
            kind: MemoryObjectKind::Constraint,
            title: title.to_string(),
            body_md: body.to_string(),
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
        }
    }

    #[test]
    fn in_memory_backend_retrieves_matching_objects() {
        let mut backend = InMemoryMemoryBackend::default();
        backend
            .ingest_source(MemorySource {
                id: "source-1".to_string(),
                project_key: "orchestrator".to_string(),
                kind: MemorySourceKind::Doc,
                uri: "docs/019-map-v3-owned-brain.md".to_string(),
                title: Some("Map v3".to_string()),
                captured_at_secs: 1,
                content_hash: None,
                metadata: serde_json::json!({}),
            })
            .unwrap();
        backend
            .add_span(MemorySpan {
                id: "span-1".to_string(),
                source_id: "source-1".to_string(),
                start_ref: Some("line:1".to_string()),
                end_ref: None,
                quote: Some("building is derived-only".to_string()),
            })
            .unwrap();
        backend
            .upsert_object(active_constraint(
                "obj-1",
                "Building is derived-only",
                "The building lifecycle must be derived from live session links, never stored.",
            ))
            .unwrap();

        let result = backend
            .retrieve(RetrievalQuery {
                project_key: "orchestrator".to_string(),
                intent: RetrievalIntent::Kickoff,
                text: "What governs stored versus derived building status?".to_string(),
                scope_memory_id: None,
                since_secs: None,
                limit: 5,
            })
            .unwrap();

        assert_eq!(result.items[0].object_id, "obj-1");
        assert!(result.context_md.contains("Building is derived-only"));
    }

    #[test]
    fn projection_filters_map_objects() {
        let mut backend = InMemoryMemoryBackend::default();
        backend
            .upsert_object(MemoryObject {
                id: "decision-1".to_string(),
                project_key: "orchestrator".to_string(),
                kind: MemoryObjectKind::Decision,
                title: "Use typed memory".to_string(),
                body_md: String::new(),
                state: MemoryObjectState::Active,
                confidence: 1.0,
                created_by: "test".to_string(),
                created_at_secs: 1,
                updated_at_secs: 1,
                valid_from_secs: None,
                valid_to_secs: None,
                superseded_by: None,
                source_span_ids: vec![],
                projection: serde_json::json!({}),
                metadata: serde_json::json!({}),
            })
            .unwrap();
        let mut area = active_constraint("area-1", "Memory substrate", "Own the memory backend.");
        area.kind = MemoryObjectKind::Area;
        backend.upsert_object(area).unwrap();

        let projection = backend
            .project(ProjectionRequest {
                project_key: "orchestrator".to_string(),
                kind: ProjectionKind::Map,
                scope_memory_id: None,
            })
            .unwrap();

        assert_eq!(projection.items.len(), 1);
        assert_eq!(projection.items[0].memory_id, "area-1");
    }

    #[test]
    fn orchestrator_memory_eval_fixture_loads() {
        let raw = include_str!("../../../fixtures/memory/orchestrator/eval.json");
        let fixture: serde_json::Value = serde_json::from_str(raw).expect("fixture json");
        assert_eq!(fixture["project_key"], "orchestrator");
        assert!(fixture["tasks"].as_array().expect("tasks").len() >= 20);
    }
}
