//! Module: Remote memory feedback and reinforcement.
//! Correctness: Correct when terse user/system feedback becomes structured, scoped reinforcement that can update trust without changing unrelated scopes.
//! Last revised: 2026-05-12
//! Last changed: Implemented Packet H feedback classification and scoped trust ledger.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::remotes::types::{FeedbackType, RemoteDeny, RemotePolicyFact, RemotePolicyKind};

/// Structured interpretation of terse feedback text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedbackSignal {
    pub feedback_type: FeedbackType,
    pub weight: f64,
    pub requires_review: bool,
    pub applicability_correction: Option<String>,
    pub explanation: String,
    pub halt_current_chain: bool,
}

impl FeedbackSignal {
    /// Classify terse natural-language feedback into a deterministic reinforcement signal.
    pub fn classify(text: &str) -> Self {
        let normalized = text.trim().to_lowercase();
        let tokens: Vec<&str> = normalized
            .split(|c: char| !c.is_alphanumeric() && c != '-')
            .filter(|s| !s.is_empty())
            .collect();
        let has = |needle: &str| tokens.contains(&needle);

        if has("stop") {
            return Self {
                feedback_type: FeedbackType::StopSignal,
                weight: -1.0,
                requires_review: true,
                applicability_correction: None,
                explanation: "user stop signal; halt the current remote-memory chain".into(),
                halt_current_chain: true,
            };
        }

        if normalized.contains("wtf") {
            return Self {
                feedback_type: FeedbackType::WrongFact,
                weight: -0.9,
                requires_review: true,
                applicability_correction: None,
                explanation: "strong negative feedback; mark candidate for human review".into(),
                halt_current_chain: false,
            };
        }

        if normalized.contains("mac-only")
            || normalized.contains("mac only")
            || normalized.contains("macos only")
            || normalized.contains("mac os only")
        {
            return Self {
                feedback_type: FeedbackType::WrongScope,
                weight: -0.65,
                requires_review: false,
                applicability_correction: Some("macos".into()),
                explanation: "feedback says the item only applies to macOS scope".into(),
                halt_current_chain: false,
            };
        }

        if has("no") || has("nope") || has("wrong") {
            return Self {
                feedback_type: FeedbackType::Irrelevant,
                weight: -0.35,
                requires_review: false,
                applicability_correction: None,
                explanation: "negative feedback candidate".into(),
                halt_current_chain: false,
            };
        }

        Self {
            feedback_type: FeedbackType::Irrelevant,
            weight: -0.1,
            requires_review: false,
            applicability_correction: None,
            explanation: "unclassified feedback; record as low-weight relevance signal".into(),
            halt_current_chain: false,
        }
    }
}

/// Scope used for reinforcement; score changes do not leak across scopes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TrustKey {
    pub remote_id: Uuid,
    pub source_namespace: String,
    pub scope: String,
}

impl TrustKey {
    pub fn new(
        remote_id: Uuid,
        source_namespace: impl Into<String>,
        scope: impl Into<String>,
    ) -> Self {
        Self {
            remote_id,
            source_namespace: source_namespace.into(),
            scope: scope.into(),
        }
    }
}

/// Reinforcement source with intentionally conservative deltas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reinforcement {
    PolicyChosen,
    UserConfirmed,
    WrongScope,
    StrongNegative,
}

impl Reinforcement {
    pub fn delta(self) -> f64 {
        match self {
            Self::PolicyChosen => 0.05,
            Self::UserConfirmed => 0.25,
            Self::WrongScope => -0.45,
            Self::StrongNegative => -0.60,
        }
    }

    pub fn is_strong_negative(self) -> bool {
        matches!(self, Self::WrongScope | Self::StrongNegative)
    }
}

/// Result of one trust ledger update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrustUpdate {
    pub key: TrustKey,
    pub delta: f64,
    pub score: f64,
    pub strong_negative_count: u32,
    pub derived_not_trusted_for: bool,
}

#[derive(Debug, Clone, Default)]
struct TrustEntry {
    score: f64,
    strong_negative_count: u32,
}

/// In-memory scoped ledger used by policy/import code and MCP handlers before persistence.
#[derive(Debug, Clone, Default)]
pub struct TrustLedger {
    entries: HashMap<TrustKey, TrustEntry>,
}

impl TrustLedger {
    pub fn apply(&mut self, key: &TrustKey, reinforcement: Reinforcement) -> TrustUpdate {
        let entry = self.entries.entry(key.clone()).or_default();
        let delta = reinforcement.delta();
        entry.score = clamp_score(entry.score + delta);
        if reinforcement.is_strong_negative() {
            entry.strong_negative_count = entry.strong_negative_count.saturating_add(1);
        }
        TrustUpdate {
            key: key.clone(),
            delta,
            score: entry.score,
            strong_negative_count: entry.strong_negative_count,
            derived_not_trusted_for: entry.strong_negative_count >= 2 || entry.score <= -1.0,
        }
    }

