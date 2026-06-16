//! RLM + EverMemOS evaluation primitives.
//!
//! The goal of this module is to make context quality measurable before the
//! live MCP/controller integration exists. RLM contributes the controller shape:
//! keep memory external, inspect symbolic candidates, and inject only accepted
//! evidence. EverMemOS contributes the memory lifecycle shape: MemCells,
//! MemScenes, profiles, time-bounded foresight, and sufficiency-checked
//! recollection.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Eval modes for ablations across current fmem behavior and the proposed stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextEvalMode {
    CurrentHooks,
    SearchThreshold,
    RlmController,
    EverMemosLifecycle,
    CombinedRlmEverMemos,
    CombinedRlmEverMemosNativeFts,
}

impl ContextEvalMode {
    pub fn roadmap_suite() -> Vec<Self> {
        vec![
            Self::CurrentHooks,
            Self::SearchThreshold,
            Self::RlmController,
            Self::EverMemosLifecycle,
            Self::CombinedRlmEverMemos,
            Self::CombinedRlmEverMemosNativeFts,
        ]
    }
}

/// Source-aware lane for memories competing for context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLane {
    Episodic,
    Semantic,
    Procedural,
    Corpus,
    Task,
    Profile,
    Foresight,
    BugDecision,
}

impl MemoryLane {
    pub fn default_threshold(self) -> f64 {
        match self {
            Self::Corpus | Self::Procedural | Self::BugDecision => 0.65,
            Self::Semantic | Self::Task | Self::Profile => 0.70,
            Self::Episodic => 0.78,
            Self::Foresight => 0.80,
        }
    }

    pub fn all() -> [Self; 8] {
        [
            Self::Episodic,
            Self::Semantic,
            Self::Procedural,
            Self::Corpus,
            Self::Task,
            Self::Profile,
            Self::Foresight,
            Self::BugDecision,
        ]
    }
}

/// Time-bounded prospective memory from EverMemOS-style MemCells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForesightSignal {
    pub content: String,
    #[serde(default)]
    pub valid_from: Option<DateTime<Utc>>,
    #[serde(default)]
    pub valid_until: Option<DateTime<Utc>>,
}

impl ForesightSignal {
    pub fn is_valid_at(&self, now: DateTime<Utc>) -> bool {
        self.valid_from.is_none_or(|start| start <= now)
            && self.valid_until.is_none_or(|end| now <= end)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryMetadata {
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
}

impl MemoryMetadata {
    pub fn has_provenance(&self) -> bool {
        self.source_id.is_some() || self.session_id.is_some()
    }
}

/// EverMemOS-style atomic unit. This is the target shape for fmem lifecycle evals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemCell {
    pub id: String,
    pub episode: String,
    #[serde(default)]
    pub atomic_facts: Vec<String>,
    #[serde(default)]
    pub foresight: Vec<ForesightSignal>,
    pub metadata: MemoryMetadata,
}

impl MemCell {
    pub fn new(id: impl Into<String>, episode: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            episode: episode.into(),
            atomic_facts: Vec::new(),
            foresight: Vec::new(),
            metadata: MemoryMetadata::default(),
        }
    }

    pub fn with_fact(mut self, fact: impl Into<String>) -> Self {
        self.atomic_facts.push(fact.into());
        self
    }

    pub fn with_foresight(mut self, foresight: ForesightSignal) -> Self {
        self.foresight.push(foresight);
        self
    }

    pub fn with_metadata(mut self, metadata: MemoryMetadata) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Scene-level consolidation over related MemCells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemScene {
    pub id: String,
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub memcell_ids: Vec<String>,
    #[serde(default)]
    pub profile_delta: Option<String>,
}

/// Candidate memory returned by a retrieval source before controller gating.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextCandidate {
    pub id: String,
    pub lane: MemoryLane,
    pub source: String,
    pub score: f64,
    pub estimated_tokens: usize,
    pub content: String,
    #[serde(default)]
    pub has_provenance: bool,
    #[serde(default)]
    pub valid_from: Option<DateTime<Utc>>,
    #[serde(default)]
    pub valid_until: Option<DateTime<Utc>>,
}

