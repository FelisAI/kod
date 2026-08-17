    use super::*;
    use orchestrator_store::{MemoryDocument, RuleBackedMemory};

    pub const MEMORY_AGENT_TIMEOUT_SECS: u64 = 600;
    pub const MEMORY_AGENT_MAX_TURNS: u32 = 30;

    pub fn propose_memory_candidates(
        project_key: &str,
        documents: &[MemoryDocument],
        cwd: &Path,
        transcript_out: &mut String,
    ) -> Result<Vec<RuleBackedMemory>, String> {
        let prompt = orchestrator_store::llm_memory_extraction_prompt(project_key, documents);
        let stdout = super::run_agent_prompt(
            &prompt,
            cwd,
            super::cartographer::STRUCTURAL_MODEL,
            &["Read", "Glob", "Grep"],
            MEMORY_AGENT_MAX_TURNS,
            MEMORY_AGENT_TIMEOUT_SECS,
            transcript_out,
        )?;
        let text =
            super::extract_result_text(&stdout).ok_or("no result text in memory agent output")?;
        orchestrator_store::parse_llm_memory_candidates(&text).map_err(|e| e.to_string())
    }
