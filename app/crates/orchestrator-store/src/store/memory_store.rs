//! Memory-layer store methods and the `MemoryBackend` trait impl for `Store`.
//! Extracted verbatim from `store.rs` (decomposition; behavior unchanged).

use rusqlite::params;

use super::{now, MemoryCandidateRow, Store};
use crate::memory::{
    HumanCorrection, MemoryBackend, MemoryEdge, MemoryEdgeKind, MemoryError, MemoryObject,
    MemoryObjectKind, MemoryObjectState, MemoryResult, MemorySource, MemorySourceKind, MemorySpan,
    Projection, ProjectionItem, ProjectionKind, ProjectionRequest, ProjectionTrust, RetrievalItem,
    RetrievalQuery, RetrievalResult,
};
use crate::memory_engine::{
    apply_native_memory_candidates, NativeMemoryCandidate, NativeMemoryDecisionKind,
    NativeMemoryEngineReport,
};
use crate::memory_extract::{MemoryDocument, RuleBackedMemory};
use crate::memory_llm::parse_llm_memory_candidates;

impl Store {
    /// Apply already-parsed memory candidates directly into the persisted memory
    /// graph. This is the non-UI bridge for provider-backed extraction:
    /// source documents + parsed candidates -> native engine -> durable memory.
    pub fn apply_memory_candidates(
        &mut self,
        project_key: &str,
        documents: &[MemoryDocument],
        candidates: &[RuleBackedMemory],
        created_by: &str,
        now_secs: u64,
    ) -> MemoryResult<NativeMemoryEngineReport> {
        let native_candidates: Vec<NativeMemoryCandidate> = candidates
            .iter()
            .cloned()
            .map(NativeMemoryCandidate::from)
            .collect();
        apply_native_memory_candidates(
            self,
            project_key,
            documents,
            &native_candidates,
            created_by,
            now_secs,
        )
    }

    /// Parse an LLM extractor response and apply it through the same evidence,
    /// duplicate, no-op, and supersession gates as reviewed candidates.
    pub fn apply_llm_memory_output(
        &mut self,
        project_key: &str,
        documents: &[MemoryDocument],
        raw_output: &str,
        created_by: &str,
        now_secs: u64,
    ) -> MemoryResult<NativeMemoryEngineReport> {
        let candidates = parse_llm_memory_candidates(raw_output)?;
        self.apply_memory_candidates(project_key, documents, &candidates, created_by, now_secs)
    }

    pub fn add_memory_candidates(
        &self,
        project_key: &str,
        candidates: &[RuleBackedMemory],
        created_by: &str,
    ) -> rusqlite::Result<Vec<i64>> {
        let mut ids = Vec::new();
        for candidate in candidates {
            self.conn.execute(
                "INSERT INTO memory_candidate(project_key,candidate_json,created_by,created_secs,status)
                 VALUES(?1,?2,?3,?4,'open')",
                params![
                    project_key,
                    serde_json::to_string(candidate).unwrap_or_default(),
                    created_by,
                    now() as i64,
                ],
            )?;
            ids.push(self.conn.last_insert_rowid());
        }
        if !ids.is_empty() {
            self.bump_gen();
        }
        Ok(ids)
    }

