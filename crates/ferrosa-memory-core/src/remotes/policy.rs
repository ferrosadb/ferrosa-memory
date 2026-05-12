//! Module: Datalog-backed remote policy decisions.
//! Correctness: Correct when remote policy facts derive query/detail/autocommit/activation/consult decisions and explanations preserve the exact grant/deny/trust facts used.
//! Last revised: 2026-05-12
//! Last changed: Implemented Packet C Datalog-backed remote policy evaluation and explanations.

use std::collections::HashSet;

use crate::datalog;
use crate::types::{FactSet, Term};

const POLICY_RULES: &[&str] = &[
    "can_query(Remote, Namespace) :- remote(Remote), grant(Remote, read, Namespace).",
    "can_fetch_detail(Remote, Item) :- grant(Remote, detail_fetch, knowledge), safe_item(Item).",
    "can_autocommit(Remote, Item) :- grant(Remote, autocommit, knowledge), item_namespace(Item, Namespace), trusted_for(Remote, Namespace), safe_item(Item), no_conflict(Item), no_prompt_injection_risk(Item), no_secret_risk(Item).",
    "should_consult(Remote, Query) :- explicit_remote(Query, Remote).",
    "should_consult(Remote, Query) :- local_coverage(Query, low), query_namespace(Query, Namespace), fallback_enabled(Remote, Namespace), trusted_for(Remote, Namespace).",
];

/// Policy action being evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyAction {
    Read,
    DetailFetch,
    Autocommit,
    RequiresActivation,
    ShouldConsult,
}

impl PolicyAction {
    fn as_fact_atom(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::DetailFetch => "detail_fetch",
            Self::Autocommit => "autocommit",
            Self::RequiresActivation => "requires_activation",
            Self::ShouldConsult => "should_consult",
        }
    }
}

/// Local coverage bucket used by consult fallback policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoverageLevel {
    Low,
    Adequate,
}

impl CoverageLevel {
    fn as_fact_atom(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Adequate => "adequate",
        }
    }
}

/// One remote policy fact. These are converted to canonical Datalog facts before evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PolicyFact {
    Remote {
        remote: String,
    },
    TrustedFor {
        remote: String,
        namespace: String,
    },
    NotTrustedFor {
        remote: String,
        namespace: String,
    },
    Grant {
        remote: String,
        action: PolicyAction,
        namespace: String,
    },
    Deny {
        remote: String,
        action: PolicyAction,
        namespace: String,
    },
    FallbackEnabled {
        remote: String,
        namespace: String,
    },
    ExplicitRemote {
        query: String,
        remote: String,
    },
    LocalCoverage {
        query: String,
        level: CoverageLevel,
    },
    QueryNamespace {
        query: String,
        namespace: String,
    },
}

impl PolicyFact {
    pub fn remote(remote: impl Into<String>) -> Self {
        Self::Remote {
            remote: remote.into(),
        }
    }

    pub fn trusted_for(remote: impl Into<String>, namespace: impl Into<String>) -> Self {
        Self::TrustedFor {
            remote: remote.into(),
            namespace: namespace.into(),
        }
    }

    pub fn not_trusted_for(remote: impl Into<String>, namespace: impl Into<String>) -> Self {
        Self::NotTrustedFor {
            remote: remote.into(),
            namespace: namespace.into(),
        }
    }

    pub fn grant(
        remote: impl Into<String>,
        action: PolicyAction,
        namespace: impl Into<String>,
    ) -> Self {
        Self::Grant {
            remote: remote.into(),
            action,
            namespace: namespace.into(),
        }
    }

    pub fn deny(
        remote: impl Into<String>,
        action: PolicyAction,
        namespace: impl Into<String>,
    ) -> Self {
        Self::Deny {
            remote: remote.into(),
            action,
            namespace: namespace.into(),
        }
    }

