//! Talking to the coordinator running beside this listener.
//!
//! The coordinator binds loopback and answers over HTTP; the app reaches this
//! machine over WebRTC. This is the piece between them: it takes a request the
//! listener has already AUTHORIZED and asks the coordinator for the answer.
//!
//! Authorization is not done here. [`crate::coordinator_command`] decides
//! whether a peer may ask; by the time anything in this module runs, that
//! question is settled.
//!
//! Two rules the code is shaped around:
//!
//! * a secret value passes through in exactly one method and is never logged,
//!   never stored, and never echoed back
//! * every wait is bounded. A coordinator that stops answering must surface as
//!   an error, not as a control session that hangs holding a frame

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where the coordinator listens unless told otherwise.
///
/// Loopback because the coordinator answers questions about, and accepts,
/// credentials. It is reachable from this host and from a guest on its own tap,
/// and from nowhere else.
pub const DEFAULT_COORDINATOR_BASE: &str = "http://127.0.0.1:17870";

/// How long any single coordinator call may take.
///
/// Bounded deliberately. The caller is holding a control frame from a phone; a
/// coordinator that has wedged must produce an error the operator can see
/// rather than a session that appears alive and answers nothing.
// Used by the HTTP client, which is feature-gated; the config and its tests
// deliberately are NOT, so they run in the default test profile rather than
// being silently skipped when the feature is off.
#[cfg_attr(not(feature = "webrtc-transport"), allow(dead_code))]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Why the coordinator could not answer.
#[derive(Debug)]
pub enum CoordinatorError {
    /// No coordinator is configured on this host.
    ///
    /// Not a failure. A machine that only serves memory has no coordinator, and
    /// saying "unavailable" is the truthful answer rather than an error the
    /// operator is expected to fix.
    NotConfigured,
    /// The token file exists but could not be read.
    Token(std::io::Error),
    /// The request did not complete.
    Transport(String),
    /// The coordinator answered, and said no.
    Status {
        /// HTTP status.
        code: u16,
        /// Body, which is the coordinator's own error text and never a secret.
        body: String,
    },
    /// The answer did not parse.
    Malformed(String),
}

impl std::fmt::Display for CoordinatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => write!(f, "no coordinator is configured on this host"),
            Self::Token(e) => write!(f, "coordinator token unreadable: {e}"),
            Self::Transport(e) => write!(f, "coordinator request failed: {e}"),
            Self::Status { code, body } => write!(f, "coordinator returned {code}: {body}"),
            Self::Malformed(e) => write!(f, "coordinator answer did not parse: {e}"),
        }
    }
}

impl std::error::Error for CoordinatorError {}

/// Where the coordinator is and how to authenticate to it.
#[derive(Clone)]
pub struct CoordinatorConfig {
    base_url: String,
    // Read only by the feature-gated client. Kept here regardless so the
    // discovery rules -- absent file, empty token -- are exercised by the
    // default test profile.
    #[cfg_attr(not(feature = "webrtc-transport"), allow(dead_code))]
    token: String,
}

impl std::fmt::Debug for CoordinatorConfig {
    /// Never render the token. A config ends up inside application state, and
    /// application state gets `{:?}`-logged eventually.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoordinatorConfig")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl CoordinatorConfig {
    /// Read the configuration, or report that this host has no coordinator.
    ///
    /// The token file is what decides. It is written 0600 by the coordinator on
    /// first start, so its ABSENCE means no coordinator has ever run here --
    /// which is a normal state for a machine that only serves memory, not a
    /// misconfiguration to complain about.
    pub fn discover(prefix: &Path) -> Result<Option<Self>, CoordinatorError> {
        let path = Self::token_path(prefix);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(CoordinatorError::Token(e)),
        };
        let token = raw.trim().to_string();
        if token.is_empty() {
            // A present-but-empty token would authenticate as the empty string.
            // Treat it as absent rather than as a credential.
            return Ok(None);
        }
        Ok(Some(Self {
            base_url: std::env::var("FERROSA_COORDINATOR_BASE")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_COORDINATOR_BASE.to_string()),
            token,
        }))
    }

    /// Where the coordinator writes its API token.
    pub fn token_path(prefix: &Path) -> PathBuf {
        prefix.join("credentials").join("coordinator.token")
    }

    /// The base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

