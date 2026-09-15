//! orchestrator-store — the persisted DESIGN tree (docs/016).
//!
//! Owns the per-project parts tree, the asserted lifecycle status, code
//! anchors, and an append-only journal (undo + since-you-were-away). Keyed by
//! the registry's canonical_key. Pure tree/diff model (`tree`) + SQLite shell
//! (`store`).

pub mod map_outline_memory;
pub mod memory;
pub mod memory_adapter;
pub mod memory_engine;
pub mod memory_extract;
pub mod memory_llm;
pub mod reconcile;
pub mod store;
pub mod tree;

pub use map_outline_memory::{
    MapOutlineDecision, MapOutlineMemorySource, MapOutlineNode, MapOutlineProjection,
    MapOutlineProjectionError, MapOutlineQuestion, KOD_ABOUT_RELATION, KOD_AREA_KIND,
    KOD_CONTAINS_RELATION, KOD_DECISION_KIND, KOD_IDEA_KIND, KOD_QUESTION_KIND, KOD_TASK_KIND,
};
pub use memory::{
    HumanCorrection, InMemoryMemoryBackend, MemoryBackend, MemoryEdge, MemoryEdgeKind, MemoryError,
    MemoryId, MemoryObject, MemoryObjectKind, MemoryObjectState, MemoryResult, MemorySource,
    MemorySourceKind, MemorySpan, Projection, ProjectionItem, ProjectionKind, ProjectionRequest,
    ProjectionTrust, RetrievalIntent, RetrievalItem, RetrievalQuery, RetrievalResult,
};
pub use memory_adapter::{
    retrieve_with_policy, retrieve_with_policy_selection, EngineCandidate, EngineRetrievalResult,
    EngineSelection, EngineSelectionScore, MemoryEngineAdapter, MemoryEngineCapabilities,
    MemoryEngineMode, MemoryEngineRecommendation, MemoryEngineTraceScorecard,
    MultiEngineRetrievalResult, NativeStoreMemoryAdapter, PolicyRetrievalResult, RawSourceDocument,
    RawSourceMemoryAdapter, RetrievalPolicy, RetrievalPolicyEvaluation, RetrievalTrace,
};
pub use memory_engine::{
    apply_native_memory_candidates, evaluate_native_memory_candidates, NativeMemoryCandidate,
    NativeMemoryDecision, NativeMemoryDecisionKind, NativeMemoryEngineReport, NativeMemoryIntent,
};
pub use memory_extract::{
    extract_rule_backed_memories, ingest_memory_documents, upsert_seeded_memories, EvidenceNeedle,
    MemoryDocument, RuleBackedMemory, SeededMemory,
};
pub use memory_llm::{llm_memory_extraction_prompt, parse_llm_memory_candidates};
pub use store::{
    flatten_changeset_flags, flatten_changeset_ops, ChangesetTreeState, HostedSessionRow,
    MemoryCandidateRow, PendingDiff, ProfileRow, SeedState, StagedChangeset, Store, SummaryRow,
    TimelineEvent, TimelineKind, EventKind,
};
pub use tree::{
    build_tree, countable_ratio, dissolve_node_ops, dissolve_tech_target, done_ratio, first_line,
    idea_tray_id, indent_op, midpoint_order, outdent_op, quiet_building, reorder_op, reparent_op,
    sibling_below_plan, subtree_removal_ops, task_rollup, DiffOp, Kind, Lifecycle, Part, PartId,
    PartRef, SiblingInsert, StatusSource, TreeNode, IDEA_TRAY_NAME,
};