    pub fn fallback_enabled(remote: impl Into<String>, namespace: impl Into<String>) -> Self {
        Self::FallbackEnabled {
            remote: remote.into(),
            namespace: namespace.into(),
        }
    }

    pub fn explicit_remote(query: impl Into<String>, remote: impl Into<String>) -> Self {
        Self::ExplicitRemote {
            query: query.into(),
            remote: remote.into(),
        }
    }

    pub fn local_coverage(query: impl Into<String>, level: CoverageLevel) -> Self {
        Self::LocalCoverage {
            query: query.into(),
            level,
        }
    }

    pub fn query_namespace(query: impl Into<String>, namespace: impl Into<String>) -> Self {
        Self::QueryNamespace {
            query: query.into(),
            namespace: namespace.into(),
        }
    }

    fn predicate_and_args(&self) -> (&'static str, Vec<String>) {
        match self {
            Self::Remote { remote } => ("remote", vec![remote.clone()]),
            Self::TrustedFor { remote, namespace } => {
                ("trusted_for", vec![remote.clone(), namespace.clone()])
            }
            Self::NotTrustedFor { remote, namespace } => {
                ("not_trusted_for", vec![remote.clone(), namespace.clone()])
            }
            Self::Grant {
                remote,
                action,
                namespace,
            } => (
                "grant",
                vec![
                    remote.clone(),
                    action.as_fact_atom().to_string(),
                    namespace.clone(),
                ],
            ),
            Self::Deny {
                remote,
                action,
                namespace,
            } => (
                "deny",
                vec![
                    remote.clone(),
                    action.as_fact_atom().to_string(),
                    namespace.clone(),
                ],
            ),
            Self::FallbackEnabled { remote, namespace } => {
                ("fallback_enabled", vec![remote.clone(), namespace.clone()])
            }
            Self::ExplicitRemote { query, remote } => {
                ("explicit_remote", vec![query.clone(), remote.clone()])
            }
            Self::LocalCoverage { query, level } => (
                "local_coverage",
                vec![query.clone(), level.as_fact_atom().to_string()],
            ),
            Self::QueryNamespace { query, namespace } => {
                ("query_namespace", vec![query.clone(), namespace.clone()])
            }
        }
    }
}

/// Per-item signals needed by learner-side policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyItem {
    pub item_id: String,
    pub namespace: String,
    pub safe: bool,
    pub conflict: bool,
    pub prompt_injection_risk: bool,
    pub secret_risk: bool,
}

impl PolicyItem {
    pub fn new(item_id: impl Into<String>, namespace: impl Into<String>) -> Self {
        Self {
            item_id: item_id.into(),
            namespace: namespace.into(),
            safe: true,
            conflict: false,
            prompt_injection_risk: false,
            secret_risk: false,
        }
    }

    fn facts(&self) -> Vec<(String, Vec<String>)> {
        let mut facts = vec![
            (
                "item_namespace".to_string(),
                vec![self.item_id.clone(), self.namespace.clone()],
            ),
            (
                if self.safe {
                    "safe_item"
                } else {
                    "unsafe_item"
                }
                .to_string(),
                vec![self.item_id.clone()],
            ),
        ];
        facts.push((
            if self.conflict {
                "conflict"
            } else {
                "no_conflict"
            }
            .to_string(),
            vec![self.item_id.clone()],
        ));
        facts.push((
            if self.prompt_injection_risk {
                "prompt_injection_risk"
            } else {
                "no_prompt_injection_risk"
            }
            .to_string(),
            vec![self.item_id.clone()],
        ));
        facts.push((
            if self.secret_risk {
                "secret_risk"
            } else {
                "no_secret_risk"
            }
            .to_string(),
            vec![self.item_id.clone()],
        ));
        facts
    }
}

/// Machine-readable reason attached to a policy decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyReason {
    pub code: String,
    pub fact: String,
    pub message: String,
}

impl PolicyReason {
    fn new(code: impl Into<String>, fact: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            fact: fact.into(),
            message: message.into(),
        }
    }
}

