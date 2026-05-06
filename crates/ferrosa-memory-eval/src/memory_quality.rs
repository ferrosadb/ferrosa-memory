//! Memory-quality evaluation primitives for fmem harness ablations.
//!
//! This module keeps the long-memory research controls explicit: retrieval
//! baselines, evidence-level grading, chunking sweeps, evidence packets, and
//! packing-position experiments. These are intentionally pure data/metric
//! helpers so scenario runners can adopt them incrementally without requiring a
//! live MCP cluster in unit tests.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// Retrieval mode used for harness ablations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalMode {
    NoMemory,
    RandomRetrieval,
    KeywordOnly,
    AnnOnly,
    ActualHybrid,
    HybridNoGraph,
    HybridWithGraph,
    HybridWithoutTemporal,
    HybridWithTemporal,
    OracleEvidence,
    OracleEvidenceShuffled,
    OracleEvidenceMiddle,
}

impl RetrievalMode {
    /// Baseline suite that separates harness, retrieval, graph, and temporal effects.
    pub fn baseline_suite() -> Vec<Self> {
        vec![
            Self::NoMemory,
            Self::RandomRetrieval,
            Self::KeywordOnly,
            Self::AnnOnly,
            Self::ActualHybrid,
            Self::HybridNoGraph,
            Self::HybridWithGraph,
            Self::HybridWithoutTemporal,
            Self::HybridWithTemporal,
            Self::OracleEvidence,
            Self::OracleEvidenceShuffled,
            Self::OracleEvidenceMiddle,
        ]
    }
}

/// Chunking/storage policy under evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkingPolicy {
    EntityOnly,
    FoldSummaryOnly,
    TurnLevel,
    HierarchicalFold,
    TemporalObservations,
    EvidencePacket,
}

impl ChunkingPolicy {
    /// Default sweep for comparing fmem's implicit chunking choices.
    pub fn sweep_suite() -> Vec<Self> {
        vec![
            Self::EntityOnly,
            Self::FoldSummaryOnly,
            Self::TurnLevel,
            Self::HierarchicalFold,
            Self::TemporalObservations,
            Self::EvidencePacket,
        ]
    }
}

/// Ground-truth evidence IDs for retrieval-first grading.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceGroundTruth {
    #[serde(default)]
    pub required_entities: Vec<String>,
    #[serde(default)]
    pub required_folds: Vec<String>,
    #[serde(default)]
    pub required_facts: Vec<String>,
    #[serde(default)]
    pub required_edges: Vec<String>,
    #[serde(default)]
    pub distractor_entities: Vec<String>,
}

impl EvidenceGroundTruth {
    pub fn required_ids(&self) -> HashSet<&str> {
        self.required_entities
            .iter()
            .chain(self.required_folds.iter())
            .chain(self.required_facts.iter())
            .chain(self.required_edges.iter())
            .map(|s| s.as_str())
            .collect()
    }

    pub fn distractor_ids(&self) -> HashSet<&str> {
        self.distractor_entities
            .iter()
            .map(|s| s.as_str())
            .collect()
    }
}

/// One retrieved evidence item, ordered by retrieval rank.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceHit {
    pub id: String,
}

impl EvidenceHit {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

/// Retrieval-level metrics computed before answer grading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryEvalMetrics {
    pub required_total: usize,
    pub required_hits: usize,
    pub recall_at_k: f64,
    pub precision_at_k: f64,
    pub mrr: f64,
    pub ndcg: f64,
    pub distractor_hits: usize,
}

/// Memory-quality score attached to a scenario grade/report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryQualityScore {
    pub retrieval_mode: RetrievalMode,
    pub chunking_policy: ChunkingPolicy,
    pub metrics: MemoryEvalMetrics,
    pub failure_kind: MemoryFailureKind,
}

