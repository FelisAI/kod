//! Application-owned memory projection for the existing Map + Outline workspace.
//!
//! This is deliberately not a generic memory UI model. A memory engine may implement
//! [`MapOutlineMemorySource`], but Kod owns these product nouns and their validation. Live sessions,
//! canvas layout, pending review, and undo are joined by the application after this accepted
//! semantic projection is read.

use crate::{Kind, Lifecycle};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;

pub const KOD_AREA_KIND: &str = "kod:area";
pub const KOD_TASK_KIND: &str = "kod:task";
pub const KOD_IDEA_KIND: &str = "kod:idea";
pub const KOD_DECISION_KIND: &str = "kod:decision";
pub const KOD_QUESTION_KIND: &str = "kod:question";
pub const KOD_CONTAINS_RELATION: &str = "kod:contains";
pub const KOD_ABOUT_RELATION: &str = "kod:about";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MapOutlineProjection {
    pub project_key: String,
    /// Opaque identity for the complete source snapshot. It is diagnostic/shadow state, not UI.
    pub source_snapshot: String,
    pub nodes: Vec<MapOutlineNode>,
    pub decisions: Vec<MapOutlineDecision>,
    pub questions: Vec<MapOutlineQuestion>,
}

impl MapOutlineProjection {
    /// Validates the accepted semantic half of the workspace before the GUI or store sees it.
    pub fn validate(&self) -> Result<(), MapOutlineProjectionError> {
        if self.project_key.trim().is_empty() || self.source_snapshot.trim().is_empty() {
            return Err(MapOutlineProjectionError::new(
                "project key and source snapshot must be nonempty",
            ));
        }

        let mut memory_ids = HashSet::new();
        let mut revision_ids = HashSet::new();
        let mut nodes = HashMap::new();
        for node in &self.nodes {
            validate_identity_and_evidence(
                &node.memory_id,
                &node.revision_id,
                &node.evidence_ids,
                &mut memory_ids,
                &mut revision_ids,
            )?;
            if node.name.trim().is_empty()
                || node.detail_md.trim().is_empty()
                || !node.sort_order.is_finite()
            {
                return Err(MapOutlineProjectionError::new(format!(
                    "node {} has incomplete display content",
                    node.memory_id
                )));
            }
            if node.lifecycle == Lifecycle::Building {
                return Err(MapOutlineProjectionError::new(format!(
                    "node {} stores derived-only building state",
                    node.memory_id
                )));
            }
            nodes.insert(node.memory_id.as_str(), node);
        }

        for node in &self.nodes {
            match node.parent_memory_id.as_deref() {
                Some(parent) if parent == node.memory_id => {
                    return Err(MapOutlineProjectionError::new(format!(
                        "node {} is its own parent",
                        node.memory_id
                    )));
                }
                Some(parent) if !nodes.contains_key(parent) => {
                    return Err(MapOutlineProjectionError::new(format!(
                        "node {} references missing parent {parent}",
                        node.memory_id
                    )));
                }
                _ => {}
            }
        }
        validate_acyclic_tree(&nodes)?;

        for decision in &self.decisions {
            validate_identity_and_evidence(
                &decision.memory_id,
                &decision.revision_id,
                &decision.evidence_ids,
                &mut memory_ids,
                &mut revision_ids,
            )?;
            if !nodes.contains_key(decision.node_memory_id.as_str())
                || decision.question.trim().is_empty()
                || decision.answer.trim().is_empty()
                || decision.rationale.trim().is_empty()
            {
                return Err(MapOutlineProjectionError::new(format!(
                    "decision {} is incomplete or detached",
                    decision.memory_id
                )));
            }
        }

        for question in &self.questions {
            validate_identity_and_evidence(
                &question.memory_id,
                &question.revision_id,
                &question.evidence_ids,
                &mut memory_ids,
                &mut revision_ids,
            )?;
            if !nodes.contains_key(question.node_memory_id.as_str())
                || question.question.trim().is_empty()
                || question.context.trim().is_empty()
            {
                return Err(MapOutlineProjectionError::new(format!(
                    "question {} is incomplete or detached",
                    question.memory_id
                )));
            }
        }
        Ok(())
    }

