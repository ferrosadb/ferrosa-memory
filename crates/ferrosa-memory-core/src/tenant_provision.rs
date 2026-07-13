//! Per-install tenant + credential provisioning.
//!
//! Every install should get its OWN tenant UUID rather than sharing a
//! hardcoded one (the dev/example tenants). The server derives a request's
//! tenant from the authenticated principal (see [`crate::auth`]) and rejects a
//! client-supplied `tenant_id` that doesn't match, so the tenant must be
//! consistent across three artifacts: the MCP config (`[server]`/`[viz]`), the
//! HTTP auth file (`[[principal]]` tenants), and the hook env
//! (`FERROSA_MEMORY_TENANT_ID`). This module holds the pure transforms the
//! `ferrosa-memory provision-tenant` subcommand applies to those files;
//! randomness and file IO live in the caller so the transforms stay testable.
//!
//! Provisioning is idempotent: a real (non-placeholder) tenant already present
//! is reused, never regenerated — regenerating would orphan all memory written
//! under the old tenant.

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Shared placeholder/example tenants that are NOT a real per-install
/// identity. Provisioning replaces any of these with a freshly generated UUID.
pub const PLACEHOLDER_TENANTS: &[&str] = &[
    "00000000-0000-0000-0000-000000000000", // nil
    "00000000-0000-0000-0000-000000000001", // example config [server]
    "11111111-1111-1111-1111-111111111111", // example ferrosa_admin
    "22222222-2222-2222-2222-222222222222", // example ferrosa_user
    "9a5f8fbf-d842-4d30-8ea5-1aa931e618a8", // historical dev/hook default
];

/// True if `id` is a shared placeholder rather than a real per-install tenant.
pub fn is_placeholder_tenant(id: Uuid) -> bool {
    PLACEHOLDER_TENANTS
        .iter()
        .any(|p| Uuid::parse_str(p).is_ok_and(|placeholder| placeholder == id))
}

/// Pick the tenant to provision: keep an existing real (non-placeholder)
/// tenant so re-installs are idempotent and never orphan data; otherwise adopt
/// the freshly generated one.
pub fn resolve_tenant(existing: Option<Uuid>, fresh: Uuid) -> Uuid {
    match existing {
        Some(id) if !is_placeholder_tenant(id) => id,
        _ => fresh,
    }
}

/// Lowercase SHA-256 hex of `password` — the exact form
/// `auth::FileAuthValidator` compares against `password_sha256`.
pub fn password_hash(password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    hex::encode(hasher.finalize())
}

fn is_header(trimmed: &str) -> bool {
    trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2
}

/// True if `trimmed` is an assignment of `key` (e.g. `key = ...`, `key=...`),
/// not a comment and not a different key with `key` as a prefix.
fn is_assignment_of(trimmed: &str, key: &str) -> bool {
    if trimmed.starts_with('#') {
        return false;
    }
    match trimmed.strip_prefix(key) {
        Some(rest) => rest.trim_start().starts_with('='),
        None => false,
    }
}

/// Set `key = "value"` inside the `[section]` table of a TOML document,
/// preserving every other line and comment. Inserts the key if the section
/// exists without it, and appends the section if it's absent entirely. Only
/// the first occurrence of `[section]` is targeted.
pub fn set_in_section(doc: &str, section: &str, key: &str, value: &str) -> String {
    let header = format!("[{section}]");
    let new_line = format!("{key} = \"{value}\"");
    let mut out: Vec<String> = Vec::new();
    let mut in_section = false;
    let mut wrote = false;
    let mut section_seen = false;

    for line in doc.lines() {
        let trimmed = line.trim();
        if is_header(trimmed) {
            // Leaving the target section without having written the key: insert
            // it just before the next section header.
            if in_section && !wrote {
                out.push(new_line.clone());
                wrote = true;
            }
            in_section = !section_seen && trimmed == header;
            if in_section {
                section_seen = true;
            }
            out.push(line.to_string());
            continue;
        }
        if in_section && !wrote && is_assignment_of(trimmed, key) {
            out.push(new_line.clone());
            wrote = true;
            continue;
        }
        out.push(line.to_string());
    }

    // Reached EOF still inside the target section without writing.
    if in_section && !wrote {
        out.push(new_line.clone());
    }
    // Section never appeared: append it.
    if !section_seen {
        if out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.push(header);
        out.push(new_line);
    }

    finish(out, doc)
}