/// Human and machine-readable policy result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDecision {
    pub allowed: bool,
    pub action: PolicyAction,
    pub reasons: Vec<PolicyReason>,
    pub explanation: String,
}

impl PolicyDecision {
    pub fn has_fact(&self, fact: &str) -> bool {
        self.reasons.iter().any(|reason| reason.fact == fact)
    }
}

/// In-memory policy wrapper that evaluates canonical facts through the shared Datalog engine.
#[derive(Debug, Clone, Default)]
pub struct RemotePolicy {
    facts: Vec<PolicyFact>,
}

impl RemotePolicy {
    pub fn from_facts(facts: impl IntoIterator<Item = PolicyFact>) -> Self {
        Self {
            facts: facts.into_iter().collect(),
        }
    }

    pub fn with_fact(mut self, fact: PolicyFact) -> Self {
        self.facts.push(fact);
        self
    }

    pub fn can_query(&self, remote: &str, namespace: &str) -> PolicyDecision {
        let args = [remote, namespace];
        let mut reasons = Vec::new();
        self.push_deny_reasons(&mut reasons, remote, PolicyAction::Read, &[namespace]);
        if !reasons.is_empty() {
            return self.decision(false, PolicyAction::Read, reasons);
        }

        let allowed = self.derived_contains("can_query", &args, &[]);
        if allowed {
            reasons.push(self.reason_for_fact(
                "grant",
                [remote, PolicyAction::Read.as_fact_atom(), namespace],
                "read grant permits querying this namespace",
            ));
        } else {
            reasons.push(PolicyReason::new(
                "missing_grant",
                format!("grant({remote}, read, {namespace})"),
                "no read grant matched this namespace",
            ));
        }
        self.decision(allowed, PolicyAction::Read, reasons)
    }

    pub fn can_fetch_detail(&self, remote: &str, item: &PolicyItem) -> PolicyDecision {
        let mut reasons = Vec::new();
        self.push_deny_reasons(
            &mut reasons,
            remote,
            PolicyAction::DetailFetch,
            &["knowledge"],
        );
        if !item.safe {
            reasons.push(PolicyReason::new(
                "unsafe_item",
                format!("unsafe_item({})", item.item_id),
                "detail fetch denied for unsafe item",
            ));
        }
        if !reasons.is_empty() {
            return self.decision(false, PolicyAction::DetailFetch, reasons);
        }

        let allowed = self.derived_contains("can_fetch_detail", &[remote, &item.item_id], &[item]);
        if allowed {
            reasons.push(self.reason_for_fact(
                "grant",
                [
                    remote,
                    PolicyAction::DetailFetch.as_fact_atom(),
                    "knowledge",
                ],
                "detail fetch grant permits retrieving safe item details",
            ));
            reasons.push(PolicyReason::new(
                "safe_item",
                format!("safe_item({})", item.item_id),
                "item safety classifier permits detail fetch",
            ));
        }
        self.decision(allowed, PolicyAction::DetailFetch, reasons)
    }

