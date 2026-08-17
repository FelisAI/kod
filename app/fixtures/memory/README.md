# Memory Eval Fixtures

These fixtures are legacy orchestrator plumbing and design-regression tests for docs/020-021. They
do not drive the standalone Ecphory V1-V3 work. The orchestrator fixture is largely self-referential
to the memory design documents, so its passing scores are not evidence that memory improves a real
agent task. Portable Ecphory scenarios live under the Ecphory subtree; application integration is
deferred to [V4](../../../ecphory/docs/execution/product-first-roadmap.md).

The first fixture is intentionally small. It should grow toward:

- public-style retrieval/update/stale-memory tasks
- Map/Decision/Evidence projection checks
- kickoff-context checks for real Claude/Codex dispatches
- correction-propagation checks after human edits or rejected proposals

Run the initial keyword baseline from `app/`:

```sh
cargo run -p orchestrator-store --example memory_eval -- fixtures/memory/orchestrator/eval.json --backend keyword_source
```

Run the hand-seeded local-reference backend:

```sh
cargo run -p orchestrator-store --example memory_eval -- fixtures/memory/orchestrator/eval.json --backend local_reference
```

Run the source-derived local extractor:

```sh
cargo run -p orchestrator-store --example memory_eval -- fixtures/memory/orchestrator/eval.json --backend local_extract
```

Run the full local suite:

```sh
cargo run -p orchestrator-store --example memory_eval -- fixtures/memory/orchestrator/eval.json --backend all
```

Run the external conversation-QA adapter smoke suite:

```sh
cargo run -p orchestrator-store --example memory_eval -- fixtures/memory/external/conversation_qa.json --format conversation_qa --backend source_memory
```

Run against the official LongMemEval oracle JSON after downloading it:

```sh
curl -L -o /tmp/longmemeval_oracle.json 'https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/resolve/main/longmemeval_oracle.json'
cargo run -p orchestrator-store --example memory_eval -- /tmp/longmemeval_oracle.json --format conversation_qa --backend source_memory --json /tmp/orchestrator-longmemeval-oracle-source-memory.json
```

The LongMemEval path is currently retrieval-only. It reports evidence hit and answer-string support,
not published QA accuracy.

Run the shadow memory effectiveness audit. This does not call a live LLM; it uses saved LLM-style
candidate output, applies it to an in-memory store, and prints insert/duplicate/supersession/
unsupported decisions plus retrieval and kickoff-context checks:

```sh
cargo run -p orchestrator-store --example memory_shadow_eval -- fixtures/memory/shadow/session_summary.json --json /tmp/orchestrator-memory-shadow.json
```

Write machine-readable reports:

```sh
cargo run -p orchestrator-store --example memory_eval -- fixtures/memory/orchestrator/eval.json --backend keyword_source --json /tmp/orchestrator-memory-keyword.json
cargo run -p orchestrator-store --example memory_eval -- fixtures/memory/orchestrator/eval.json --backend local_extract --json /tmp/orchestrator-memory-local-extract.json
cargo run -p orchestrator-store --example memory_eval -- fixtures/memory/orchestrator/eval.json --backend local_reference --json /tmp/orchestrator-memory-local-reference.json
cargo run -p orchestrator-store --example memory_eval -- fixtures/memory/orchestrator/eval.json --backend all --json /tmp/orchestrator-memory-suite.json
```
