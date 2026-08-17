//! Compile-time OSS-prep gates.
//!
//! The open-source build ships WITHOUT the Map and Memory subsystems: both are
//! mid-iteration and both spend LLM tokens (the living-map proposer + the agentic
//! cartographer; the agentic memory extractor). Gating them keeps a stranger's
//! first run at **zero background LLM cost** and hides the unfinished UI.
//!
//! These are `cfg!(feature = ...)` *expressions*, not `#[cfg]` attribute-stripping:
//! the code is always compiled and tested, so it can't silently rot. The consts
//! just fold to `false` in the default build, turning every gated LLM-trigger /
//! UI branch into dead code the optimizer removes — a compile-time zero-cost
//! guarantee that a repo reader can't flip (unlike an env var).
//!
//! Iterate on either subsystem with:
//!   cargo run -p orchestrator-gui --features map
//!   cargo run -p orchestrator-gui --features memory
//!   cargo run -p orchestrator-gui --features map,memory

/// The per-project visual product map (living map + outline editor) and its LLM
/// pipeline (cartographer / seed / living-map proposer / breakdown). Default-off.
pub const MAP_ENABLED: bool = cfg!(feature = "map");

/// The typed memory-graph subsystem: the agentic extractor (`propose_memory_candidates`)
/// and the session-kickoff retrieval. Independent of the Map. Default-off.
pub const MEMORY_ENABLED: bool = cfg!(feature = "memory");

#[cfg(test)]
mod tests {
    //! Guardrail canaries. The map/memory LLM entry points MUST stay behind a
    //! feature gate so the default (open-source) build can never spend tokens on
    //! them. These tests grep the sibling source (compile-time `include_str!`) and
    //! fail if a guard is removed or renamed — turning a silent cost regression
    //! into a red test that forces a conscious decision. They are a tripwire for
    //! *removed* guards; a newly-added ungated caller still needs human review.

    #[test]
    fn map_and_memory_llm_triggers_stay_gated() {
        // G1 (map proposer) + the memory extractor: the two recurring tick spawns,
        // each wrapped in its own feature gate. Standup's maybe_spawn_summary is
        // deliberately NOT gated and must stay ungated.
        let summaries = include_str!("summaries.rs");
        assert!(
            summaries.contains("if crate::features::MAP_ENABLED {")
                && summaries.contains("self.maybe_spawn_map_proposal();"),
            "summaries.rs: the map proposer (G1) must stay behind MAP_ENABLED"
        );
        assert!(
            summaries.contains("if crate::features::MEMORY_ENABLED {")
                && summaries.contains("self.maybe_spawn_memory_proposal();"),
            "summaries.rs: the memory extractor must stay behind MEMORY_ENABLED"
        );

        // G2 (start_agentic_run) + G3 (start_extract): the user-invoked
        // cartographer entries, each an early-return backstop.
        let agentic = include_str!("agentic.rs");
        assert!(
            agentic
                .matches("if !crate::features::MAP_ENABLED {")
                .count()
                >= 2,
            "agentic.rs: start_extract AND start_agentic_run must each early-return on !MAP_ENABLED"
        );

        // G4 ('◇ break down'): early-return backstop.
        let spawn = include_str!("spawn.rs");
        assert!(
            spawn.contains("if !crate::features::MAP_ENABLED {"),
            "spawn.rs: spawn_breakdown (G4) must early-return on !MAP_ENABLED"
        );
    }
}