    pub fn can_autocommit(&self, remote: &str, item: &PolicyItem) -> PolicyDecision {
        let mut reasons = Vec::new();
        self.push_deny_reasons(
            &mut reasons,
            remote,
            PolicyAction::Autocommit,
            &["knowledge", &item.namespace],
        );
        if self.has_policy_fact("not_trusted_for", &[remote, &item.namespace]) {
            reasons.push(self.reason_for_fact(
                "not_trusted_for",
                [remote, &item.namespace],
                "remote is explicitly not trusted for this namespace",
            ));
        }
        if !item.safe {
            reasons.push(PolicyReason::new(
                "unsafe_item",
                format!("unsafe_item({})", item.item_id),
                "unsafe items cannot autocommit",
            ));
        }
        if item.conflict {
            reasons.push(PolicyReason::new(
                "conflict",
                format!("conflict({})", item.item_id),
                "conflicting items cannot autocommit",
            ));
        }
        if item.prompt_injection_risk {
            reasons.push(PolicyReason::new(
                "prompt_injection_risk",
                format!("prompt_injection_risk({})", item.item_id),
                "prompt-injection risk blocks autocommit",
            ));
        }
        if item.secret_risk {
            reasons.push(PolicyReason::new(
                "secret_risk",
                format!("secret_risk({})", item.item_id),
                "secret risk blocks autocommit",
            ));
        }
        if !reasons.is_empty() {
            return self.decision(false, PolicyAction::Autocommit, reasons);
        }

        let allowed = self.derived_contains("can_autocommit", &[remote, &item.item_id], &[item]);
        if allowed {
            reasons.extend([
                self.reason_for_fact(
                    "grant",
                    [remote, PolicyAction::Autocommit.as_fact_atom(), "knowledge"],
                    "autocommit grant permits safe knowledge imports",
                ),
                self.reason_for_fact(
                    "trusted_for",
                    [remote, &item.namespace],
                    "remote is trusted for the item namespace",
                ),
                PolicyReason::new(
                    "safe_item",
                    format!("safe_item({})", item.item_id),
                    "item safety classifier permits autocommit",
                ),
                PolicyReason::new(
                    "no_conflict",
                    format!("not conflict({})", item.item_id),
                    "no local or higher-trust conflict was reported",
                ),
            ]);
        } else {
            reasons.push(PolicyReason::new(
                "not_derived",
                format!("can_autocommit({remote}, {})", item.item_id),
                "policy facts did not derive can_autocommit",
            ));
        }
        self.decision(allowed, PolicyAction::Autocommit, reasons)
    }

    /// Separate autocommit policy for skill teaching.
    ///
    /// Skill proposals never reuse the normal `knowledge` autocommit grant. A personal remote
    /// must have `grant(remote, autocommit, skills)`, be `trusted_for(remote, skills)`, and the
    /// skill item must be safe/non-conflicting.
    pub fn can_autocommit_skill(&self, remote: &str, item: &PolicyItem) -> PolicyDecision {
        let mut reasons = Vec::new();
        self.push_deny_reasons(
            &mut reasons,
            remote,
            PolicyAction::Autocommit,
            &["skills", &item.namespace],
        );
        if item.namespace != "skills" {
            reasons.push(PolicyReason::new(
                "wrong_namespace",
                format!("item_namespace({}, {})", item.item_id, item.namespace),
                "skill autocommit only applies to the skills namespace",
            ));
        }
        if !self.has_policy_fact(
            "grant",
            &[remote, PolicyAction::Autocommit.as_fact_atom(), "skills"],
        ) {
            reasons.push(PolicyReason::new(
                "missing_grant",
                format!("grant({remote}, autocommit, skills)"),
                "no explicit skill autocommit grant matched",
            ));
        }
        if !self.has_policy_fact("trusted_for", &[remote, "skills"]) {
            reasons.push(PolicyReason::new(
                "not_trusted_for_skills",
                format!("trusted_for({remote}, skills)"),
                "remote is not trusted for skill teaching",
            ));
        }
        if !item.safe {
            reasons.push(PolicyReason::new(
                "unsafe_item",
                format!("unsafe_item({})", item.item_id),
                "unsafe skills cannot become active candidates",
            ));
        }
        if item.conflict {
            reasons.push(PolicyReason::new(
                "conflict",
                format!("conflict({})", item.item_id),
                "local skill conflicts require review",
            ));
        }
        if item.prompt_injection_risk {
            reasons.push(PolicyReason::new(
                "prompt_injection_risk",
                format!("prompt_injection_risk({})", item.item_id),
                "prompt-injection risk blocks skill autocommit",
            ));
        }
        if item.secret_risk {
            reasons.push(PolicyReason::new(
                "secret_risk",
                format!("secret_risk({})", item.item_id),
                "secret risk blocks skill autocommit",
            ));
        }

        if reasons.is_empty() {
            reasons.extend([
                self.reason_for_fact(
                    "grant",
                    [remote, PolicyAction::Autocommit.as_fact_atom(), "skills"],
                    "explicit skill autocommit grant permits an active candidate",
                ),
                self.reason_for_fact(
                    "trusted_for",
                    [remote, "skills"],
                    "remote is trusted for skill teaching",
                ),
            ]);
            self.decision(true, PolicyAction::Autocommit, reasons)
        } else {
            self.decision(false, PolicyAction::Autocommit, reasons)
        }
    }

