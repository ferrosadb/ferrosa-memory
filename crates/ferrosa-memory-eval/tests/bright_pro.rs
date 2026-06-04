use ferrosa_memory_eval::bright_pro::{
    AgenticFailureMode, AspectTrace, BrightProGroundTruth, BrightProHit, BrightProRound,
    FixedRoundProtocol, adaptive_efficiency_reward, aspect_recall_at_k, classify_agentic_trace,
    fixed_round_budget, novelty_alpha_ndcg_at_k,
};

fn sample_truth() -> BrightProGroundTruth {
    BrightProGroundTruth::new([
        ("cause", 2.0, vec!["doc:cause-1", "doc:cause-2"]),
        ("mitigation", 1.0, vec!["doc:mitigation-1"]),
        ("risk", 1.0, vec!["doc:risk-1"]),
    ])
}

#[test]
fn alpha_ndcg_discounts_repeated_same_aspect_hits() {
    let truth = BrightProGroundTruth::new([
        ("cause", 1.0, vec!["doc:cause-1", "doc:cause-2"]),
        ("mitigation", 1.0, vec!["doc:mitigation-1"]),
        ("risk", 1.0, vec!["doc:risk-1"]),
    ]);
    let diverse = vec![
        BrightProHit::new("doc:cause-1"),
        BrightProHit::new("doc:mitigation-1"),
        BrightProHit::new("doc:risk-1"),
    ];
    let repetitive = vec![
        BrightProHit::new("doc:cause-1"),
        BrightProHit::new("doc:cause-2"),
        BrightProHit::new("doc:mitigation-1"),
    ];

    let diverse_score = novelty_alpha_ndcg_at_k(&truth, &diverse, 3, 0.5);
    let repetitive_score = novelty_alpha_ndcg_at_k(&truth, &repetitive, 3, 0.5);

    assert!(diverse_score > repetitive_score);
    assert!((diverse_score - 1.0).abs() < 1e-10);
}

#[test]
fn aspect_recall_credits_each_reasoning_aspect_once_by_weight() {
    let truth = sample_truth();
    let hits = vec![
        BrightProHit::new("doc:cause-1"),
        BrightProHit::new("doc:cause-2"),
        BrightProHit::new("doc:risk-1"),
    ];

    let recall = aspect_recall_at_k(&truth, &hits, 3);

    assert!((recall - 0.75).abs() < 1e-10);
}

#[test]
fn fixed_round_protocol_maps_rounds_to_top_five_budget() {
    assert_eq!(fixed_round_budget(FixedRoundProtocol::One), 5);
    assert_eq!(fixed_round_budget(FixedRoundProtocol::Two), 10);
    assert_eq!(fixed_round_budget(FixedRoundProtocol::Three), 15);
    assert_eq!(
        FixedRoundProtocol::suite(),
        vec![
            FixedRoundProtocol::One,
            FixedRoundProtocol::Two,
            FixedRoundProtocol::Three,
        ]
    );
}

#[test]
fn adaptive_efficiency_reward_penalizes_extra_rounds() {
    let one_round = adaptive_efficiency_reward(4.5, 1, 0.05);
    let six_rounds = adaptive_efficiency_reward(4.5, 6, 0.05);

    assert!((one_round - 4.5).abs() < 1e-10);
    assert!(six_rounds < one_round);
    assert!((six_rounds - (4.5_f64 * (-0.25_f64).exp())).abs() < 1e-10);
}

#[test]
fn agentic_failure_classifier_labels_bright_pro_trace_patterns() {
    let deprivation = AspectTrace::new(vec![BrightProRound::new(vec![])], 0.2, false);
    assert_eq!(
        classify_agentic_trace(&sample_truth(), &deprivation),
        AgenticFailureMode::EvidenceDeprivation
    );

    let repetition = AspectTrace::new(
        vec![
            BrightProRound::new(vec![BrightProHit::new("doc:cause-1")]),
            BrightProRound::new(vec![BrightProHit::new("doc:cause-1")]),
            BrightProRound::new(vec![BrightProHit::new("doc:cause-1")]),
        ],
        0.4,
        false,
    );
    assert_eq!(
        classify_agentic_trace(&sample_truth(), &repetition),
        AgenticFailureMode::RepetitionBias
    );

    let tunnel = AspectTrace::new(
        vec![
            BrightProRound::new(vec![BrightProHit::new("doc:cause-1")]),
            BrightProRound::new(vec![BrightProHit::new("doc:cause-2")]),
        ],
        0.5,
        false,
    );
    assert_eq!(
        classify_agentic_trace(&sample_truth(), &tunnel),
        AgenticFailureMode::AspectTunnelVision
    );

    let hopping = AspectTrace::new(
        vec![
            BrightProRound::new(vec![
                BrightProHit::new("doc:cause-1"),
                BrightProHit::new("doc:mitigation-1"),
                BrightProHit::new("doc:risk-1"),
            ]),
            BrightProRound::new(vec![BrightProHit::new("doc:noise")]),
            BrightProRound::new(vec![BrightProHit::new("doc:other-noise")]),
        ],
        0.6,
        false,
    );
    assert_eq!(
        classify_agentic_trace(&sample_truth(), &hopping),
        AgenticFailureMode::HypothesisHopping
    );

    let efficient = AspectTrace::new(
        vec![BrightProRound::new(vec![
            BrightProHit::new("doc:cause-1"),
            BrightProHit::new("doc:mitigation-1"),
            BrightProHit::new("doc:risk-1"),
        ])],
        4.3,
        true,
    );
    assert_eq!(
        classify_agentic_trace(&sample_truth(), &efficient),
        AgenticFailureMode::EarlyRoundEfficiency
    );
}