/// A VM id that is safe to place in a URL path, or an error saying why not.
///
/// The id arrives inside a control frame written by a peer and goes straight
/// into a path segment. A slash re-points the request at a different endpoint
/// and a percent-encoded one can walk out of `/v1/vms` altogether. The
/// coordinator validates ids as well, but a request that should never have been
/// sent is better refused here than answered with a 404 that reads as a missing
/// VM.
///
/// A whitelist rather than an escaper, and deliberately narrower than what the
/// coordinator accepts: everything a real VM id has ever contained passes, and
/// anything that would need encoding does not.
fn safe_vm_id(id: &str) -> Result<&str, CoordinatorError> {
    if id.is_empty() {
        return Err(CoordinatorError::Malformed("a vm id is required".to_owned()));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(CoordinatorError::Malformed(format!(
            "vm id {id:?} contains a character that cannot go in a url path"
        )));
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vm_id_that_could_reshape_the_url_is_refused() {
        // The id arrives in a control frame from a peer and goes straight into
        // a URL PATH. A slash makes it address a different endpoint; an encoded
        // one can walk out of /v1/vms entirely.
        for bad in ["../secrets", "a/b", "vm%2f..", "", "with space"] {
            assert!(safe_vm_id(bad).is_err(), "{bad:?} was accepted into a url path");
        }
        for good in ["vm-1", "hib-demo", "a_b.c"] {
            assert!(safe_vm_id(good).is_ok(), "{good:?} was refused");
        }
    }


    fn write_token(dir: &Path, contents: &str) -> PathBuf {
        let creds = dir.join("credentials");
        std::fs::create_dir_all(&creds).expect("mkdir");
        let path = creds.join("coordinator.token");
        std::fs::write(&path, contents).expect("write");
        path
    }

    #[test]
    fn a_host_with_no_coordinator_is_not_an_error() {
        // A machine that only serves memory has no coordinator. Reporting that
        // as a failure would put an error in front of an operator who has
        // nothing to fix.
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            CoordinatorConfig::discover(dir.path())
                .expect("absence is not an error")
                .is_none()
        );
    }

    #[test]
    fn a_token_file_makes_the_coordinator_discoverable() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_token(dir.path(), "abc123\n");
        let config = CoordinatorConfig::discover(dir.path())
            .expect("readable")
            .expect("configured");
        assert_eq!(config.base_url(), DEFAULT_COORDINATOR_BASE);
    }

    #[test]
    fn a_trailing_newline_is_trimmed() {
        // Editors add one. Without trimming every request would 401 and the
        // cause would look like a coordinator bug.
        let dir = tempfile::tempdir().expect("tempdir");
        write_token(dir.path(), "abc123\n");
        let config = CoordinatorConfig::discover(dir.path())
            .expect("readable")
            .expect("configured");
        assert_eq!(config.token, "abc123");
    }

    #[test]
    fn an_empty_token_reads_as_no_coordinator_rather_than_a_credential() {
        // Trusting it would authenticate as the empty string.
        let dir = tempfile::tempdir().expect("tempdir");
        write_token(dir.path(), "   \n");
        assert!(
            CoordinatorConfig::discover(dir.path())
                .expect("readable")
                .is_none()
        );
    }

    #[test]
    fn the_debug_rendering_does_not_leak_the_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_token(dir.path(), "super-secret-token");
        let config = CoordinatorConfig::discover(dir.path())
            .expect("readable")
            .expect("configured");
        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains("super-secret-token"),
            "token leaked into Debug: {rendered}"
        );
        assert!(rendered.contains("base_url"));
    }

    #[test]
    fn the_default_base_is_loopback() {
        // The coordinator accepts credentials. It must not be reachable from a
        // network interface because a variable was unset.
        assert!(DEFAULT_COORDINATOR_BASE.starts_with("http://127.0.0.1"));
    }

    #[test]
    fn every_call_is_bounded() {
        // The caller holds a control frame from a phone. A wedged coordinator
        // must surface as an error rather than as a session that never answers.
        assert!(REQUEST_TIMEOUT <= Duration::from_secs(30));
        assert!(REQUEST_TIMEOUT > Duration::from_secs(0));
    }

    #[test]
    fn the_token_path_matches_where_the_coordinator_writes_it() {
        // Both sides derive this from the prefix; a mismatch would look like a
        // host with no coordinator on a host that has one.
        let path = CoordinatorConfig::token_path(Path::new("/opt/ferrosa"));
        assert_eq!(
            path,
            Path::new("/opt/ferrosa/credentials/coordinator.token")
        );
    }
}

/// Calls the coordinator's loopback API.
///
/// Every method here answers a request the listener has ALREADY authorized.
/// Nothing in this type decides whether a peer may ask.
#[cfg(feature = "webrtc-transport")]
#[derive(Debug, Clone)]
pub struct CoordinatorClient {
    config: CoordinatorConfig,
    http: reqwest::Client,
}