    /// Produces deterministic shadow/evaluation output without assigning GUI layout.
    pub fn normalize(mut self) -> Result<Self, MapOutlineProjectionError> {
        self.validate()?;
        self.nodes.sort_by(|left, right| {
            left.parent_memory_id
                .cmp(&right.parent_memory_id)
                .then_with(|| left.sort_order.total_cmp(&right.sort_order))
                .then_with(|| left.memory_id.cmp(&right.memory_id))
        });
        self.decisions
            .sort_by(|left, right| left.memory_id.cmp(&right.memory_id));
        self.questions
            .sort_by(|left, right| left.memory_id.cmp(&right.memory_id));
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MapOutlineNode {
    pub memory_id: String,
    pub revision_id: String,
    pub parent_memory_id: Option<String>,
    pub name: String,
    pub detail_md: String,
    /// Application-authoritative type. Existing Kod maps may use any node kind at the root.
    pub kind: Kind,
    /// Asserted state only. `Building` is composed later from Kod's live sessions.
    pub lifecycle: Lifecycle,
    pub sort_order: f64,
    pub anchors: Vec<String>,
    /// Kept for an on-demand Why?/source drill-in; never rendered as normal node chrome.
    pub evidence_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapOutlineDecision {
    pub memory_id: String,
    pub revision_id: String,
    pub node_memory_id: String,
    pub question: String,
    pub answer: String,
    pub rationale: String,
    pub evidence_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapOutlineQuestion {
    pub memory_id: String,
    pub revision_id: String,
    pub node_memory_id: String,
    pub question: String,
    pub context: String,
    pub evidence_ids: Vec<String>,
}

/// Read-only application port. Implementations may use an external memory engine, fixtures, or another source.
/// Mutation, prompt injection, and UI rendering deliberately do not belong to this contract.
pub trait MapOutlineMemorySource {
    fn project_map_outline(
        &self,
        project_key: &str,
    ) -> Result<MapOutlineProjection, MapOutlineProjectionError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapOutlineProjectionError {
    message: String,
}

impl MapOutlineProjectionError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for MapOutlineProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(formatter)
    }
}

impl Error for MapOutlineProjectionError {}

fn validate_identity_and_evidence(
    memory_id: &str,
    revision_id: &str,
    evidence_ids: &[String],
    memory_ids: &mut HashSet<String>,
    revision_ids: &mut HashSet<String>,
) -> Result<(), MapOutlineProjectionError> {
    if memory_id.trim().is_empty()
        || revision_id.trim().is_empty()
        || !memory_ids.insert(memory_id.to_owned())
        || !revision_ids.insert(revision_id.to_owned())
    {
        return Err(MapOutlineProjectionError::new(format!(
            "duplicate or empty projection identity: {memory_id}/{revision_id}"
        )));
    }
    let mut evidence = HashSet::new();
    if evidence_ids.is_empty()
        || evidence_ids
            .iter()
            .any(|id| id.trim().is_empty() || !evidence.insert(id.as_str()))
    {
        return Err(MapOutlineProjectionError::new(format!(
            "projected memory {memory_id} must have unique evidence"
        )));
    }
    Ok(())
}

fn validate_acyclic_tree(
    nodes: &HashMap<&str, &MapOutlineNode>,
) -> Result<(), MapOutlineProjectionError> {
    for start in nodes.keys() {
        let mut seen = HashSet::new();
        let mut cursor = Some(*start);
        while let Some(id) = cursor {
            if !seen.insert(id) {
                return Err(MapOutlineProjectionError::new(format!(
                    "Map projection contains a parent cycle through {id}"
                )));
            }
            cursor = nodes
                .get(id)
                .and_then(|node| node.parent_memory_id.as_deref());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, parent: Option<&str>, kind: Kind) -> MapOutlineNode {
        MapOutlineNode {
            memory_id: id.into(),
            revision_id: format!("rev-{id}"),
            parent_memory_id: parent.map(str::to_owned),
            name: id.into(),
            detail_md: format!("What {id} owns."),
            kind,
            lifecycle: Lifecycle::Todo,
            sort_order: 1.0,
            anchors: Vec::new(),
            evidence_ids: vec![format!("ev-{id}")],
        }
    }

    fn valid_projection() -> MapOutlineProjection {
        MapOutlineProjection {
            project_key: "kod".into(),
            source_snapshot: "snapshot-7".into(),
            nodes: vec![
                node("area", None, Kind::Area),
                node("task", Some("area"), Kind::Task),
            ],
            decisions: vec![MapOutlineDecision {
                memory_id: "decision".into(),
                revision_id: "rev-decision".into(),
                node_memory_id: "area".into(),
                question: "Which surface owns memory?".into(),
                answer: "Map + Outline".into(),
                rationale: "The user manages projects, not memory internals.".into(),
                evidence_ids: vec!["ev-decision".into()],
            }],
            questions: vec![MapOutlineQuestion {
                memory_id: "question".into(),
                revision_id: "rev-question".into(),
                node_memory_id: "task".into(),
                question: "Enable the shadow adapter?".into(),
                context: "It changes no visible UI.".into(),
                evidence_ids: vec!["ev-question".into()],
            }],
        }
    }

    #[test]
    fn valid_projection_is_normalized_without_ui_state() {
        let projection = valid_projection().normalize().unwrap();
        assert_eq!(projection.nodes[0].memory_id, "area");
        assert_eq!(projection.nodes[1].memory_id, "task");
        let json = serde_json::to_string(&projection).unwrap();
        assert!(!json.contains("map_x"));
        assert!(!json.contains("working"));
        assert!(!json.contains("retrieval_score"));
    }

    #[test]
    fn derived_building_state_is_rejected() {
        let mut projection = valid_projection();
        projection.nodes[1].lifecycle = Lifecycle::Building;
        assert!(projection
            .validate()
            .unwrap_err()
            .message()
            .contains("derived-only building"));
    }

    #[test]
    fn application_authoritative_task_root_is_valid_but_missing_parent_is_not() {
        let mut projection = valid_projection();
        projection.nodes[1].parent_memory_id = None;
        projection.validate().unwrap();

        projection.nodes[1].parent_memory_id = Some("missing".into());
        assert!(projection.validate().is_err());
    }

    #[test]
    fn parent_cycle_is_rejected() {
        let mut projection = valid_projection();
        projection.nodes[0].parent_memory_id = Some("task".into());
        assert!(projection
            .validate()
            .unwrap_err()
            .message()
            .contains("parent cycle"));
    }

    #[test]
    fn detached_decisions_and_missing_evidence_are_rejected() {
        let mut projection = valid_projection();
        projection.decisions[0].node_memory_id = "missing".into();
        assert!(projection.validate().is_err());

        let mut projection = valid_projection();
        projection.questions[0].evidence_ids.clear();
        assert!(projection.validate().is_err());
    }
}
