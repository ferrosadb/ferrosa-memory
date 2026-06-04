use ferrosa_memory_eval::fixture::{
    CorpusDocument, LexicalFixtureRetriever, run_bright_pro_fixture,
};
use ferrosa_memory_eval::memorybench::{
    MemoryBenchFixture, run_memorybench_fixture, synthetic_memorybench_fixture,
};
use ferrosa_memory_eval::{bright_pro, fixture};
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(FileFailurePersistence::Off)),
        .. ProptestConfig::default()
    })]

    #[test]
    fn generated_synthetic_conversations_are_retrievable(
        topic in "[A-Z][a-z]{2,8}(-[A-Z][a-z]{2,8})?",
        count in 1usize..8,
    ) {
        let fixture = synthetic_memorybench_fixture(&topic, count);
        let retriever = LexicalFixtureRetriever::new(fixture.corpus_documents());

        let result = run_memorybench_fixture(&fixture, &retriever, count.min(5));

        prop_assert_eq!(result.cases.len(), 1);
        prop_assert!(result.mean_recall_at_k > 0.0);
        prop_assert!(result.mean_feedback_gain >= 0.0);
    }

    #[test]
    fn adding_unrelated_synthetic_conversations_does_not_hide_exact_topic_memory(
        topic in "[A-Z][a-z]{2,8}",
        distractor in "[A-Z][a-z]{2,8}",
        distractor_count in 1usize..6,
    ) {
        prop_assume!(topic != distractor);
        let target = synthetic_memorybench_fixture(&topic, 1);
        let distractors = synthetic_memorybench_fixture(&distractor, distractor_count);
        let fixture = MemoryBenchFixture {
            synthetic_conversations: target
                .synthetic_conversations
                .iter()
                .cloned()
                .chain(distractors.synthetic_conversations.iter().cloned())
                .collect(),
            ..target
        };
        let retriever = LexicalFixtureRetriever::new(fixture.corpus_documents());

        let result = run_memorybench_fixture(&fixture, &retriever, 3);

        prop_assert_eq!(result.cases[0].retrieval.required_hits, 1);
    }

    #[test]
    fn bright_pro_fixture_recall_is_monotonic_when_relevant_corpus_is_added(
        topic in "[A-Z][a-z]{2,8}",
    ) {
        let aspect_id = format!("aspect-{topic}");
        let relevant_id = format!("doc:{topic}:relevant");
        let config = bright_pro::BrightProConfig {
            protocol: bright_pro::BrightProProtocol::Static,
            alpha: 0.5,
            gamma: 0.05,
            aspects: vec![bright_pro::ReasoningAspect {
                id: aspect_id,
                weight: 1.0,
                evidence_ids: vec![relevant_id.clone()],
            }],
        };
        let query = format!("{topic} concrete implementation details");
        let without_relevant = fixture::BrightProFixture {
            id: "without".into(),
            query: query.clone(),
            config: config.clone(),
            corpus: vec![CorpusDocument::new("doc:noise", "unrelated background material")],
        };
        let with_relevant = fixture::BrightProFixture {
            id: "with".into(),
            query,
            config,
            corpus: vec![
                CorpusDocument::new("doc:noise", "unrelated background material"),
                CorpusDocument::new(relevant_id, format!("{topic} concrete implementation details")),
            ],
        };

        let before = run_bright_pro_fixture(
            &without_relevant,
            &LexicalFixtureRetriever::new(without_relevant.corpus.clone()),
            5,
        );
        let after = run_bright_pro_fixture(
            &with_relevant,
            &LexicalFixtureRetriever::new(with_relevant.corpus.clone()),
            5,
        );

        prop_assert!(after.score.aspect_recall >= before.score.aspect_recall);
    }
}
