//! Corpus-backed fixture runners for deterministic benchmark checks.
//!
//! These helpers intentionally keep benchmark execution independent from a
//! live MCP server. They provide a stable contract that CI can run cheaply,
//! while later adapters can replace [`LexicalFixtureRetriever`] with Ferrosa
//! MCP calls and keep the same fixture/result shapes.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::bright_pro::{
    AgenticFailureMode, AspectTrace, BrightProConfig, BrightProHit, BrightProProtocol,
    BrightProRound, BrightProScore, adaptive_efficiency_reward, aspect_recall_at_k,
    classify_agentic_trace, fixed_round_budget, novelty_alpha_ndcg_at_k,
};

/// One document or memory entry available to a benchmark fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusDocument {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

impl CorpusDocument {
    pub fn new(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            metadata: HashMap::new(),
        }
    }
}

/// Ranked retrieval hit over a fixture corpus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixtureHit {
    pub id: String,
    pub text: String,
    pub score: f64,
    pub rank: usize,
}

impl FixtureHit {
    fn bright_hit(&self) -> BrightProHit {
        BrightProHit::new(self.id.clone())
    }
}

/// Minimal retriever interface used by corpus-backed fixtures.
pub trait FixtureRetriever {
    fn retrieve(&self, query: &str, k: usize) -> Vec<FixtureHit>;
}

/// Deterministic lexical retriever used for CI smoke fixtures and properties.
#[derive(Debug, Clone)]
pub struct LexicalFixtureRetriever {
    documents: Vec<CorpusDocument>,
}

impl LexicalFixtureRetriever {
    pub fn new(documents: Vec<CorpusDocument>) -> Self {
        Self { documents }
    }

    pub fn documents(&self) -> &[CorpusDocument] {
        &self.documents
    }
}

impl FixtureRetriever for LexicalFixtureRetriever {
    fn retrieve(&self, query: &str, k: usize) -> Vec<FixtureHit> {
        let query_terms = tokenize(query);
        let mut hits = self
            .documents
            .iter()
            .filter_map(|doc| {
                let doc_terms = tokenize(&doc.text);
                let score = lexical_score(&query_terms, &doc_terms);
                (score > 0.0).then(|| FixtureHit {
                    id: doc.id.clone(),
                    text: doc.text.clone(),
                    score,
                    rank: 0,
                })
            })
            .collect::<Vec<_>>();

        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        hits.truncate(k);
        for (idx, hit) in hits.iter_mut().enumerate() {
            hit.rank = idx + 1;
        }
        hits
    }
}

fn lexical_score(query_terms: &HashSet<String>, doc_terms: &HashSet<String>) -> f64 {
    if query_terms.is_empty() || doc_terms.is_empty() {
        return 0.0;
    }
    let overlap = query_terms.intersection(doc_terms).count() as f64;
    let coverage = overlap / query_terms.len() as f64;
    let precision = overlap / doc_terms.len() as f64;
    coverage + (0.25 * precision)
}

pub(crate) fn tokenize(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter_map(|token| {
            let token = token.trim().to_lowercase();
            (token.len() >= 2 && !STOPWORDS.contains(&token.as_str())).then_some(token)
        })
        .collect()
}

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "from", "has", "have", "how",
    "in", "into", "is", "it", "of", "on", "or", "the", "to", "was", "were", "what", "when",
    "where", "which", "who", "why", "with",
];

/// BRIGHT-Pro fixture over a local corpus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrightProFixture {
    pub id: String,
    pub query: String,
    #[serde(default)]
    pub config: BrightProConfig,
    #[serde(default)]
    pub corpus: Vec<CorpusDocument>,
}

/// Result of running a BRIGHT-Pro fixture against a retriever.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrightProFixtureResult {
    pub fixture_id: String,
    pub hits: Vec<FixtureHit>,
    pub score: BrightProScore,
}

