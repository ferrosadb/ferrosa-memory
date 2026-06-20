//! Module: Deterministic applicability extraction and comparison for remote memory.
//! Correctness: Correct when OS, host, runtime, repository, and environment hints are extracted conservatively and cross-scope comparisons do not silently activate wrong-environment memory.
//! Last revised: 2026-05-12
//! Last changed: Implemented deterministic Packet D applicability classification.

use crate::remotes::types::ApplicabilityFrame;
use std::collections::BTreeSet;

/// Deterministic alias mapping supplied by policy facts or local configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicabilityAlias {
    phrase: String,
    target: ApplicabilityAliasTarget,
}

impl ApplicabilityAlias {
    /// Create an alias rule. Matching is case-insensitive substring matching after ASCII folding.
    pub fn new(phrase: impl Into<String>, target: ApplicabilityAliasTarget) -> Self {
        Self {
            phrase: normalize_text(&phrase.into()),
            target,
        }
    }
}

/// Target field populated when an applicability alias phrase is present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplicabilityAliasTarget {
    HostOs(String),
    ContainerRuntime(String),
    Hardware(String),
    RequiredTag(String),
    ExcludedTag(String),
}

/// Deterministic extractor for low-entropy environment hints in remote memory text.
#[derive(Debug, Clone, Default)]
pub struct ApplicabilityClassifier {
    aliases: Vec<ApplicabilityAlias>,
}

impl ApplicabilityClassifier {
    pub fn new(aliases: Vec<ApplicabilityAlias>) -> Self {
        Self { aliases }
    }

    /// Extract a conservative applicability frame from free text.
    pub fn extract(&self, text: &str) -> ApplicabilityFrame {
        let normalized = normalize_text(text);
        let mut frame = ApplicabilityFrame {
            namespaces: Vec::new(),
            host_os: detect_host_os(&normalized),
            container_runtime: detect_container_runtime(&normalized),
            hardware: Vec::new(),
            required_tags: Vec::new(),
            excluded_tags: Vec::new(),
            confidence: 0.0,
        };

        if contains_any(&normalized, &["gpu", "cuda", "nvidia"])
            && !contains_exact(&frame.hardware, "gpu")
        {
            frame.hardware.push("gpu".into());
        }

        if contains_any(&normalized, &["ferrosa memory", "ferrosa-memory", "fmem"])
            && !contains_exact(&frame.required_tags, "project:ferrosa-memory")
        {
            frame.required_tags.push("project:ferrosa-memory".into());
        }

        if contains_any(&normalized, &["production", "prod"])
            && !contains_exact(&frame.required_tags, "environment:prod")
        {
            frame.required_tags.push("environment:prod".into());
        }
        if contains_any(&normalized, &["staging"])
            && !contains_exact(&frame.required_tags, "environment:staging")
        {
            frame.required_tags.push("environment:staging".into());
        }

        for alias in &self.aliases {
            if normalized.contains(&alias.phrase) {
                apply_alias(&mut frame, &alias.target);
            }
        }

        dedup_sort(&mut frame.hardware);
        dedup_sort(&mut frame.required_tags);
        dedup_sort(&mut frame.excluded_tags);
        frame.confidence = confidence_for(&frame);
        frame
    }
}

/// Coarse deterministic match class for remote-vs-local applicability frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicabilityMatch {
    Exact,
    Partial,
    Disjoint,
    Unknown,
    ConflictProne,
}

/// Comparison result with caveats suitable for import-plan explanations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicabilityComparison {
    pub kind: ApplicabilityMatch,
    pub reasons: Vec<String>,
    pub requires_review: bool,
}

