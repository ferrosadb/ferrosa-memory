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

use std::collections::HashMap;
use std::fs;
use std::sync::RwLock;

use serde::Deserialize;
use sha2::{Digest, Sha256};
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
    #[error("failed to load auth file: {0}")]
    AuthFileLoad(String),
}

#[derive(Debug, Deserialize)]
struct AuthFile {
    #[serde(default)]
    principal: Vec<AuthPrincipal>,
}

#[derive(Debug, Deserialize)]
struct AuthPrincipal {
    username: String,
    password_sha256: String,
    tenant_id: String,
}

#[derive(Debug)]
pub struct FileAuthValidator {
    principals: RwLock<HashMap<String, PrincipalRecord>>,
    path: String,
}

#[derive(Debug, Clone)]
struct PrincipalRecord {
    password_sha256: String,
    tenant_id: Uuid,
}

impl FileAuthValidator {
    pub fn from_path(path: &str) -> Result<Self, AuthError> {
        let principals = Self::load_principals(path)?;
        Ok(Self {
            principals: RwLock::new(principals),
            path: path.to_string(),
        })
    }

    pub fn reload(&self) -> Result<usize, AuthError> {
        let new_principals = Self::load_principals(&self.path)?;
        let count = new_principals.len();
        let mut guard = self.principals.write().unwrap();
        guard.clear();
        guard.extend(new_principals);
        Ok(count)
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    fn load_principals(path: &str) -> Result<HashMap<String, PrincipalRecord>, AuthError> {
        let raw = fs::read_to_string(path).map_err(|e| AuthError::AuthFileLoad(e.to_string()))?;
        let parsed: AuthFile =
            toml::from_str(&raw).map_err(|e| AuthError::AuthFileLoad(e.to_string()))?;

        let mut principals = HashMap::new();
        for principal in parsed.principal {
            let tenant_id = Uuid::parse_str(&principal.tenant_id)
                .map_err(|e| AuthError::AuthFileLoad(e.to_string()))?;
            principals.insert(
                principal.username,
                PrincipalRecord {
                    password_sha256: principal.password_sha256.to_ascii_lowercase(),
                    tenant_id,
                },
            );
        }
        Ok(principals)
    }

    pub fn validate(&self, username: &str, password: &str) -> Option<Uuid> {
        let guard = self.principals.read().unwrap();
        let record = guard.get(username)?;
        let provided = sha256_hex(password);
        if constant_time_eq(record.password_sha256.as_bytes(), provided.as_bytes()) {
            Some(record.tenant_id)
        } else {
            None
        }
    }
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
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
    use std::io::Write;
    use tempfile::NamedTempFile;

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

    #[test]
    fn file_auth_validator_accepts_valid_credentials() {
        let tenant_id = Uuid::new_v4();
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            "[[principal]]\nusername = \"alice\"\npassword_sha256 = \"{}\"\ntenant_id = \"{}\"",
            sha256_hex("s3cret"),
            tenant_id
        )
        .unwrap();

        let validator = FileAuthValidator::from_path(file.path().to_str().unwrap()).unwrap();
        assert_eq!(validator.validate("alice", "s3cret"), Some(tenant_id));
    }

    #[test]
    fn file_auth_validator_rejects_invalid_credentials() {
        let tenant_id = Uuid::new_v4();
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            "[[principal]]\nusername = \"alice\"\npassword_sha256 = \"{}\"\ntenant_id = \"{}\"",
            sha256_hex("s3cret"),
            tenant_id
        )
        .unwrap();

        let validator = FileAuthValidator::from_path(file.path().to_str().unwrap()).unwrap();
        assert_eq!(validator.validate("alice", "wrong"), None);
        assert_eq!(validator.validate("bob", "s3cret"), None);
    }

    #[test]
    fn reload_picks_up_new_password() {
        let tenant_id = Uuid::new_v4();
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            "[[principal]]\nusername = \"alice\"\npassword_sha256 = \"{}\"\ntenant_id = \"{}\"",
            sha256_hex("old_password"),
            tenant_id
        )
        .unwrap();

        let path = file.path().to_str().unwrap().to_string();
        let validator = FileAuthValidator::from_path(&path).unwrap();
        assert_eq!(validator.validate("alice", "old_password"), Some(tenant_id));
        assert_eq!(validator.validate("alice", "new_password"), None);

        let new_tenant = Uuid::new_v4();
        let mut file2 = NamedTempFile::new().unwrap();
        writeln!(
            file2,
            "[[principal]]\nusername = \"alice\"\npassword_sha256 = \"{}\"\ntenant_id = \"{}\"",
            sha256_hex("new_password"),
            new_tenant
        )
        .unwrap();
        std::fs::copy(file2.path(), &path).unwrap();

        let count = validator.reload().unwrap();
        assert_eq!(count, 1);
        assert_eq!(validator.validate("alice", "old_password"), None);
        assert_eq!(
            validator.validate("alice", "new_password"),
            Some(new_tenant)
        );
    }

    #[test]
    fn reload_preserves_old_principals_on_bad_file() {
        let tenant_id = Uuid::new_v4();
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            "[[principal]]\nusername = \"alice\"\npassword_sha256 = \"{}\"\ntenant_id = \"{}\"",
            sha256_hex("s3cret"),
            tenant_id
        )
        .unwrap();

        let path = file.path().to_str().unwrap().to_string();
        let validator = FileAuthValidator::from_path(&path).unwrap();
        assert_eq!(validator.validate("alice", "s3cret"), Some(tenant_id));

        std::fs::write(&path, "this is invalid toml {{{").unwrap();
        let result = validator.reload();
        assert!(result.is_err());

        assert_eq!(validator.validate("alice", "s3cret"), Some(tenant_id));
    }

    #[test]
    fn reload_adds_and_removes_principals() {
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            "[[principal]]\nusername = \"alice\"\npassword_sha256 = \"{}\"\ntenant_id = \"{}\"",
            sha256_hex("password_a"),
            tenant_a
        )
        .unwrap();

        let path = file.path().to_str().unwrap().to_string();
        let validator = FileAuthValidator::from_path(&path).unwrap();
        assert_eq!(validator.validate("alice", "password_a"), Some(tenant_a));
        assert_eq!(validator.validate("bob", "password_b"), None);

        let mut file2 = NamedTempFile::new().unwrap();
        writeln!(
            file2,
            "[[principal]]\nusername = \"bob\"\npassword_sha256 = \"{}\"\ntenant_id = \"{}\"",
            sha256_hex("password_b"),
            tenant_b
        )
        .unwrap();
        std::fs::copy(file2.path(), &path).unwrap();

        validator.reload().unwrap();
        assert_eq!(validator.validate("alice", "password_a"), None);
        assert_eq!(validator.validate("bob", "password_b"), Some(tenant_b));
    }
}