/// Evaluate retrieval quality against explicit evidence IDs.
pub fn evaluate_retrieval(
    truth: &EvidenceGroundTruth,
    retrieved: &[EvidenceHit],
    k: usize,
) -> MemoryEvalMetrics {
    let required = truth.required_ids();
    let distractors = truth.distractor_ids();
    let cutoff = retrieved.len().min(k);
    let top_k = &retrieved[..cutoff];

    let mut seen_required = HashSet::new();
    let mut distractor_hits = 0usize;
    let mut reciprocal_rank = 0.0;
    let mut dcg = 0.0;

    for (idx, hit) in top_k.iter().enumerate() {
        let rank = idx + 1;
        if required.contains(hit.id.as_str()) && seen_required.insert(hit.id.as_str()) {
            if reciprocal_rank == 0.0 {
                reciprocal_rank = 1.0 / rank as f64;
            }
            dcg += 1.0 / ((rank as f64) + 1.0).log2();
        }
        if distractors.contains(hit.id.as_str()) {
            distractor_hits += 1;
        }
    }

    let required_total = required.len();
    let required_hits = seen_required.len();
    let recall_at_k = if required_total == 0 {
        1.0
    } else {
        required_hits as f64 / required_total as f64
    };
    let precision_at_k = if cutoff == 0 {
        if required_total == 0 { 1.0 } else { 0.0 }
    } else {
        required_hits as f64 / cutoff as f64
    };

    let ideal_hits = required_total.min(cutoff);
    let idcg: f64 = (1..=ideal_hits)
        .map(|rank| 1.0 / ((rank as f64) + 1.0).log2())
        .sum();
    let ndcg = if idcg == 0.0 { 1.0 } else { dcg / idcg };

    MemoryEvalMetrics {
        required_total,
        required_hits,
        recall_at_k,
        precision_at_k,
        mrr: reciprocal_rank,
        ndcg,
        distractor_hits,
    }
}

/// Current-vs-superseded status for evidence packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupersessionStatus {
    Current,
    Superseded,
    Unknown,
}

impl SupersessionStatus {
    pub fn is_current(self) -> bool {
        matches!(self, Self::Current)
    }
}

/// Provenance attached to an evidence packet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceProvenance {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub fold_id: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// Coherent retrieval bundle used to reduce fragmented-context failures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidencePacket {
    pub primary_memory_id: String,
    #[serde(default)]
    pub source_fold_id: Option<String>,
    #[serde(default)]
    pub temporal_fact_ids: Vec<String>,
    pub supersession_status: SupersessionStatus,
    #[serde(default)]
    pub related_entity_ids: Vec<String>,
    #[serde(default)]
    pub supporting_edge_ids: Vec<String>,
    #[serde(default)]
    pub provenance: EvidenceProvenance,
}

impl EvidencePacket {
    pub fn builder(primary_memory_id: impl Into<String>) -> EvidencePacketBuilder {
        EvidencePacketBuilder {
            packet: EvidencePacket {
                primary_memory_id: primary_memory_id.into(),
                source_fold_id: None,
                temporal_fact_ids: Vec::new(),
                supersession_status: SupersessionStatus::Unknown,
                related_entity_ids: Vec::new(),
                supporting_edge_ids: Vec::new(),
                provenance: EvidenceProvenance::default(),
            },
        }
    }
}

pub struct EvidencePacketBuilder {
    packet: EvidencePacket,
}

impl EvidencePacketBuilder {
    pub fn source_fold(mut self, fold_id: impl Into<String>) -> Self {
        self.packet.source_fold_id = Some(fold_id.into());
        self
    }

    pub fn temporal_fact(mut self, fact_id: impl Into<String>) -> Self {
        self.packet.temporal_fact_ids.push(fact_id.into());
        self
    }

    pub fn supporting_edge(mut self, edge_id: impl Into<String>) -> Self {
        self.packet.supporting_edge_ids.push(edge_id.into());
        self
    }

    pub fn related_entity(mut self, entity_id: impl Into<String>) -> Self {
        self.packet.related_entity_ids.push(entity_id.into());
        self
    }

    pub fn current(mut self, is_current: bool) -> Self {
        self.packet.supersession_status = if is_current {
            SupersessionStatus::Current
        } else {
            SupersessionStatus::Superseded
        };
        self
    }