pub fn run_bright_pro_fixture<R: FixtureRetriever>(
    fixture: &BrightProFixture,
    retriever: &R,
    k: usize,
) -> BrightProFixtureResult {
    let hits = retriever.retrieve(&fixture.query, k);
    let bright_hits = hits.iter().map(FixtureHit::bright_hit).collect::<Vec<_>>();
    let truth = crate::bright_pro::BrightProGroundTruth::from(&fixture.config);
    let cutoff = bright_pro_cutoff(fixture.config.protocol, bright_hits.len());
    let alpha_ndcg = novelty_alpha_ndcg_at_k(&truth, &bright_hits, cutoff, fixture.config.alpha);
    let aspect_recall = aspect_recall_at_k(&truth, &bright_hits, cutoff);
    let unique_doc_ratio = unique_doc_ratio(&bright_hits);
    let rounds = vec![BrightProRound::new(bright_hits.clone())];
    let trace = AspectTrace::new(rounds, aspect_recall, true);
    let failure_mode = if truth.aspects.is_empty() {
        AgenticFailureMode::EvidenceDeprivation
    } else {
        classify_agentic_trace(&truth, &trace)
    };
    let observed_rounds = trace.rounds.len().max(1);
    let aer = (fixture.config.protocol == BrightProProtocol::Adaptive)
        .then(|| adaptive_efficiency_reward(aspect_recall, observed_rounds, fixture.config.gamma));

    BrightProFixtureResult {
        fixture_id: fixture.id.clone(),
        hits,
        score: BrightProScore {
            protocol: fixture.config.protocol,
            alpha_ndcg,
            aspect_recall,
            rounds: observed_rounds,
            unique_doc_ratio,
            aer,
            failure_mode,
        },
    }
}

fn bright_pro_cutoff(protocol: BrightProProtocol, observed_hits: usize) -> usize {
    match protocol {
        BrightProProtocol::FixedOne => {
            fixed_round_budget(crate::bright_pro::FixedRoundProtocol::One)
        }
        BrightProProtocol::FixedTwo => {
            fixed_round_budget(crate::bright_pro::FixedRoundProtocol::Two)
        }
        BrightProProtocol::FixedThree => {
            fixed_round_budget(crate::bright_pro::FixedRoundProtocol::Three)
        }
        BrightProProtocol::Static | BrightProProtocol::Adaptive => observed_hits,
    }
}

fn unique_doc_ratio(hits: &[BrightProHit]) -> f64 {
    if hits.is_empty() {
        return 0.0;
    }
    let unique = hits
        .iter()
        .map(|hit| hit.id.as_str())
        .collect::<HashSet<_>>();
    unique.len() as f64 / hits.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bright_pro::{BrightProProtocol, ReasoningAspect};

    #[test]
    fn lexical_fixture_retriever_ranks_overlapping_corpus_documents() {
        let retriever = LexicalFixtureRetriever::new(vec![
            CorpusDocument::new("doc:noise", "unrelated climate material"),
            CorpusDocument::new("doc:bright", "BRIGHT-Pro covers aspect aware retrieval"),
        ]);

        let hits = retriever.retrieve("aspect retrieval BRIGHT-Pro", 2);

        assert_eq!(hits[0].id, "doc:bright");
        assert_eq!(hits[0].rank, 1);
    }

    #[test]
    fn bright_pro_fixture_scores_aspect_coverage_from_corpus_hits() {
        let fixture = BrightProFixture {
            id: "bp-smoke".into(),
            query: "aspect aware retrieval needs complementary evidence".into(),
            config: BrightProConfig {
                protocol: BrightProProtocol::Static,
                alpha: 0.5,
                gamma: 0.05,
                aspects: vec![
                    ReasoningAspect {
                        id: "aspect".into(),
                        weight: 1.0,
                        evidence_ids: vec!["doc:aspect".into()],
                    },
                    ReasoningAspect {
                        id: "evidence".into(),
                        weight: 1.0,
                        evidence_ids: vec!["doc:evidence".into()],
                    },
                ],
            },
            corpus: vec![
                CorpusDocument::new("doc:aspect", "aspect aware retrieval avoids redundancy"),
                CorpusDocument::new("doc:evidence", "complementary evidence improves reasoning"),
            ],
        };
        let retriever = LexicalFixtureRetriever::new(fixture.corpus.clone());

        let result = run_bright_pro_fixture(&fixture, &retriever, 5);

        assert_eq!(result.score.aspect_recall, 1.0);
        assert!(result.score.alpha_ndcg > 0.9);
    }
}