/// Remove every active assignment of `key` from a TOML table while preserving
/// comments, unrelated fields, and the document's trailing-newline style.
///
/// HTTP deployments derive tenant identity exclusively from the authenticated
/// principal, so this lets provisioning repair an obsolete `[server]`
/// `tenant_id` fallback instead of leaving the configuration invalid.
pub fn remove_from_section(doc: &str, section: &str, key: &str) -> String {
    let header = format!("[{section}]");
    let mut out = Vec::new();
    let mut in_section = false;

    for line in doc.lines() {
        let trimmed = line.trim();
        if is_header(trimmed) {
            in_section = trimmed == header;
        }
        if in_section && is_assignment_of(trimmed, key) {
            continue;
        }
        out.push(line.to_string());
    }

    finish(out, doc)
}

/// Set `tenant_id = "<tenant>"` inside every `[[principal]]` array-table of a
/// TOML auth document, preserving all other lines. Inserts the key into a
/// principal block that lacks it.
pub fn set_each_principal_tenant(doc: &str, tenant: &str) -> String {
    let new_line = format!("tenant_id = \"{tenant}\"");
    let mut out: Vec<String> = Vec::new();
    let mut in_principal = false;
    let mut wrote_current = false;

    for line in doc.lines() {
        let trimmed = line.trim();
        if is_header(trimmed) {
            // Close out the previous principal block if it had no tenant_id.
            if in_principal && !wrote_current {
                out.push(new_line.clone());
            }
            in_principal = trimmed == "[[principal]]";
            wrote_current = false;
            out.push(line.to_string());
            continue;
        }
        if in_principal && !wrote_current && is_assignment_of(trimmed, "tenant_id") {
            out.push(new_line.clone());
            wrote_current = true;
            continue;
        }
        out.push(line.to_string());
    }
    if in_principal && !wrote_current {
        out.push(new_line);
    }

    finish(out, doc)
}

fn finish(lines: Vec<String>, original: &str) -> String {
    let mut s = lines.join("\n");
    if original.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// A generated principal credential for a freshly created auth file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedCredential {
    pub username: String,
    pub password: String,
    pub tenant_id: Uuid,
}

