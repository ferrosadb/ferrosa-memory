# Auth File Hot-Reload

## Problem

`FileAuthValidator::from_path` reads the auth file once at startup (`fs::read_to_string`) and caches principals in a `HashMap`. When the auth file is updated (password rotation, new principals), the running server continues using the stale cached credentials. This caused a real production incident: the containerized ferrosa-memory-mcp rejected valid credentials after an auth file update because it was started before the password was rotated.

## Required Changes

### 1. SIGHUP-triggered reload

When the process receives `SIGHUP`, re-read the auth file and swap the in-memory `HashMap`:

```rust
// In auth.rs
impl FileAuthValidator {
    pub fn reload(&self, path: &str) -> Result<(), AuthError> {
        let new_validator = Self::from_path(path)?;
        // swap self.principals with new_validator.principals
    }
}
```

In the HTTP server loop, register a `tokio::signal` handler for SIGHUP that calls `validator.reload(path)`.

### 2. File-system watch (optional, nice-to-have)

Use `notify` crate to watch the auth file for changes. On write, trigger the same reload. This is lower-latency than SIGHUP for interactive use but adds a dependency.

### 3. Reload logging

On every reload (SIGHUP or file watch), log:
- `INFO` with principal count and file path on success
- `ERROR` with parse details on failure (keep old principals active — don't wipe on bad file)

### 4. Container integration

The podman container bind-mounts the auth file (`/Users/bkearns/src/ferrosa-memory/.runtime -> /run/secrets/ferrosa-memory`). SIGHUP works through podman: `podman kill --signal SIGHUP <container>`.

## Invariants

- On reload failure, preserve the last-known-good principals (fail-open on file error, fail-closed on auth check)
- Reload is atomic: swap the entire HashMap, not individual entries
- No request drops: in-flight requests complete with the old validator; new requests use the reloaded one

## Verification

1. Start server with auth file containing `codex:password1` → auth succeeds
2. Change auth file to `codex:password2` → auth still uses `password1` (stale)
3. Send SIGHUP → auth now uses `password2`, `password1` rejected
4. Corrupt auth file (bad TOML) → send SIGHUP → ERROR logged, old principals still active, `password2` still works
5. Fix auth file → send SIGHUP → new principals loaded
6. Podman: `podman kill --signal SIGHUP ferrosa-memory_ferrosa-memory-mcp_1` → reload triggers

## References

- `crates/ferrosa-memory-core/src/auth.rs:66` — `FileAuthValidator::from_path` (one-shot read)
- `crates/ferrosa-memory-mcp/src/main.rs:148` — `build_http_validator` (wires validator at startup)
- Incident: April 11 2026 — container rejected valid credentials after auth file rotation