    pub fn provenance(mut self, session_id: impl Into<String>, fold_id: impl Into<String>) -> Self {
        self.packet.provenance.session_id = Some(session_id.into());
        self.packet.provenance.fold_id = Some(fold_id.into());
        self
    }

    pub fn build(self) -> EvidencePacket {
        self.packet
    }
}

/// Position/policy used for packing a fixed evidence set into model context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidencePosition {
    First,
    Middle,
    Last,
    Shuffled,
    Chronological,
    ReverseChronological,
    GroupedByEntity,
    GroupedBySession,
    SummaryFirst,
}

/// Scores for context-packing variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackingExperiment {
    scores: HashMap<EvidencePosition, f64>,
}

impl PackingExperiment {
    pub fn new(scores: Vec<(EvidencePosition, f64)>) -> Self {
        Self {
            scores: scores.into_iter().collect(),
        }
    }

    pub fn score_for(&self, position: EvidencePosition) -> Option<f64> {
        self.scores.get(&position).copied()
    }

    pub fn best_score(&self) -> f64 {
        self.scores.values().copied().fold(0.0, f64::max)
    }

    pub fn packing_loss_against(&self, position: EvidencePosition) -> Option<f64> {
        self.score_for(position)
            .map(|score| self.best_score() - score)
    }
}

/// Input scores for derived retrieval ablation deltas.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RetrievalRunScores {
    pub random_score: f64,
    pub actual_score: f64,
    pub oracle_score: f64,
    pub hybrid_no_graph_score: f64,
    pub hybrid_with_graph_score: f64,
    pub hybrid_without_temporal_score: f64,
    pub hybrid_with_temporal_score: f64,
}

/// Derived deltas that explain where the benchmark result came from.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RetrievalRunDeltas {
    pub memory_value: f64,
    pub retrieval_gap: f64,
    pub graph_value: f64,
    pub temporal_value: f64,
}

pub fn compare_retrieval_runs(scores: RetrievalRunScores) -> RetrievalRunDeltas {
    RetrievalRunDeltas {
        memory_value: scores.actual_score - scores.random_score,
        retrieval_gap: scores.oracle_score - scores.actual_score,
        graph_value: scores.hybrid_with_graph_score - scores.hybrid_no_graph_score,
        temporal_value: scores.hybrid_with_temporal_score - scores.hybrid_without_temporal_score,
    }
}

/// Failure type for memory-quality triage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryFailureKind {
    ChunkingLoss,
    RetrievalMiss,
    FragmentationOrPackingLoss,
    StaleTemporalFact,
    GeneratorReasoningFailure,
    Passed,
}

impl MemoryFailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChunkingLoss => "chunking_loss",
            Self::RetrievalMiss => "retrieval_miss",
            Self::FragmentationOrPackingLoss => "fragmentation_or_packing_loss",
            Self::StaleTemporalFact => "stale_temporal_fact",
            Self::GeneratorReasoningFailure => "generator_reasoning_failure",
            Self::Passed => "passed",
        }
    }
}

/// Classify a failed scenario using retrieval, actual, oracle, and packing scores.
pub fn classify_failure(
    retrieval: &MemoryEvalMetrics,
    actual_score: f64,
    oracle_score: f64,
    best_packed_score: f64,
    stale_temporal_evidence_present: bool,
) -> MemoryFailureKind {
    if actual_score >= 0.8 {
        if stale_temporal_evidence_present {
            return MemoryFailureKind::StaleTemporalFact;
        }
        return MemoryFailureKind::Passed;
    }

    if retrieval.required_total > 0 && retrieval.required_hits == 0 {
        return MemoryFailureKind::RetrievalMiss;
    }

    if stale_temporal_evidence_present {
        return MemoryFailureKind::StaleTemporalFact;
    }

    if oracle_score >= 0.8 && best_packed_score >= 0.8 && actual_score < 0.8 {
        return MemoryFailureKind::FragmentationOrPackingLoss;
    }

    if oracle_score < 0.8 {
        return MemoryFailureKind::ChunkingLoss;
    }

    MemoryFailureKind::GeneratorReasoningFailure
}
