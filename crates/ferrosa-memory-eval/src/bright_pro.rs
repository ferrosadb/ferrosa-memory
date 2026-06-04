//! BRIGHT-Pro style metrics for reasoning-intensive retrieval evaluation.
//!
//! The primitives here are pure data/metric helpers so the eval harness can
//! score fmem retrieval traces before wiring live multi-round agents. They
//! model the paper's key ideas: aspect-aware static scoring, fixed-round
//! budgets, adaptive efficiency reward, and agentic failure-mode labels.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// One non-overlapping reasoning aspect for a query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningAspect {
    pub id: String,
    pub weight: f64,
    #[serde(default)]
    pub evidence_ids: Vec<String>,
}

/// BRIGHT-Pro protocol requested by a scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrightProProtocol {
    Static,
    FixedOne,
    FixedTwo,
    FixedThree,
    Adaptive,
}

fn default_protocol() -> BrightProProtocol {
    BrightProProtocol::Static
}

fn default_alpha() -> f64 {
    0.5
}

fn default_gamma() -> f64 {
    0.05
}

/// Scenario-level BRIGHT-Pro configuration and aspect ground truth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrightProConfig {
    #[serde(default = "default_protocol")]
    pub protocol: BrightProProtocol,
    #[serde(default = "default_alpha")]
    pub alpha: f64,
    #[serde(default = "default_gamma")]
    pub gamma: f64,
    #[serde(default)]
    pub aspects: Vec<ReasoningAspect>,
}

impl Default for BrightProConfig {
    fn default() -> Self {
        Self {
            protocol: default_protocol(),
            alpha: default_alpha(),
            gamma: default_gamma(),
            aspects: Vec::new(),
        }
    }
}

/// Aspect-aware ground truth for a BRIGHT-Pro query.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BrightProGroundTruth {
    pub aspects: Vec<ReasoningAspect>,
}

impl From<&BrightProConfig> for BrightProGroundTruth {
    fn from(config: &BrightProConfig) -> Self {
        Self {
            aspects: config.aspects.clone(),
        }
    }
}

impl BrightProGroundTruth {
    pub fn new<I, S>(aspects: I) -> Self
    where
        I: IntoIterator<Item = (S, f64, Vec<S>)>,
        S: Into<String>,
    {
        Self {
            aspects: aspects
                .into_iter()
                .map(|(id, weight, evidence_ids)| ReasoningAspect {
                    id: id.into(),
                    weight,
                    evidence_ids: evidence_ids.into_iter().map(Into::into).collect(),
                })
                .collect(),
        }
    }

    fn total_weight(&self) -> f64 {
        self.aspects.iter().map(|a| a.weight).sum()
    }

    fn evidence_to_aspects(&self) -> HashMap<&str, Vec<&ReasoningAspect>> {
        let mut index: HashMap<&str, Vec<&ReasoningAspect>> = HashMap::new();
        for aspect in &self.aspects {
            for evidence_id in &aspect.evidence_ids {
                index.entry(evidence_id.as_str()).or_default().push(aspect);
            }
        }
        index
    }

    fn aspect_by_id(&self) -> HashMap<&str, &ReasoningAspect> {
        self.aspects.iter().map(|a| (a.id.as_str(), a)).collect()
    }
}

/// One retrieved passage, fold, entity, or fact in rank order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrightProHit {
    pub id: String,
}

impl BrightProHit {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

/// Novelty-penalized alpha-nDCG@k over reasoning aspects.
///
/// Repeated hits for the same aspect are discounted by `(1 - alpha)^seen`.
pub fn novelty_alpha_ndcg_at_k(
    truth: &BrightProGroundTruth,
    hits: &[BrightProHit],
    k: usize,
    alpha: f64,
) -> f64 {
    let cutoff = hits.len().min(k);
    if cutoff == 0 || truth.aspects.is_empty() {
        return if truth.aspects.is_empty() { 1.0 } else { 0.0 };
    }

    let evidence_index = truth.evidence_to_aspects();
    let mut seen_by_aspect: HashMap<&str, usize> = HashMap::new();
    let mut dcg = 0.0;

    for (idx, hit) in hits.iter().take(cutoff).enumerate() {
        let gain = evidence_index
            .get(hit.id.as_str())
            .map(|aspects| {
                aspects
                    .iter()
                    .map(|aspect| {
                        let seen = *seen_by_aspect.get(aspect.id.as_str()).unwrap_or(&0);
                        aspect.weight * (1.0 - alpha).powi(seen as i32)
                    })
                    .sum::<f64>()
            })
            .unwrap_or(0.0);

        if let Some(aspects) = evidence_index.get(hit.id.as_str()) {
            for aspect in aspects {
                *seen_by_aspect.entry(aspect.id.as_str()).or_insert(0) += 1;
            }
        }

        dcg += gain / discount(idx + 1);
    }

    let idcg = ideal_alpha_dcg(truth, cutoff, alpha);
    if idcg == 0.0 { 0.0 } else { dcg / idcg }
}

fn ideal_alpha_dcg(truth: &BrightProGroundTruth, k: usize, alpha: f64) -> f64 {
    let mut seen_by_aspect: HashMap<&str, usize> = HashMap::new();
    let aspect_map = truth.aspect_by_id();
    let mut dcg = 0.0;

    for rank in 1..=k {
        let best = aspect_map
            .values()
            .map(|aspect| {
                let seen = *seen_by_aspect.get(aspect.id.as_str()).unwrap_or(&0);
                (
                    aspect.id.as_str(),
                    aspect.weight * (1.0 - alpha).powi(seen as i32),
                )
            })
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        if let Some((aspect_id, gain)) = best {
            if gain <= 0.0 {
                break;
            }
            *seen_by_aspect.entry(aspect_id).or_insert(0) += 1;
            dcg += gain / discount(rank);
        }
    }

    dcg
}

fn discount(rank: usize) -> f64 {
    ((rank as f64) + 1.0).log2()
}

/// Weighted aspect recall@k: each aspect receives credit once if any of its
/// evidence IDs appears in the top-k hits.
pub fn aspect_recall_at_k(truth: &BrightProGroundTruth, hits: &[BrightProHit], k: usize) -> f64 {
    let total_weight = truth.total_weight();
    if total_weight == 0.0 {
        return 1.0;
    }

    let evidence_index = truth.evidence_to_aspects();
    let covered = covered_aspects(&evidence_index, hits, k);
    let covered_weight: f64 = truth
        .aspects
        .iter()
        .filter(|aspect| covered.contains(aspect.id.as_str()))
        .map(|aspect| aspect.weight)
        .sum();

    covered_weight / total_weight
}

fn covered_aspects<'a>(
    evidence_index: &HashMap<&'a str, Vec<&'a ReasoningAspect>>,
    hits: &[BrightProHit],
    k: usize,
) -> HashSet<&'a str> {
    hits.iter()
        .take(hits.len().min(k))
        .filter_map(|hit| evidence_index.get(hit.id.as_str()))
        .flat_map(|aspects| aspects.iter().map(|aspect| aspect.id.as_str()))
        .collect()
}