#[cfg(feature = "webrtc-transport")]
impl CoordinatorClient {
    /// Build a client, or `None` when this host has no coordinator.
    pub fn discover(prefix: &Path) -> Result<Option<Self>, CoordinatorError> {
        let Some(config) = CoordinatorConfig::discover(prefix)? else {
            return Ok(None);
        };
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| CoordinatorError::Transport(e.to_string()))?;
        Ok(Some(Self { config, http }))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.config.base_url)
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<String, CoordinatorError> {
        let response = request
            .bearer_auth(&self.config.token)
            .send()
            .await
            .map_err(|e| CoordinatorError::Transport(e.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| CoordinatorError::Transport(e.to_string()))?;
        if status.is_success() {
            Ok(body)
        } else {
            // The coordinator's error bodies are its own text and never contain
            // a secret -- it refuses to put one in a response at all.
            Err(CoordinatorError::Status {
                code: status.as_u16(),
                body,
            })
        }
    }

    async fn get_json(&self, path: &str) -> Result<serde_json::Value, CoordinatorError> {
        let body = self.send(self.http.get(self.url(path))).await?;
        serde_json::from_str(&body).map_err(|e| CoordinatorError::Malformed(e.to_string()))
    }

    /// The team defined on this machine, with its enforcement report.
    pub async fn teammates(&self) -> Result<serde_json::Value, CoordinatorError> {
        self.get_json("/v1/teammates").await
    }

    /// Secret requests awaiting a human.
    pub async fn pending_secrets(&self) -> Result<serde_json::Value, CoordinatorError> {
        self.get_json("/v1/secrets/pending").await
    }

    /// Running microVMs.
    pub async fn vms(&self) -> Result<serde_json::Value, CoordinatorError> {
        self.get_json("/v1/vms").await
    }

    /// What this machine can run: live tiers, images, and remaining capacity.
    ///
    /// Two calls in one command, because a controller needs both and asking
    /// twice over the control channel would double the round trips on the one
    /// question every machine is asked. `/v1/setup` is best effort: a
    /// coordinator older than that endpoint reports no setup, which is not the
    /// same as a host that needs nothing and must not render as one.
    pub async fn offering(&self) -> Result<serde_json::Value, CoordinatorError> {
        let offering = self.get_json("/v1/offering").await?;
        let setup = self.get_json("/v1/setup").await.ok();
        Ok(serde_json::json!({ "offering": offering, "setup": setup }))
    }



    /// Start a microVM from an image this machine advertised.
    ///
    /// The body is passed through VERBATIM. It was built by shared Rust on the
    /// controller from the same offering this coordinator published, and
    /// rewriting it here would give the two sides two different ideas of what
    /// was asked for. The coordinator validates it again regardless -- it does
    /// not trust the controller's copy either.
    pub async fn launch_vm(&self, body: &str) -> Result<serde_json::Value, CoordinatorError> {
        // Parsed only to reject a malformed body before a round trip; the
        // original text is what gets sent.
        let _: serde_json::Value =
            serde_json::from_str(body).map_err(|e| CoordinatorError::Malformed(e.to_string()))?;
        let reply = self
            .send(
                self.http
                    .post(self.url("/v1/launch"))
                    .header("content-type", "application/json")
                    .body(body.to_owned()),
            )
            .await?;
        serde_json::from_str(&reply).map_err(|e| CoordinatorError::Malformed(e.to_string()))
    }

    /// Write a running microVM to disk and stop it.
    ///
    /// Returns what the coordinator wrote -- both paths and the memory file's
    /// size -- rather than an acknowledgement. A caller that only saw "ok"
    /// could not tell a real snapshot from a plausible one.
    pub async fn hibernate_vm(&self, id: &str) -> Result<serde_json::Value, CoordinatorError> {
        let id = safe_vm_id(id)?;
        let body = self
            .send(self.http.post(self.url(&format!("/v1/vms/{id}/hibernate"))))
            .await?;
        serde_json::from_str(&body).map_err(|e| CoordinatorError::Malformed(e.to_string()))
    }

    /// Wake a hibernated microVM from its snapshot.
    pub async fn resume_vm(&self, id: &str) -> Result<serde_json::Value, CoordinatorError> {
        let id = safe_vm_id(id)?;
        let body = self
            .send(self.http.post(self.url(&format!("/v1/vms/{id}/resume"))))
            .await?;
        serde_json::from_str(&body).map_err(|e| CoordinatorError::Malformed(e.to_string()))
    }

    /// Answer a secret request.
    ///
    /// The ONLY method that touches a secret. `value` is moved in, sent once,
    /// and dropped; it is not logged here, not retained, and the coordinator
    /// does not echo it back. The result is the request's new state, which
    /// carries a path and never a value.
    pub async fn fulfil_secret(
        &self,
        request_id: u64,
        value: String,
    ) -> Result<serde_json::Value, CoordinatorError> {
        let body = self
            .send(
                self.http
                    .post(self.url(&format!("/v1/secrets/{request_id}/fulfil")))
                    .json(&serde_json::json!({ "value": value })),
            )
            .await?;
        serde_json::from_str(&body).map_err(|e| CoordinatorError::Malformed(e.to_string()))
    }

    /// Refuse a secret request.
    pub async fn deny_secret(&self, request_id: u64) -> Result<(), CoordinatorError> {
        self.send(
            self.http
                .post(self.url(&format!("/v1/secrets/{request_id}/deny"))),
        )
        .await
        .map(|_| ())
    }
}