    pub fn requires_activation(&self, remote: &str, item: &PolicyItem) -> PolicyDecision {
        let autocommit = self.can_autocommit(remote, item);
        if autocommit.allowed {
            return self.decision(
                false,
                PolicyAction::RequiresActivation,
                vec![PolicyReason::new(
                    "autocommit_allowed",
                    format!("can_autocommit({remote}, {})", item.item_id),
                    "autocommit is allowed, so activation is not required",
                )],
            );
        }
        self.decision(true, PolicyAction::RequiresActivation, autocommit.reasons)
    }

    pub fn should_consult(&self, remote: &str, query: &str) -> PolicyDecision {
        let allowed = self.derived_contains("should_consult", &[remote, query], &[]);
        let mut reasons = Vec::new();
        if allowed {
            if self.has_policy_fact("explicit_remote", &[query, remote]) {
                reasons.push(self.reason_for_fact(
                    "explicit_remote",
                    [query, remote],
                    "query explicitly requested this remote",
                ));
            }
            let fallback_namespace = self.query_namespace(query).filter(|namespace| {
                self.has_policy_fact("local_coverage", &[query, "low"])
                    && self.has_policy_fact("fallback_enabled", &[remote, namespace])
                    && self.has_policy_fact("trusted_for", &[remote, namespace])
            });
            if let Some(namespace) = fallback_namespace {
                reasons.extend([
                    self.reason_for_fact("local_coverage", [query, "low"], "local coverage is low"),
                    self.reason_for_fact(
                        "fallback_enabled",
                        [remote, namespace],
                        "remote fallback is enabled for query namespace",
                    ),
                    self.reason_for_fact(
                        "trusted_for",
                        [remote, namespace],
                        "remote is trusted for fallback namespace",
                    ),
                ]);
            }
        } else {
            reasons.push(PolicyReason::new(
                "not_derived",
                format!("should_consult({remote}, {query})"),
                "policy facts did not derive should_consult",
            ));
        }
        self.decision(allowed, PolicyAction::ShouldConsult, reasons)
    }

    fn derived_contains(&self, predicate: &str, args: &[&str], items: &[&PolicyItem]) -> bool {
        let mut fact_set = self.fact_set();
        for item in items {
            insert_item_facts(&mut fact_set, item);
        }
        let Ok(rules) = POLICY_RULES
            .iter()
            .map(|rule| datalog::parse_rule(rule))
            .collect::<anyhow::Result<Vec<_>>>()
        else {
            return false;
        };
        let (facts, _) = datalog::evaluate(&rules, &fact_set, 16, 1024);
        let terms: Vec<Term> = args
            .iter()
            .map(|arg| Term::ConstStr((*arg).into()))
            .collect();
        facts.contains(predicate, &terms)
    }

    fn fact_set(&self) -> FactSet {
        let mut fact_set = FactSet::new();
        for fact in &self.facts {
            let (predicate, args) = fact.predicate_and_args();
            insert_fact(&mut fact_set, predicate, &args);
        }
        fact_set
    }

    fn has_policy_fact(&self, predicate: &str, args: &[&str]) -> bool {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
        self.facts.iter().any(|fact| {
            let (fact_predicate, fact_args) = fact.predicate_and_args();
            fact_predicate == predicate && fact_args == args
        })
    }