/// Compare a remote item frame against the current/local frame.
pub fn compare_applicability(
    remote: &ApplicabilityFrame,
    current: &ApplicabilityFrame,
) -> ApplicabilityComparison {
    let mut reasons = Vec::new();

    if is_unknown(remote) || is_unknown(current) {
        reasons.push("unknown applicability scope requires conservative review".into());
        return ApplicabilityComparison {
            kind: ApplicabilityMatch::Unknown,
            reasons,
            requires_review: true,
        };
    }

    if field_conflicts(remote.host_os.as_deref(), current.host_os.as_deref()) {
        reasons.push(format!(
            "host_os differs: remote={} current={}",
            remote.host_os.as_deref().unwrap_or("unknown"),
            current.host_os.as_deref().unwrap_or("unknown")
        ));
        return ApplicabilityComparison {
            kind: ApplicabilityMatch::Disjoint,
            reasons,
            requires_review: true,
        };
    }

    if tag_sets_conflict(&remote.required_tags, &current.excluded_tags) {
        reasons.push("remote required_tags intersect current excluded_tags".into());
        return ApplicabilityComparison {
            kind: ApplicabilityMatch::Disjoint,
            reasons,
            requires_review: true,
        };
    }
    if tag_sets_conflict(&current.required_tags, &remote.excluded_tags) {
        reasons.push("current required_tags intersect remote excluded_tags".into());
        return ApplicabilityComparison {
            kind: ApplicabilityMatch::Disjoint,
            reasons,
            requires_review: true,
        };
    }

    if same_known(remote.host_os.as_deref(), current.host_os.as_deref())
        && field_conflicts(
            remote.container_runtime.as_deref(),
            current.container_runtime.as_deref(),
        )
    {
        reasons.push(format!(
            "same host_os but different container runtime: remote={} current={}",
            remote.container_runtime.as_deref().unwrap_or("unknown"),
            current.container_runtime.as_deref().unwrap_or("unknown")
        ));
        return ApplicabilityComparison {
            kind: ApplicabilityMatch::ConflictProne,
            reasons,
            requires_review: true,
        };
    }

    if frames_exactly_match(remote, current) {
        reasons.push("all known applicability dimensions match exactly".into());
        return ApplicabilityComparison {
            kind: ApplicabilityMatch::Exact,
            reasons,
            requires_review: false,
        };
    }

    reasons.push("some applicability dimensions match but scope is not exact".into());
    ApplicabilityComparison {
        kind: ApplicabilityMatch::Partial,
        reasons,
        requires_review: true,
    }
}

fn normalize_text(text: &str) -> String {
    text.to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn detect_host_os(text: &str) -> Option<String> {
    if contains_any(text, &["mac", "macos", "osx", "darwin"]) {
        Some("macos".into())
    } else if contains_any(text, &["linux", "ubuntu", "debian", "fedora", "nixos"]) {
        Some("linux".into())
    } else if contains_any(text, &["windows", "win32", "powershell"]) {
        Some("windows".into())
    } else {
        None
    }
}

fn detect_container_runtime(text: &str) -> Option<String> {
    if contains_any(text, &["docker", "dockerd"]) {
        Some("docker".into())
    } else if contains_any(text, &["podman", "podman machine"]) {
        Some("podman".into())
    } else if contains_any(text, &["kubernetes", "k8s"]) {
        Some("kubernetes".into())
    } else {
        None
    }
}

fn apply_alias(frame: &mut ApplicabilityFrame, target: &ApplicabilityAliasTarget) {
    match target {
        ApplicabilityAliasTarget::HostOs(value) => frame.host_os = Some(canonical_value(value)),
        ApplicabilityAliasTarget::ContainerRuntime(value) => {
            frame.container_runtime = Some(canonical_value(value));
        }
        ApplicabilityAliasTarget::Hardware(value) => push_unique(&mut frame.hardware, value),
        ApplicabilityAliasTarget::RequiredTag(value) => {
            push_unique(&mut frame.required_tags, value)
        }
        ApplicabilityAliasTarget::ExcludedTag(value) => {
            push_unique(&mut frame.excluded_tags, value)
        }
    }
}

fn canonical_value(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('_', "-")
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    let canonical = canonical_value(value);
    if !values.iter().any(|existing| existing == &canonical) {
        values.push(canonical);
    }
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn contains_exact(values: &[String], needle: &str) -> bool {
    values.iter().any(|value| value == needle)
}

fn dedup_sort(values: &mut Vec<String>) {
    let unique = values.drain(..).collect::<BTreeSet<_>>();
    values.extend(unique);
}

fn confidence_for(frame: &ApplicabilityFrame) -> f64 {
    let mut signals = 0;
    signals += usize::from(frame.host_os.is_some());
    signals += usize::from(frame.container_runtime.is_some());
    signals += frame.hardware.len();
    signals += frame.required_tags.len();
    signals += frame.excluded_tags.len();
    (signals as f64 / 2.0).clamp(0.0, 1.0)
}

fn is_unknown(frame: &ApplicabilityFrame) -> bool {
    frame.host_os.is_none()
        && frame.container_runtime.is_none()
        && frame.hardware.is_empty()
        && frame.required_tags.is_empty()
        && frame.excluded_tags.is_empty()
}

fn field_conflicts(left: Option<&str>, right: Option<&str>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left != right)
}

fn same_known(left: Option<&str>, right: Option<&str>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left == right)
}

