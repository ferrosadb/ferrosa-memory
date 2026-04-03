//! Prospective memory — "remember to do X when Y happens."
//!
//! Inspired by vestige's intention system and the neuroscience of prospective
//! memory (Brandimonte et al. 1996). Intentions are deferred actions that
//! trigger when a context condition is met.
//!
//! Unlike entities (what you know) or plans (what you're doing), intentions
//! are about what you WILL need to do when certain conditions arise.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// An intention — a deferred action with a trigger condition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intention {
    pub id: Uuid,
    /// Repository path this intention is scoped to (e.g. "/Users/ben/src/ferrosa-memory").
    pub repo: String,
    pub description: String,
    pub trigger: IntentionTrigger,
    pub priority: Priority,
    pub status: IntentionStatus,
    pub created_at: DateTime<Utc>,
    pub triggered_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IntentionTrigger {
    /// Trigger when a topic is mentioned in conversation.
    Topic { keywords: Vec<String> },
    /// Trigger when working in a specific codebase/file pattern.
    FilePattern { pattern: String },
    /// Trigger after a duration from creation.
    Duration { minutes: u32 },
    /// Trigger on any context match (most flexible).
    Context { condition: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Normal,
    High,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum IntentionStatus {
    Pending,
    Triggered,
    Completed,
    Snoozed,
    Cancelled,
}

/// In-memory intention store backed by CQL persistence.
/// Intentions are loaded from CQL on session start and written through on mutation.
pub struct IntentionStore {
    intentions: Vec<Intention>,
}

impl Default for IntentionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl IntentionStore {
    pub fn new() -> Self {
        Self {
            intentions: Vec::new(),
        }
    }

    /// Set a new intention scoped to a repository. Returns the created Intention for persistence.
    pub fn set(
        &mut self,
        repo: &str,
        description: &str,
        trigger: IntentionTrigger,
        priority: Priority,
    ) -> Intention {
        let id = Uuid::new_v4();
        let intention = Intention {
            id,
            repo: repo.to_string(),
            description: description.to_string(),
            trigger,
            priority,
            status: IntentionStatus::Pending,
            created_at: Utc::now(),
            triggered_at: None,
            completed_at: None,
        };
        tracing::info!(%id, repo, description, "intention set");
        self.intentions.push(intention.clone());
        intention
    }

    /// Load intentions from storage (for session restart recovery).
    pub fn load(&mut self, intentions: Vec<Intention>) {
        self.intentions = intentions;
    }

    /// Check which intentions are triggered by the current context.
    /// If `repo` is non-empty, only intentions for that repo are checked.
    pub fn check(&mut self, context: &str, repo: &str) -> Vec<&Intention> {
        let context_lower = context.to_lowercase();
        let mut triggered = Vec::new();

        for intention in &mut self.intentions {
            if intention.status != IntentionStatus::Pending {
                continue;
            }
            // Skip intentions for other repos
            if !repo.is_empty() && intention.repo != repo {
                continue;
            }

            let matches = match &intention.trigger {
                IntentionTrigger::Topic { keywords } => keywords
                    .iter()
                    .any(|k| context_lower.contains(&k.to_lowercase())),
                IntentionTrigger::FilePattern { pattern } => {
                    context_lower.contains(&pattern.to_lowercase())
                }
                IntentionTrigger::Duration { minutes } => {
                    let elapsed = Utc::now() - intention.created_at;
                    elapsed.num_minutes() >= i64::from(*minutes)
                }
                IntentionTrigger::Context { condition } => {
                    // Simple keyword match on the condition string
                    condition
                        .split_whitespace()
                        .any(|word| context_lower.contains(&word.to_lowercase()))
                }
            };

            if matches {
                intention.status = IntentionStatus::Triggered;
                intention.triggered_at = Some(Utc::now());
                tracing::info!(id = %intention.id, description = %intention.description, "intention TRIGGERED");
            }
        }

        // Collect triggered intentions
        for intention in &self.intentions {
            if intention.status == IntentionStatus::Triggered {
                triggered.push(intention);
            }
        }

        triggered
    }

    /// Mark an intention as completed.
    pub fn complete(&mut self, id: Uuid) -> bool {
        if let Some(i) = self.intentions.iter_mut().find(|i| i.id == id) {
            i.status = IntentionStatus::Completed;
            i.completed_at = Some(Utc::now());
            true
        } else {
            false
        }
    }

    /// List all intentions.
    pub fn list(&self) -> &[Intention] {
        &self.intentions
    }

    /// List pending intentions only.
    pub fn pending(&self) -> Vec<&Intention> {
        self.intentions
            .iter()
            .filter(|i| i.status == IntentionStatus::Pending)
            .collect()
    }

    /// Snooze a triggered intention — resets to Pending so it can trigger again later.
    pub fn snooze(&mut self, id: Uuid) -> bool {
        if let Some(i) = self.intentions.iter_mut().find(|i| i.id == id) {
            if i.status == IntentionStatus::Triggered {
                i.status = IntentionStatus::Pending;
                i.triggered_at = None;
                true
            } else {
                false
            }
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_check_topic_intention() {
        let mut store = IntentionStore::new();
        store.set(
            "/test/repo",
            "Review error handling in auth module",
            IntentionTrigger::Topic {
                keywords: vec!["auth".into(), "authentication".into()],
            },
            Priority::High,
        );

        // Should not trigger on unrelated context
        let triggered = store.check("working on the database layer", "/test/repo");
        assert!(triggered.is_empty());

        // Should trigger on matching context
        let triggered = store.check("now looking at the auth middleware", "/test/repo");
        assert_eq!(triggered.len(), 1);
        assert!(triggered[0].description.contains("error handling"));
    }

    #[test]
    fn complete_intention() {
        let mut store = IntentionStore::new();
        let intention = store.set(
            "/test/repo",
            "Add tests",
            IntentionTrigger::Topic {
                keywords: vec!["test".into()],
            },
            Priority::Normal,
        );

        store.check("running tests", "/test/repo");
        assert!(store.complete(intention.id));

        let pending = store.pending();
        assert!(pending.is_empty());
    }

    #[test]
    fn file_pattern_trigger() {
        let mut store = IntentionStore::new();
        store.set(
            "/test/repo",
            "Check for SQL injection",
            IntentionTrigger::FilePattern {
                pattern: "query".into(),
            },
            Priority::Critical,
        );

        let triggered = store.check("editing cql_storage.rs with query methods", "/test/repo");
        assert_eq!(triggered.len(), 1);
    }
}
