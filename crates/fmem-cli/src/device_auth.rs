//! Client half of the device authorization grant (RFC 8628).
//!
//! The server half lives in `ferrosa-dbaas` (`dbaas-api::device_grant`). This
//! asks for a grant, shows the operator a short code, and polls until a browser
//! somewhere approves it.
//!
//! # Nothing is ever pasted back
//!
//! The code travels terminal → human → browser. The secret ([`Grant::device_code`])
//! stays in this process; the thing printed on screen is useless without it. A
//! design where the browser shows a code to paste back here would make the
//! screen the enrolment authority, which is what this direction avoids.
//!
//! Correctness: Correct when the CLI never claims success it was not told, and
//! when a terminal server response ends the loop instead of spinning.
//! Last revised: 2026-08-22
//! Last changed: Initial device-grant client.

use std::time::{Duration, Instant};

use serde::Deserialize;

/// A grant the server minted for us.
#[derive(Debug, Clone, Deserialize)]
pub struct Grant {
    /// Secret. Proves on poll that we are the process that started this.
    /// Never printed, never logged.
    pub device_code: String,
    /// Short, grouped, meant to be read aloud or retyped.
    pub user_code: String,
    /// Where to go, without the code.
    pub verification_uri: String,
    /// Where to go, with it.
    pub verification_uri_complete: String,
    /// Seconds until the grant dies.
    pub expires_in: u64,
    /// Seconds the server wants between polls.
    pub interval: u64,
}

/// What the device became, once a human approved it.
#[derive(Debug, Clone, Deserialize)]
pub struct Enrolment {
    /// Device id the gateway assigned.
    pub device_id: String,
    /// Server-derived fingerprint.
    pub fingerprint: String,
    /// Who it bound as.
    pub email: Option<String>,
}

/// Why a grant did not complete.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("{0}")]
    Refused(String),

    /// The grant died before anyone approved it.
    #[error("the code expired before it was approved. Run the command again for a new one.")]
    Expired,

    /// We stopped waiting. Distinct from [`AuthError::Expired`]: the grant may
    /// still be live server-side, so telling the operator it expired would be a
    /// guess about state we cannot see.
    #[error(
        "gave up waiting after {0:?}. The code may still work — run the command again to check."
    )]
    GaveUp(Duration),

    #[error("talking to {url}: {source}")]
    Http { url: String, source: reqwest::Error },

    #[error("{url} returned {status}: {body}")]
    Unexpected {
        url: String,
        status: u16,
        body: String,
    },
}

/// The server's error vocabulary, which is RFC 8628's.
#[derive(Debug, Deserialize)]
struct ErrorBody {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
    /// The server may raise the interval to slow us down.
    #[serde(default)]
    interval: Option<u64>,
}

/// Ask for a grant.
pub async fn start(
    client: &reqwest::Client,
    base_url: &str,
    public_key_hex: &str,
    label: &str,
    kind: &str,
) -> Result<Grant, AuthError> {
    let url = format!("{}/v1/device-auth/start", base_url.trim_end_matches('/'));
    let response = client
        .post(&url)
        .json(&serde_json::json!({
            "public_key": public_key_hex,
            "label": label,
            "kind": kind,
        }))
        .send()
        .await
        .map_err(|source| AuthError::Http {
            url: url.clone(),
            source,
        })?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        // The server validates the key and the kind before minting, so a
        // failure here is a bad request we made — surface its words rather than
        // a generic one, since they say which field.
        return Err(AuthError::Unexpected {
            url,
            status: status.as_u16(),
            body,
        });
    }
    serde_json::from_str(&body).map_err(|_| AuthError::Unexpected {
        url,
        status: status.as_u16(),
        body,
    })
}

