//! Module: Define policy-governed memory artifact records.
//! Correctness: Correct when every artifact has a stable `a_` identity, user
//! tags normalize consistently, reserved system tags cannot be forged, and a
//! pending artifact cannot be activated without its required policy decision.
//! Last revised: 2026-08-31
//! Last changed: Added policy-gated activation decisions for artifact links.

use std::collections::BTreeSet;

use uuid::Uuid;

/// Stable, user-addressable identifier for one semantic link to content.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtifactId(String);

impl ArtifactId {
    /// Creates a new globally unique artifact identifier.
    pub fn new() -> Self {
        Self(format!("a_{}", Uuid::new_v4().simple()))
    }

    /// Returns the identifier as stored and displayed by clients.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ArtifactId {
    fn default() -> Self {
        Self::new()
    }
}

/// Tags which are supplied by the platform rather than an uploader.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SystemTag(String);

impl SystemTag {
    /// The state tag applied before policy review completes.
    pub const PENDING: &'static str = "_sys_pending";

    /// Validates a system-owned tag name.
    pub fn new(tag: impl Into<String>) -> Result<Self, TagError> {
        let tag = normalize_tag(&tag.into())?;
        if !tag.starts_with("_sys_") {
            return Err(TagError::NotSystemTag);
        }
        Ok(Self(tag))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a user-supplied tag was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagError {
    Empty,
    Reserved,
    NotSystemTag,
}

/// Normalizes a user-entered tag while retaining free-form spelling.
pub fn normalize_tag(tag: &str) -> Result<String, TagError> {
    let tag = tag.trim().to_lowercase();
    if tag.is_empty() {
        return Err(TagError::Empty);
    }
    Ok(tag)
}

/// Normalizes a user tag and rejects the reserved `_sys_` namespace.
pub fn normalize_user_tag(tag: &str) -> Result<String, TagError> {
    let tag = normalize_tag(tag)?;
    if tag.starts_with("_sys_") {
        return Err(TagError::Reserved);
    }
    Ok(tag)
}

/// Lifecycle state exposed by an artifact link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactState {
    Pending,
    Active,
    Deleted,
}

/// The authorization failure that prevents a pending link from becoming
/// readable. The caller must distinguish a missing policy-change grant from a
/// reviewer who is not in the tenant's reviewer set so it can build the right
/// queue entry without leaking the artifact bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationError {
    NotPending,
    ReviewerNotAuthorized,
    PolicyChangeRequired,
}

/// Determines whether a reviewer may activate a pending artifact link.
///
/// The uploader is allowed to approve a normal classification decision. A
/// policy-expanding decision requires an authorized reviewer and an explicit
/// `policy_change` grant; it is never implied by the artifact's tags.
pub fn activation_allowed(
    state: ArtifactState,
    uploader_id: &str,
    reviewer_id: &str,
    authorized_reviewers: &BTreeSet<String>,
    expands_policy: bool,
    policy_change: bool,
) -> Result<(), ActivationError> {
    if state != ArtifactState::Pending {
        return Err(ActivationError::NotPending);
    }
    let authorized = reviewer_id == uploader_id || authorized_reviewers.contains(reviewer_id);
    if !authorized {
        return Err(ActivationError::ReviewerNotAuthorized);
    }
    if expands_policy && !policy_change {
        return Err(ActivationError::PolicyChangeRequired);
    }
    Ok(())
}

/// The minimum durable semantic record for a content-addressed artifact link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub id: ArtifactId,
    pub checksum: String,
    pub uploader_id: String,
    pub captured_path: String,
    pub host_id: String,
    pub user_tags: BTreeSet<String>,
    pub system_tags: BTreeSet<SystemTag>,
    pub state: ArtifactState,
}

impl Artifact {
    /// Starts a new artifact inaccessible to ordinary readers until review.
    pub fn pending(
        checksum: impl Into<String>,
        uploader_id: impl Into<String>,
        captured_path: impl Into<String>,
        host_id: impl Into<String>,
        tags: impl IntoIterator<Item = String>,
    ) -> Result<Self, TagError> {
        let user_tags = tags
            .into_iter()
            .map(|tag| normalize_user_tag(&tag))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            id: ArtifactId::new(),
            checksum: checksum.into(),
            uploader_id: uploader_id.into(),
            captured_path: captured_path.into(),
            host_id: host_id.into(),
            user_tags,
            system_tags: [SystemTag::new(SystemTag::PENDING)?].into_iter().collect(),
            state: ArtifactState::Pending,
        })
    }

    /// Activates a reviewed artifact and removes its pending-only marker.
    pub fn activate(&mut self) -> bool {
        if self.state != ArtifactState::Pending {
            return false;
        }
        self.state = ArtifactState::Active;
        self.system_tags
            .retain(|tag| tag.as_str() != SystemTag::PENDING);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_ids_are_prefixed_and_unique() {
        let first = ArtifactId::new();
        let second = ArtifactId::new();
        assert!(first.as_str().starts_with("a_"));
        assert_ne!(first, second);
    }

    #[test]
    fn user_tags_are_trimmed_lowercase_and_deduplicated() {
        let artifact = Artifact::pending(
            "sha256:1",
            "u_1",
            "/source/report.csv",
            "host_1",
            [" Finance ".to_owned(), "finance".to_owned()],
        )
        .unwrap();
        assert_eq!(artifact.user_tags, BTreeSet::from(["finance".to_owned()]));
    }

    #[test]
    fn users_cannot_forge_system_tags() {
        assert_eq!(normalize_user_tag("_SYS_PENDING"), Err(TagError::Reserved));
    }

    #[test]
    fn pending_artifact_is_marked_and_activation_removes_the_marker() {
        let mut artifact = Artifact::pending("sha256:1", "u_1", "/a", "host_1", []).unwrap();
        assert_eq!(artifact.state, ArtifactState::Pending);
        assert!(
            artifact
                .system_tags
                .iter()
                .any(|tag| tag.as_str() == SystemTag::PENDING)
        );
        assert!(artifact.activate());
        assert_eq!(artifact.state, ArtifactState::Active);
        assert!(
            !artifact
                .system_tags
                .iter()
                .any(|tag| tag.as_str() == SystemTag::PENDING)
        );
        assert!(!artifact.activate());
    }

    #[test]
    fn normal_activation_allows_the_uploader_but_policy_expansion_requires_explicit_grant() {
        let reviewers = BTreeSet::new();
        assert!(
            activation_allowed(
                ArtifactState::Pending,
                "device-a",
                "device-a",
                &reviewers,
                false,
                false,
            )
            .is_ok()
        );
        assert_eq!(
            activation_allowed(
                ArtifactState::Pending,
                "device-a",
                "device-a",
                &reviewers,
                true,
                false,
            ),
            Err(ActivationError::PolicyChangeRequired)
        );
        assert!(
            activation_allowed(
                ArtifactState::Pending,
                "device-a",
                "device-a",
                &reviewers,
                true,
                true,
            )
            .is_ok()
        );
    }

    #[test]
    fn activation_rejects_an_unlisted_reviewer_without_exposing_content() {
        let reviewers = BTreeSet::from(["reviewer-a".to_owned()]);
        assert_eq!(
            activation_allowed(
                ArtifactState::Pending,
                "device-a",
                "reviewer-b",
                &reviewers,
                false,
                false,
            ),
            Err(ActivationError::ReviewerNotAuthorized)
        );
    }
}