    pub fn open_memory_candidates(
        &self,
        project_key: &str,
    ) -> rusqlite::Result<Vec<MemoryCandidateRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,project_key,candidate_json,created_by,created_secs,status
             FROM memory_candidate WHERE project_key=?1 AND status='open' ORDER BY id",
        )?;
        let rows = stmt.query_map(params![project_key], map_memory_candidate_row)?;
        rows.collect()
    }

    pub fn memory_candidate(&self, id: i64) -> rusqlite::Result<Option<MemoryCandidateRow>> {
        match self.conn.query_row(
            "SELECT id,project_key,candidate_json,created_by,created_secs,status
             FROM memory_candidate WHERE id=?1",
            params![id],
            map_memory_candidate_row,
        ) {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub fn memory_candidate_status(&self, id: i64) -> Option<String> {
        self.conn
            .query_row(
                "SELECT status FROM memory_candidate WHERE id=?1",
                params![id],
                |r| r.get(0),
            )
            .ok()
    }

    pub fn reject_memory_candidate(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE memory_candidate SET status='rejected' WHERE id=?1 AND status='open'",
            params![id],
        )?;
        self.bump_gen();
        Ok(())
    }

    /// Accept one staged memory candidate through the native memory engine. The
    /// candidate is not trusted just because it was staged: evidence is checked,
    /// duplicates/no-ops are skipped, and same-title revisions supersede older
    /// memory before the row receives a terminal status.
    pub fn accept_memory_candidate(
        &mut self,
        id: i64,
        documents: &[MemoryDocument],
        accepted_by: &str,
        now_secs: u64,
    ) -> MemoryResult<usize> {
        let row = self
            .memory_candidate(id)
            .map_err(memory_sql_error)?
            .ok_or_else(|| MemoryError::new(format!("unknown memory candidate {id}")))?;
        if row.status != "open" {
            return Err(MemoryError::new(format!(
                "memory candidate {id} is {}",
                row.status
            )));
        }
        let report = apply_native_memory_candidates(
            self,
            &row.project_key,
            documents,
            &[NativeMemoryCandidate::from(row.candidate)],
            accepted_by,
            now_secs,
        )?;
        let decision = report.decisions.first().map(|decision| decision.kind);
        let status = memory_candidate_terminal_status(decision, report.inserted);
        self.conn
            .execute(
                "UPDATE memory_candidate SET status=?2 WHERE id=?1",
                params![id, status],
            )
            .map_err(memory_sql_error)?;
        self.bump_gen();
        Ok(report.inserted)
    }
}

fn map_memory_candidate_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryCandidateRow> {
    let raw: String = r.get(2)?;
    let candidate = serde_json::from_str::<RuleBackedMemory>(&raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(MemoryCandidateRow {
        id: r.get(0)?,
        project_key: r.get(1)?,
        candidate,
        created_by: r.get(3)?,
        created_secs: r.get::<_, i64>(4)? as u64,
        status: r.get(5)?,
    })
}

fn memory_candidate_terminal_status(
    decision: Option<NativeMemoryDecisionKind>,
    inserted: usize,
) -> &'static str {
    match decision {
        Some(NativeMemoryDecisionKind::Insert) if inserted > 0 => "accepted",
        Some(NativeMemoryDecisionKind::Supersedes) if inserted > 0 => "superseded",
        Some(NativeMemoryDecisionKind::Duplicate) => "duplicate",
        Some(NativeMemoryDecisionKind::NoOp) => "noop",
        Some(NativeMemoryDecisionKind::Unsupported) => "unsupported",
        _ => "unsupported",
    }
}

impl MemoryBackend for Store {
    fn ingest_source(&mut self, source: MemorySource) -> MemoryResult<()> {
        self.conn
            .execute(
                "INSERT INTO memory_source(id,project_key,kind,uri,title,captured_at_secs,content_hash,metadata_json)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(id) DO UPDATE SET
                   project_key=excluded.project_key,
                   kind=excluded.kind,
                   uri=excluded.uri,
                   title=excluded.title,
                   captured_at_secs=excluded.captured_at_secs,
                   content_hash=excluded.content_hash,
                   metadata_json=excluded.metadata_json",
                params![
                    source.id,
                    source.project_key,
                    memory_source_kind_label(source.kind),
                    source.uri,
                    source.title,
                    source.captured_at_secs as i64,
                    source.content_hash,
                    json_string(&source.metadata),
                ],
            )
            .map_err(memory_sql_error)?;
        self.bump_gen();
        Ok(())
    }