/// Poll until approved, refused, or out of time.
///
/// `on_wait` is called before each sleep so a caller can show progress. It is a
/// callback rather than a print here because this module must stay usable from
/// a test that has no terminal.
pub async fn wait_for_approval<F>(
    client: &reqwest::Client,
    base_url: &str,
    grant: &Grant,
    mut on_wait: F,
) -> Result<Enrolment, AuthError>
where
    F: FnMut(Duration),
{
    let url = format!("{}/v1/device-auth/poll", base_url.trim_end_matches('/'));
    let deadline = Instant::now() + Duration::from_secs(grant.expires_in);
    // Never below the server's stated interval, and never zero — a server that
    // reported 0 would turn this into a hot loop against its own rate limits.
    let mut interval = Duration::from_secs(grant.interval.max(1));

    loop {
        if Instant::now() >= deadline {
            return Err(AuthError::GaveUp(Duration::from_secs(grant.expires_in)));
        }

        let response = client
            .post(&url)
            .json(&serde_json::json!({ "device_code": grant.device_code }))
            .send()
            .await
            .map_err(|source| AuthError::Http {
                url: url.clone(),
                source,
            })?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if status.is_success() {
            return serde_json::from_str(&body).map_err(|_| AuthError::Unexpected {
                url: url.clone(),
                status: status.as_u16(),
                body,
            });
        }

        let parsed: ErrorBody = serde_json::from_str(&body).map_err(|_| AuthError::Unexpected {
            url: url.clone(),
            status: status.as_u16(),
            body: body.clone(),
        })?;

        match parsed.error.as_str() {
            "authorization_pending" => {
                if let Some(secs) = parsed.interval {
                    interval = Duration::from_secs(secs.max(1));
                }
            }
            // RFC 8628: back off, then carry on.
            "slow_down" => {
                interval = interval.saturating_add(Duration::from_secs(5));
            }
            "expired_token" => return Err(AuthError::Expired),
            "access_denied" => {
                return Err(AuthError::Refused(
                    parsed
                        .error_description
                        .unwrap_or_else(|| "the request was refused".to_string()),
                ));
            }
            // An error we do not know is terminal. Continuing to poll against a
            // server saying something we cannot interpret would spin until the
            // deadline and then report the wrong reason.
            other => {
                return Err(AuthError::Refused(format!(
                    "{other}: {}",
                    parsed.error_description.unwrap_or_default()
                )));
            }
        }

        on_wait(interval);
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grant_parses_from_the_servers_shape() {
        let body = r#"{
            "device_code":"f7281712151c4bb5",
            "user_code":"B45T-X3EB",
            "verification_uri":"https://example.test/d",
            "verification_uri_complete":"https://example.test/d/B45T-X3EB",
            "expires_in":600,
            "interval":5
        }"#;
        let grant: Grant = serde_json::from_str(body).expect("parses");
        assert_eq!(grant.user_code, "B45T-X3EB");
        assert_eq!(grant.interval, 5);
        assert_eq!(grant.expires_in, 600);
    }

    /// The exact body the deployed server returns on success.
    #[test]
    fn an_enrolment_parses_from_the_servers_shape() {
        let body = r#"{
            "status":"approved",
            "device_id":"11111111-1111-4111-8111-111111111111",
            "fingerprint":"a8b5d7b0",
            "email":"ben@example.com"
        }"#;
        let done: Enrolment = serde_json::from_str(body).expect("parses");
        assert_eq!(done.email.as_deref(), Some("ben@example.com"));
        assert_eq!(done.fingerprint, "a8b5d7b0");
    }

    /// An enrolment with no email must still parse — the field is optional and
    /// a provider that does not release it must not break the flow.
    #[test]
    fn an_enrolment_without_an_email_still_parses() {
        let body = r#"{"device_id":"d","fingerprint":"f","email":null}"#;
        let done: Enrolment = serde_json::from_str(body).expect("parses");
        assert!(done.email.is_none());
    }

    #[test]
    fn the_error_vocabulary_parses() {
        let pending: ErrorBody =
            serde_json::from_str(r#"{"error":"authorization_pending","interval":5}"#)
                .expect("parses");
        assert_eq!(pending.error, "authorization_pending");
        assert_eq!(pending.interval, Some(5));

        let expired: ErrorBody = serde_json::from_str(
            r#"{"error":"expired_token","error_description":"this grant is no longer valid; start a new one"}"#,
        )
        .expect("parses");
        assert_eq!(expired.error, "expired_token");
    }

    /// Giving up and expiring are DIFFERENT messages.
    ///
    /// The grant may still be live server-side when we stop waiting, so
    /// reporting expiry would state something we cannot observe.
    #[test]
    fn giving_up_does_not_claim_the_code_expired() {
        let gave_up = AuthError::GaveUp(Duration::from_secs(600)).to_string();
        let expired = AuthError::Expired.to_string();

        assert!(gave_up.contains("may still work"), "{gave_up}");
        assert!(!gave_up.contains("expired before"), "{gave_up}");
        assert!(expired.contains("expired"), "{expired}");
        assert_ne!(gave_up, expired);
    }

    /// Every terminal error tells the operator what to do next.
    #[test]
    fn terminal_errors_say_what_to_do() {
        for message in [
            AuthError::Expired.to_string(),
            AuthError::GaveUp(Duration::from_secs(1)).to_string(),
        ] {
            assert!(message.contains("again"), "no next step in: {message}");
        }
    }
}