    fn reason_for_fact<const N: usize>(
        &self,
        code: impl Into<String>,
        parts: [&str; N],
        message: impl Into<String>,
    ) -> PolicyReason {
        let code = code.into();
        let fact = format!("{}({})", code, parts.join(", "));
        PolicyReason::new(code, fact, message)
    }

    fn push_deny_reasons(
        &self,
        reasons: &mut Vec<PolicyReason>,
        remote: &str,
        action: PolicyAction,
        namespaces: &[&str],
    ) {
        let mut seen = HashSet::new();
        for namespace in namespaces {
            if seen.insert(*namespace)
                && self.has_policy_fact("deny", &[remote, action.as_fact_atom(), namespace])
            {
                reasons.push(self.reason_for_fact(
                    "deny",
                    [remote, action.as_fact_atom(), namespace],
                    "explicit deny overrides any derived grant",
                ));
            }
        }
    }

    fn query_namespace(&self, query: &str) -> Option<&str> {
        self.facts.iter().find_map(|fact| match fact {
            PolicyFact::QueryNamespace {
                query: fact_query,
                namespace,
            } if fact_query == query => Some(namespace.as_str()),
            _ => None,
        })
    }

    fn decision(
        &self,
        allowed: bool,
        action: PolicyAction,
        reasons: Vec<PolicyReason>,
    ) -> PolicyDecision {
        let explanation = explain(allowed, action, &reasons);
        PolicyDecision {
            allowed,
            action,
            reasons,
            explanation,
        }
    }
}

fn insert_item_facts(fact_set: &mut FactSet, item: &PolicyItem) {
    for (predicate, args) in item.facts() {
        insert_fact(fact_set, &predicate, &args);
    }
}

fn insert_fact(fact_set: &mut FactSet, predicate: &str, args: &[String]) {
    fact_set.insert(
        predicate,
        args.iter().map(|arg| Term::ConstStr(arg.clone())).collect(),
    );
}

