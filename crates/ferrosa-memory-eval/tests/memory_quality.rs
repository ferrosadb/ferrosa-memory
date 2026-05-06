use ferrosa_memory_eval::memory_quality::{
    ChunkingPolicy, EvidenceGroundTruth, EvidenceHit, EvidencePacket, EvidencePosition,
    MemoryEvalMetrics, PackingExperiment, RetrievalMode, RetrievalRunScores, classify_failure,
    compare_retrieval_runs, evaluate_retrieval,
};

#[test]
fn retrieval_modes_include_oracle_random_and_no_memory_baselines() {
    let modes = RetrievalMode::baseline_suite();

    assert!(modes.contains(&RetrievalMode::NoMemory));
    assert!(modes.contains(&RetrievalMode::RandomRetrieval));
    assert!(modes.contains(&RetrievalMode::ActualHybrid));
    assert!(modes.contains(&RetrievalMode::OracleEvidence));
    assert!(modes.contains(&RetrievalMode::HybridNoGraph));
    assert!(modes.contains(&RetrievalMode::HybridWithGraph));
    assert!(modes.contains(&RetrievalMode::HybridWithTemporal));
}

#[test]
fn evidence_ground_truth_scores_recall_precision_mrr_and_ndcg() {
    let truth = EvidenceGroundTruth {
        required_entities: vec!["entity:a".into(), "entity:b".into()],
        required_folds: vec!["fold:root".into()],
        required_facts: vec![],
        required_edges: vec![],
        distractor_entities: vec!["entity:noise".into()],
    };
    let retrieved = vec![
        EvidenceHit::new("entity:noise"),
        EvidenceHit::new("entity:a"),
        EvidenceHit::new("fold:root"),
    ];

    let metrics = evaluate_retrieval(&truth, &retrieved, 3);

    assert_eq!(metrics.required_total, 3);
    assert_eq!(metrics.required_hits, 2);
    assert!((metrics.recall_at_k - (2.0 / 3.0)).abs() < 1e-10);
    assert!((metrics.precision_at_k - (2.0 / 3.0)).abs() < 1e-10);
    assert!((metrics.mrr - 0.5).abs() < 1e-10);
    assert!(metrics.ndcg > 0.0 && metrics.ndcg < 1.0);
    assert_eq!(metrics.distractor_hits, 1);
}

#[test]
fn chunking_policy_suite_covers_the_five_experiment_families() {
    let policies = ChunkingPolicy::sweep_suite();

    assert!(policies.contains(&ChunkingPolicy::EntityOnly));
    assert!(policies.contains(&ChunkingPolicy::FoldSummaryOnly));
    assert!(policies.contains(&ChunkingPolicy::TurnLevel));
    assert!(policies.contains(&ChunkingPolicy::HierarchicalFold));
    assert!(policies.contains(&ChunkingPolicy::TemporalObservations));
    assert!(policies.contains(&ChunkingPolicy::EvidencePacket));
}

#[test]
fn evidence_packet_reports_current_status_and_provenance() {
    let packet = EvidencePacket::builder("entity:a")
        .source_fold("fold:root")
        .temporal_fact("fact:current")
        .supporting_edge("edge:a->b")
        .related_entity("entity:b")
        .current(true)
        .provenance("session:1", "fold:root")
        .build();

    assert_eq!(packet.primary_memory_id, "entity:a");
    assert_eq!(packet.source_fold_id.as_deref(), Some("fold:root"));
    assert!(packet.supersession_status.is_current());
    assert_eq!(packet.temporal_fact_ids, vec!["fact:current"]);
    assert_eq!(packet.supporting_edge_ids, vec!["edge:a->b"]);
    assert_eq!(packet.provenance.session_id.as_deref(), Some("session:1"));
}

#[test]
fn packing_experiment_estimates_lost_in_the_middle_loss() {
    let experiment = PackingExperiment::new(vec![
        (EvidencePosition::First, 0.92),
        (EvidencePosition::Middle, 0.61),
        (EvidencePosition::Last, 0.81),
    ]);

    assert!((experiment.best_score() - 0.92).abs() < 1e-10);
    assert!((experiment.score_for(EvidencePosition::Middle).unwrap() - 0.61).abs() < 1e-10);
    assert!(
        (experiment
            .packing_loss_against(EvidencePosition::Middle)
            .unwrap()
            - 0.31)
            .abs()
            < 1e-10
    );
}

#[test]
fn compare_retrieval_runs_computes_actionable_deltas() {
    let deltas = compare_retrieval_runs(RetrievalRunScores {
        random_score: 0.20,
        actual_score: 0.62,
        oracle_score: 0.91,
        hybrid_no_graph_score: 0.55,
        hybrid_with_graph_score: 0.70,
        hybrid_without_temporal_score: 0.50,
        hybrid_with_temporal_score: 0.77,
    });

    assert!((deltas.memory_value - 0.42).abs() < 1e-10);
    assert!((deltas.retrieval_gap - 0.29).abs() < 1e-10);
    assert!((deltas.graph_value - 0.15).abs() < 1e-10);
    assert!((deltas.temporal_value - 0.27).abs() < 1e-10);
}

#[test]
fn failure_classifier_distinguishes_retrieval_packing_staleness_and_reasoning() {
    let no_evidence = MemoryEvalMetrics {
        required_total: 2,
        required_hits: 0,
        recall_at_k: 0.0,
        precision_at_k: 0.0,
        mrr: 0.0,
        ndcg: 0.0,
        distractor_hits: 0,
    };
    assert_eq!(
        classify_failure(&no_evidence, 0.2, 0.9, 0.9, false).as_str(),
        "retrieval_miss"
    );

    let retrieved = MemoryEvalMetrics {
        recall_at_k: 1.0,
        required_hits: 2,
        ..no_evidence.clone()
    };
    assert_eq!(
        classify_failure(&retrieved, 0.4, 0.9, 0.9, false).as_str(),
        "fragmentation_or_packing_loss"
    );
    assert_eq!(
        classify_failure(&retrieved, 0.9, 0.9, 0.9, true).as_str(),
        "stale_temporal_fact"
    );
    assert_eq!(
        classify_failure(&retrieved, 0.4, 0.9, 0.3, false).as_str(),
        "generator_reasoning_failure"
    );
}
