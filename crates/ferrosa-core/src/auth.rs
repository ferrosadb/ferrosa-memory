//! Tenant authentication and context extraction.
//!
//! The auth module is the security boundary that ensures every tool handler
//! receives a validated [`TenantContext`]. The `tenant_id` is NEVER client-supplied —
//! it is derived from the authenticated session.
//!
//! ## Transport modes
//!
//! - **stdio**: inherits process owner credentials (local trust model).
//!   The `tenant_id` is read from the config file or environment.
//! - **HTTP**: HTTP Basic auth against CQL credentials. The server extracts
//!   `tenant_id` from the authenticated user mapping.
//!
//! ## Invariant
//!
//! Tool handlers take `TenantContext` as a required (non-Option) parameter.
//! This is enforced at compile time — a handler cannot execute without auth.

use uuid::Uuid;

use crate::types::TenantContext;

/// Authentication error types.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("missing credentials")]
    MissingCredentials,
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("unrecognized session origin: {0}")]
    UnrecognizedOrigin(String),
}

/// Authenticate a stdio session. In stdio mode, the process is trusted and
/// the tenant_id comes from the local config or environment.
///
/// # Arguments
///
/// * `tenant_id` — The configured tenant UUID for this local installation.
pub fn authenticate_stdio(tenant_id: Uuid) -> TenantContext {
    TenantContext {
        tenant_id,
        session_origin: "stdio".into(),
    }
}

/// Authenticate an HTTP session from Basic auth credentials.
///
/// # Arguments
///
/// * `username` — HTTP Basic auth username
/// * `password` — HTTP Basic auth password
/// * `credential_validator` — closure that validates credentials and returns tenant_id
///
/// # Errors
///
/// Returns `AuthError::InvalidCredentials` if the validator rejects the credentials.
pub fn authenticate_http<F>(
    username: &str,
    password: &str,
    credential_validator: F,
) -> Result<TenantContext, AuthError>
where
    F: FnOnce(&str, &str) -> Option<Uuid>,
{
    let tenant_id =
        credential_validator(username, password).ok_or(AuthError::InvalidCredentials)?;

    Ok(TenantContext {
        tenant_id,
        session_origin: "http".into(),
    })
}

/// Validate that a session origin is recognized by the server.
/// Implements the MCPShield pattern: reject unrecognized origins before storage.
pub fn validate_origin(origin: &str) -> Result<(), AuthError> {
    match origin {
        "stdio" | "http" | "sse" => Ok(()),
        other => Err(AuthError::UnrecognizedOrigin(other.into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_auth_sets_origin() {
        let tid = Uuid::new_v4();
        let ctx = authenticate_stdio(tid);
        assert_eq!(ctx.tenant_id, tid);
        assert_eq!(ctx.session_origin, "stdio");
    }

    #[test]
    fn http_auth_valid_credentials() {
        let tid = Uuid::new_v4();
        let ctx = authenticate_http("user", "pass", |u, p| {
            if u == "user" && p == "pass" {
                Some(tid)
            } else {
                None
            }
        })
        .expect("valid credentials should succeed");
        assert_eq!(ctx.tenant_id, tid);
        assert_eq!(ctx.session_origin, "http");
    }

    #[test]
    fn http_auth_invalid_credentials() {
        let err = authenticate_http("user", "wrong", |_, _| None);
        assert!(matches!(err, Err(AuthError::InvalidCredentials)));
    }

    #[test]
    fn validate_known_origins() {
        assert!(validate_origin("stdio").is_ok());
        assert!(validate_origin("http").is_ok());
        assert!(validate_origin("sse").is_ok());
    }

    #[test]
    fn reject_unknown_origin() {
        assert!(matches!(
            validate_origin("websocket"),
            Err(AuthError::UnrecognizedOrigin(_))
        ));
    }
}