/// BRIGHT-Pro fixed-round protocol variants: R in {1, 2, 3}, top-5 per round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixedRoundProtocol {
    One,
    Two,
    Three,
}

impl FixedRoundProtocol {
    pub fn suite() -> Vec<Self> {
        vec![Self::One, Self::Two, Self::Three]
    }

    pub fn rounds(self) -> usize {
        match self {
            Self::One => 1,
            Self::Two => 2,
            Self::Three => 3,
        }
    }
}

pub fn fixed_round_budget(protocol: FixedRoundProtocol) -> usize {
    protocol.rounds() * 5
}

/// Adaptive Efficiency Reward: OQ * e^(-gamma * (R - 1)).
pub fn adaptive_efficiency_reward(overall_quality: f64, rounds: usize, gamma: f64) -> f64 {
    let extra_rounds = rounds.saturating_sub(1) as f64;
    overall_quality * (-(gamma * extra_rounds)).exp()
}

/// One agentic retrieval round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrightProRound {
    #[serde(default)]
    pub hits: Vec<BrightProHit>,
}

impl BrightProRound {
    pub fn new(hits: Vec<BrightProHit>) -> Self {
        Self { hits }
    }
}

/// Trace needed to classify BRIGHT-Pro agentic failure modes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AspectTrace {
    #[serde(default)]
    pub rounds: Vec<BrightProRound>,
    pub overall_quality: f64,
    pub stopped_with_answer: bool,
}

impl AspectTrace {
    pub fn new(
        rounds: Vec<BrightProRound>,
        overall_quality: f64,
        stopped_with_answer: bool,
    ) -> Self {
        Self {
            rounds,
            overall_quality,
            stopped_with_answer,
        }
    }

    pub fn all_hits(&self) -> Vec<BrightProHit> {
        self.rounds
            .iter()
            .flat_map(|round| round.hits.iter().cloned())
            .collect()
    }
}

/// BRIGHT-Pro style agentic failure mode taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgenticFailureMode {
    EarlyRoundEfficiency,
    EvidenceDeprivation,
    RepetitionBias,
    AspectTunnelVision,
    HypothesisHopping,
}

/// BRIGHT-Pro metrics attached to a scenario grade/report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrightProScore {
    pub protocol: BrightProProtocol,
    pub alpha_ndcg: f64,
    pub aspect_recall: f64,
    pub rounds: usize,
    pub unique_doc_ratio: f64,
    #[serde(default)]
    pub aer: Option<f64>,
    pub failure_mode: AgenticFailureMode,
}

pub fn classify_agentic_trace(
    truth: &BrightProGroundTruth,
    trace: &AspectTrace,
) -> AgenticFailureMode {
    let all_hits = trace.all_hits();
    let total_hits = all_hits.len();
    let aspect_recall = aspect_recall_at_k(truth, &all_hits, total_hits);

    if aspect_recall == 0.0 {
        return AgenticFailureMode::EvidenceDeprivation;
    }

    if trace.stopped_with_answer && trace.rounds.len() <= 2 && aspect_recall >= 0.8 {
        return AgenticFailureMode::EarlyRoundEfficiency;
    }

    if total_hits >= 3 {
        let unique_docs = all_hits
            .iter()
            .map(|hit| hit.id.as_str())
            .collect::<HashSet<_>>()
            .len();
        if (unique_docs as f64 / total_hits as f64) <= 0.5 {
            return AgenticFailureMode::RepetitionBias;
        }
    }

    let evidence_index = truth.evidence_to_aspects();
    let covered = covered_aspects(&evidence_index, &all_hits, total_hits);
    if aspect_recall < 1.0 && covered.len() <= 1 {
        return AgenticFailureMode::AspectTunnelVision;
    }

    if let Some(first_round) = trace.rounds.first() {
        let first_recall = aspect_recall_at_k(truth, &first_round.hits, first_round.hits.len());
        if first_recall >= 0.8 && trace.rounds.len() > 1 && !trace.stopped_with_answer {
            return AgenticFailureMode::HypothesisHopping;
        }
    }

    AgenticFailureMode::AspectTunnelVision
}