fn explain(allowed: bool, action: PolicyAction, reasons: &[PolicyReason]) -> String {
    let verdict = if allowed { "Allowed" } else { "Denied" };
    let facts = reasons
        .iter()
        .map(|reason| reason.fact.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if facts.is_empty() {
        format!(
            "{verdict} {} with no matching policy facts.",
            action.as_fact_atom()
        )
    } else {
        format!("{verdict} {} because {facts}.", action.as_fact_atom())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu_policy() -> RemotePolicy {
        RemotePolicy::from_facts([
            PolicyFact::remote("gpu"),
            PolicyFact::trusted_for("gpu", "research"),
            PolicyFact::trusted_for("gpu", "gpu_builds"),
            PolicyFact::not_trusted_for("gpu", "deployment_info"),
            PolicyFact::grant("gpu", PolicyAction::Read, "knowledge"),
            PolicyFact::grant("gpu", PolicyAction::DetailFetch, "knowledge"),
            PolicyFact::grant("gpu", PolicyAction::Autocommit, "knowledge"),
            PolicyFact::deny("gpu", PolicyAction::Read, "raw_context"),
            PolicyFact::fallback_enabled("gpu", "gpu_builds"),
        ])
    }

    fn item(id: &str, namespace: &str) -> PolicyItem {
        PolicyItem::new(id, namespace)
    }

    #[test]
    fn trusted_gpu_research_autocommits_safe_knowledge() {
        let decision = gpu_policy().can_autocommit("gpu", &item("item1", "gpu_builds"));

        assert!(decision.allowed, "{}", decision.explanation);
        assert_eq!(decision.action, PolicyAction::Autocommit);
        assert!(decision.has_fact("grant(gpu, autocommit, knowledge)"));
        assert!(decision.has_fact("trusted_for(gpu, gpu_builds)"));
        assert!(decision.has_fact("safe_item(item1)"));
        assert!(decision.has_fact("not conflict(item1)"));
    }

    #[test]
    fn not_trusted_for_gpu_deployment_blocks_deployment_autocommit() {
        let decision = gpu_policy().can_autocommit("gpu", &item("deploy1", "deployment_info"));

        assert!(!decision.allowed);
        assert!(decision.has_fact("not_trusted_for(gpu, deployment_info)"));
        assert!(
            decision
                .explanation
                .contains("not_trusted_for(gpu, deployment_info)")
        );
    }

    #[test]
    fn explicit_ask_derives_should_consult() {
        let policy = gpu_policy().with_fact(PolicyFact::explicit_remote("query1", "gpu"));

        let decision = policy.should_consult("gpu", "query1");

        assert!(decision.allowed, "{}", decision.explanation);
        assert!(decision.has_fact("explicit_remote(query1, gpu)"));
    }

    #[test]
    fn detail_fetch_requires_detail_grant_and_safe_item() {
        let decision = gpu_policy().can_fetch_detail("gpu", &item("item_detail", "gpu_builds"));

        assert!(decision.allowed, "{}", decision.explanation);
        assert_eq!(decision.action, PolicyAction::DetailFetch);
        assert!(decision.has_fact("grant(gpu, detail_fetch, knowledge)"));
        assert!(decision.has_fact("safe_item(item_detail)"));
    }

    #[test]
    fn low_local_coverage_with_fallback_derives_should_consult() {
        let policy = gpu_policy()
            .with_fact(PolicyFact::local_coverage("query2", CoverageLevel::Low))
            .with_fact(PolicyFact::query_namespace("query2", "gpu_builds"));

        let decision = policy.should_consult("gpu", "query2");

        assert!(decision.allowed, "{}", decision.explanation);
        assert!(decision.has_fact("local_coverage(query2, low)"));
        assert!(decision.has_fact("fallback_enabled(gpu, gpu_builds)"));
        assert!(decision.has_fact("trusted_for(gpu, gpu_builds)"));
    }

    #[test]
    fn deny_overrides_grant() {
        let policy =
            gpu_policy().with_fact(PolicyFact::deny("gpu", PolicyAction::Read, "knowledge"));

        let decision = policy.can_query("gpu", "knowledge");

        assert!(!decision.allowed);
        assert!(decision.has_fact("deny(gpu, read, knowledge)"));
        assert!(decision.explanation.contains("deny(gpu, read, knowledge)"));
    }

    #[test]
    fn blocked_deployment_explanation_includes_not_trusted_for_fact() {
        let decision = gpu_policy().can_autocommit("gpu", &item("deploy2", "deployment_info"));

        assert!(
            decision
                .explanation
                .contains("not_trusted_for(gpu, deployment_info)")
        );
    }

    #[test]
    fn autocommit_explains_grant_trust_safety_and_conflict_conditions() {
        let decision = gpu_policy().can_autocommit("gpu", &item("item2", "gpu_builds"));

        assert!(decision.allowed, "{}", decision.explanation);
        assert!(
            decision
                .explanation
                .contains("grant(gpu, autocommit, knowledge)")
        );
        assert!(
            decision
                .explanation
                .contains("trusted_for(gpu, gpu_builds)")
        );
        assert!(decision.explanation.contains("safe_item(item2)"));
        assert!(decision.explanation.contains("not conflict(item2)"));
    }

    #[test]
    fn deny_override_explains_deny() {
        let policy =
            gpu_policy().with_fact(PolicyFact::deny("gpu", PolicyAction::Read, "knowledge"));

        let decision = policy.can_query("gpu", "knowledge");

        assert!(!decision.allowed);
        assert!(decision.explanation.contains("deny(gpu, read, knowledge)"));
    }

    #[test]
    fn denied_autocommit_requires_activation() {
        let decision = gpu_policy().requires_activation("gpu", &item("deploy3", "deployment_info"));

        assert!(decision.allowed);
        assert_eq!(decision.action, PolicyAction::RequiresActivation);
        assert!(decision.has_fact("not_trusted_for(gpu, deployment_info)"));
    }
}
