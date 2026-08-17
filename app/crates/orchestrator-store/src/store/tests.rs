    use super::*;
    use crate::tree::build_tree;

    fn add(
        temp: &str,
        parent: PartRef,
        name: &str,
        detail: &str,
        lc: Lifecycle,
        anchors: Vec<String>,
    ) -> DiffOp {
        DiffOp::Add {
            temp: temp.into(),
            parent,
            name: name.into(),
            detail: detail.into(),
            lifecycle: lc,
            anchors,
            kind: Kind::Task,
            detail_md: None,
            sort_order: None,
            source_file: None,
            source_quote: None,
            rationale: None,
        }
    }

    fn seed_ops() -> Vec<DiffOp> {
        vec![
            add(
                "a",
                PartRef::Root,
                "Terminal host",
                "PTY",
                Lifecycle::Done,
                vec!["crates/orchestrator-host/**".into()],
            ),
            add("b", PartRef::Root, "Flow map", "", Lifecycle::Todo, vec![]),
            add(
                "b1",
                PartRef::Temp("b".into()),
                "Store",
                "",
                Lifecycle::Todo,
                vec![],
            ),
        ]
    }

    fn memory_doc_source(id: &str) -> MemorySource {
        MemorySource {
            id: id.to_string(),
            project_key: "k".to_string(),
            kind: MemorySourceKind::Doc,
            uri: "docs/memory.md".to_string(),
            title: Some("Memory".to_string()),
            captured_at_secs: 1,
            content_hash: None,
            metadata: serde_json::json!({ "test": true }),
        }
    }

    fn memory_obj(
        id: &str,
        kind: MemoryObjectKind,
        title: &str,
        body: &str,
        spans: Vec<String>,
    ) -> MemoryObject {
        MemoryObject {
            id: id.to_string(),
            project_key: "k".to_string(),
            kind,
            title: title.to_string(),
            body_md: body.to_string(),
            state: MemoryObjectState::Active,
            confidence: 0.9,
            created_by: "test".to_string(),
            created_at_secs: 1,
            updated_at_secs: 1,
            valid_from_secs: None,
            valid_to_secs: None,
            superseded_by: None,
            source_span_ids: spans,
            projection: serde_json::json!({}),
            metadata: serde_json::json!({}),
        }
    }

    fn rule_memory(
        id: &str,
        kind: MemoryObjectKind,
        title: &str,
        body: &str,
        source_id: &str,
        needle: &str,
    ) -> RuleBackedMemory {
        RuleBackedMemory {
            id: id.to_string(),
            kind,
            title: title.to_string(),
            body_md: body.to_string(),
            evidence: vec![crate::memory_extract::EvidenceNeedle {
                source_id: source_id.to_string(),
                needle: needle.to_string(),
            }],
        }
    }

    #[test]
    fn sqlite_memory_backend_rejects_span_without_source() {
        let mut s = Store::open_in_memory().unwrap();
        let err = s
            .add_span(MemorySpan {
                id: "span-1".to_string(),
                source_id: "missing".to_string(),
                start_ref: None,
                end_ref: None,
                quote: Some("quote".to_string()),
            })
            .unwrap_err();
        assert!(err.to_string().contains("unknown source missing"));
    }

    #[test]
    fn sqlite_memory_backend_retrieves_and_projects() {
        let mut s = Store::open_in_memory().unwrap();
        s.ingest_source(memory_doc_source("doc-1")).unwrap();
        s.add_span(MemorySpan {
            id: "span-1".to_string(),
            source_id: "doc-1".to_string(),
            start_ref: None,
            end_ref: None,
            quote: Some("Map should be a view of memory".to_string()),
        })
        .unwrap();
        s.upsert_object(memory_obj(
            "area-memory",
            MemoryObjectKind::Area,
            "Memory substrate",
            "Backend memory graph serves retrieval and projections.",
            vec!["span-1".to_string()],
        ))
        .unwrap();
        s.upsert_object(memory_obj(
            "decision-map",
            MemoryObjectKind::Decision,
            "Map is projection over memory graph",
            "The Map is a view, not the memory substrate.",
            vec!["span-1".to_string()],
        ))
        .unwrap();

        let retrieval = s
            .retrieve(RetrievalQuery {
                project_key: "k".to_string(),
                intent: crate::memory::RetrievalIntent::Kickoff,
                text: "memory graph projection".to_string(),
                scope_memory_id: None,
                since_secs: None,
                limit: 5,
            })
            .unwrap();
        assert_eq!(retrieval.items[0].object_id, "decision-map");
        assert!(retrieval.context_md.contains("Map is projection"));

        let map = s
            .project(ProjectionRequest {
                project_key: "k".to_string(),
                kind: ProjectionKind::Map,
                scope_memory_id: None,
            })
            .unwrap();
        assert_eq!(map.items.len(), 1);
        assert_eq!(map.items[0].memory_id, "area-memory");
        assert_eq!(map.items[0].trust, ProjectionTrust::Verified);
    }

    #[test]
    fn sqlite_memory_backend_persists_across_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "orch-memory-store-test-{}-{}",
            std::process::id(),
            now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("store.sqlite");
        {
            let mut s = Store::open(&db).unwrap();
            s.ingest_source(memory_doc_source("doc-1")).unwrap();
            s.add_span(MemorySpan {
                id: "span-1".to_string(),
                source_id: "doc-1".to_string(),
                start_ref: Some("line:1".to_string()),
                end_ref: None,
                quote: Some("corrections propagate".to_string()),
            })
            .unwrap();
            s.upsert_object(memory_obj(
                "constraint-corrections",
                MemoryObjectKind::Constraint,
                "corrections propagate",
                "Rejected or edited memories must affect future retrieval.",
                vec!["span-1".to_string()],
            ))
            .unwrap();
        }

        let s = Store::open(&db).unwrap();
        let retrieval = s
            .retrieve(RetrievalQuery {
                project_key: "k".to_string(),
                intent: crate::memory::RetrievalIntent::MidSession,
                text: "human corrections affect retrieval".to_string(),
                scope_memory_id: None,
                since_secs: None,
                limit: 3,
            })
            .unwrap();
        assert_eq!(retrieval.items[0].object_id, "constraint-corrections");
    }

    #[test]
    fn session_summaries_become_memory_documents() {
        let s = Store::open_in_memory().unwrap();
        s.record_summary(
            "sess-1",
            "k",
            2_000,
            1_900,
            42,
            "/tmp/transcript.jsonl",
            "fix memory retrieval",
            "wired source ledger",
            "add review queue",
            r#"["added typed sources","kept Map as projection"]"#,
        )
        .unwrap();

        let docs = s.summary_memory_documents_since("k", 0).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].id, "summary:sess-1:2000");
        assert_eq!(docs[0].kind, MemorySourceKind::Session);
        assert_eq!(docs[0].uri, "/tmp/transcript.jsonl");
        assert!(docs[0].text.contains("goal: fix memory retrieval"));
        assert!(docs[0].text.contains("headline: wired source ledger"));
        assert!(docs[0].text.contains("- kept Map as projection"));

        let (window_docs, watermark) = s
            .summary_memory_documents_since_with_watermark("k", 0)
            .unwrap();
        assert_eq!(window_docs, docs);
        assert_eq!(watermark, 2_000);

        assert_eq!(s.last_memory_proposal_secs("k"), 0);
        s.set_last_memory_proposal_secs("k", 123).unwrap();
        assert_eq!(s.last_memory_proposal_secs("k"), 123);
    }

    #[test]
    fn llm_memory_output_from_session_summary_applies_to_store_and_retrieval() {
        let mut s = Store::open_in_memory().unwrap();
        s.record_summary(
            "sess-1",
            "k",
            2_000,
            1_900,
            42,
            "/tmp/transcript.jsonl",
            "harden memory",
            "wired native memory engine",
            "connect session extraction",
            r#"["kept Map as projection","source ledger validates durable memory"]"#,
        )
        .unwrap();
        let docs = s.summary_memory_documents_since("k", 0).unwrap();
        let raw = r#"assistant preface
{"memories":[
{"id":"map-projection","kind":"Decision","title":"Map is projection over memory graph","body_md":"The Map should represent durable memory, not become the memory substrate.","evidence":[{"source_id":"summary:sess-1:2000","needle":"kept Map as projection"}]},
{"id":"hallucinated","kind":"Decision","title":"Hallucinated memory","body_md":"This should not be persisted.","evidence":[{"source_id":"summary:sess-1:2000","needle":"not present in the summary"}]}
]}"#;

        let report = s
            .apply_llm_memory_output("k", &docs, raw, "memory_agent:test", 20)
            .unwrap();

        assert_eq!(report.inserted, 1);
        assert_eq!(report.count(NativeMemoryDecisionKind::Insert), 1);
        assert_eq!(report.count(NativeMemoryDecisionKind::Unsupported), 1);
        assert!(s.load_object("map-projection").unwrap().is_some());
        assert!(s.load_object("hallucinated").unwrap().is_none());

        let retrieval = s
            .retrieve(RetrievalQuery {
                project_key: "k".to_string(),
                intent: crate::memory::RetrievalIntent::MidSession,
                text: "map projection memory graph".to_string(),
                scope_memory_id: None,
                since_secs: None,
                limit: 3,
            })
            .unwrap();
        assert_eq!(retrieval.items[0].object_id, "map-projection");
    }

    #[test]
    fn memory_candidates_stage_accept_and_reject() {
        let mut s = Store::open_in_memory().unwrap();
        let docs = vec![MemoryDocument {
            id: "summary:sess-1:2000".to_string(),
            kind: MemorySourceKind::Session,
            uri: "/tmp/transcript.jsonl".to_string(),
            title: Some("wired memory".to_string()),
            text: "The Map should be a view of memory, not the memory substrate.".to_string(),
        }];
        let accept = rule_memory(
            "mem-map-projection",
            MemoryObjectKind::Decision,
            "Map is projection over memory graph",
            "The Map is a human-facing projection over typed memory.",
            "summary:sess-1:2000",
            "Map should be a view of memory",
        );
        let reject = rule_memory(
            "mem-reject",
            MemoryObjectKind::Claim,
            "temporary claim",
            "This should be rejected before insertion.",
            "summary:sess-1:2000",
            "Map should be a view of memory",
        );

        let ids = s
            .add_memory_candidates("k", &[accept, reject], "memory_agent:test")
            .unwrap();
        let open = s.open_memory_candidates("k").unwrap();
        assert_eq!(open.len(), 2);
        assert_eq!(open[0].created_by, "memory_agent:test");

        let inserted = s
            .accept_memory_candidate(ids[0], &docs, "review:test", 10)
            .unwrap();
        assert_eq!(inserted, 1);
        assert_eq!(
            s.memory_candidate_status(ids[0]).as_deref(),
            Some("accepted")
        );

        s.reject_memory_candidate(ids[1]).unwrap();
        assert_eq!(
            s.memory_candidate_status(ids[1]).as_deref(),
            Some("rejected")
        );
        assert!(s.open_memory_candidates("k").unwrap().is_empty());

        let retrieval = s
            .retrieve(RetrievalQuery {
                project_key: "k".to_string(),
                intent: crate::memory::RetrievalIntent::MidSession,
                text: "map memory projection".to_string(),
                scope_memory_id: None,
                since_secs: None,
                limit: 3,
            })
            .unwrap();
        assert_eq!(retrieval.items[0].object_id, "mem-map-projection");
    }

    #[test]
    fn memory_candidate_accept_marks_unsupported_when_evidence_missing() {
        let mut s = Store::open_in_memory().unwrap();
        let docs = vec![MemoryDocument {
            id: "summary:sess-1:2000".to_string(),
            kind: MemorySourceKind::Session,
            uri: "/tmp/transcript.jsonl".to_string(),
            title: None,
            text: "Only this sentence exists.".to_string(),
        }];
        let candidate = rule_memory(
            "mem-missing",
            MemoryObjectKind::Decision,
            "missing evidence",
            "Should not be inserted without verified evidence.",
            "summary:sess-1:2000",
            "not in the source",
        );
        let id = s
            .add_memory_candidates("k", &[candidate], "memory_agent:test")
            .unwrap()[0];

        let inserted = s
            .accept_memory_candidate(id, &docs, "review:test", 10)
            .unwrap();
        assert_eq!(inserted, 0);
        assert_eq!(
            s.memory_candidate_status(id).as_deref(),
            Some("unsupported")
        );

        let retrieval = s
            .retrieve(RetrievalQuery {
                project_key: "k".to_string(),
                intent: crate::memory::RetrievalIntent::MidSession,
                text: "missing evidence".to_string(),
                scope_memory_id: None,
                since_secs: None,
                limit: 3,
            })
            .unwrap();
        assert!(retrieval.items.is_empty());
    }

    #[test]
    fn memory_candidate_accept_marks_duplicate_without_reinserting() {
        let mut s = Store::open_in_memory().unwrap();
        let docs = vec![MemoryDocument {
            id: "summary:sess-1:2000".to_string(),
            kind: MemorySourceKind::Session,
            uri: "/tmp/transcript.jsonl".to_string(),
            title: Some("source ledger".to_string()),
            text: "The source ledger keeps evidence for durable memory.".to_string(),
        }];
        let candidate = rule_memory(
            "mem-source-ledger",
            MemoryObjectKind::Decision,
            "Source ledger keeps evidence",
            "The source ledger keeps evidence for durable memory.",
            "summary:sess-1:2000",
            "source ledger keeps evidence",
        );
        let ids = s
            .add_memory_candidates("k", &[candidate.clone(), candidate], "memory_agent:test")
            .unwrap();

        let first_inserted = s
            .accept_memory_candidate(ids[0], &docs, "review:test", 10)
            .unwrap();
        let second_inserted = s
            .accept_memory_candidate(ids[1], &docs, "review:test", 11)
            .unwrap();

        assert_eq!(first_inserted, 1);
        assert_eq!(second_inserted, 0);
        assert_eq!(
            s.memory_candidate_status(ids[0]).as_deref(),
            Some("accepted")
        );
        assert_eq!(
            s.memory_candidate_status(ids[1]).as_deref(),
            Some("duplicate")
        );

        let retrieval = s
            .retrieve(RetrievalQuery {
                project_key: "k".to_string(),
                intent: crate::memory::RetrievalIntent::MidSession,
                text: "source ledger evidence memory".to_string(),
                scope_memory_id: None,
                since_secs: None,
                limit: 5,
            })
            .unwrap();
        assert_eq!(retrieval.items.len(), 1);
        assert_eq!(retrieval.items[0].object_id, "mem-source-ledger");
    }

    #[test]
    fn memory_candidate_accept_supersedes_prior_same_title_memory() {
        let mut s = Store::open_in_memory().unwrap();
        let docs = vec![MemoryDocument {
            id: "summary:sess-1:2000".to_string(),
            kind: MemorySourceKind::Session,
            uri: "/tmp/transcript.jsonl".to_string(),
            title: Some("engine choice".to_string()),
            text:
                "Use source_memory baseline first. Use local_extract instead for typed memory now."
                    .to_string(),
        }];
        let first = rule_memory(
            "mem-engine-choice-v1",
            MemoryObjectKind::Decision,
            "Memory engine choice",
            "Use source_memory baseline first.",
            "summary:sess-1:2000",
            "Use source_memory baseline",
        );
        let second = rule_memory(
            "mem-engine-choice-v2",
            MemoryObjectKind::Decision,
            "Memory engine choice",
            "Use local_extract instead for typed memory now.",
            "summary:sess-1:2000",
            "Use local_extract instead",
        );
        let ids = s
            .add_memory_candidates("k", &[first, second], "memory_agent:test")
            .unwrap();

        let first_inserted = s
            .accept_memory_candidate(ids[0], &docs, "review:test", 10)
            .unwrap();
        let second_inserted = s
            .accept_memory_candidate(ids[1], &docs, "review:test", 11)
            .unwrap();

        assert_eq!(first_inserted, 1);
        assert_eq!(second_inserted, 1);
        assert_eq!(
            s.memory_candidate_status(ids[0]).as_deref(),
            Some("accepted")
        );
        assert_eq!(
            s.memory_candidate_status(ids[1]).as_deref(),
            Some("superseded")
        );

        let old = s.load_object("mem-engine-choice-v1").unwrap().unwrap();
        let new = s.load_object("mem-engine-choice-v2").unwrap().unwrap();
        assert_eq!(old.state, MemoryObjectState::Superseded);
        assert_eq!(old.superseded_by.as_deref(), Some("mem-engine-choice-v2"));
        assert_eq!(new.state, MemoryObjectState::Active);

        let retrieval = s
            .retrieve(RetrievalQuery {
                project_key: "k".to_string(),
                intent: crate::memory::RetrievalIntent::MidSession,
                text: "local extract typed memory engine choice".to_string(),
                scope_memory_id: None,
                since_secs: None,
                limit: 5,
            })
            .unwrap();
        assert_eq!(retrieval.items[0].object_id, "mem-engine-choice-v2");
    }

    /// docs/019 commitment 3 + slice 2: an accepted machine Add stores its
    /// verified provenance quad, and the "why is this here?" popover reads it
    /// straight back. Plus the taxonomy_note get/set (empty collapses to None).
    #[test]
    fn machine_add_roundtrips_provenance_and_taxonomy_note() {
        let mut s = Store::open_in_memory().unwrap();
        s.ensure_project("k", "K").unwrap();
        let op = DiffOp::Add {
            temp: "m1".into(),
            parent: PartRef::Root,
            name: "Sync engine".into(),
            detail: "keeps replicas consistent".into(),
            lifecycle: Lifecycle::Todo,
            anchors: vec![],
            kind: Kind::Area,
            detail_md: None,
            sort_order: None,
            source_file: Some("docs/019-map-v3-owned-brain.md".into()),
            source_quote: Some("The return channel is daemon-owned".into()),
            rationale: Some("seed: the sync area is named from the design doc".into()),
        };
        s.accept_diff_from("k", &[op], "seed:run1", None).unwrap();
        let part = s
            .load_tree("k")
            .unwrap()
            .into_iter()
            .find(|p| p.name == "Sync engine")
            .unwrap();
        let (who, sf, sq, ra) = s.part_provenance(part.id).unwrap();
        assert_eq!(
            who, "seed:run1",
            "created_by is stamped from the accept origin"
        );
        assert_eq!(sf.as_deref(), Some("docs/019-map-v3-owned-brain.md"));
        assert_eq!(sq.as_deref(), Some("The return channel is daemon-owned"));
        assert_eq!(
            ra.as_deref(),
            Some("seed: the sync area is named from the design doc")
        );

        // taxonomy_note: absent → None; set → value; blank → None (so callers
        // can `if let Some(note)` without guarding against an empty string).
        assert!(s.taxonomy_note("k").is_none());
        s.set_taxonomy_note(
            "k",
            "organized by goal-area; code quarantined under one 'from code' node",
        )
        .unwrap();
        assert_eq!(
            s.taxonomy_note("k").as_deref(),
            Some("organized by goal-area; code quarantined under one 'from code' node")
        );
        s.set_taxonomy_note("k", "   ").unwrap();
        assert!(
            s.taxonomy_note("k").is_none(),
            "blank note collapses to None"
        );

        // ⌘Z of a machine node RESTORES its citation (review: undo dropped the
        // provenance quad because load_tree didn't hydrate it onto Part).
        s.accept_diff("k", &[DiffOp::Remove { id: part.id }])
            .unwrap();
        assert!(
            s.load_tree("k")
                .unwrap()
                .iter()
                .all(|p| p.name != "Sync engine"),
            "removed"
        );
        assert!(s.undo_last("k").unwrap());
        let restored = s
            .load_tree("k")
            .unwrap()
            .into_iter()
            .find(|p| p.name == "Sync engine")
            .unwrap();
        let (_, sf, sq, ra) = s.part_provenance(restored.id).unwrap();
        assert_eq!(
            sf.as_deref(),
            Some("docs/019-map-v3-owned-brain.md"),
            "citation survives undo"
        );
        assert_eq!(sq.as_deref(), Some("The return channel is daemon-owned"));
        assert_eq!(
            ra.as_deref(),
            Some("seed: the sync area is named from the design doc")
        );
    }

    #[test]
    fn timeline_merges_kinds_batches_map_accepts_and_orders_desc() {
        let mut s = Store::open_in_memory().unwrap();
        // two accepts moments apart → ONE batched map entry (+ node for notes)
        s.accept_diff("k", &seed_ops()).unwrap();
        s.accept_diff(
            "k",
            &[add(
                "c",
                PartRef::Root,
                "Later",
                "",
                Lifecycle::Todo,
                vec![],
            )],
        )
        .unwrap();
        let part = s.load_tree("k").unwrap()[0].id;
        s.add_note("k", part, "session", "▶ session started", "sess-abc")
            .unwrap();
        s.add_note("k", part, "decision", "ship it friday", "user")
            .unwrap();
        s.add_note("k", part, "note", "not on the timeline", "user")
            .unwrap();
        s.record_summary(
            "abc",
            "k",
            9_000_000,
            1,
            10,
            "/tmp/x",
            "goal",
            "landed the thing",
            "",
            "[\"b1\"]",
        )
        .unwrap();
        let tl = s.timeline(50);
        // newest first
        assert!(tl.windows(2).all(|w| w[0].ts_ms >= w[1].ts_ms));
        let kinds: Vec<&TimelineKind> = tl.iter().map(|e| &e.kind).collect();
        assert!(kinds.contains(&&TimelineKind::Summary));
        assert!(kinds.contains(&&TimelineKind::Trail));
        assert!(kinds.contains(&&TimelineKind::Decision));
        // 4 accepted ops across two accepts within 10 min = ONE map entry
        let maps: Vec<_> = tl.iter().filter(|e| e.kind == TimelineKind::Map).collect();
        assert_eq!(maps.len(), 1);
        assert_eq!(maps[0].count, 4);
        // plain notes stay off the thread; node attribution rides along
        assert!(!tl.iter().any(|e| e.text.contains("not on the timeline")));
        assert!(tl.iter().any(|e| e.kind == TimelineKind::Decision
            && e.node.as_ref().is_some_and(|(_, n)| n == "Terminal host")));
    }

    #[test]
    fn memory_layer_notes_search_pos_provenance() {
        let mut s = Store::open_in_memory().unwrap();
        s.accept_diff(
            "k",
            &[add(
                "a",
                PartRef::Root,
                "Widget ops",
                "go-live",
                Lifecycle::Todo,
                vec![],
            )],
        )
        .unwrap();
        let part = s.load_tree("k").unwrap()[0].id;
        // append-only provenanced log
        let n1 = s
            .add_note(
                "k",
                part,
                "decision",
                "feature-flag rollout plan",
                "user",
            )
            .unwrap();
        s.add_note("k", part, "note", "staging deploy approved", "sess-abc")
            .unwrap();
        let log = s.notes_for_part(part).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].source, "sess-abc"); // newest first
                                               // cross-cutting link surfaces the note on a second node
        s.accept_diff(
            "k",
            &[add(
                "b",
                PartRef::Root,
                "Signals",
                "",
                Lifecycle::Todo,
                vec![],
            )],
        )
        .unwrap();
        let part2 = s
            .load_tree("k")
            .unwrap()
            .iter()
            .find(|p| p.name == "Signals")
            .unwrap()
            .id;
        s.link_note(n1, part2).unwrap();
        assert!(s
            .notes_for_part(part2)
            .unwrap()
            .iter()
            .any(|n| n.text.contains("feature-flag")));
        // ⌘K recall across all three layers
        s.record_summary(
            "sess-abc",
            "k",
            1000,
            900,
            10,
            "/t/x.jsonl",
            "wire feature-flag",
            "wired the feature-flag toggle logic",
            "",
            "[]",
        )
        .unwrap();
        let hits = s.search_all("feature-flag", 10).unwrap();
        let kinds: std::collections::HashSet<String> =
            hits.iter().map(|h| h.kind.clone()).collect();
        assert!(
            kinds.contains("note") && kinds.contains("summary"),
            "kinds: {kinds:?}"
        );
        assert!(s
            .search_all("Widget", 10)
            .unwrap()
            .iter()
            .any(|h| h.kind == "node"));
        assert!(s.search_all("", 10).unwrap().is_empty());
        // spatial position persists — and Unpin (docs/019 CANVAS menu) clears
        // it back to auto-layout.
        s.set_part_pos(part, 0.42, 0.17).unwrap();
        let p = s
            .load_tree("k")
            .unwrap()
            .into_iter()
            .find(|p| p.id == part)
            .unwrap();
        assert_eq!((p.map_x, p.map_y), (Some(0.42), Some(0.17)));
        s.clear_part_pos(part).unwrap();
        let p = s
            .load_tree("k")
            .unwrap()
            .into_iter()
            .find(|p| p.id == part)
            .unwrap();
        assert_eq!((p.map_x, p.map_y), (None, None));
        // the Why-is-this-here quad (docs/019): user-authored Adds stamp
        // created_by from origin; the evidence trio stays empty until the
        // slice-2 cartographer writes it. Unknown ids answer None.
        let (who, sf, sq, ra) = s.part_provenance(part).unwrap();
        assert_eq!(who, "user");
        assert_eq!((sf, sq, ra), (None, None, None));
        assert!(s.part_provenance(999_999).is_none());
        // provenance lands in the journal
        s.accept_diff_from(
            "k",
            &[DiffOp::SetStatus {
                id: part,
                lifecycle: Lifecycle::Done,
                source: StatusSource::User,
            }],
            "summary",
            Some("sess-abc"),
        )
        .unwrap();
        let (origin, src): (String, String) = s
            .conn
            .query_row(
                "SELECT origin, source_sess FROM tree_event ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((origin.as_str(), src.as_str()), ("summary", "sess-abc"));
    }

    /// THE SLICE-1a GATE (docs/019): compound-undo of the exact Dissolve-Tech
    /// shape — N Moves + 1 Remove, undone back to identity. Under the old
    /// forward-order inverse replay this orphaned every moved child (the Move
    /// inverses referenced the removed root's dead id).
    #[test]
    fn compound_undo_of_dissolve_shape_restores_identity() {
        use crate::tree::dissolve_node_ops;
        let mut s = Store::open_in_memory().unwrap();
        // the user's actual shape: a "Tech" root wrapping 6 children, one
        // of which has its own child (so undo must also keep grandchildren).
        let mut ops = vec![add(
            "tech",
            PartRef::Root,
            "Tech",
            "the codebase",
            Lifecycle::Todo,
            vec![],
        )];
        for i in 1..=6 {
            ops.push(add(
                &format!("c{i}"),
                PartRef::Temp("tech".into()),
                &format!("Area {i}"),
                "d",
                Lifecycle::Done,
                vec![format!("crates/a{i}/**")],
            ));
        }
        ops.push(add(
            "g",
            PartRef::Temp("c3".into()),
            "Grandchild",
            "",
            Lifecycle::Todo,
            vec![],
        ));
        s.accept_diff("k", &ops).unwrap();
        let parts = s.load_tree("k").unwrap();
        let tech = parts.iter().find(|p| p.name == "Tech").unwrap().id;

        // dissolve: 6 Moves to root + Remove(tech), one transaction
        let ops = dissolve_node_ops(&parts, tech);
        assert_eq!(ops.len(), 7);
        s.accept_diff("k", &ops).unwrap();
        let after = s.load_tree("k").unwrap();
        assert!(after.iter().all(|p| p.name != "Tech"), "husk removed");
        assert_eq!(
            after.iter().filter(|p| p.parent_id.is_none()).count(),
            6,
            "children promoted to roots"
        );

        // ⌘Z: one undo restores the exact shape — Tech root (new id), all 6
        // children back under it, grandchild intact, statuses + anchors intact.
        assert!(s.undo_last("k").unwrap());
        let undone = s.load_tree("k").unwrap();
        let tech2 = undone
            .iter()
            .find(|p| p.name == "Tech")
            .expect("tech re-added");
        assert!(tech2.parent_id.is_none());
        for i in 1..=6 {
            let c = undone
                .iter()
                .find(|p| p.name == format!("Area {i}"))
                .unwrap();
            assert_eq!(
                c.parent_id,
                Some(tech2.id),
                "child {i} back under Tech, not orphaned"
            );
            assert_eq!(c.lifecycle, Lifecycle::Done);
            assert_eq!(c.anchors, vec![format!("crates/a{i}/**")]);
        }
        let g = undone.iter().find(|p| p.name == "Grandchild").unwrap();
        let c3 = undone.iter().find(|p| p.name == "Area 3").unwrap();
        assert_eq!(g.parent_id, Some(c3.id), "grandchild follows its parent");
        assert_eq!(undone.len(), 8, "identity: same node count");
        // sibling ORDER survives (review: Add inverses carry sort_order —
        // without it, reversed replay + next_order re-added children C,B,A).
        let mut kids: Vec<&Part> = undone
            .iter()
            .filter(|p| p.parent_id == Some(tech2.id))
            .collect();
        kids.sort_by(|a, b| a.sort_order.partial_cmp(&b.sort_order).unwrap());
        let names: Vec<&str> = kids.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Area 1", "Area 2", "Area 3", "Area 4", "Area 5", "Area 6"],
            "sibling order restored exactly"
        );
    }

    /// Remove NEVER orphans a survivor (review 1c, the load-bearing fix): a
    /// dissolve op-set that removes the container but does NOT move every child
    /// out (a toggled-off Move, or a child added after seeding) must PROMOTE
    /// the survivor, never strand it under a deleted parent — and one ⌘Z must
    /// still restore the survivor under the re-added container.
    #[test]
    fn remove_promotes_survivors_instead_of_orphaning_them() {
        let mut s = Store::open_in_memory().unwrap();
        // Tech over 3 children; one grandchild under c1 (survivor keeps its own subtree).
        let mut ops = vec![add(
            "tech",
            PartRef::Root,
            "Tech",
            "the codebase",
            Lifecycle::Todo,
            vec![],
        )];
        for i in 1..=3 {
            ops.push(add(
                &format!("c{i}"),
                PartRef::Temp("tech".into()),
                &format!("Area {i}"),
                "d",
                Lifecycle::Todo,
                vec![],
            ));
        }
        ops.push(add(
            "g",
            PartRef::Temp("c1".into()),
            "Grandchild",
            "",
            Lifecycle::Todo,
            vec![],
        ));
        s.accept_diff("k", &ops).unwrap();
        let parts = s.load_tree("k").unwrap();
        let id = |n: &str| parts.iter().find(|p| p.name == n).unwrap().id;
        let (tech, c1) = (id("Tech"), id("Area 1"));
        let c1_order = parts.iter().find(|p| p.id == c1).unwrap().sort_order;

        // a dissolve with c1's Move TOGGLED OFF: only c2,c3 move out, then Remove(tech).
        let ops = vec![
            DiffOp::Move {
                id: id("Area 2"),
                parent: PartRef::Root,
                sort_order: 10.0,
            },
            DiffOp::Move {
                id: id("Area 3"),
                parent: PartRef::Root,
                sort_order: 11.0,
            },
            DiffOp::Remove { id: tech },
        ];
        s.accept_diff("k", &ops).unwrap();
        let after = s.load_tree("k").unwrap();
        assert!(after.iter().all(|p| p.name != "Tech"), "husk removed");
        // c1 must be PROMOTED to root, never orphaned — build_tree reaches it.
        let c1_after = after
            .iter()
            .find(|p| p.id == c1)
            .expect("survivor still exists");
        assert_eq!(
            c1_after.parent_id, None,
            "survivor promoted to root, not orphaned"
        );
        let tree = build_tree(&after);
        assert!(
            tree.iter().any(|n| n.part.id == c1),
            "survivor is a reachable root"
        );
        // its grandchild rides along.
        assert!(
            after
                .iter()
                .find(|p| p.name == "Grandchild")
                .unwrap()
                .parent_id
                == Some(c1)
        );
        assert_eq!(
            after.iter().filter(|p| p.parent_id.is_none()).count(),
            3,
            "c1, c2, c3 all roots"
        );

        // ⌘Z restores Tech with the survivor back under it at its old order.
        assert!(s.undo_last("k").unwrap());
        let undone = s.load_tree("k").unwrap();
        let tech2 = undone
            .iter()
            .find(|p| p.name == "Tech")
            .expect("tech re-added");
        assert_eq!(
            undone.iter().find(|p| p.id == c1).unwrap().parent_id,
            Some(tech2.id),
            "survivor restored under Tech"
        );
        assert_eq!(
            undone.iter().find(|p| p.id == c1).unwrap().sort_order,
            c1_order,
            "survivor order restored"
        );
        assert_eq!(
            undone
                .iter()
                .find(|p| p.name == "Grandchild")
                .unwrap()
                .parent_id,
            Some(c1),
            "grandchild intact"
        );
    }

    /// A cycle-forming Move is SKIPPED, never corrupts the tree (review 2b: a
    /// machine `A→B` + `B→A` pair would make both unreachable from any root).
    #[test]
    fn cycle_forming_move_is_skipped_not_applied() {
        let mut s = Store::open_in_memory().unwrap();
        // roots A(1), B(2); B has child C(3).
        s.accept_diff(
            "k",
            &[
                add("a", PartRef::Root, "A", "", Lifecycle::Todo, vec![]),
                add("b", PartRef::Root, "B", "", Lifecycle::Todo, vec![]),
                add(
                    "c",
                    PartRef::Temp("b".into()),
                    "C",
                    "",
                    Lifecycle::Todo,
                    vec![],
                ),
            ],
        )
        .unwrap();
        let id = |n: &str| {
            s.load_tree("k")
                .unwrap()
                .iter()
                .find(|p| p.name == n)
                .unwrap()
                .id
        };
        let (a, b, c) = (id("A"), id("B"), id("C"));
        // move A under C (fine), then B under A — B under A under C under B = cycle.
        s.accept_diff(
            "k",
            &[
                DiffOp::Move {
                    id: a,
                    parent: PartRef::Id(c),
                    sort_order: 1.0,
                },
                DiffOp::Move {
                    id: b,
                    parent: PartRef::Id(a),
                    sort_order: 1.0,
                },
            ],
        )
        .unwrap();
        let after = s.load_tree("k").unwrap();
        // A moved under C; the B→A move is SKIPPED (would cycle: A<C<B), so B
        // stays a root and every node is still reachable from a root.
        assert_eq!(after.iter().find(|p| p.id == a).unwrap().parent_id, Some(c));
        assert_eq!(
            after.iter().find(|p| p.id == b).unwrap().parent_id,
            None,
            "cycle move skipped, B stays root"
        );
        let reachable = build_tree(&after);
        fn count(ns: &[crate::tree::TreeNode]) -> usize {
            ns.iter().map(|n| 1 + count(&n.children)).sum()
        }
        assert_eq!(count(&reachable), 3, "no node lost to a cycle");
        // a node moved onto its OWN descendant directly is also skipped.
        s.accept_diff(
            "k",
            &[DiffOp::Move {
                id: c,
                parent: PartRef::Id(a),
                sort_order: 1.0,
            }],
        )
        .unwrap();
        assert_eq!(
            s.load_tree("k")
                .unwrap()
                .iter()
                .find(|p| p.id == c)
                .unwrap()
                .parent_id,
            Some(b),
            "C onto its ancestor A skipped"
        );
    }

    /// Removing a node cleans its needs_you flag (review 4: an orphaned flag
    /// pulses the one-summons forever for a node that no longer exists).
    #[test]
    fn remove_cleans_needs_you_flag() {
        let mut s = Store::open_in_memory().unwrap();
        s.accept_diff("k", &seed_ops()).unwrap();
        let id = s
            .load_tree("k")
            .unwrap()
            .iter()
            .find(|p| p.name == "Store")
            .unwrap()
            .id;
        s.set_needs_you("k", id, "why is this blocked?", 100)
            .unwrap();
        assert!(s.needs_you_for(id).is_some());
        s.accept_diff("k", &[DiffOp::Remove { id }]).unwrap();
        assert!(s.needs_you_for(id).is_none(), "flag gone with the node");
        assert!(
            s.needs_you_flags("k").is_empty(),
            "no orphan in the project list"
        );
    }

    /// A Move onto a NONEXISTENT parent is skipped, never orphans the node
    /// (review 2b: a stale rework snapshot could move onto a node deleted
    /// mid-run — part.parent_id has no FK, so the row would vanish).
    #[test]
    fn move_onto_deleted_parent_is_skipped() {
        let mut s = Store::open_in_memory().unwrap();
        s.accept_diff(
            "k",
            &[
                add("a", PartRef::Root, "A", "", Lifecycle::Todo, vec![]),
                add("b", PartRef::Root, "B", "", Lifecycle::Todo, vec![]),
            ],
        )
        .unwrap();
        let id = |n: &str| {
            s.load_tree("k")
                .unwrap()
                .iter()
                .find(|p| p.name == n)
                .unwrap()
                .id
        };
        let (a, b) = (id("A"), id("B"));
        // move A under a parent id that does not exist (999) → skipped, A stays root.
        s.accept_diff(
            "k",
            &[DiffOp::Move {
                id: a,
                parent: PartRef::Id(999),
                sort_order: 1.0,
            }],
        )
        .unwrap();
        assert_eq!(
            s.load_tree("k")
                .unwrap()
                .iter()
                .find(|p| p.id == a)
                .unwrap()
                .parent_id,
            None,
            "move onto missing parent skipped"
        );
        // sanity: a move onto an existing parent still works.
        s.accept_diff(
            "k",
            &[DiffOp::Move {
                id: a,
                parent: PartRef::Id(b),
                sort_order: 1.0,
            }],
        )
        .unwrap();
        assert_eq!(
            s.load_tree("k")
                .unwrap()
                .iter()
                .find(|p| p.id == a)
                .unwrap()
                .parent_id,
            Some(b)
        );
        assert_eq!(
            build_tree(&s.load_tree("k").unwrap()).len(),
            1,
            "B root with A under it — nothing lost"
        );
    }

    /// Order-insensitive undo (review): a diff that removes root-FIRST (the
    /// public API allows any op order) must still undo to the right shape —
    /// the deferred temp fixup re-parents children whose undo-temp resolved
    /// only after their op ran.
    #[test]
    fn root_first_removal_still_undoes_to_shape() {
        let mut s = Store::open_in_memory().unwrap();
        s.accept_diff("k", &seed_ops()).unwrap(); // Flow map > Store
        let parts = s.load_tree("k").unwrap();
        let flow = parts.iter().find(|p| p.name == "Flow map").unwrap().id;
        let store_id = parts.iter().find(|p| p.name == "Store").unwrap().id;
        // deliberately WRONG order: parent first, child second
        s.accept_diff(
            "k",
            &[DiffOp::Remove { id: flow }, DiffOp::Remove { id: store_id }],
        )
        .unwrap();
        assert!(s.undo_last("k").unwrap());
        let undone = s.load_tree("k").unwrap();
        let flow2 = undone.iter().find(|p| p.name == "Flow map").unwrap();
        let store2 = undone.iter().find(|p| p.name == "Store").unwrap();
        assert_eq!(
            store2.parent_id,
            Some(flow2.id),
            "child re-parented via deferred fixup, not scattered to root"
        );
    }

    /// Rename keeps the detail = first_line(detail_md) invariant (review) and
    /// round-trips through undo.
    #[test]
    fn rename_syncs_body_first_line_and_undoes() {
        let mut s = Store::open_in_memory().unwrap();
        s.accept_diff("k", &seed_ops()).unwrap();
        let id = s
            .load_tree("k")
            .unwrap()
            .iter()
            .find(|p| p.name == "Terminal host")
            .unwrap()
            .id;
        s.accept_diff(
            "k",
            &[DiffOp::SetDetail {
                id,
                detail_md: "PTY\n\nEmulator internals.".into(),
            }],
        )
        .unwrap();
        s.accept_diff(
            "k",
            &[DiffOp::Rename {
                id,
                name: "Host".into(),
                detail: "PTY + resume".into(),
            }],
        )
        .unwrap();
        let p = s
            .load_tree("k")
            .unwrap()
            .into_iter()
            .find(|p| p.id == id)
            .unwrap();
        assert_eq!(p.detail, "PTY + resume");
        assert_eq!(
            p.detail_md, "PTY + resume\n\nEmulator internals.",
            "body first line follows the one-liner"
        );
        assert!(s.undo_last("k").unwrap());
        let p = s
            .load_tree("k")
            .unwrap()
            .into_iter()
            .find(|p| p.id == id)
            .unwrap();
        assert_eq!(p.detail, "PTY");
        assert_eq!(
            p.detail_md, "PTY\n\nEmulator internals.",
            "undo restores the body's first line too"
        );
    }

    /// Leaf-first subtree removal + undo: the whole subtree comes back with
    /// its internal parent/child shape threaded through temp refs.
    #[test]
    fn subtree_delete_undo_restores_nested_shape() {
        use crate::tree::subtree_removal_ops;
        let mut s = Store::open_in_memory().unwrap();
        s.accept_diff("k", &seed_ops()).unwrap(); // Terminal host, Flow map > Store
        let parts = s.load_tree("k").unwrap();
        let flow = parts.iter().find(|p| p.name == "Flow map").unwrap().id;
        let ops = subtree_removal_ops(&parts, flow);
        assert_eq!(ops.len(), 2, "Store first, then Flow map");
        assert!(matches!(ops[0], DiffOp::Remove { .. }));
        s.accept_diff("k", &ops).unwrap();
        assert_eq!(
            s.load_tree("k").unwrap().len(),
            1,
            "only Terminal host left"
        );
        assert!(s.undo_last("k").unwrap());
        let undone = s.load_tree("k").unwrap();
        assert_eq!(undone.len(), 3);
        let flow2 = undone.iter().find(|p| p.name == "Flow map").unwrap();
        let store2 = undone.iter().find(|p| p.name == "Store").unwrap();
        assert_eq!(
            store2.parent_id,
            Some(flow2.id),
            "nested child re-parents to the re-added parent"
        );
    }

    /// docs/019 commitment 2: `building` is never stored — asserting it lands
    /// `todo` — and existing journal rows serializing Building still parse.
    #[test]
    fn building_coerces_at_write_and_old_journal_rows_still_parse() {
        let mut s = Store::open_in_memory().unwrap();
        s.accept_diff("k", &seed_ops()).unwrap();
        let id = s
            .load_tree("k")
            .unwrap()
            .iter()
            .find(|p| p.name == "Store")
            .unwrap()
            .id;
        s.set_status("k", id, Lifecycle::Building).unwrap();
        let p = s
            .load_tree("k")
            .unwrap()
            .into_iter()
            .find(|p| p.id == id)
            .unwrap();
        assert_eq!(
            p.lifecycle,
            Lifecycle::Todo,
            "building is derived-only; the stored assertion is todo"
        );
        // pre-019 journal JSON (no kind/detail_md fields, Building lifecycle)
        // must still deserialize — undo history depends on it.
        let old = r#"[{"Add":{"temp":"t","parent":"Root","name":"X","detail":"","lifecycle":"Building","anchors":[]}},
                      {"SetStatus":{"id":1,"lifecycle":"Building","source":"User"}}]"#;
        let ops: Vec<DiffOp> = serde_json::from_str(old).expect("old rows parse forever");
        assert!(matches!(
            &ops[0],
            DiffOp::Add {
                kind: Kind::Task,
                detail_md: None,
                lifecycle: Lifecycle::Building,
                ..
            }
        ));
        // pre-019 STORED building rows still load as Building (the data
        // migration is deferred to slice 3, when derived-building rendering
        // replaces them — review: erasing them earlier would drop visible
        // in-progress state with nothing in its place).
        s.conn
            .execute(
                "UPDATE part SET lifecycle='building' WHERE id=?1",
                params![id],
            )
            .unwrap();
        let p = s
            .load_tree("k")
            .unwrap()
            .into_iter()
            .find(|p| p.id == id)
            .unwrap();
        assert_eq!(p.lifecycle, Lifecycle::Building);
    }

    /// docs/019: kind backfill — parents become areas (journaled, once);
    /// task_rollup then makes area state computed.
    #[test]
    fn kind_backfill_marks_parents_as_areas_once_and_is_journaled() {
        use crate::tree::{build_tree, task_rollup};
        let mut s = Store::open_in_memory().unwrap();
        s.accept_diff("k", &seed_ops()).unwrap();
        // simulate a pre-019 DB: reset kinds + drop the gate, then re-migrate
        s.conn.execute("UPDATE part SET kind='task'", []).unwrap();
        s.conn
            .execute("DELETE FROM app_settings WHERE key='kind_backfill_v3'", [])
            .unwrap();
        s.conn
            .execute("DELETE FROM tree_event WHERE origin='migration'", [])
            .unwrap();
        let events_before: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM tree_event", [], |r| r.get(0))
            .unwrap();
        s.migrate().unwrap();
        let parts = s.load_tree("k").unwrap();
        assert_eq!(
            parts.iter().find(|p| p.name == "Flow map").unwrap().kind,
            Kind::Area,
            "has a child → area"
        );
        assert_eq!(
            parts.iter().find(|p| p.name == "Store").unwrap().kind,
            Kind::Task,
            "leaf stays task"
        );
        let events_after: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM tree_event", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            events_after,
            events_before + 1,
            "backfill journals ONE event"
        );
        // gated: re-running migrate must not re-fire
        s.migrate().unwrap();
        let n: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM tree_event WHERE origin='migration'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        // ⌘Z NEVER reaches the migration event (review: the newest-event undo
        // silently reverted the backfill). A user edit after the backfill
        // undoes first; the next undo SKIPS the migration and lands on the
        // seed — the migration row stays in the journal as a record.
        let flow_id = parts.iter().find(|p| p.name == "Flow map").unwrap().id;
        s.set_status("k", flow_id, Lifecycle::Done).unwrap();
        assert!(s.undo_last("k").unwrap());
        let after_first = s.load_tree("k").unwrap();
        assert_eq!(
            after_first
                .iter()
                .find(|p| p.name == "Flow map")
                .unwrap()
                .lifecycle,
            Lifecycle::Todo,
            "user edit undone"
        );
        assert_eq!(
            after_first
                .iter()
                .find(|p| p.name == "Flow map")
                .unwrap()
                .kind,
            Kind::Area,
            "migration untouched"
        );
        assert!(
            s.undo_last("k").unwrap(),
            "second undo skips the migration event..."
        );
        assert!(
            s.load_tree("k").unwrap().is_empty(),
            "...and undoes the seed"
        );
        let still: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM tree_event WHERE origin='migration'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still, 1, "migration event survives as a journal record");
        // rollup: Flow map (area) rolls up its one task
        let tree = build_tree(&parts);
        let flow = tree.iter().find(|n| n.part.name == "Flow map").unwrap();
        assert_eq!(task_rollup(flow), (0, 1));
    }

    /// docs/019: SetDetail edits the body + re-derives the one-liner; SetKind
    /// flips; both undo exactly.
    #[test]
    fn set_detail_and_set_kind_roundtrip_and_undo() {
        let mut s = Store::open_in_memory().unwrap();
        s.accept_diff("k", &seed_ops()).unwrap();
        let id = s
            .load_tree("k")
            .unwrap()
            .iter()
            .find(|p| p.name == "Store")
            .unwrap()
            .id;
        s.accept_diff(
            "k",
            &[DiffOp::SetDetail {
                id,
                detail_md: "The SQLite shell.\n\nOwns the journal.".into(),
            }],
        )
        .unwrap();
        s.accept_diff(
            "k",
            &[DiffOp::SetKind {
                id,
                kind: Kind::Area,
            }],
        )
        .unwrap();
        let p = s
            .load_tree("k")
            .unwrap()
            .into_iter()
            .find(|p| p.id == id)
            .unwrap();
        assert_eq!(p.detail_md, "The SQLite shell.\n\nOwns the journal.");
        assert_eq!(
            p.detail, "The SQLite shell.",
            "one-liner derives from first line"
        );
        assert_eq!(p.kind, Kind::Area);
        assert!(s.undo_last("k").unwrap()); // kind back
        assert!(s.undo_last("k").unwrap()); // body back
        let p = s
            .load_tree("k")
            .unwrap()
            .into_iter()
            .find(|p| p.id == id)
            .unwrap();
        assert_eq!(p.kind, Kind::Task);
        assert_eq!(p.detail_md, "", "pre-edit body restored");
    }

    /// docs/019: changeset shells + the durable summary-job queue.
    #[test]
    fn changesets_and_summary_jobs_roundtrip() {
        let s = Store::open_in_memory().unwrap();
        let cs = s
            .create_changeset(
                "k",
                "Dissolve Tech",
                "machine repairs its own mess",
                Some(25),
                "canned",
            )
            .unwrap();
        let pid = s.add_pending_diff("k", "changeset", &seed_ops()).unwrap();
        s.link_pending_to_changeset(pid, cs).unwrap();
        let open = s.open_changesets("k");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].1, "Dissolve Tech");
        assert_eq!(open[0].3, Some(25));
        s.set_changeset_status(cs, "accepted").unwrap();
        assert!(s.open_changesets("k").is_empty());
        // summary jobs: dedupe while queued, retry to dead, dead rows surface
        let j1 = s.enqueue_summary_job("sess-1", "k", "end").unwrap();
        let j2 = s.enqueue_summary_job("sess-1", "k", "idle").unwrap();
        assert_eq!(j1, j2, "queued job absorbs the second trigger");
        let claimed = s.claim_summary_job().expect("claimable");
        assert_eq!(claimed.0, j1);
        assert_eq!(claimed.3, "end");
        // a trigger arriving while the job RUNS is ABSORBED when it is no
        // stronger than the running one. It used not to be, and the cost showed
        // up in the user's live store: `claim` flips the row to 'running', so
        // the very next tick saw no queued row for a still-firing idle trigger
        // and inserted a TWIN — 11 twin pairs, every summary generated and paid
        // for twice, halving the shared 20/hr budget.
        let j_mid = s.enqueue_summary_job("sess-1", "k", "idle").unwrap();
        assert_eq!(j_mid, j1, "a running job absorbs an equal-or-weaker trigger");
        for _ in 0..3 {
            s.finish_summary_job(j1, Some("model timeout")).unwrap();
            let _ = s.claim_summary_job();
        }
        let dead = s.dead_summary_jobs();
        assert_eq!(dead.len(), 1, "3 failed attempts → dead, surfaced");
        assert_eq!(dead[0].3, "model timeout");
        // a dead job no longer dedupes — a fresh trigger gets a fresh job
        let j3 = s.enqueue_summary_job("sess-1", "k", "end").unwrap();
        assert_ne!(j3, j1);
        // success path
        let (id, ..) = s.claim_summary_job().unwrap();
        s.finish_summary_job(id, None).unwrap();
        assert!(s.claim_summary_job().is_none());
        assert_eq!(s.dead_summary_jobs().len(), 1, "done jobs don't surface");
    }

    /// docs/019 slice 3 review: queue hardening — trigger upgrade, transcript-
    /// miss defer (no attempt spent), and the dead-session enqueue skip.
    #[test]
    fn summary_queue_upgrade_defer_and_dead_skip() {
        let s = Store::open_in_memory().unwrap();
        // trigger UPGRADE: a queued delta absorbs a later 'end' (review finding 4).
        let j = s.enqueue_summary_job("s1", "k", "delta").unwrap();
        let j2 = s.enqueue_summary_job("s1", "k", "end").unwrap();
        assert_eq!(j, j2, "same job");
        let (_, _, _, trig) = s.claim_summary_job().unwrap();
        assert_eq!(trig, "end", "trigger upgraded delta → end");
        // a weaker trigger never downgrades a queued 'end'.
        s.finish_summary_job(j, Some("x")).unwrap(); // back to queued (attempts 1)
        s.enqueue_summary_job("s1", "k", "idle").unwrap();
        let t = s
            .conn
            .query_row(
                "SELECT trigger FROM summary_job WHERE id=?1",
                params![j],
                |r| r.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(t, "end", "idle doesn't downgrade a queued end");

        // DEFER: a transcript-miss requeues WITHOUT marching toward dead
        // (review finding 1). Claim spends an attempt; defer gives it back.
        // And it goes to the BACK of the line — the refund alone was a
        // starvation bug (see `a_deferred_job_yields_the_queue`).
        let s2 = Store::open_in_memory().unwrap();
        let jj = s2.enqueue_summary_job("s2", "k", "idle").unwrap();
        for _ in 0..5 {
            let (id, ..) = s2.claim_summary_job().expect("claimable");
            assert!(
                s2.defer_summary_job(id, 60, "transcript: not ready").unwrap(),
                "still inside its defer allowance"
            );
            assert!(
                s2.claim_summary_job().is_none(),
                "a deferred job is NOT eligible again until its retry-after elapses"
            );
            due_now(&s2, jj); // …time passes.
        }
        assert!(
            s2.dead_summary_jobs().is_empty(),
            "a defer loop never kills a healthy job"
        );
        let att: i64 = s2
            .conn
            .query_row(
                "SELECT attempts FROM summary_job WHERE id=?1",
                params![jj],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(att, 0, "the speculative attempt is returned on defer");

        // DEATH is TIMESTAMPED, not terminal. The enqueue loop reads these
        // stamps and cools the session off for a while (escalating with how
        // many there were) instead of the old boolean `session_has_dead_job`,
        // which skipped a session that had ever died FOREVER — two days of no
        // standup, 10 sessions blacklisted by one transient codex window.
        for _ in 0..3 {
            let (id, ..) = s2.claim_summary_job().unwrap();
            s2.finish_summary_job(id, Some("real failure")).unwrap();
        }
        let deaths = s2.session_death_times("s2");
        assert_eq!(deaths.len(), 1, "the death is recorded");
        assert!(
            deaths[0] > 0,
            "a death carries WHEN it happened — the cool-off expires from it, \
             with no migration and no startup hook"
        );
        assert!(s2.session_death_times("s-other").is_empty());
    }

    /// The two rows that the cool-off + in-flight dedup depend on: a LEGACY dead
    /// row (written before `updated_ms` shipped) must still yield a death time,
    /// and a crash-orphaned 'running' row must be reclaimed.
    #[test]
    fn legacy_deaths_have_a_time_and_orphaned_running_jobs_are_reclaimed() {
        let ms = |secs_ago: i64| {
            (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
                - secs_ago)
                * 1000
        };
        let s = Store::open_in_memory().unwrap();
        let j = s.enqueue_summary_job("legacy", "k", "idle").unwrap();
        for _ in 0..3 {
            let (id, ..) = s.claim_summary_job().unwrap();
            s.finish_summary_job(id, Some("cli: codex exec exit=1 | stderr(2167B) tail: …429"))
                .unwrap();
        }
        // exactly the user's rows: dead, no death stamp, enqueued 3 days ago.
        let three_days_ago = ms(3 * 86_400);
        s.conn
            .execute(
                "UPDATE summary_job SET updated_ms=NULL, enqueued_ms=?2 WHERE id=?1",
                params![j, three_days_ago],
            )
            .unwrap();
        assert_eq!(
            s.session_death_times("legacy"),
            vec![three_days_ago as u64],
            "a pre-migration death falls back to enqueued_ms — days old, so every \
             cool-off window has long expired and the session summarizes again"
        );

        // a 'running' row had no writer but the claim, so killing the app mid-
        // summary stranded it there forever. Now that an in-flight job absorbs
        // its session's triggers, a stranded row would wedge that session — so
        // an expired lease is reclaimed on the next claim.
        let k = s.enqueue_summary_job("orphan", "k", "idle").unwrap();
        let (id, ..) = s.claim_summary_job().expect("claimable");
        assert_eq!(id, k);
        assert!(
            s.claim_summary_job().is_none(),
            "a fresh lease is respected — no double-claim"
        );
        s.conn
            .execute(
                "UPDATE summary_job SET updated_ms=?2 WHERE id=?1",
                params![k, ms(3600)],
            )
            .unwrap();
        let (again, ..) = s.claim_summary_job().expect("the orphan is reclaimed");
        assert_eq!(again, k, "an expired lease returns the job to the queue");
        // …and the reclaim REFUNDS the attempt the crashed claim charged. Two
        // crashes mid-summary used to silently eat 2 of the job's 3 attempts, so
        // its first REAL failure died on try one and bought the session a
        // cool-off it hadn't earned. A crash is not the job's fault — same rule
        // as `defer_summary_job`: a requeue may never march a healthy job to
        // death. (attempts: claim=1, reclaim=0, re-claim=1.)
        assert_eq!(
            attempts_of(&s, k),
            1,
            "the crashed claim's attempt was given back, so this re-claim is try ONE"
        );
    }

    /// Read a job's `attempts`. The whole death/cool-off machine is driven by it,
    /// so every requeue path has to be explicit about what it does to the count.
    fn attempts_of(s: &Store, id: i64) -> i64 {
        s.conn
            .query_row(
                "SELECT attempts FROM summary_job WHERE id=?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// Make a deferred job due RIGHT NOW — the test's way of letting its
    /// retry-after elapse without sleeping.
    fn due_now(s: &Store, id: i64) {
        s.conn
            .execute(
                "UPDATE summary_job SET next_attempt_ms=0 WHERE id=?1",
                params![id],
            )
            .unwrap();
    }

    /// FINDING 2, half one: a deferred job must YIELD.
    ///
    /// `claim` is strictly oldest-first and takes ONE job at a time, and a defer
    /// refunds the attempt. Before `next_attempt_ms`, a defer left `enqueued_ms`
    /// untouched — so the deferred job went straight back to the HEAD of the
    /// queue and was re-claimed on the very next tick, forever. One session that
    /// can never be summarized (an empty codex rollout; a parse failure
    /// misread as a rate limit) starved EVERY other session's standup
    /// indefinitely — the exact "one bad session blinds everyone" outcome the
    /// permanent blacklist was reaching for, reintroduced by its replacement.
    #[test]
    fn a_deferred_job_yields_the_queue() {
        let s = Store::open_in_memory().unwrap();
        let bad = s.enqueue_summary_job("never-summarizable", "k", "idle").unwrap();
        let (id, ..) = s.claim_summary_job().expect("the oldest job is claimed");
        assert_eq!(id, bad);
        s.defer_summary_job(bad, 60, "transcript: not ready").unwrap();

        // a YOUNGER session enqueues while the bad one is deferred.
        let good = s.enqueue_summary_job("healthy", "k", "idle").unwrap();
        let (id, cid, ..) = s.claim_summary_job().expect("the healthy job is claimable");
        assert_eq!(
            (id, cid.as_str()),
            (good, "healthy"),
            "the deferred job does not hold the head of the queue — the younger, \
             READY job is claimed instead. This is the starvation fix."
        );
        s.finish_summary_job(good, None).unwrap();

        // even once it is due again, it sorts BEHIND work that was ready first:
        // the not-before is the sort key, not just a filter.
        let later = s.enqueue_summary_job("later", "k", "idle").unwrap();
        due_now(&s, bad);
        let (id, ..) = s.claim_summary_job().expect("claimable");
        assert_eq!(id, bad, "…and when it IS due, it gets its turn (never dropped)");
        s.defer_summary_job(bad, 60, "transcript: not ready").unwrap();
        let (id, ..) = s.claim_summary_job().expect("claimable");
        assert_eq!(id, later, "…and yields again. It can delay nobody indefinitely.");
    }

    /// FINDING 2, half two: an IMMORTAL JOB IS IMPOSSIBLE BY CONSTRUCTION.
    ///
    /// A defer refunds the attempt, so a job that always defers can never die of
    /// attempts. If the rate-limit classifier is ever wrong — and it WAS: an
    /// unanchored `contains("rate")` read the model's own "…happy to gene-RATE a
    /// summary…" as a rate limit — a permanent, deterministic failure would
    /// defer forever: never dying, never surfacing, never summarized. So the
    /// defers are BOUNDED. This test does not trust the classifier; it feeds the
    /// queue a job that defers every single time and proves the queue kills it.
    #[test]
    fn defers_are_bounded_so_a_job_can_never_be_immortal() {
        let s = Store::open_in_memory().unwrap();
        let j = s.enqueue_summary_job("hopeless", "k", "idle").unwrap();
        let mut deferred = 0;
        // way more rounds than the allowance: the loop must NOT be what stops it.
        for _ in 0..(Store::MAX_SUMMARY_DEFERS + 10) {
            let Some((id, ..)) = s.claim_summary_job() else {
                break;
            };
            if s.defer_summary_job(id, 900, "cli: 429 rate limit").unwrap() {
                deferred += 1;
            }
            due_now(&s, j); // …and time is never the constraint either.
        }
        assert_eq!(
            deferred,
            Store::MAX_SUMMARY_DEFERS,
            "the allowance is spent exactly once"
        );
        let dead = s.dead_summary_jobs();
        assert_eq!(dead.len(), 1, "the job DIES — it cannot defer forever");
        assert_eq!(dead[0].1, "hopeless");
        assert!(
            dead[0].3.contains("rate limit"),
            "and it dies with the reason legible: {}",
            dead[0].3
        );
        assert!(
            s.claim_summary_job().is_none(),
            "a dead job is out of the queue: it starves nobody, and the session \
             now cools off (escalating, self-expiring) instead of being blacklisted"
        );
        assert_eq!(
            s.session_death_times("hopeless").len(),
            1,
            "the death is stamped, so the cool-off can expire from it"
        );
    }

    /// The defer's retry-after ESCALATES (a rate limit is ridden out, not spun
    /// on) and is capped (the last defers don't stretch to days).
    #[test]
    fn defer_backoff_escalates_then_caps() {
        assert_eq!(Store::defer_backoff_secs(0, 900), 900);
        assert_eq!(Store::defer_backoff_secs(1, 900), 1800);
        assert_eq!(Store::defer_backoff_secs(2, 900), 3600);
        assert_eq!(
            Store::defer_backoff_secs(3, 900),
            Store::DEFER_BACKOFF_CAP_SECS,
            "capped, not doubling to days"
        );
        assert_eq!(Store::defer_backoff_secs(0, 60), 60);
        assert_eq!(Store::defer_backoff_secs(4, 60), 960);
        assert_eq!(Store::defer_backoff_secs(i64::MAX, 60), Store::DEFER_BACKOFF_CAP_SECS);
        // the 10-defer rate-limit allowance rides out ~8.7h — past a provider's
        // 5h usage window — and then the job fails normally.
        let total: u64 = (0..Store::MAX_SUMMARY_DEFERS)
            .map(|d| Store::defer_backoff_secs(d, 900))
            .sum();
        assert!(
            total > 5 * 3600,
            "the defer allowance must outlast a real quota window, got {total}s"
        );
    }

    /// docs/019 slice 3 review finding 5: per-session freshness so one session's
    /// fresh summary can't mask another session that's behind.
    #[test]
    fn project_freshness_is_per_session() {
        let s = Store::open_in_memory().unwrap();
        // session A: event then a later summary → current. session B: event, no summary.
        s.record_event("A", "k", 1000, "x").unwrap();
        s.record_summary("A", "k", 2000, 1000, 10, "/t", "g", "h", "n", "[]")
            .unwrap();
        s.record_event("B", "k", 3000, "y").unwrap();
        let f = s.project_session_freshness("k");
        let a = f.iter().find(|_| true); // just assert shape + that B is behind
        assert!(a.is_some());
        let b = f.iter().find(|(ev, _)| *ev == 3000).expect("B present");
        assert_eq!(
            b.1, None,
            "B has events but no summary → behind, even though A is fresh"
        );
    }

    /// docs/019 slice 1c: changeset_pending loads the linked rows,
    /// flatten_changeset_ops preserves the (row, op) global index, and
    /// changeset_id lets pending_diffs partition legacy singletons from
    /// grouped changeset rows.
    #[test]
    fn changeset_pending_flattens_in_stable_index_order() {
        let s = Store::open_in_memory().unwrap();
        let cs = s
            .create_changeset("k", "Restructure", "i", None, "canned")
            .unwrap();
        // one legacy singleton (changeset_id NULL) + two changeset rows.
        let loose = s
            .add_pending_diff(
                "k",
                "drift",
                &[add(
                    "x",
                    PartRef::Root,
                    "Loose",
                    "",
                    Lifecycle::Todo,
                    vec![],
                )],
            )
            .unwrap();
        let r1 = s
            .add_pending_diff_with_evidence(
                "k",
                "changeset",
                &[add("a", PartRef::Root, "A", "", Lifecycle::Todo, vec![])],
                &[Some("q-a".into())],
            )
            .unwrap();
        let r2 = s
            .add_pending_diff_with_evidence(
                "k",
                "changeset",
                &[
                    add("b", PartRef::Root, "B", "", Lifecycle::Todo, vec![]),
                    add("c", PartRef::Root, "C", "", Lifecycle::Todo, vec![]),
                ],
                &[Some("q-b".into()), None],
            )
            .unwrap();
        s.link_pending_to_changeset(r1, cs).unwrap();
        s.link_pending_to_changeset(r2, cs).unwrap();

        // pending_diffs surfaces changeset_id so the GUI can split the lanes.
        let all = s.pending_diffs("k").unwrap();
        assert_eq!(
            all.iter().find(|p| p.id == loose).unwrap().changeset_id,
            None
        );
        assert_eq!(
            all.iter().find(|p| p.id == r1).unwrap().changeset_id,
            Some(cs)
        );

        // changeset_pending returns ONLY the linked rows, oldest first;
        // flatten preserves (row-id, then op) order as the global index.
        let rows = s.changeset_pending(cs).unwrap();
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![r1, r2]);
        let flat = flatten_changeset_ops(&rows);
        let names: Vec<&str> = flat
            .iter()
            .map(|(op, _)| match op {
                DiffOp::Add { name, .. } => name.as_str(),
                _ => "?",
            })
            .collect();
        assert_eq!(
            names,
            vec!["A", "B", "C"],
            "global index is row-then-op order"
        );
        assert_eq!(flat[0].1.as_deref(), Some("q-a"));
        assert_eq!(
            flat[2].1, None,
            "evidence stays index-aligned through the flatten"
        );
    }

    /// docs/019 slice 2: the cartographer's per-op EVIDENCE FLAGS survive the
    /// pending_diff round-trip and flatten in the SAME global order as the ops,
    /// so the review surface can zip them by index. Legacy rows (written via
    /// add_pending_diff, no flags) load as all-false.
    #[test]
    fn changeset_flags_roundtrip_and_flatten_index_aligned() {
        let s = Store::open_in_memory().unwrap();
        let cs = s
            .create_changeset("k", "Seed", "i", None, "seed:1")
            .unwrap();
        // one flagged (unverified) op + one clean op on the same row, then a
        // legacy no-flag row after it — flatten must interleave them by index.
        let r1 = s
            .add_pending_diff_full(
                "k",
                "changeset",
                &[
                    add("a", PartRef::Root, "A", "", Lifecycle::Todo, vec![]),
                    add("b", PartRef::Root, "B", "", Lifecycle::Todo, vec![]),
                ],
                &[Some("q-a".into()), Some("q-b".into())],
                &[true, false],
            )
            .unwrap();
        let r2 = s
            .add_pending_diff(
                "k",
                "changeset",
                &[add("c", PartRef::Root, "C", "", Lifecycle::Todo, vec![])],
            )
            .unwrap();
        s.link_pending_to_changeset(r1, cs).unwrap();
        s.link_pending_to_changeset(r2, cs).unwrap();

        let rows = s.changeset_pending(cs).unwrap();
        let flags = flatten_changeset_flags(&rows);
        assert_eq!(
            flags,
            vec![true, false, false],
            "flags flatten row-then-op, legacy row all-false"
        );
        // still index-aligned with the ops themselves.
        let ops = flatten_changeset_ops(&rows);
        assert_eq!(
            ops.len(),
            flags.len(),
            "flags parallel the flattened ops exactly"
        );
    }

    /// THE SLICE-1c GATE (docs/019 ruling 6, house rule): accepting a changeset
    /// — the KEPT ops minus a toggled-off op, with an Add name edited — applies
    /// as ONE journal event, so ONE ⌘Z reverts the WHOLE restructure. Proves
    /// the GUI's accept semantics (rebuild → one accept_diff_from) at the store.
    #[test]
    fn changeset_accept_is_one_undo_across_edit_and_toggle() {
        use crate::tree::dissolve_node_ops;
        let mut s = Store::open_in_memory().unwrap();
        // a Tech husk over three children (the user's actual mess shape).
        let mut seed = vec![add(
            "tech",
            PartRef::Root,
            "Tech",
            "codebase",
            Lifecycle::Todo,
            vec![],
        )];
        for i in 1..=3 {
            seed.push(add(
                &format!("c{i}"),
                PartRef::Temp("tech".into()),
                &format!("Area {i}"),
                "d",
                Lifecycle::Done,
                vec![],
            ));
        }
        s.accept_diff("k", &seed).unwrap();
        let parts = s.load_tree("k").unwrap();
        let tech = parts.iter().find(|p| p.name == "Tech").unwrap().id;

        // the canned changeset: dissolve ops carried on a pending row.
        let cs = s
            .create_changeset(
                "k",
                "Dissolve Tech",
                "unwrap the husk",
                Some(tech),
                "canned",
            )
            .unwrap();
        let dissolve = dissolve_node_ops(&parts, tech); // 3 Moves + 1 Remove
        let pid = s.add_pending_diff("k", "changeset", &dissolve).unwrap();
        s.link_pending_to_changeset(pid, cs).unwrap();

        // review: keep everything EXCEPT drop the Remove (toggle off idx 3),
        // and there are no Add ops here to rename — the rename path is proven
        // by the outline slot; here the load-bearing claim is the ONE undo.
        let rows = s.changeset_pending(cs).unwrap();
        let flat = flatten_changeset_ops(&rows);
        let kept: Vec<DiffOp> = flat
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 3)
            .map(|(_, (op, _))| op.clone())
            .collect();
        assert_eq!(kept.len(), 3, "toggled off the Remove — 3 Moves remain");

        let before = s.load_tree("k").unwrap();
        s.accept_diff_from("k", &kept, "human:review", None)
            .unwrap();
        s.set_changeset_status(cs, "partial").unwrap();
        s.drop_pending_diff(pid).unwrap();
        let after = s.load_tree("k").unwrap();
        // the 3 children moved to root; Tech survives (its Remove was toggled).
        assert!(
            after.iter().any(|p| p.name == "Tech"),
            "husk kept — Remove toggled off"
        );
        assert_eq!(
            after
                .iter()
                .filter(|p| p.parent_id.is_none() && p.name.starts_with("Area"))
                .count(),
            3
        );

        // ONE ⌘Z reverts the WHOLE restructure back to identity.
        assert!(s.undo_last("k").unwrap());
        let undone = s.load_tree("k").unwrap();
        assert_eq!(
            undone.len(),
            before.len(),
            "one undo, whole restructure gone"
        );
        for i in 1..=3 {
            let c = undone
                .iter()
                .find(|p| p.name == format!("Area {i}"))
                .unwrap();
            assert_eq!(
                c.parent_id,
                Some(tech),
                "child {i} back under Tech in one step"
            );
        }
        assert!(
            s.open_changesets("k").is_empty(),
            "resolved changeset is gone from the open set"
        );
    }

    /// docs/019 slice 1c auto-seed, end-to-end on the USER'S REAL SHAPE
    /// (Panel ruling 2 + SCOPE 3): a "Tech" husk over SIX children (app DB id
    /// 25). Proves the whole pipeline the GUI drives — the detector fires, the
    /// canned changeset carries a 7-op diff (6 Moves + 1 Remove), accept-all
    /// applies as ONE journal event (6 roots, no Tech), and ONE ⌘Z restores
    /// Tech with all 6 children reparented (the 1a compound-undo guarantee).
    #[test]
    fn dissolve_tech_seed_accept_and_one_undo_on_six_children() {
        use crate::tree::{dissolve_node_ops, dissolve_tech_target};
        let mut s = Store::open_in_memory().unwrap();
        // the app DB's shape: a "Tech" root carrying the migration's detail
        // words, over six real areas.
        let mut seed = vec![add(
            "tech",
            PartRef::Root,
            "Tech",
            "the codebase — one aspect of the product",
            Lifecycle::Todo,
            vec![],
        )];
        for i in 1..=6 {
            seed.push(add(
                &format!("c{i}"),
                PartRef::Temp("tech".into()),
                &format!("Area {i}"),
                "d",
                Lifecycle::Todo,
                vec![],
            ));
        }
        s.accept_diff("k", &seed).unwrap();
        let parts = s.load_tree("k").unwrap();

        // the detector recognizes the husk (and, per its unit test, only it).
        let tech = dissolve_tech_target(&parts).expect("the migration husk is detected");
        assert_eq!(parts.iter().find(|p| p.id == tech).unwrap().name, "Tech");

        // the auto-seed's store calls: create_changeset + dissolve ops on a
        // linked pending row = a 7-op changeset (6 Moves + 1 Remove).
        let cs = s
            .create_changeset(
                "k",
                "Dissolve Tech — group by product area",
                "promote the areas, drop the husk",
                Some(tech),
                "canned",
            )
            .unwrap();
        let ops = dissolve_node_ops(&parts, tech);
        let pid = s.add_pending_diff("k", "changeset", &ops).unwrap();
        s.link_pending_to_changeset(pid, cs).unwrap();
        let flat = flatten_changeset_ops(&s.changeset_pending(cs).unwrap());
        assert_eq!(flat.len(), 7, "6 Moves + 1 Remove");
        assert_eq!(
            flat.iter()
                .filter(|(op, _)| matches!(op, DiffOp::Move { .. }))
                .count(),
            6
        );
        assert_eq!(
            flat.iter()
                .filter(|(op, _)| matches!(op, DiffOp::Remove { .. }))
                .count(),
            1
        );

        let before = s.load_tree("k").unwrap();
        // accept-all: ONE accept_diff_from on the review lane — the GUI's exact
        // call — then drop the rows and resolve the changeset.
        let kept: Vec<DiffOp> = flat.iter().map(|(op, _)| op.clone()).collect();
        s.accept_diff_from("k", &kept, "human:review", None)
            .unwrap();
        for pd in s.changeset_pending(cs).unwrap() {
            s.drop_pending_diff(pd.id).unwrap();
        }
        s.set_changeset_status(cs, "accepted").unwrap();

        let after = s.load_tree("k").unwrap();
        assert!(!after.iter().any(|p| p.name == "Tech"), "husk removed");
        assert_eq!(
            after.iter().filter(|p| p.parent_id.is_none()).count(),
            6,
            "6 roots, no Tech"
        );
        assert!(
            s.open_changesets("k").is_empty(),
            "resolved — off the open set"
        );

        // ONE ⌘Z reverts the WHOLE restructure: Tech back, all 6 reparented.
        assert!(s.undo_last("k").unwrap());
        let undone = s.load_tree("k").unwrap();
        assert_eq!(
            undone.len(),
            before.len(),
            "one undo restores the whole tree"
        );
        let tech2 = undone
            .iter()
            .find(|p| p.name == "Tech")
            .expect("Tech restored in one step");
        assert_eq!(
            undone
                .iter()
                .filter(|p| p.parent_id == Some(tech2.id))
                .count(),
            6,
            "all 6 children back under Tech"
        );
    }

    /// docs/019: role precedence — dispatch > declared > trail > touch; a
    /// touch accumulates weight without ever rewriting intent.
    #[test]
    fn link_role_precedence_and_touch_accumulation() {
        let mut s = Store::open_in_memory().unwrap();
        s.accept_diff("k", &seed_ops()).unwrap();
        let part = s
            .load_tree("k")
            .unwrap()
            .iter()
            .find(|p| p.name == "Store")
            .unwrap()
            .id;
        // touch first, then the session declares — the row upgrades
        s.record_touch("sess-1", part, "k", 1.0).unwrap();
        s.link_session_part("sess-1", part, "k", "declared")
            .unwrap();
        let role: String = s
            .conn
            .query_row(
                "SELECT role FROM session_part WHERE cli_session_id='sess-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(role, "declared", "declared upgrades an observed touch");
        // a later touch must NOT downgrade the declared row — but still accumulates
        s.record_touch("sess-1", part, "k", 2.0).unwrap();
        let (role, w): (String, f64) = s
            .conn
            .query_row(
                "SELECT role, weight FROM session_part WHERE cli_session_id='sess-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(role, "declared", "observation never rewrites intent");
        assert_eq!(w, 3.0, "weight accumulates across roles");
        let lt: Option<i64> = s
            .conn
            .query_row(
                "SELECT last_touch_secs FROM session_part WHERE cli_session_id='sess-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(lt.is_some(), "recency stamped");
        // dispatch beats declared
        s.link_session_part("sess-1", part, "k", "dispatch")
            .unwrap();
        let role: String = s
            .conn
            .query_row(
                "SELECT role FROM session_part WHERE cli_session_id='sess-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(role, "dispatch");
    }

    #[test]
    fn session_parts_snapshots_role_weight_and_recency() {
        // docs/019 slice 3: the per-frame snapshot the map derives building /
        // chip tiers / drift from — every column the render needs, project-scoped.
        let mut s = Store::open_in_memory().unwrap();
        s.accept_diff("k", &seed_ops()).unwrap();
        let ids: Vec<i64> = s.load_tree("k").unwrap().iter().map(|p| p.id).collect();
        let (a, b) = (ids[0], ids[1]);
        s.link_session_part("sess-1", a, "k", "dispatch").unwrap();
        s.record_touch("sess-1", b, "k", 3.0).unwrap();
        s.link_session_part("sess-2", b, "other", "dispatch")
            .unwrap(); // different project
        let rows = s.session_parts("k");
        assert_eq!(rows.len(), 2, "only project k rows");
        let dispatch = rows.iter().find(|r| r.role == "dispatch").unwrap();
        assert_eq!(dispatch.part_id, a);
        assert_eq!(
            dispatch.weight, 0.0,
            "a pure dispatch link carries no touch weight"
        );
        let touch = rows.iter().find(|r| r.role == "touch").unwrap();
        assert_eq!(touch.part_id, b);
        assert_eq!(touch.weight, 3.0);
        assert!(
            touch.last_touch_secs.is_some(),
            "recency stamped on a touch row"
        );
    }

    #[test]
    fn summaries_append_and_latest_wins() {
        let s = Store::open_in_memory().unwrap();
        s.record_summary(
            "s1",
            "orch",
            1000,
            900,
            500,
            "/t/a.jsonl",
            "fix bug",
            "old headline",
            "",
            "[]",
        )
        .unwrap();
        s.record_summary(
            "s1",
            "orch",
            2000,
            1900,
            700,
            "/t/a.jsonl",
            "fix bug",
            "new headline",
            "review diff",
            "[\"x\"]",
        )
        .unwrap();
        s.record_summary(
            "s2",
            "web",
            1500,
            1400,
            300,
            "/t/b.jsonl",
            "ship page",
            "web headline",
            "",
            "[]",
        )
        .unwrap();
        let latest = s.latest_summaries().unwrap();
        assert_eq!(latest.len(), 2);
        let s1 = latest.iter().find(|r| r.sess == "s1").unwrap();
        assert_eq!(s1.headline, "new headline");
        assert_eq!(s1.thru_at_ms, 1900);
        // history preserved (append, not upsert)
        let n: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM session_summary", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);
        // durable freshness anchor
        s.record_event("s1", "orch", 2_500, "another turn").unwrap();
        let ev = s.latest_event_by_sess().unwrap();
        assert_eq!(ev.iter().find(|(k, _)| k == "s1").unwrap().1, 2_500);
    }

    #[test]
    fn session_events_persist_dedup_and_window() {
        let s = Store::open_in_memory().unwrap();
        s.record_event("sess-1", "orch", 1_000_000, "shipped char-selection")
            .unwrap();
        s.record_event("sess-1", "orch", 2_000_000, "perf audit — 2 fixes")
            .unwrap();
        s.record_event("sess-2", "web", 1_500_000, "migrate-db 0007")
            .unwrap();
        // re-observing the SAME turn (resume/backfill: same sess + at_ms) must NOT dup.
        s.record_event("sess-1", "orch", 1_000_000, "shipped char-selection")
            .unwrap();
        let all = s.events_since(0).unwrap();
        assert_eq!(all.len(), 3, "dedup on (sess, at_ms)");
        assert_eq!(all[0].1, 2_000_000, "newest first");
        // the ~24h "today" window drops older rows.
        let recent = s.events_since(1_500_000).unwrap();
        assert_eq!(recent.len(), 2);
        assert!(recent.iter().all(|(_, at, _)| *at >= 1_500_000));
    }

    #[test]
    fn truth_meter_and_delta_count_inputs() {
        // docs/019 slice 3: the counts + edges that feed the truth meter
        // (project upstream vs downstream) and the Delta turn trigger (per sess).
        let s = Store::open_in_memory().unwrap();
        s.record_event("s1", "orch", 1_000, "t1").unwrap();
        s.record_event("s1", "orch", 2_000, "t2").unwrap();
        s.record_event("s1", "orch", 3_000, "t3").unwrap();
        s.record_event("s2", "orch", 2_500, "t4").unwrap();
        s.record_event("s9", "web", 5_000, "other-project").unwrap();
        // per-session Delta count: events STRICTLY after the last summary ts.
        assert_eq!(s.count_events_since_sess("s1", 0), 3);
        assert_eq!(
            s.count_events_since_sess("s1", 2_000),
            1,
            "strictly-after: only t3"
        );
        assert_eq!(s.count_events_since_sess("s2", 0), 1);
        // project-wide edges + events_behind for the meter.
        assert_eq!(s.latest_event_ms("orch"), 3_000);
        assert_eq!(
            s.latest_summary_ms("orch"),
            0,
            "no summary yet → blind edge"
        );
        assert_eq!(
            s.count_events_since("orch", 0),
            4,
            "s1(3) + s2(1), web excluded"
        );
        s.record_summary("s1", "orch", 2_600, 2_500, 10, "/t/x", "g", "h", "", "[]")
            .unwrap();
        assert_eq!(s.latest_summary_ms("orch"), 2_600);
        // one orch event (t3 @3000) is past the summary — the meter's events_behind.
        assert_eq!(s.count_events_since("orch", 2_600), 1);
    }

    #[test]
    fn hosted_session_crash_recovery_semantics() {
        let s = Store::open_in_memory().unwrap();
        // spawn two sessions (fresh claude + a resumed/imported one) — both recorded.
        s.record_session("uuid-fresh", "proj-a", "claude", "/Users/me/local/a", None)
            .unwrap();
        s.record_session("uuid-import", "proj-b", "codex", "/Users/me/local/b", None)
            .unwrap();
        // user gracefully closes one — it must NOT be offered for restore.
        s.close_session("uuid-fresh").unwrap();

        // simulate a crash + relaunch: the still-alive row is the restore set.
        let restorable = s.restorable_sessions().unwrap();
        assert_eq!(
            restorable.len(),
            1,
            "only the un-closed session is restorable"
        );
        assert_eq!(restorable[0].cli_session_id, "uuid-import");
        assert_eq!(restorable[0].project_key, "proj-b"); // rebinds to its project
        assert_eq!(restorable[0].cwd, "/Users/me/local/b"); // and its recorded cwd

        // resuming an imported session re-records it (UPSERT) → crash-proof AGAIN,
        // exactly like a fresh one. The user's instinct: resumed == crash-proof.
        s.record_session("uuid-import", "proj-b", "codex", "/Users/me/local/b", None)
            .unwrap();
        // after presenting the restore prompt we clear the set so it doesn't re-offer…
        s.clear_restorable().unwrap();
        assert_eq!(s.restorable_sessions().unwrap().len(), 0);
        // …but a session spawned AFTER (this run) is alive again and crash-proof.
        s.record_session("uuid-new", "proj-a", "claude", "/Users/me/local/a", None)
            .unwrap();
        assert_eq!(s.restorable_sessions().unwrap().len(), 1);
        assert_eq!(
            s.restorable_sessions().unwrap()[0].cli_session_id,
            "uuid-new"
        );
    }

    #[test]
    fn override_roundtrip_and_does_not_pollute_restore_set() {
        let s = Store::open_in_memory().unwrap();
        // manual attach: file a (disk-scan) id under a project, before any record.
        s.set_override("uuid-x", "path:/Users/me/a").unwrap();
        assert_eq!(
            s.overrides_map().unwrap().get("uuid-x").unwrap(),
            "path:/Users/me/a"
        );
        // re-file (UPSERT overwrites).
        s.set_override("uuid-x", "path:/Users/me/b").unwrap();
        assert_eq!(
            s.overrides_map().unwrap().get("uuid-x").unwrap(),
            "path:/Users/me/b"
        );

        // INVARIANT: attaching must NOT create a restorable (alive) session row.
        assert_eq!(
            s.restorable_sessions().unwrap().len(),
            0,
            "override must not pollute restore set"
        );
        s.record_session("uuid-real", "path:/Users/me/a", "claude", "/Users/me/a", None)
            .unwrap();
        s.set_override("uuid-other", "path:/Users/me/c").unwrap();
        assert_eq!(
            s.restorable_sessions().unwrap().len(),
            1,
            "only hosted rows are restorable"
        );

        s.clear_override("uuid-x").unwrap();
        assert!(s.overrides_map().unwrap().get("uuid-x").is_none());
    }

    #[test]
    fn seed_then_load_builds_tree_with_anchors() {
        let mut s = Store::open_in_memory().unwrap();
        s.ensure_project("k", "orchestrator").unwrap();
        s.accept_diff("k", &seed_ops()).unwrap();
        let parts = s.load_tree("k").unwrap();
        assert_eq!(parts.len(), 3);
        let tree = build_tree(&parts);
        assert_eq!(tree.len(), 2); // two roots
        let flow = tree.iter().find(|n| n.part.name == "Flow map").unwrap();
        assert_eq!(flow.children.len(), 1);
        assert_eq!(flow.children[0].part.name, "Store");
        let host = tree
            .iter()
            .find(|n| n.part.name == "Terminal host")
            .unwrap();
        assert_eq!(host.part.anchors, vec!["crates/orchestrator-host/**"]);
        assert_eq!(s.seed_state("k"), SeedState::Seeded);
    }

    #[test]
    fn set_status_is_an_assertion_with_source() {
        let mut s = Store::open_in_memory().unwrap();
        s.ensure_project("k", "p").unwrap();
        s.accept_diff("k", &seed_ops()).unwrap();
        let id = s
            .load_tree("k")
            .unwrap()
            .iter()
            .find(|p| p.name == "Store")
            .unwrap()
            .id;
        s.set_status("k", id, Lifecycle::Done).unwrap();
        let p = s
            .load_tree("k")
            .unwrap()
            .into_iter()
            .find(|p| p.id == id)
            .unwrap();
        assert_eq!(p.lifecycle, Lifecycle::Done);
        assert_eq!(p.status_source, StatusSource::User);
    }

    #[test]
    fn stale_lowers_confidence_without_flipping_lifecycle() {
        let mut s = Store::open_in_memory().unwrap();
        s.ensure_project("k", "p").unwrap();
        s.accept_diff(
            "k",
            &[add("a", PartRef::Root, "X", "", Lifecycle::Done, vec![])],
        )
        .unwrap();
        let id = s.load_tree("k").unwrap()[0].id;
        s.set_stale(id, true, Some("anchored code changed"))
            .unwrap();
        let p = s.load_tree("k").unwrap().into_iter().next().unwrap();
        assert_eq!(p.lifecycle, Lifecycle::Done); // unchanged
        assert!(p.stale);
        assert_eq!(p.stale_reason.as_deref(), Some("anchored code changed"));
    }

    #[test]
    fn reconcile_marks_done_stale_when_anchored_code_changed_then_clears_on_reassert() {
        // a real project dir with one anchored file (mtime ≈ now).
        let root = std::env::temp_dir().join(format!("orch-recon-{}-{}", std::process::id(), "a"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/foo.rs"), b"fn main() {}").unwrap();

        let mut s = Store::open_in_memory().unwrap();
        s.ensure_project("k", "p").unwrap();
        s.accept_diff(
            "k",
            &[add(
                "a",
                PartRef::Root,
                "Foo",
                "",
                Lifecycle::Todo,
                vec!["src/foo.rs".into()],
            )],
        )
        .unwrap();
        let id = s.load_tree("k").unwrap()[0].id;
        s.set_status("k", id, Lifecycle::Done).unwrap();
        // force the assertion into the PAST so the file (mtime ≈ now) is newer.
        s.conn
            .execute(
                "UPDATE part SET status_at_secs=1000 WHERE id=?1",
                params![id],
            )
            .unwrap();

        let changed = s.reconcile_staleness("k", &root).unwrap();
        assert_eq!(
            changed, 1,
            "the done part's code changed since → one flip to stale"
        );
        let p = s.load_tree("k").unwrap().into_iter().next().unwrap();
        assert!(p.stale);
        assert_eq!(
            p.lifecycle,
            Lifecycle::Done,
            "staleness never flips the lifecycle"
        );
        assert_eq!(
            p.stale_reason.as_deref(),
            Some("anchored code changed since you marked it done")
        );

        // re-assert done NOW (status_at ≈ now ≥ file mtime, and stale auto-cleared) → no longer stale.
        s.set_status("k", id, Lifecycle::Done).unwrap();
        s.reconcile_staleness("k", &root).unwrap();
        assert!(!s.load_tree("k").unwrap().into_iter().next().unwrap().stale);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn undo_reverts_the_last_accept() {
        let mut s = Store::open_in_memory().unwrap();
        s.ensure_project("k", "p").unwrap();
        s.accept_diff("k", &seed_ops()).unwrap();
        let before = s.load_tree("k").unwrap().len();
        let id = s.load_tree("k").unwrap()[0].id;
        s.accept_diff(
            "k",
            &[DiffOp::SetStatus {
                id,
                lifecycle: Lifecycle::Building,
                source: StatusSource::User,
            }],
        )
        .unwrap();
        // undo the status change
        assert!(s.undo_last("k").unwrap());
        let p = s
            .load_tree("k")
            .unwrap()
            .into_iter()
            .find(|p| p.id == id)
            .unwrap();
        assert_eq!(p.lifecycle, Lifecycle::Done); // reverted to seed value
                                                  // undo the seed itself
        assert!(s.undo_last("k").unwrap());
        assert_eq!(s.load_tree("k").unwrap().len(), before - 3);
    }

    #[test]
    fn pending_diffs_roundtrip() {
        let s = Store::open_in_memory().unwrap();
        s.ensure_project("k", "p").unwrap();
        let pid = s.add_pending_diff("k", "seed", &seed_ops()).unwrap();
        let pend = s.pending_diffs("k").unwrap();
        assert_eq!(pend.len(), 1);
        assert_eq!(pend[0].ops.len(), 3);
        // the no-evidence path loads index-aligned all-None (never a shorter vec)
        assert_eq!(pend[0].evidence, vec![None, None, None]);
        s.drop_pending_diff(pid).unwrap();
        assert!(s.pending_diffs("k").unwrap().is_empty());
    }

    #[test]
    fn pending_evidence_roundtrips_and_old_null_rows_load_none_filled() {
        let s = Store::open_in_memory().unwrap();
        s.ensure_project("k", "p").unwrap();
        // evidence roundtrip: quotes stay index-aligned with ops, gaps stay None.
        let ev = vec![
            Some("wired the store column".to_string()),
            None,
            Some("added Store subtree".to_string()),
        ];
        s.add_pending_diff_with_evidence("k", "summary", &seed_ops(), &ev)
            .unwrap();
        let pend = s.pending_diffs("k").unwrap();
        assert_eq!(pend.len(), 1);
        assert_eq!(pend[0].kind, "summary");
        assert_eq!(pend[0].evidence, ev);
        assert_eq!(pend[0].evidence.len(), pend[0].ops.len());
        // old-row compat: a pre-migration row has evidence_json NULL — it must
        // still load, None-filled to ops.len().
        s.conn
            .execute(
                "INSERT INTO pending_diff(project_key,kind,ops_json,created_secs) VALUES('k','seed',?1,0)",
                params![serde_json::to_string(&seed_ops()).unwrap()],
            )
            .unwrap();
        let pend = s.pending_diffs("k").unwrap();
        let old = pend
            .iter()
            .find(|pd| pd.kind == "seed")
            .expect("NULL-evidence row loads");
        assert_eq!(old.ops.len(), 3);
        assert_eq!(old.evidence, vec![None, None, None]);
        // and evidence survives a real reopen (the migration must not clobber it)
        let dir = std::env::temp_dir().join(format!("orch-evidence-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("design.db");
        {
            let s = Store::open(&db).unwrap();
            s.ensure_project("k", "p").unwrap();
            s.add_pending_diff_with_evidence("k", "summary", &seed_ops(), &ev)
                .unwrap();
        }
        let s2 = Store::open(&db).unwrap();
        assert_eq!(s2.pending_diffs("k").unwrap()[0].evidence, ev);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_gen_bumps_on_every_gui_visible_write_and_never_on_reads() {
        let mut s = Store::open_in_memory().unwrap();
        s.ensure_project("k", "p").unwrap();
        let g0 = s.write_gen();
        // accept_diff (an fts_dirty site)
        s.accept_diff("k", &seed_ops()).unwrap();
        let g1 = s.write_gen();
        assert!(g1 > g0, "accept_diff bumps");
        // add_note (an fts_dirty site)
        let part = s.load_tree("k").unwrap()[0].id;
        s.add_note("k", part, "note", "x", "user").unwrap();
        let g2 = s.write_gen();
        assert!(g2 > g1, "add_note bumps");
        // record_summary (an fts_dirty site)
        s.record_summary("s1", "k", 1000, 900, 10, "/t/x.jsonl", "g", "h", "n", "[]")
            .unwrap();
        let g3 = s.write_gen();
        assert!(g3 > g2, "record_summary bumps");
        // pending add/drop (the GUI memoizes pending reads against the gen too)
        let pid = s.add_pending_diff("k", "summary", &seed_ops()).unwrap();
        let g4 = s.write_gen();
        assert!(g4 > g3, "add_pending_diff bumps");
        s.drop_pending_diff(pid).unwrap();
        let g5 = s.write_gen();
        assert!(g5 > g4, "drop_pending_diff bumps");
        // reads never bump — the memoization key must be stable across frames.
        let _ = s.pending_diffs("k").unwrap();
        let _ = s.notes_for_part(part).unwrap();
        let _ = s.search_all("x", 5).unwrap();
        let _ = s.summaries_since("k", 0).unwrap();
        assert_eq!(s.write_gen(), g5, "reads do not bump");
    }

    #[test]
    fn map_proposal_bookkeeping_lives_in_app_settings() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.last_map_proposal_secs("k"), 0, "never proposed => 0");
        s.set_last_map_proposal_secs("k", 1234).unwrap();
        assert_eq!(s.last_map_proposal_secs("k"), 1234);
        s.set_last_map_proposal_secs("k", 5678).unwrap(); // upsert overwrites
        assert_eq!(s.last_map_proposal_secs("k"), 5678);
        assert_eq!(s.last_map_proposal_secs("other"), 0, "per-project keys");
        // stored as an app_settings row — no new table.
        assert_eq!(s.get_setting("map_prop_at:k").as_deref(), Some("5678"));
    }

    #[test]
    fn summaries_since_windows_by_project_strictly_after_ascending() {
        let s = Store::open_in_memory().unwrap();
        s.record_summary(
            "s1",
            "orch",
            1000,
            900,
            10,
            "/t/a.jsonl",
            "g1",
            "h1",
            "n1",
            "[]",
        )
        .unwrap();
        s.record_summary(
            "s1",
            "orch",
            2000,
            1900,
            20,
            "/t/a.jsonl",
            "g2",
            "h2",
            "n2",
            "[]",
        )
        .unwrap();
        s.record_summary(
            "s2",
            "orch",
            3000,
            2900,
            30,
            "/t/b.jsonl",
            "g3",
            "h3",
            "n3",
            "[]",
        )
        .unwrap();
        s.record_summary(
            "s3",
            "web",
            2500,
            2400,
            40,
            "/t/c.jsonl",
            "g4",
            "h4",
            "n4",
            "[]",
        )
        .unwrap();
        // strictly AFTER since_ms (at_ms > since), other projects excluded.
        let rows = s.summaries_since("orch", 1000).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].at_ms, rows[1].at_ms), (2000, 3000), "ascending");
        assert!(rows.iter().all(|r| r.project_key == "orch"));
        assert_eq!(rows[0].headline, "h2");
        // since = newest row => empty (the expected steady-state)
        assert!(s.summaries_since("orch", 3000).unwrap().is_empty());
        // since = 0 => everything for the project
        assert_eq!(s.summaries_since("orch", 0).unwrap().len(), 3);
    }

    #[test]
    fn session_part_link_is_idempotent_and_answers_dispatched_part() {
        let mut s = Store::open_in_memory().unwrap();
        s.accept_diff("k", &seed_ops()).unwrap();
        let part = s
            .load_tree("k")
            .unwrap()
            .iter()
            .find(|p| p.name == "Store")
            .unwrap()
            .id;
        let g0 = s.write_gen();
        s.link_session_part("sess-1", part, "k", "dispatch")
            .unwrap();
        assert!(s.write_gen() > g0, "link bumps the gen (chips re-read)");
        s.link_session_part("sess-1", part, "k", "dispatch")
            .unwrap(); // OR IGNORE
        let n: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM session_part", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "re-linking the same (session,node) is a no-op");
        assert_eq!(s.dispatched_part("sess-1"), Some(part));
        assert_eq!(s.dispatched_part("sess-unknown"), None);
        // a touch-only session was never DISPATCHED anywhere.
        s.link_session_part("sess-2", part, "k", "touch").unwrap();
        assert_eq!(s.dispatched_part("sess-2"), None);
    }

    #[test]
    fn relink_demotes_prior_dispatch_to_trail_and_touch_rows_survive() {
        let mut s = Store::open_in_memory().unwrap();
        s.accept_diff("k", &seed_ops()).unwrap();
        let parts = s.load_tree("k").unwrap();
        let id = |name: &str| parts.iter().find(|p| p.name == name).unwrap().id;
        let (host, flow, store) = (id("Terminal host"), id("Flow map"), id("Store"));
        s.link_session_part("sess-1", host, "k", "dispatch")
            .unwrap();
        s.link_session_part("sess-1", flow, "k", "touch").unwrap();
        let g0 = s.write_gen();
        s.relink_session_part("sess-1", store, "k").unwrap();
        assert!(s.write_gen() > g0, "relink bumps the gen");
        assert_eq!(s.dispatched_part("sess-1"), Some(store));
        let dispatches: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM session_part WHERE cli_session_id='sess-1' AND role='dispatch'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            dispatches, 1,
            "invariant: at most one dispatch row per session"
        );
        // docs/019: the prior dispatch DEMOTES to a trail breadcrumb, never deleted
        let host_rows = s.sessions_for_part(host);
        assert_eq!(host_rows.len(), 1, "history kept");
        assert_eq!(
            (host_rows[0].0.as_str(), host_rows[0].1.as_str()),
            ("sess-1", "trail")
        );
        let flow_rows = s.sessions_for_part(flow);
        assert_eq!(flow_rows.len(), 1, "touch row untouched by relink");
        assert_eq!(
            (flow_rows[0].0.as_str(), flow_rows[0].1.as_str()),
            ("sess-1", "touch")
        );
        // relinking onto the touched node upgrades that row via role precedence.
        s.relink_session_part("sess-1", flow, "k").unwrap();
        assert_eq!(s.dispatched_part("sess-1"), Some(flow));
        let roles: Vec<(i64, String)> = {
            let mut stmt = s.conn.prepare("SELECT part_id, role FROM session_part WHERE cli_session_id='sess-1' ORDER BY part_id").unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect();
            rows
        };
        assert_eq!(
            roles.len(),
            3,
            "host=trail, flow=dispatch, store=trail — nothing deleted"
        );
        assert!(roles.contains(&(host, "trail".into())));
        assert!(roles.contains(&(flow, "dispatch".into())));
        assert!(roles.contains(&(store, "trail".into())));
        // relinking to the CURRENT dispatch node is a no-op, not a demotion
        s.relink_session_part("sess-1", flow, "k").unwrap();
        assert_eq!(s.dispatched_part("sess-1"), Some(flow));
    }

    #[test]
    fn session_dispatch_map_filters_role_and_project() {
        let mut s = Store::open_in_memory().unwrap();
        s.accept_diff("k", &seed_ops()).unwrap();
        s.accept_diff(
            "other",
            &[add(
                "x",
                PartRef::Root,
                "Elsewhere",
                "",
                Lifecycle::Todo,
                vec![],
            )],
        )
        .unwrap();
        let part = s
            .load_tree("k")
            .unwrap()
            .iter()
            .find(|p| p.name == "Store")
            .unwrap()
            .id;
        let elsewhere = s.load_tree("other").unwrap()[0].id;
        s.link_session_part("sess-1", part, "k", "dispatch")
            .unwrap();
        s.link_session_part("sess-2", part, "k", "touch").unwrap(); // never a chip
        s.link_session_part("sess-3", elsewhere, "other", "dispatch")
            .unwrap();
        let map = s.session_dispatch_map("k");
        assert_eq!(map.len(), 1, "touch rows and other projects excluded");
        assert_eq!(map.get("sess-1"), Some(&part));
        assert!(s.session_dispatch_map("empty-project").is_empty());
    }

    #[test]
    fn sessions_for_part_orders_dispatch_first_then_newest() {
        let mut s = Store::open_in_memory().unwrap();
        s.accept_diff("k", &seed_ops()).unwrap();
        let part = s
            .load_tree("k")
            .unwrap()
            .iter()
            .find(|p| p.name == "Store")
            .unwrap()
            .id;
        s.link_session_part("sess-d", part, "k", "dispatch")
            .unwrap();
        s.link_session_part("sess-t1", part, "k", "touch").unwrap();
        s.link_session_part("sess-t2", part, "k", "touch").unwrap();
        // force distinct times with the dispatch row OLDEST — it still sorts first.
        s.conn
            .execute(
                "UPDATE session_part SET at_secs=100 WHERE cli_session_id='sess-d'",
                [],
            )
            .unwrap();
        s.conn
            .execute(
                "UPDATE session_part SET at_secs=200 WHERE cli_session_id='sess-t1'",
                [],
            )
            .unwrap();
        s.conn
            .execute(
                "UPDATE session_part SET at_secs=300 WHERE cli_session_id='sess-t2'",
                [],
            )
            .unwrap();
        let rows = s.sessions_for_part(part);
        let ids: Vec<&str> = rows.iter().map(|(id, _, _)| id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["sess-d", "sess-t2", "sess-t1"],
            "dispatch first, then newest-first"
        );
        assert_eq!(rows[0].1, "dispatch");
        assert_eq!(rows[0].2, 100);
    }

    #[test]
    fn remove_cleans_session_linkage_and_undo_does_not_resurrect_it() {
        let mut s = Store::open_in_memory().unwrap();
        s.accept_diff("k", &seed_ops()).unwrap();
        let id = s
            .load_tree("k")
            .unwrap()
            .iter()
            .find(|p| p.name == "Store")
            .unwrap()
            .id;
        s.link_session_part("sess-1", id, "k", "dispatch").unwrap();
        s.link_session_part("sess-2", id, "k", "touch").unwrap();
        s.accept_diff("k", &[DiffOp::Remove { id }]).unwrap();
        assert!(
            s.sessions_for_part(id).is_empty(),
            "Remove deletes the node's linkage"
        );
        assert_eq!(s.dispatched_part("sess-1"), None);
        let n: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM session_part", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "both roles cleaned");
        // undo re-adds the node under a NEW id — linkage deliberately stays gone.
        assert!(s.undo_last("k").unwrap());
        let readded = s
            .load_tree("k")
            .unwrap()
            .into_iter()
            .find(|p| p.name == "Store")
            .unwrap();
        assert!(s.sessions_for_part(readded.id).is_empty());
        assert_eq!(s.dispatched_part("sess-1"), None);
    }

    #[test]
    fn session_part_persists_across_reopen() {
        let dir = std::env::temp_dir().join(format!("orch-linkage-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("design.db");
        let part = {
            let mut s = Store::open(&db).unwrap();
            s.ensure_project("k", "p").unwrap();
            s.accept_diff("k", &seed_ops()).unwrap();
            let part = s
                .load_tree("k")
                .unwrap()
                .iter()
                .find(|p| p.name == "Store")
                .unwrap()
                .id;
            s.link_session_part("sess-1", part, "k", "dispatch")
                .unwrap();
            part
        };
        // reopen re-runs migrate() — idempotent, and the linkage survives.
        let s2 = Store::open(&db).unwrap();
        assert_eq!(s2.dispatched_part("sess-1"), Some(part));
        assert_eq!(s2.session_dispatch_map("k").get("sess-1"), Some(&part));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persists_across_reopen() {
        let dir = std::env::temp_dir().join(format!("orch-store-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("design.db");
        {
            let mut s = Store::open(&db).unwrap();
            s.ensure_project("k", "p").unwrap();
            s.accept_diff("k", &seed_ops()).unwrap();
        }
        let s2 = Store::open(&db).unwrap();
        assert_eq!(s2.load_tree("k").unwrap().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn part_activity_maxes_across_sessions_any_role_and_notes() {
        let mut s = Store::open_in_memory().unwrap();
        s.accept_diff("k", &seed_ops()).unwrap();
        let parts = s.load_tree("k").unwrap();
        let a = parts.iter().find(|p| p.name == "Terminal host").unwrap().id;
        let b = parts.iter().find(|p| p.name == "Flow map").unwrap().id;
        let c = parts.iter().find(|p| p.name == "Store").unwrap().id;
        // raw inserts: the public methods stamp now(), tests need fixed clocks
        let sp = "INSERT INTO session_part(cli_session_id,part_id,project_key,role,at_secs) VALUES(?1,?2,?3,?4,?5)";
        let pn = "INSERT INTO part_note(part_id,project_key,ts_secs,kind,text,source) VALUES(?1,?2,?3,'note','x','user')";
        s.conn
            .execute(sp, params!["s1", a, "k", "dispatch", 100])
            .unwrap();
        s.conn
            .execute(sp, params!["s2", a, "k", "touch", 500])
            .unwrap();
        s.conn.execute(pn, params![a, "k", 300]).unwrap();
        s.conn.execute(pn, params![b, "k", 800]).unwrap();
        s.conn
            .execute(sp, params!["s3", c, "other", "dispatch", 900])
            .unwrap();
        s.conn.execute(pn, params![c, "other", 900]).unwrap();
        let act = s.part_activity("k");
        assert_eq!(
            act.get(&a),
            Some(&500),
            "max across both tables; a touch counts"
        );
        assert_eq!(act.get(&b), Some(&800), "note-only activity counts");
        assert_eq!(act.get(&c), None, "other-project rows don't leak");
        assert_eq!(act.len(), 2, "never-active parts are absent");
    }

    #[test]
    fn frame_migration_nulls_child_pins_once_and_never_refires() {
        let dir = std::env::temp_dir().join(format!("orch-frame-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("design.db");
        let (root, child) = {
            let mut s = Store::open(&db).unwrap();
            s.accept_diff("k", &seed_ops()).unwrap();
            let parts = s.load_tree("k").unwrap();
            let root = parts.iter().find(|p| p.name == "Flow map").unwrap().id;
            let child = parts.iter().find(|p| p.name == "Store").unwrap().id;
            s.set_part_pos(root, 1.0, 2.0).unwrap();
            s.set_part_pos(child, 3.0, 4.0).unwrap();
            // simulate a pre-v2 store: drop the flag so the next open migrates
            s.conn
                .execute("DELETE FROM app_settings WHERE key='map_frame_v2'", [])
                .unwrap();
            (root, child)
        };
        {
            let s = Store::open(&db).unwrap();
            let parts = s.load_tree("k").unwrap();
            let r = parts.iter().find(|p| p.id == root).unwrap();
            let c = parts.iter().find(|p| p.id == child).unwrap();
            assert_eq!(
                (r.map_x, r.map_y),
                (Some(1.0), Some(2.0)),
                "root pins survive"
            );
            assert_eq!((c.map_x, c.map_y), (None, None), "child pins are NULLed");
            // re-pin the child: the flag is set, so reopening must NOT re-fire
            s.set_part_pos(child, 5.0, 6.0).unwrap();
        }
        let s = Store::open(&db).unwrap();
        let c = s
            .load_tree("k")
            .unwrap()
            .into_iter()
            .find(|p| p.id == child)
            .unwrap();
        assert_eq!(
            (c.map_x, c.map_y),
            (Some(5.0), Some(6.0)),
            "migration is one-time"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // docs/019 C5: the user-set needs-you flag round-trips its verbatim
    // question + set-time, lists per project, and clears cleanly.
    #[test]
    fn needs_you_flag_set_get_list_clear() {
        let s = Store::open_in_memory().unwrap();
        s.ensure_project("k", "K").unwrap();
        s.set_needs_you("k", 7, "ship the beta?", 1000).unwrap();
        s.set_needs_you("k", 3, "kill the old API?", 500).unwrap();
        assert_eq!(
            s.needs_you_for(7),
            Some(("ship the beta?".to_string(), 1000))
        );
        // list is ordered by set_secs then id (oldest first).
        assert_eq!(
            s.needs_you_flags("k"),
            vec![
                (3, "kill the old API?".to_string(), 500),
                (7, "ship the beta?".to_string(), 1000)
            ]
        );
        // re-setting REPLACES (a re-flag edits the question in place).
        s.set_needs_you("k", 7, "ship the beta NOW?", 2000).unwrap();
        assert_eq!(
            s.needs_you_for(7),
            Some(("ship the beta NOW?".to_string(), 2000))
        );
        s.clear_needs_you(7).unwrap();
        assert_eq!(s.needs_you_for(7), None);
        assert_eq!(
            s.needs_you_flags("k"),
            vec![(3, "kill the old API?".to_string(), 500)]
        );
    }

    // #34 "Open folder…": RE-ATTACH must read a dormant project's STORED name
    // (never rename it to the folder basename). project_name is that reader.
    #[test]
    fn project_name_reads_the_stored_name() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.project_name("path:/x"), None, "no row → None");
        s.ensure_project("path:/x", "Foo the Original").unwrap();
        assert_eq!(s.project_name("path:/x").as_deref(), Some("Foo the Original"));
        // re-attach records the path only — the stored NAME is untouched, so a
        // later read still returns the survivor's name, not the folder basename.
        s.set_project_path("path:/x", "/Users/me/work/foo").unwrap();
        assert_eq!(s.project_name("path:/x").as_deref(), Some("Foo the Original"));
    }

    // docs/019 T7: a triage-sweep SetStatus persists a DISTINCT status_source
    // ("human:triage") so audits can weight it below a deliberate hand-set.
    #[test]
    fn triage_status_source_roundtrips() {
        let mut s = Store::open_in_memory().unwrap();
        s.ensure_project("k", "K").unwrap();
        s.accept_diff(
            "k",
            &[add("a", PartRef::Root, "auth", "", Lifecycle::Todo, vec![])],
        )
        .unwrap();
        let id = s.load_tree("k").unwrap()[0].id;
        s.accept_diff_from(
            "k",
            &[DiffOp::SetStatus {
                id,
                lifecycle: Lifecycle::Done,
                source: StatusSource::Triage,
            }],
            "human:triage",
            None,
        )
        .unwrap();
        let p = s
            .load_tree("k")
            .unwrap()
            .into_iter()
            .find(|p| p.id == id)
            .unwrap();
        assert_eq!(p.lifecycle, Lifecycle::Done);
        assert_eq!(p.status_source, StatusSource::Triage);
    }

    // ---- #29: a project's canonical key moves without orphaning anything ----

    /// Populate every project-keyed surface we can reach, then promote the key.
    /// The assertion is SCHEMA-DRIVEN (it re-reads the live table list), so a
    /// table added later that forgets the migration fails this test instead of
    /// silently losing the user's data.
    #[test]
    fn rename_project_key_moves_every_slug_keyed_row() {
        let mut s = Store::open_in_memory().unwrap();
        let old = "idea:my-app";
        let new = "path:/tmp/projects/my-app";
        s.ensure_project(old, "My App").unwrap();
        // map + journal (part, tree_event) and a note
        s.accept_diff_from(
            old,
            &[add("a", PartRef::Root, "auth", "", Lifecycle::Todo, vec![])],
            "user",
            None,
        )
        .unwrap();
        let part_id = s.load_tree(old).unwrap()[0].id;
        s.add_note(old, part_id, "decision", "sqlite, not postgres", "user")
            .unwrap();
        s.set_needs_you(old, part_id, "which db?", 100).unwrap();
        s.add_pending_diff(old, "seed", &[add("b", PartRef::Root, "ui", "", Lifecycle::Todo, vec![])])
            .unwrap();
        // sessions
        s.record_session("cli-1", old, "claude", "/tmp/projects/my-app", None)
            .unwrap();
        s.set_override("cli-1", old).unwrap();
        s.link_session_part("cli-1", part_id, old, "dispatch").unwrap();
        s.enqueue_summary_job("cli-1", old, "end").unwrap();
        // memory substrate + summaries + events (raw: the engine's own writers
        // take richer types than this test needs — the columns are what matter)
        s.conn.execute(
            "INSERT INTO memory_object(id,project_key,kind,title,body_md,state,confidence,created_by,created_at_secs,updated_at_secs) VALUES('m1',?1,'decision','db','sqlite','active',1.0,'user',1,1)",
            params![old],
        )
        .unwrap();
        s.conn.execute(
            "INSERT INTO memory_edge(id,project_key,src_id,dst_id,kind,confidence,created_at_secs) VALUES('e1',?1,'m1','m1','relates',1.0,1)",
            params![old],
        )
        .unwrap();
        s.conn.execute(
            "INSERT INTO memory_source(id,project_key,kind,uri,captured_at_secs) VALUES('s1',?1,'session','x',1)",
            params![old],
        )
        .unwrap();
        s.conn.execute(
            "INSERT INTO memory_correction(project_key,target_id,action,note,corrected_at_secs) VALUES(?1,'m1','drop','no',1)",
            params![old],
        )
        .unwrap();
        s.conn.execute(
            "INSERT INTO memory_candidate(project_key,candidate_json,created_by,created_secs) VALUES(?1,'{}','user',1)",
            params![old],
        )
        .unwrap();
        s.conn.execute(
            "INSERT INTO session_summary(sess,project_key,at_ms,thru_at_ms,src_bytes,src_path,goal,headline,next_action,detail_json) VALUES('cli-1',?1,1,1,0,'p','g','h','n','[]')",
            params![old],
        )
        .unwrap();
        s.conn.execute(
            "INSERT INTO session_event(sess,project_key,at_ms,summary) VALUES('cli-1',?1,1,'did')",
            params![old],
        )
        .unwrap();
        s.conn.execute(
            "INSERT INTO changeset(project_key,title,created_secs) VALUES(?1,'cs',1)",
            params![old],
        )
        .unwrap();
        // the slug-suffixed settings + the rail order
        s.set_setting(&format!("map_root:{old}"), "7").unwrap();
        s.set_setting(&format!("map_stars:{old}"), "[1,2]").unwrap();
        s.set_setting(&format!("memory_prop_thru:{old}"), "99").unwrap();
        s.set_setting("project_order", &format!("[\"path:/other\",\"{old}\"]"))
            .unwrap();
        s.set_setting("toast_secs", "15").unwrap(); // an unrelated setting: untouched

        // every table that carries a project_key, read from the LIVE schema
        let tables = s.tables_with_project_key().unwrap();
        assert!(tables.len() >= 15, "expected the full keyed-table set, got {tables:?}");
        let count = |s: &Store, t: &str, k: &str| -> i64 {
            s.conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {t} WHERE project_key=?1"),
                    params![k],
                    |r| r.get(0),
                )
                .unwrap()
        };
        let before: Vec<(String, i64)> = tables
            .iter()
            .map(|t| (t.clone(), count(&s, t, old)))
            .collect();
        assert!(
            before.iter().filter(|(_, n)| *n > 0).count() >= 12,
            "test must populate most keyed tables, got {before:?}"
        );

        s.rename_project_key(old, new).unwrap();

        for (t, n) in &before {
            assert_eq!(count(&s, t, old), 0, "{t} still has rows under the OLD key");
            assert_eq!(count(&s, t, new), *n, "{t} lost rows in the migration");
        }
        // the project row itself moved (and nothing lingers under the old key)
        assert!(s.project_exists(new));
        assert!(!s.project_exists(old));
        // the map/memory settings followed, unrelated settings didn't
        assert_eq!(s.get_setting(&format!("map_root:{new}")).as_deref(), Some("7"));
        assert_eq!(s.get_setting(&format!("map_stars:{new}")).as_deref(), Some("[1,2]"));
        assert_eq!(s.get_setting(&format!("memory_prop_thru:{new}")).as_deref(), Some("99"));
        assert_eq!(s.get_setting(&format!("map_root:{old}")), None);
        assert_eq!(s.get_setting("toast_secs").as_deref(), Some("15"));
        // the rail order kept its slot
        assert_eq!(
            s.get_setting("project_order").as_deref(),
            Some(format!("[\"path:/other\",\"{new}\"]").as_str())
        );
        // the map itself is intact under the new key
        let parts = s.load_tree(new).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].name, "auth");
        assert!(s.load_tree(old).unwrap().is_empty());
        assert_eq!(s.notes_for_part(part_id).unwrap().len(), 1);
    }

    #[test]
    fn store_projects_returns_ideas_and_path_backed_projects() {
        let s = Store::open_in_memory().unwrap();
        s.ensure_project("idea:pathless", "Pathless").unwrap();
        s.ensure_project("path:/tmp/projects/owned", "Owned").unwrap();
        s.set_project_path("path:/tmp/projects/owned", "/tmp/projects/owned")
            .unwrap();
        // a scan-discovered project (no path recorded) is NOT injected — the
        // scan finds it from its own sessions.
        s.ensure_project("path:/tmp/projects/scanned", "Scanned").unwrap();

        let rows = s.store_projects().unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows.contains(&("idea:pathless".into(), "Pathless".into(), None)));
        assert!(rows.contains(&(
            "path:/tmp/projects/owned".into(),
            "Owned".into(),
            Some("/tmp/projects/owned".into())
        )));
    }

    #[test]
    fn rename_project_key_refuses_to_merge_into_an_existing_identity() {
        // findings 1/3/4: `UPDATE OR REPLACE project SET key=…` DELETED the row
        // sitting at `new` and re-keyed 18 project_key tables onto it — fusing two
        // maps into one tree, no prompt, no undo. There is no DELETE FROM project
        // anywhere, so a project whose transcripts aged out still owns its key.
        let mut s = Store::open_in_memory().unwrap();
        let old = "idea:epsilon";
        let new = "path:/tmp/projects/epsilon";
        s.ensure_project(old, "Epsilon").unwrap();
        s.ensure_project(new, "The Victim").unwrap();
        s.accept_diff_from(
            new,
            &[add("v", PartRef::Root, "victim-root", "", Lifecycle::Todo, vec![])],
            "user",
            None,
        )
        .unwrap();
        s.accept_diff_from(
            old,
            &[add("m", PartRef::Root, "my-root", "", Lifecycle::Todo, vec![])],
            "user",
            None,
        )
        .unwrap();
        s.set_setting(&format!("map_root:{new}"), "111").unwrap();
        s.set_setting(&format!("map_root:{old}"), "222").unwrap();

        let err = s.rename_project_key(old, new).unwrap_err();
        assert!(
            err.to_string().contains("already belongs to another project"),
            "{err}"
        );

        // NOTHING moved: both projects still exist, each map is its own.
        assert!(s.project_exists(old) && s.project_exists(new));
        let victim_tree = s.load_tree(new).unwrap();
        assert_eq!(victim_tree.len(), 1, "the victim's map was FUSED: {victim_tree:?}");
        assert_eq!(victim_tree[0].name, "victim-root");
        let my_tree = s.load_tree(old).unwrap();
        assert_eq!(my_tree.len(), 1);
        assert_eq!(my_tree[0].name, "my-root");
        assert_eq!(s.get_setting(&format!("map_root:{new}")).as_deref(), Some("111"));
        assert_eq!(s.get_setting(&format!("map_root:{old}")).as_deref(), Some("222"));
    }

    #[test]
    fn promotion_leaves_exactly_one_store_row() {
        // the anti-duplicate invariant at the STORE layer: after promotion the
        // idea key is GONE, so the next scan injects exactly one source for it.
        let mut s = Store::open_in_memory().unwrap();
        s.ensure_project("idea:my-app", "My App").unwrap();
        s.rename_project_key("idea:my-app", "path:/tmp/projects/my-app")
            .unwrap();
        s.set_project_path("path:/tmp/projects/my-app", "/tmp/projects/my-app")
            .unwrap();
        let rows = s.store_projects().unwrap();
        assert_eq!(rows.len(), 1, "an idea + its promotion must never both exist: {rows:?}");
        assert_eq!(rows[0].0, "path:/tmp/projects/my-app");
        assert_eq!(rows[0].1, "My App");
    }

    #[test]
    fn rename_project_key_leaves_a_settings_key_that_merely_ends_in_the_slug() {
        // The slug-suffixed settings sweep matched by UNANCHORED suffix, and
        // project slugs are themselves colon-schemed — so `map_root:idea:api`
        // (project `idea:api`) parsed as prefix `map_root:idea:` + slug `api`
        // and got rewritten when the UNRELATED project keyed `api` was renamed.
        // One project's promotion silently moved another's map root.
        let mut s = Store::open_in_memory().unwrap();
        let old = "api";
        let new = "path:/tmp/projects/api";
        s.ensure_project(old, "Api").unwrap();
        s.ensure_project("idea:api", "A decoy that ENDS in :api").unwrap();
        s.ensure_project("legacy-api", "A decoy that ends in api").unwrap();
        s.set_setting(&format!("map_root:{old}"), "mine").unwrap();
        s.set_setting("map_root:idea:api", "colon decoy").unwrap();
        s.set_setting("map_root:legacy-api", "dash decoy").unwrap();
        s.set_setting("memory_prop_thru:idea:api", "7").unwrap();

        s.rename_project_key(old, new).unwrap();

        // mine moved…
        assert_eq!(s.get_setting(&format!("map_root:{new}")).as_deref(), Some("mine"));
        assert_eq!(s.get_setting(&format!("map_root:{old}")), None);
        // …and NOBODY else's did.
        assert_eq!(
            s.get_setting("map_root:idea:api").as_deref(),
            Some("colon decoy"),
            "a DIFFERENT project's setting was rewritten by the rename"
        );
        assert_eq!(s.get_setting("map_root:legacy-api").as_deref(), Some("dash decoy"));
        assert_eq!(s.get_setting("memory_prop_thru:idea:api").as_deref(), Some("7"));
        // the decoys' own identities are untouched (project_key rows are matched
        // by equality, but assert it so the whole sweep is covered).
        assert!(s.project_exists("idea:api") && s.project_exists("legacy-api"));
    }

    /// How many journal events `key` still owns.
    fn journal_len(s: &Store, key: &str) -> i64 {
        s.conn
            .query_row(
                "SELECT COUNT(*) FROM tree_event WHERE project_key=?1",
                params![key],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn only_part_name(s: &Store, key: &str) -> String {
        let parts = s.load_tree(key).unwrap();
        assert_eq!(parts.len(), 1, "{parts:?}");
        parts[0].name.clone()
    }

    #[test]
    fn undo_rolls_back_the_tree_when_consuming_the_journal_event_fails() {
        // ⌘Z used to be TWO transactions: apply_raw committed the inverse, then
        // a separate DELETE consumed the event. Fail (or crash) in between and
        // the tree and the journal disagree — the event is still the newest, so
        // the SAME undo replays and eats the edit before it. One transaction.
        let mut s = Store::open_in_memory().unwrap();
        s.accept_diff(
            "k",
            &[add("a", PartRef::Root, "auth", "", Lifecycle::Todo, vec![])],
        )
        .unwrap();
        let id = s.load_tree("k").unwrap()[0].id;
        s.accept_diff(
            "k",
            &[DiffOp::Rename {
                id,
                name: "authz".into(),
                detail: String::new(),
            }],
        )
        .unwrap();
        assert_eq!(journal_len(&s, "k"), 2);

        // make consuming the event fail exactly where the old code was exposed:
        // AFTER the inverse has been applied.
        s.conn
            .execute_batch(
                "CREATE TRIGGER undo_journal_boom BEFORE DELETE ON tree_event
                 BEGIN SELECT RAISE(ABORT,'journal delete failed'); END;",
            )
            .unwrap();
        assert!(s.undo_last("k").is_err());
        assert_eq!(
            only_part_name(&s, "k"),
            "authz",
            "the inverse was COMMITTED even though its event survived — replaying \
             that undo would now revert the Add instead"
        );
        assert_eq!(journal_len(&s, "k"), 2);

        // with the journal writable again the pair lands together, and each
        // event is consumed exactly once.
        s.conn.execute_batch("DROP TRIGGER undo_journal_boom;").unwrap();
        assert!(s.undo_last("k").unwrap());
        assert_eq!(only_part_name(&s, "k"), "auth");
        assert_eq!(journal_len(&s, "k"), 1);
        assert!(s.undo_last("k").unwrap());
        assert!(s.load_tree("k").unwrap().is_empty(), "the Add undid, not the rename twice");
        assert_eq!(journal_len(&s, "k"), 0);
        assert!(!s.undo_last("k").unwrap(), "nothing left to undo");
    }

    #[test]
    fn undo_keeps_the_journal_event_when_applying_the_inverse_fails() {
        // the mirror of the above: a half-applied inverse must not consume the
        // event either, or a real edit vanishes with no way back.
        let mut s = Store::open_in_memory().unwrap();
        s.accept_diff(
            "k",
            &[add("a", PartRef::Root, "auth", "", Lifecycle::Todo, vec![])],
        )
        .unwrap();
        let id = s.load_tree("k").unwrap()[0].id;
        s.accept_diff(
            "k",
            &[DiffOp::Rename {
                id,
                name: "authz".into(),
                detail: String::new(),
            }],
        )
        .unwrap();

        // the inverse renames the part back to "auth" — abort exactly that.
        s.conn
            .execute_batch(
                "CREATE TRIGGER undo_apply_boom BEFORE UPDATE ON part WHEN NEW.name='auth'
                 BEGIN SELECT RAISE(ABORT,'apply failed'); END;",
            )
            .unwrap();
        assert!(s.undo_last("k").is_err());
        assert_eq!(only_part_name(&s, "k"), "authz", "tree untouched");
        assert_eq!(journal_len(&s, "k"), 2, "journal untouched");

        s.conn.execute_batch("DROP TRIGGER undo_apply_boom;").unwrap();
        assert!(s.undo_last("k").unwrap());
        assert_eq!(only_part_name(&s, "k"), "auth");
        assert_eq!(journal_len(&s, "k"), 1);
    }