    fn add_span(&mut self, span: MemorySpan) -> MemoryResult<()> {
        if !memory_source_exists(self, &span.source_id) {
            return Err(MemoryError::new(format!(
                "unknown source {}",
                span.source_id
            )));
        }
        self.conn
            .execute(
                "INSERT INTO memory_span(id,source_id,start_ref,end_ref,quote)
                 VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(id) DO UPDATE SET
                   source_id=excluded.source_id,
                   start_ref=excluded.start_ref,
                   end_ref=excluded.end_ref,
                   quote=excluded.quote",
                params![
                    span.id,
                    span.source_id,
                    span.start_ref,
                    span.end_ref,
                    span.quote
                ],
            )
            .map_err(memory_sql_error)?;
        self.bump_gen();
        Ok(())
    }

    fn upsert_object(&mut self, object: MemoryObject) -> MemoryResult<()> {
        self.conn
            .execute(
                "INSERT INTO memory_object(id,project_key,kind,title,body_md,state,confidence,created_by,created_at_secs,updated_at_secs,valid_from_secs,valid_to_secs,superseded_by,source_span_ids_json,projection_json,metadata_json)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
                 ON CONFLICT(id) DO UPDATE SET
                   project_key=excluded.project_key,
                   kind=excluded.kind,
                   title=excluded.title,
                   body_md=excluded.body_md,
                   state=excluded.state,
                   confidence=excluded.confidence,
                   created_by=excluded.created_by,
                   created_at_secs=excluded.created_at_secs,
                   updated_at_secs=excluded.updated_at_secs,
                   valid_from_secs=excluded.valid_from_secs,
                   valid_to_secs=excluded.valid_to_secs,
                   superseded_by=excluded.superseded_by,
                   source_span_ids_json=excluded.source_span_ids_json,
                   projection_json=excluded.projection_json,
                   metadata_json=excluded.metadata_json",
                params![
                    object.id,
                    object.project_key,
                    memory_object_kind_label(object.kind),
                    object.title,
                    object.body_md,
                    memory_object_state_label(object.state),
                    object.confidence as f64,
                    object.created_by,
                    object.created_at_secs as i64,
                    object.updated_at_secs as i64,
                    object.valid_from_secs.map(|v| v as i64),
                    object.valid_to_secs.map(|v| v as i64),
                    object.superseded_by,
                    serde_json::to_string(&object.source_span_ids).unwrap_or_else(|_| "[]".into()),
                    json_string(&object.projection),
                    json_string(&object.metadata),
                ],
            )
            .map_err(memory_sql_error)?;
        self.bump_gen();
        Ok(())
    }

    fn upsert_edge(&mut self, edge: MemoryEdge) -> MemoryResult<()> {
        self.conn
            .execute(
                "INSERT INTO memory_edge(id,project_key,src_id,dst_id,kind,confidence,created_at_secs,source_span_id,metadata_json)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
                 ON CONFLICT(id) DO UPDATE SET
                   project_key=excluded.project_key,
                   src_id=excluded.src_id,
                   dst_id=excluded.dst_id,
                   kind=excluded.kind,
                   confidence=excluded.confidence,
                   created_at_secs=excluded.created_at_secs,
                   source_span_id=excluded.source_span_id,
                   metadata_json=excluded.metadata_json",
                params![
                    edge.id,
                    edge.project_key,
                    edge.src_id,
                    edge.dst_id,
                    memory_edge_kind_label(edge.kind),
                    edge.confidence as f64,
                    edge.created_at_secs as i64,
                    edge.source_span_id,
                    json_string(&edge.metadata),
                ],
            )
            .map_err(memory_sql_error)?;
        self.bump_gen();
        Ok(())
    }

    fn load_object(&self, id: &str) -> MemoryResult<Option<MemoryObject>> {
        memory_object_by_id(self, id)
    }