fn tag_sets_conflict(required: &[String], excluded: &[String]) -> bool {
    required
        .iter()
        .any(|required| excluded.iter().any(|excluded| excluded == required))
}

fn frames_exactly_match(remote: &ApplicabilityFrame, current: &ApplicabilityFrame) -> bool {
    remote.host_os == current.host_os
        && remote.container_runtime == current.container_runtime
        && sorted(&remote.hardware) == sorted(&current.hardware)
        && sorted(&remote.required_tags) == sorted(&current.required_tags)
        && sorted(&remote.excluded_tags) == sorted(&current.excluded_tags)
}

fn sorted(values: &[String]) -> Vec<String> {
    values
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(
        host_os: Option<&str>,
        runtime: Option<&str>,
        hardware: &[&str],
        required_tags: &[&str],
    ) -> ApplicabilityFrame {
        ApplicabilityFrame {
            namespaces: Vec::new(),
            host_os: host_os.map(str::to_owned),
            container_runtime: runtime.map(str::to_owned),
            hardware: hardware.iter().map(|v| v.to_string()).collect(),
            required_tags: required_tags.iter().map(|v| v.to_string()).collect(),
            excluded_tags: Vec::new(),
            confidence: 1.0,
        }
    }

    #[test]
    fn extractor_maps_mac_to_macos() {
        let classifier = ApplicabilityClassifier::default();

        let extracted = classifier.extract("This fix is for my Mac laptop.");

        assert_eq!(extracted.host_os.as_deref(), Some("macos"));
        assert!(extracted.confidence >= 0.5);
    }

    #[test]
    fn extractor_resolves_gpu_box_alias_from_policy_facts() {
        let classifier = ApplicabilityClassifier::new(vec![ApplicabilityAlias::new(
            "GPU box",
            ApplicabilityAliasTarget::Hardware("gpu".into()),
        )]);

        let extracted = classifier.extract("No, that workaround is only for the GPU box.");

        assert!(extracted.hardware.iter().any(|value| value == "gpu"));
    }

    #[test]
    fn extractor_detects_container_runtimes() {
        let classifier = ApplicabilityClassifier::default();

        let docker = classifier.extract("Run this inside Docker on Linux.");
        let podman = classifier.extract("Use Podman on the Mac dev machine.");

        assert_eq!(docker.container_runtime.as_deref(), Some("docker"));
        assert_eq!(podman.container_runtime.as_deref(), Some("podman"));
    }

    #[test]
    fn extractor_detects_ferrosa_memory_project_hint() {
        let classifier = ApplicabilityClassifier::default();

        let extracted = classifier.extract("Ferrosa Memory needs the remote packet tests.");

        assert!(
            extracted
                .required_tags
                .iter()
                .any(|tag| tag == "project:ferrosa-memory")
        );
    }

    #[test]
    fn linux_docker_vs_mac_podman_is_disjoint() {
        let linux_docker = frame(Some("linux"), Some("docker"), &[], &[]);
        let mac_podman = frame(Some("macos"), Some("podman"), &[], &[]);

        let comparison = compare_applicability(&linux_docker, &mac_podman);

        assert_eq!(comparison.kind, ApplicabilityMatch::Disjoint);
        assert!(comparison.requires_review);
        assert!(
            comparison
                .reasons
                .iter()
                .any(|reason| reason.contains("host_os"))
        );
    }

    #[test]
    fn same_linux_host_with_different_runtimes_is_conflict_prone() {
        let linux_docker = frame(Some("linux"), Some("docker"), &["gpu"], &[]);
        let linux_podman = frame(Some("linux"), Some("podman"), &["gpu"], &[]);

        let comparison = compare_applicability(&linux_docker, &linux_podman);

        assert_eq!(comparison.kind, ApplicabilityMatch::ConflictProne);
        assert!(comparison.requires_review);
        assert!(
            comparison
                .reasons
                .iter()
                .any(|reason| reason.contains("runtime"))
        );
    }

    #[test]
    fn unknown_scope_is_conservative_and_caveated() {
        let unknown = frame(None, None, &[], &[]);
        let mac_podman = frame(Some("macos"), Some("podman"), &[], &[]);

        let comparison = compare_applicability(&unknown, &mac_podman);

        assert_eq!(comparison.kind, ApplicabilityMatch::Unknown);
        assert!(comparison.requires_review);
        assert!(
            comparison
                .reasons
                .iter()
                .any(|reason| reason.contains("unknown"))
        );
    }
}