/// Render an `http-auth.toml` for the given principals + tenant. Passwords are
/// stored only as their SHA-256 hash; the plaintext lives in
/// [`GeneratedCredential`] so the caller can surface it once (it is otherwise
/// unrecoverable).
pub fn render_auth_file(creds: &[GeneratedCredential]) -> String {
    let mut s = String::from(
        "# Generated by `ferrosa-memory provision-tenant`. Per-install\n\
         # credentials + tenant. Passwords are stored as SHA-256 only.\n",
    );
    for c in creds {
        s.push_str(&format!(
            "\n[[principal]]\nusername = \"{}\"\npassword_sha256 = \"{}\"\ntenant_id = \"{}\"\n",
            c.username,
            password_hash(&c.password),
            c.tenant_id,
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_detected_real_tenants_are_not() {
        assert!(is_placeholder_tenant(
            Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap()
        ));
        assert!(is_placeholder_tenant(
            Uuid::parse_str("9a5f8fbf-d842-4d30-8ea5-1aa931e618a8").unwrap()
        ));
        assert!(is_placeholder_tenant(Uuid::nil()));
        assert!(!is_placeholder_tenant(
            Uuid::parse_str("0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").unwrap()
        ));
    }

    #[test]
    fn resolve_keeps_real_tenant_replaces_placeholder() {
        let fresh = Uuid::parse_str("0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").unwrap();
        let real = Uuid::parse_str("aaaabbbb-cccc-4ddd-8eee-ffff00001111").unwrap();
        // Idempotent: a real existing tenant is kept.
        assert_eq!(resolve_tenant(Some(real), fresh), real);
        // Placeholder or absent -> adopt fresh.
        assert_eq!(resolve_tenant(Some(Uuid::nil()), fresh), fresh);
        assert_eq!(resolve_tenant(None, fresh), fresh);
    }

    #[test]
    fn password_hash_matches_known_dev_value() {
        // examples/http-auth.toml documents password=ferrosa_user -> this hash.
        assert_eq!(
            password_hash("ferrosa_user"),
            "13f073c47057958ae2bcee1b01a51469d147a5db1fa437aa934090b996285bde"
        );
    }

    #[test]
    fn set_in_section_replaces_existing_key() {
        let doc = "[server]\ntransport = \"http\"\ntenant_id = \"00000000-0000-0000-0000-000000000001\"\n\n[viz]\nenabled = true\n";
        let out = set_in_section(doc, "server", "tenant_id", "NEW");
        assert!(out.contains("tenant_id = \"NEW\""));
        assert!(!out.contains("000000000001"));
        // Other sections/keys preserved.
        assert!(out.contains("transport = \"http\""));
        assert!(out.contains("[viz]"));
        assert!(out.contains("enabled = true"));
    }

    #[test]
    fn set_in_section_inserts_missing_key_into_existing_section() {
        let doc = "[server]\ntransport = \"http\"\n\n[viz]\nenabled = true\n";
        let out = set_in_section(doc, "viz", "tenant_id", "T");
        assert!(out.contains("[viz]"));
        assert!(out.contains("tenant_id = \"T\""));
        // The inserted key is within [viz], not [server].
        let viz_idx = out.find("[viz]").unwrap();
        assert!(out[viz_idx..].contains("tenant_id = \"T\""));
    }

    #[test]
    fn set_in_section_appends_missing_section() {
        let doc = "[server]\ntransport = \"http\"\n";
        let out = set_in_section(doc, "viz", "tenant_id", "T");
        assert!(out.contains("[viz]"));
        assert!(out.contains("tenant_id = \"T\""));
    }

    #[test]
    fn set_in_section_ignores_commented_key() {
        let doc = "[server]\n# tenant_id = \"commented\"\ntransport = \"http\"\n";
        let out = set_in_section(doc, "server", "tenant_id", "T");
        // The comment is preserved and a real assignment is added.
        assert!(out.contains("# tenant_id = \"commented\""));
        assert!(out.contains("tenant_id = \"T\""));
    }

    #[test]
    fn remove_from_section_removes_only_the_target_assignment() {
        let doc = "[server]\ntransport = \"http\"\ntenant_id = \"OLD\"\n# tenant_id = \"commented\"\n\n[viz]\ntenant_id = \"OLD\"\n";
        let out = remove_from_section(doc, "server", "tenant_id");
        assert!(out.contains("[server]\ntransport = \"http\""));
        assert!(out.contains("# tenant_id = \"commented\""));
        assert!(out.contains("[viz]\ntenant_id = \"OLD\""));
        assert!(!out.contains("[server]\ntransport = \"http\"\ntenant_id = \"OLD\""));
    }

    #[test]
    fn set_each_principal_tenant_updates_all_blocks() {
        let doc = "[[principal]]\nusername = \"a\"\ntenant_id = \"OLD\"\n\n[[principal]]\nusername = \"b\"\npassword_sha256 = \"x\"\n";
        let out = set_each_principal_tenant(doc, "NEW");
        // First block's tenant replaced; second block (no tenant) gets one.
        assert_eq!(out.matches("tenant_id = \"NEW\"").count(), 2);
        assert!(!out.contains("OLD"));
        assert!(out.contains("username = \"a\""));
        assert!(out.contains("username = \"b\""));
        assert!(out.contains("password_sha256 = \"x\""));
    }

    #[test]
    fn render_auth_file_round_trips_and_hashes() {
        let creds = vec![GeneratedCredential {
            username: "ferrosa_user".into(),
            password: "ferrosa_user".into(),
            tenant_id: Uuid::parse_str("aaaabbbb-cccc-4ddd-8eee-ffff00001111").unwrap(),
        }];
        let toml = render_auth_file(&creds);
        assert!(toml.contains("[[principal]]"));
        assert!(toml.contains("username = \"ferrosa_user\""));
        assert!(toml.contains(&format!(
            "password_sha256 = \"{}\"",
            password_hash("ferrosa_user")
        )));
        // Plaintext password is never written to the file.
        assert!(!toml.contains("password = \"ferrosa_user\""));
        // It parses back as valid TOML.
        let _: toml::Value = toml::from_str(&toml).expect("valid TOML");
    }
}