    pub fn score(&self, key: &TrustKey) -> f64 {
        self.entries
            .get(key)
            .map(|entry| entry.score)
            .unwrap_or(0.0)
    }

    pub fn not_trusted_for_fact(&self, key: &TrustKey) -> Option<RemotePolicyFact> {
        let entry = self.entries.get(key)?;
        if entry.strong_negative_count < 2 && entry.score > -1.0 {
            return None;
        }
        Some(RemotePolicyFact {
            fact_id: Uuid::new_v4(),
            remote_id: key.remote_id,
            kind: RemotePolicyKind::Deny(RemoteDeny {
                namespace: key.source_namespace.clone(),
                deny: format!("not_trusted_for:{}", key.scope),
            }),
            created_at: chrono::Utc::now(),
            expires_at: None,
        })
    }
}

fn clamp_score(value: f64) -> f64 {
    let clamped = value.clamp(-1.0, 1.0);
    (clamped * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remotes::types::FeedbackType;
    use uuid::Uuid;

    #[test]
    fn no_becomes_negative_feedback_candidate() {
        let signal = FeedbackSignal::classify("no");

        assert_eq!(signal.feedback_type, FeedbackType::Irrelevant);
        assert!(signal.weight < 0.0);
        assert!(!signal.requires_review);
        assert!(!signal.halt_current_chain);
    }

    #[test]
    fn stop_becomes_high_weight_stop_signal() {
        let signal = FeedbackSignal::classify("stop");

        assert_eq!(signal.feedback_type, FeedbackType::StopSignal);
        assert!(signal.weight <= -0.9);
        assert!(signal.halt_current_chain);
    }

    #[test]
    fn wtf_is_strong_negative_requiring_review() {
        let signal = FeedbackSignal::classify("WTF");

        assert_eq!(signal.feedback_type, FeedbackType::WrongFact);
        assert!(signal.weight <= -0.8);
        assert!(signal.requires_review);
    }

    #[test]
    fn mac_only_feedback_is_wrong_scope_with_applicability_correction() {
        let signal = FeedbackSignal::classify("that is Mac-only");

        assert_eq!(signal.feedback_type, FeedbackType::WrongScope);
        assert_eq!(signal.applicability_correction.as_deref(), Some("macos"));
        assert!(signal.weight < 0.0);
    }

    #[test]
    fn trust_policy_chosen_item_gets_small_boost() {
        let key = TrustKey::new(Uuid::from_u128(1), "gpu_builds", "linux");
        let mut scores = TrustLedger::default();

        let update = scores.apply(&key, Reinforcement::PolicyChosen);

        assert_eq!(update.delta, 0.05);
        assert_eq!(scores.score(&key), 0.05);
        assert!(!update.derived_not_trusted_for);
    }

    #[test]
    fn user_confirmation_gets_larger_boost_and_repeated_success_accumulates() {
        let key = TrustKey::new(Uuid::from_u128(2), "gpu_builds", "linux");
        let mut scores = TrustLedger::default();

        scores.apply(&key, Reinforcement::UserConfirmed);
        scores.apply(&key, Reinforcement::PolicyChosen);

        assert_eq!(scores.score(&key), 0.30);
    }

    #[test]
    fn wrong_scope_demotes_that_scope_not_global_namespace() {
        let remote = Uuid::from_u128(3);
        let linux = TrustKey::new(remote, "gpu_builds", "linux");
        let mac = TrustKey::new(remote, "gpu_builds", "macos");
        let mut scores = TrustLedger::default();

        scores.apply(&linux, Reinforcement::WrongScope);

        assert!(scores.score(&linux) < 0.0);
        assert_eq!(scores.score(&mac), 0.0);
    }

    #[test]
    fn repeated_strong_negatives_derive_not_trusted_for() {
        let key = TrustKey::new(Uuid::from_u128(4), "deployment_info", "linux");
        let mut scores = TrustLedger::default();

        assert!(
            !scores
                .apply(&key, Reinforcement::StrongNegative)
                .derived_not_trusted_for
        );
        assert!(
            scores
                .apply(&key, Reinforcement::StrongNegative)
                .derived_not_trusted_for
        );

        let fact = scores
            .not_trusted_for_fact(&key)
            .expect("derived policy fact");
        assert_eq!(fact.remote_id, key.remote_id);
    }
}