    fn retrieve(&self, query: RetrievalQuery) -> MemoryResult<RetrievalResult> {
        let query_terms = memory_terms(&query.text);
        let mut items: Vec<RetrievalItem> = memory_objects_for_project(self, &query.project_key)?
            .into_iter()
            .filter(|object| {
                !matches!(
                    object.state,
                    MemoryObjectState::Rejected | MemoryObjectState::Superseded
                )
            })
            .filter_map(|object| {
                let title_score = memory_overlap_score(&query_terms, &object.title);
                let body_score = memory_overlap_score(&query_terms, &object.body_md);
                let score = title_score * 2.0 + body_score;
                (score > 0.0).then(|| RetrievalItem {
                    object_id: object.id,
                    score,
                    reason: "sqlite keyword-overlap baseline".to_string(),
                    source_span_ids: object.source_span_ids,
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

        let mut context_lines = Vec::new();
        for item in &items {
            if let Some(object) = memory_object_by_id(self, &item.object_id)? {
                context_lines.push(format!("- **{}**: {}", object.title, object.body_md));
            }
        }

        Ok(RetrievalResult {
            query,
            items,
            context_md: context_lines.join("\n"),
        })
    }

    fn project(&self, request: ProjectionRequest) -> MemoryResult<Projection> {
        let mut items: Vec<ProjectionItem> =
            memory_objects_for_project(self, &request.project_key)?
                .into_iter()
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
                    trust: memory_trust_for(&object),
                    actions: vec!["show_evidence".to_string(), "edit".to_string()],
                    explanation_span_ids: object.source_span_ids.clone(),
                })
                .collect();
        items.sort_by(|a, b| a.label.cmp(&b.label));
        Ok(Projection { request, items })
    }

    fn record_correction(&mut self, correction: HumanCorrection) -> MemoryResult<()> {
        self.conn
            .execute(
                "INSERT INTO memory_correction(project_key,target_id,action,note,corrected_at_secs)
                 VALUES(?1,?2,?3,?4,?5)",
                params![
                    correction.project_key,
                    correction.target_id,
                    correction.action,
                    correction.note,
                    correction.corrected_at_secs as i64,
                ],
            )
            .map_err(memory_sql_error)?;
        self.bump_gen();
        Ok(())
    }
}

fn memory_sql_error(err: rusqlite::Error) -> MemoryError {
    MemoryError::new(err.to_string())
}

fn memory_source_exists(store: &Store, source_id: &str) -> bool {
    store
        .conn
        .query_row(
            "SELECT 1 FROM memory_source WHERE id=?1",
            params![source_id],
            |_| Ok(()),
        )
        .is_ok()
}

fn memory_objects_for_project(store: &Store, project_key: &str) -> MemoryResult<Vec<MemoryObject>> {
    let mut stmt = store
        .conn
        .prepare(
            "SELECT id,project_key,kind,title,body_md,state,confidence,created_by,created_at_secs,updated_at_secs,valid_from_secs,valid_to_secs,superseded_by,source_span_ids_json,projection_json,metadata_json
             FROM memory_object WHERE project_key=?1",
        )
        .map_err(memory_sql_error)?;
    let rows = stmt
        .query_map(params![project_key], memory_object_from_row)
        .map_err(memory_sql_error)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(memory_sql_error)
}

fn memory_object_by_id(store: &Store, id: &str) -> MemoryResult<Option<MemoryObject>> {
    match store.conn.query_row(
        "SELECT id,project_key,kind,title,body_md,state,confidence,created_by,created_at_secs,updated_at_secs,valid_from_secs,valid_to_secs,superseded_by,source_span_ids_json,projection_json,metadata_json
         FROM memory_object WHERE id=?1",
        params![id],
        memory_object_from_row,
    ) {
        Ok(object) => Ok(Some(object)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(err) => Err(memory_sql_error(err)),
    }
}

fn memory_object_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryObject> {
    let span_ids_json: String = row.get(13)?;
    let projection_json: String = row.get(14)?;
    let metadata_json: String = row.get(15)?;
    Ok(MemoryObject {
        id: row.get(0)?,
        project_key: row.get(1)?,
        kind: parse_memory_object_kind_label(&row.get::<_, String>(2)?),
        title: row.get(3)?,
        body_md: row.get(4)?,
        state: parse_memory_object_state_label(&row.get::<_, String>(5)?),
        confidence: row.get::<_, f64>(6)? as f32,
        created_by: row.get(7)?,
        created_at_secs: row.get::<_, i64>(8)? as u64,
        updated_at_secs: row.get::<_, i64>(9)? as u64,
        valid_from_secs: row.get::<_, Option<i64>>(10)?.map(|v| v as u64),
        valid_to_secs: row.get::<_, Option<i64>>(11)?.map(|v| v as u64),
        superseded_by: row.get(12)?,
        source_span_ids: serde_json::from_str(&span_ids_json).unwrap_or_default(),
        projection: serde_json::from_str(&projection_json)
            .unwrap_or_else(|_| serde_json::json!({})),
        metadata: serde_json::from_str(&metadata_json).unwrap_or_else(|_| serde_json::json!({})),
    })
}

fn memory_trust_for(object: &MemoryObject) -> ProjectionTrust {
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

fn memory_terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| s.len() > 2)
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

fn memory_overlap_score(query_terms: &[String], text: &str) -> f32 {
    if query_terms.is_empty() {
        return 0.0;
    }
    let haystack = memory_terms(text);
    let hits = query_terms
        .iter()
        .filter(|term| haystack.contains(term))
        .count();
    hits as f32 / query_terms.len() as f32
}

fn json_string(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
}

fn memory_source_kind_label(kind: MemorySourceKind) -> &'static str {
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

fn parse_memory_object_kind_label(raw: &str) -> MemoryObjectKind {
    match raw {
        "area" => MemoryObjectKind::Area,
        "task" => MemoryObjectKind::Task,
        "decision" => MemoryObjectKind::Decision,
        "constraint" => MemoryObjectKind::Constraint,
        "claim" => MemoryObjectKind::Claim,
        "question" => MemoryObjectKind::Question,
        "session_event" => MemoryObjectKind::SessionEvent,
        "learning" => MemoryObjectKind::Learning,
        "artifact" => MemoryObjectKind::Artifact,
        _ => MemoryObjectKind::Concept,
    }
}

fn memory_object_kind_label(kind: MemoryObjectKind) -> &'static str {
    match kind {
        MemoryObjectKind::Concept => "concept",
        MemoryObjectKind::Area => "area",
        MemoryObjectKind::Task => "task",
        MemoryObjectKind::Decision => "decision",
        MemoryObjectKind::Constraint => "constraint",
        MemoryObjectKind::Claim => "claim",
        MemoryObjectKind::Question => "question",
        MemoryObjectKind::SessionEvent => "session_event",
        MemoryObjectKind::Learning => "learning",
        MemoryObjectKind::Artifact => "artifact",
    }
}

fn parse_memory_object_state_label(raw: &str) -> MemoryObjectState {
    match raw {
        "candidate" => MemoryObjectState::Candidate,
        "rejected" => MemoryObjectState::Rejected,
        "superseded" => MemoryObjectState::Superseded,
        "stale" => MemoryObjectState::Stale,
        _ => MemoryObjectState::Active,
    }
}

fn memory_object_state_label(state: MemoryObjectState) -> &'static str {
    match state {
        MemoryObjectState::Candidate => "candidate",
        MemoryObjectState::Active => "active",
        MemoryObjectState::Rejected => "rejected",
        MemoryObjectState::Superseded => "superseded",
        MemoryObjectState::Stale => "stale",
    }
}

fn memory_edge_kind_label(kind: MemoryEdgeKind) -> &'static str {
    match kind {
        MemoryEdgeKind::DerivedFrom => "derived_from",
        MemoryEdgeKind::Mentions => "mentions",
        MemoryEdgeKind::Supports => "supports",
        MemoryEdgeKind::Contradicts => "contradicts",
        MemoryEdgeKind::Supersedes => "supersedes",
        MemoryEdgeKind::DependsOn => "depends_on",
        MemoryEdgeKind::Blocks => "blocks",
        MemoryEdgeKind::Implements => "implements",
        MemoryEdgeKind::BelongsTo => "belongs_to",
        MemoryEdgeKind::TouchesFile => "touches_file",
        MemoryEdgeKind::SameAs => "same_as",
        MemoryEdgeKind::RelatedTo => "related_to",
    }
}