impl ContextCandidate {
    pub fn new(
        id: impl Into<String>,
        lane: MemoryLane,
        score: f64,
        content: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            lane,
            source: "unknown".into(),
            score,
            estimated_tokens: 0,
            content: content.into(),
            has_provenance: false,
            valid_from: None,
            valid_until: None,
        }
    }

    pub fn sourced(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    pub fn tokens(mut self, estimated_tokens: usize) -> Self {
        self.estimated_tokens = estimated_tokens;
        self
    }

    pub fn provenance(mut self, has_provenance: bool) -> Self {
        self.has_provenance = has_provenance;
        self
    }

    pub fn validity(
        mut self,
        valid_from: Option<DateTime<Utc>>,
        valid_until: Option<DateTime<Utc>>,
    ) -> Self {
        self.valid_from = valid_from;
        self.valid_until = valid_until;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControllerPolicy {
    pub mode: ContextEvalMode,
    pub lane_thresholds: HashMap<MemoryLane, f64>,
    pub max_accepted: usize,
    pub max_injected_tokens: usize,
    pub require_provenance: bool,
    pub allow_raw_episodic: bool,
}

impl ControllerPolicy {
    pub fn combined_default() -> Self {
        let lane_thresholds = MemoryLane::all()
            .into_iter()
            .map(|lane| (lane, lane.default_threshold()))
            .collect();
        Self {
            mode: ContextEvalMode::CombinedRlmEverMemos,
            lane_thresholds,
            max_accepted: 8,
            max_injected_tokens: 1600,
            require_provenance: true,
            allow_raw_episodic: false,
        }
    }

    pub fn threshold_for(&self, lane: MemoryLane) -> f64 {
        self.lane_thresholds
            .get(&lane)
            .copied()
            .unwrap_or_else(|| lane.default_threshold())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DropReason {
    LowScore,
    MissingProvenance,
    RawEpisodicDisallowed,
    ExpiredForesight,
    TokenBudget,
    Duplicate,
    AcceptedLimit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcceptedEvidence {
    pub id: String,
    pub lane: MemoryLane,
    pub score: f64,
    pub estimated_tokens: usize,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DroppedEvidence {
    pub id: String,
    pub lane: MemoryLane,
    pub score: f64,
    pub estimated_tokens: usize,
    pub reason: DropReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SufficiencyVerdict {
    pub sufficient: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControllerTrace {
    pub mode: ContextEvalMode,
    pub candidates_seen: usize,
    pub accepted: Vec<AcceptedEvidence>,
    pub dropped: Vec<DroppedEvidence>,
    pub sufficiency: SufficiencyVerdict,
}

pub fn evaluate_context_candidates(
    candidates: &[ContextCandidate],
    policy: &ControllerPolicy,
    now: DateTime<Utc>,
) -> ControllerTrace {
    let mut accepted = Vec::new();
    let mut dropped = Vec::new();
    let mut seen_ids = HashSet::new();
    let mut injected_tokens = 0usize;

    for candidate in candidates {
        let drop_reason = classify_drop(candidate, policy, now, &seen_ids, injected_tokens);
        if let Some(reason) = drop_reason {
            dropped.push(DroppedEvidence {
                id: candidate.id.clone(),
                lane: candidate.lane,
                score: candidate.score,
                estimated_tokens: candidate.estimated_tokens,
                reason,
            });
            continue;
        }

        if accepted.len() >= policy.max_accepted {
            dropped.push(DroppedEvidence {
                id: candidate.id.clone(),
                lane: candidate.lane,
                score: candidate.score,
                estimated_tokens: candidate.estimated_tokens,
                reason: DropReason::AcceptedLimit,
            });
            continue;
        }

        if injected_tokens + candidate.estimated_tokens > policy.max_injected_tokens {
            dropped.push(DroppedEvidence {
                id: candidate.id.clone(),
                lane: candidate.lane,
                score: candidate.score,
                estimated_tokens: candidate.estimated_tokens,
                reason: DropReason::TokenBudget,
            });
            continue;
        }

        seen_ids.insert(candidate.id.clone());
        injected_tokens += candidate.estimated_tokens;
        accepted.push(AcceptedEvidence {
            id: candidate.id.clone(),
            lane: candidate.lane,
            score: candidate.score,
            estimated_tokens: candidate.estimated_tokens,
            reason: format!(
                "{:?} score {:.3} met threshold {:.3}",
                candidate.lane,
                candidate.score,
                policy.threshold_for(candidate.lane)
            ),
        });
    }

    let sufficiency = if accepted.is_empty() {
        SufficiencyVerdict {
            sufficient: false,
            reason: "no accepted evidence".into(),
        }
    } else {
        SufficiencyVerdict {
            sufficient: true,
            reason: "accepted evidence is available for answer generation".into(),
        }
    };

    ControllerTrace {
        mode: policy.mode,
        candidates_seen: candidates.len(),
        accepted,
        dropped,
        sufficiency,
    }
}

fn classify_drop(
    candidate: &ContextCandidate,
    policy: &ControllerPolicy,
    now: DateTime<Utc>,
    seen_ids: &HashSet<String>,
    injected_tokens: usize,
) -> Option<DropReason> {
    if seen_ids.contains(&candidate.id) {
        return Some(DropReason::Duplicate);
    }
    if candidate.score < policy.threshold_for(candidate.lane) {
        return Some(DropReason::LowScore);
    }
    if policy.require_provenance && !candidate.has_provenance {
        return Some(DropReason::MissingProvenance);
    }
    if candidate.lane == MemoryLane::Episodic
        && !policy.allow_raw_episodic
        && candidate.source == "raw_context"
    {
        return Some(DropReason::RawEpisodicDisallowed);
    }
    if candidate.lane == MemoryLane::Foresight
        && (candidate.valid_from.is_some_and(|start| start > now)
            || candidate.valid_until.is_some_and(|end| now > end))
    {
        return Some(DropReason::ExpiredForesight);
    }
    if injected_tokens + candidate.estimated_tokens > policy.max_injected_tokens {
        return Some(DropReason::TokenBudget);
    }
    None
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextGroundTruth {
    #[serde(default)]
    pub required_ids: Vec<String>,
    #[serde(default)]
    pub irrelevant_ids: Vec<String>,
    #[serde(default)]
    pub expect_silence: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextQualityMetrics {
    pub surfaced_count: usize,
    pub accepted_count: usize,
    pub dropped_count: usize,
    pub accepted_tokens: usize,
    pub clutter_tokens: usize,
    pub precision: f64,
    pub recall: f64,
    pub clutter_token_ratio: f64,
    pub good_silence: bool,
}

pub fn score_controller_trace(
    trace: &ControllerTrace,
    truth: &ContextGroundTruth,
) -> ContextQualityMetrics {
    let required: HashSet<_> = truth.required_ids.iter().map(String::as_str).collect();
    let irrelevant: HashSet<_> = truth.irrelevant_ids.iter().map(String::as_str).collect();
    let accepted_tokens = trace
        .accepted
        .iter()
        .map(|hit| hit.estimated_tokens)
        .sum::<usize>();
    let clutter_tokens = trace
        .accepted
        .iter()
        .filter(|hit| irrelevant.contains(hit.id.as_str()))
        .map(|hit| hit.estimated_tokens)
        .sum::<usize>();
    let accepted_required = trace
        .accepted
        .iter()
        .filter(|hit| required.contains(hit.id.as_str()))
        .count();

    let precision = if trace.accepted.is_empty() {
        if truth.expect_silence { 1.0 } else { 0.0 }
    } else {
        accepted_required as f64 / trace.accepted.len() as f64
    };
    let recall = if required.is_empty() {
        1.0
    } else {
        accepted_required as f64 / required.len() as f64
    };
    let clutter_token_ratio = if accepted_tokens == 0 {
        0.0
    } else {
        clutter_tokens as f64 / accepted_tokens as f64
    };

    ContextQualityMetrics {
        surfaced_count: trace.candidates_seen,
        accepted_count: trace.accepted.len(),
        dropped_count: trace.dropped.len(),
        accepted_tokens,
        clutter_tokens,
        precision,
        recall,
        clutter_token_ratio,
        good_silence: truth.expect_silence && trace.accepted.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 15, 12, 0, 0).unwrap()
    }

    #[test]
    fn roadmap_suite_covers_current_and_target_ablations() {
        assert_eq!(
            ContextEvalMode::roadmap_suite(),
            vec![
                ContextEvalMode::CurrentHooks,
                ContextEvalMode::SearchThreshold,
                ContextEvalMode::RlmController,
                ContextEvalMode::EverMemosLifecycle,
                ContextEvalMode::CombinedRlmEverMemos,
                ContextEvalMode::CombinedRlmEverMemosNativeFts,
            ]
        );
    }

    #[test]
    fn controller_drops_low_score_and_raw_episodic_context() {
        let policy = ControllerPolicy::combined_default();
        let candidates = vec![
            ContextCandidate::new("raw", MemoryLane::Episodic, 0.95, "user[0] noisy log")
                .sourced("raw_context")
                .provenance(true)
                .tokens(400),
            ContextCandidate::new(
                "corpus",
                MemoryLane::Corpus,
                0.90,
                "curated paper distillation",
            )
            .sourced("document_bm25")
            .provenance(true)
            .tokens(120),
            ContextCandidate::new("weak", MemoryLane::Semantic, 0.20, "barely related")
                .sourced("entity_ann")
                .provenance(true)
                .tokens(80),
        ];

        let trace = evaluate_context_candidates(&candidates, &policy, now());

        assert_eq!(trace.accepted.len(), 1);
        assert_eq!(trace.accepted[0].id, "corpus");
        assert_eq!(
            trace
                .dropped
                .iter()
                .map(|drop| drop.reason)
                .collect::<Vec<_>>(),
            vec![DropReason::RawEpisodicDisallowed, DropReason::LowScore]
        );
    }

    #[test]
    fn controller_requires_provenance_before_context_injection() {
        let policy = ControllerPolicy::combined_default();
        let candidates = vec![
            ContextCandidate::new("orphan", MemoryLane::Procedural, 0.95, "do this procedure")
                .tokens(40),
        ];

        let trace = evaluate_context_candidates(&candidates, &policy, now());

        assert!(trace.accepted.is_empty());
        assert_eq!(trace.dropped[0].reason, DropReason::MissingProvenance);
    }

    #[test]
    fn controller_filters_expired_foresight() {
        let policy = ControllerPolicy::combined_default();
        let expired_until = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let candidates = vec![
            ContextCandidate::new("expired", MemoryLane::Foresight, 0.98, "temporary reminder")
                .sourced("foresight")
                .provenance(true)
                .tokens(30)
                .validity(None, Some(expired_until)),
        ];

        let trace = evaluate_context_candidates(&candidates, &policy, now());

        assert!(trace.accepted.is_empty());
        assert_eq!(trace.dropped[0].reason, DropReason::ExpiredForesight);
    }

    #[test]
    fn metrics_reward_silence_when_all_candidates_are_rejected() {
        let policy = ControllerPolicy::combined_default();
        let candidates = vec![
            ContextCandidate::new("clutter", MemoryLane::Semantic, 0.10, "irrelevant memory")
                .provenance(true)
                .tokens(500),
        ];
        let trace = evaluate_context_candidates(&candidates, &policy, now());
        let metrics = score_controller_trace(
            &trace,
            &ContextGroundTruth {
                expect_silence: true,
                irrelevant_ids: vec!["clutter".into()],
                ..ContextGroundTruth::default()
            },
        );

        assert!(metrics.good_silence);
        assert_eq!(metrics.accepted_tokens, 0);
        assert_eq!(metrics.precision, 1.0);
    }

    #[test]
    fn metrics_penalize_accepted_clutter_tokens() {
        let trace = ControllerTrace {
            mode: ContextEvalMode::CombinedRlmEverMemos,
            candidates_seen: 2,
            accepted: vec![
                AcceptedEvidence {
                    id: "needed".into(),
                    lane: MemoryLane::Corpus,
                    score: 0.9,
                    estimated_tokens: 100,
                    reason: "test".into(),
                },
                AcceptedEvidence {
                    id: "clutter".into(),
                    lane: MemoryLane::Episodic,
                    score: 0.9,
                    estimated_tokens: 300,
                    reason: "test".into(),
                },
            ],
            dropped: Vec::new(),
            sufficiency: SufficiencyVerdict {
                sufficient: true,
                reason: "test".into(),
            },
        };

        let metrics = score_controller_trace(
            &trace,
            &ContextGroundTruth {
                required_ids: vec!["needed".into()],
                irrelevant_ids: vec!["clutter".into()],
                expect_silence: false,
            },
        );

        assert_eq!(metrics.precision, 0.5);
        assert_eq!(metrics.recall, 1.0);
        assert_eq!(metrics.clutter_tokens, 300);
        assert!((metrics.clutter_token_ratio - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn memcell_captures_episode_facts_foresight_and_metadata() {
        let cell = MemCell::new("cell-1", "User finished RLM ingestion")
            .with_fact("RLM distillation is retrievable")
            .with_foresight(ForesightSignal {
                content: "Run combined eval after FTS branch passes CI".into(),
                valid_from: Some(now()),
                valid_until: None,
            })
            .with_metadata(MemoryMetadata {
                source_id: Some("turn-42".into()),
                session_id: Some("session-1".into()),
                created_at: Some(now()),
            });

        assert!(cell.metadata.has_provenance());
        assert_eq!(cell.atomic_facts, vec!["RLM distillation is retrievable"]);
        assert!(cell.foresight[0].is_valid_at(now()));
    }
}
