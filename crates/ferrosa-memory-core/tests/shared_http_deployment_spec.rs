use std::io::Write;

use ferrosa_memory_core::auth::{self, AuthError, FileAuthValidator};
use ferrosa_memory_core::config::{parse_config, validate_shared_http_config};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use uuid::Uuid;

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn shared_http_toml(server_body: &str) -> String {
    format!(
        r#"
[ferrosa]
contact_points = ["localhost:19042"]

[server]
transport = "http"
{server_body}
"#
    )
}

fn write_auth_file(entries: &[(&str, &str, Uuid)]) -> NamedTempFile {
    let mut file = NamedTempFile::new().expect("create auth file");
    for (username, password, tenant_id) in entries {
        writeln!(
            file,
            "[[principal]]\nusername = \"{username}\"\npassword_sha256 = \"{}\"\ntenant_id = \"{tenant_id}\"",
            sha256_hex(password)
        )
        .expect("write principal");
    }
    file
}

/// T-U-001
/// Given shared HTTP mode is enabled without an auth mapping source
/// When config validation runs
/// Then startup validation rejects the configuration before the listener binds.
#[test]
fn tu001_shared_http_requires_auth_backend() {
    let config = parse_config(&shared_http_toml(
        r#"
require_tls = true
cert_path = "/etc/ssl/cert.pem"
key_path = "/etc/ssl/key.pem"
"#,
    ))
    .expect("parse shared-http config");

    let err = validate_shared_http_config(&config).expect_err("auth file is required");
    assert!(err.to_string().contains("auth_file"));
}

/// T-U-002
/// Given shared HTTP mode is enabled with TLS required but secret paths missing
/// When config validation runs
/// Then startup validation fails with a clear secret-wiring error.
#[test]
fn tu002_tls_secret_wiring_validates_on_startup() {
    let config = parse_config(&shared_http_toml(
        r#"
require_tls = true
auth_file = "/etc/ferrosa/auth.toml"
"#,
    ))
    .expect("parse shared-http config");

    let err = validate_shared_http_config(&config).expect_err("TLS secret paths are required");
    assert!(err.to_string().contains("cert_path and key_path"));
}

/// T-U-003
/// Given a valid auth mapping source
/// When validation is asked about an unknown principal or wrong password
/// Then no tenant context is returned.
#[test]
fn tu003_invalid_credentials_reject_cleanly() {
    let tenant_id = Uuid::new_v4();
    let file = write_auth_file(&[("alice", "s3cret", tenant_id)]);
    let validator =
        FileAuthValidator::from_path(file.path().to_str().unwrap()).expect("load auth validator");

    assert_eq!(validator.validate("alice", "wrong"), None);
    assert_eq!(validator.validate("bob", "s3cret"), None);
    assert!(matches!(
        auth::authenticate_http("alice", "wrong", |u, p| validator.validate(u, p)),
        Err(AuthError::InvalidCredentials)
    ));
}

/// T-U-004
/// Given shared HTTP mode uses local stdio tenant fallback settings
/// When the mode is changed to HTTP
/// Then the validator rejects the mixed configuration.
#[test]
fn tu004_http_mode_forbids_tenant_fallback() {
    let config = parse_config(&shared_http_toml(
        r#"
require_tls = true
cert_path = "/etc/ssl/cert.pem"
key_path = "/etc/ssl/key.pem"
auth_file = "/etc/ferrosa/auth.toml"
tenant_id = "00000000-0000-0000-0000-000000000001"
"#,
    ))
    .expect("parse shared-http config");

    let err = validate_shared_http_config(&config).expect_err("tenant fallback must be rejected");
    assert!(err.to_string().contains("tenant_id fallback"));
}

/// T-C-001
/// Auth backend contract: one principal maps to one tenant and fails closed otherwise.
#[test]
fn tc001_auth_backend_maps_principal_to_one_tenant() {
    let alice_tenant = Uuid::new_v4();
    let bob_tenant = Uuid::new_v4();
    let file = write_auth_file(&[
        ("alice", "alice-pass", alice_tenant),
        ("bob", "bob-pass", bob_tenant),
    ]);
    let validator =
        FileAuthValidator::from_path(file.path().to_str().unwrap()).expect("load auth validator");

    assert_eq!(
        validator.validate("alice", "alice-pass"),
        Some(alice_tenant)
    );
    assert_eq!(validator.validate("bob", "bob-pass"), Some(bob_tenant));
    assert_eq!(validator.validate("alice", "bob-pass"), None);
    assert_eq!(validator.validate("charlie", "alice-pass"), None);

    let ctx = auth::authenticate_http("alice", "alice-pass", |u, p| validator.validate(u, p))
        .expect("principal should authenticate");
    assert_eq!(ctx.tenant_id, alice_tenant);
    assert_eq!(ctx.session_origin, "http");
}
