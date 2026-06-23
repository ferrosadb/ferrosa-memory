//! Config-robustness Tier B — boot the REAL `ferrosa-memory-mcp` process per
//! config against the live test cluster and assert it never CRASHES on startup.
//!
//! Tier A (`config_property.rs`) fuzzes `parse_config` + the validators
//! hermetically. Tier B exercises the part Tier A structurally can't: the full
//! startup path — config→runtime construction, UUID parsing (tenant/session/viz),
//! socket bind, viz spawn, CQL connect. For each config we spawn the binary and
//! classify its exit:
//!
//!   * killed by a SIGNAL (SIGSEGV/SIGABRT/…)  -> CRASH  (fail)
//!   * exited with code 101 (Rust panic)       -> CRASH  (fail)
//!   * exited 0, or any other non-zero code    -> clean  (pass — fail-loud is OK)
//!   * still running after the grace window     -> started/serving (pass; killed)
//!
//! Migrations are disabled for the spawned process so it never mutates the
//! shared cluster schema (and can't race the migration_drift tests). The configs
//! point at the test cluster so startup proceeds deep before any clean exit.
//!
//! Cluster-gated: requires FERROSA_TEST_CQL_PORT + a built `ferrosa-memory-mcp`.
//! Runs in the integration job. This is a curated, high-risk config set (the PoC
//! for the boot tier); random-sampled boots from the Tier-A strategies are a
//! follow-on (see the epic).

#![cfg(unix)]

use std::fs::File;
use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn workspace_root() -> &'static str {
    env!("CARGO_MANIFEST_DIR")
        .strip_suffix("/crates/ferrosa-memory-core")
        .unwrap_or(env!("CARGO_MANIFEST_DIR"))
}

fn mcp_binary() -> String {
    let root = workspace_root();
    for profile in ["debug", "release"] {
        let p = format!("{root}/target/{profile}/ferrosa-memory-mcp");
        if Path::new(&p).exists() {
            return p;
        }
    }
    panic!(
        "ferrosa-memory-mcp binary not found under {root}/target/{{debug,release}}. \
         Build first with: cargo build -p ferrosa-memory-mcp"
    );
}

fn cql_endpoint() -> String {
    let host = std::env::var("FERROSA_TEST_CQL_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port = std::env::var("FERROSA_TEST_CQL_PORT")
        .expect("FERROSA_TEST_CQL_PORT must be set — this is a cluster-gated test (run --ignored)");
    format!("{host}:{port}")
}

fn test_keyspace() -> String {
    std::env::var("FERROSA_TEST_KEYSPACE").unwrap_or_else(|_| "agent_memory_test".into())
}

/// Curated high-risk configs: each targets a startup path that could panic —
/// invalid UUIDs (tenant/session/viz), HTTP/TLS/auth combinations, extreme
/// ports, garbage transport, NaN/huge numerics, empty contact points.
fn boot_configs(endpoint: &str, keyspace: &str) -> Vec<(&'static str, String)> {
    let base = format!(
        "[ferrosa]\ncontact_points = [\"{endpoint}\"]\nkeyspace = \"{keyspace}\"\n\
         username = \"ferrosa_user\"\npassword = \"ferrosa_user\"\n"
    );
    vec![
        (
            "stdio-minimal",
            format!("[server]\ntransport = \"stdio\"\n{base}"),
        ),
        (
            "tenant-invalid-uuid",
            format!("[server]\ntransport = \"stdio\"\ntenant_id = \"not-a-uuid\"\n{base}"),
        ),
        (
            "session-invalid-uuid",
            format!("[server]\ntransport = \"stdio\"\nsession_id = \"zzz-not-a-uuid\"\n{base}"),
        ),
        (
            "viz-http-no-tenant",
            format!(
                "[server]\ntransport = \"http\"\nbind_addr = \"127.0.0.1\"\nrequire_tls = false\n\
                 auth_file = \"/nonexistent/auth.toml\"\n{base}[viz]\nenabled = true\n"
            ),
        ),
        (
            "viz-tenant-invalid-uuid",
            format!(
                "[server]\ntransport = \"stdio\"\n{base}[viz]\nenabled = true\ntenant_id = \"not-a-uuid\"\n"
            ),
        ),
        (
            "http-port-zero",
            format!(
                "[server]\ntransport = \"http\"\nbind_addr = \"127.0.0.1\"\nhttp_port = 0\n\
                 require_tls = false\nauth_file = \"/nonexistent/auth.toml\"\n{base}"
            ),
        ),
        (
            "http-require-tls-missing-cert",
            format!(
                "[server]\ntransport = \"http\"\nbind_addr = \"127.0.0.1\"\nrequire_tls = true\n{base}"
            ),
        ),
        (
            "empty-contact-points",
            format!(
                "[server]\ntransport = \"stdio\"\n[ferrosa]\ncontact_points = []\nkeyspace = \"{keyspace}\"\n"
            ),
        ),
        (
            "garbage-transport",
            format!("[server]\ntransport = \"banana\"\n{base}"),
        ),
        (
            "huge-numerics",
            format!(
                "[server]\ntransport = \"stdio\"\nidle_consolidation_seconds = 18446744073709551615\n\
                 stale_edge_max_days = 4294967295\nrequest_timeout_seconds = 18446744073709551615\n{base}"
            ),
        ),
        (
            "decay-nan",
            format!("[server]\ntransport = \"stdio\"\nedge_decay_factor = nan\n{base}"),
        ),
        (
            "extreme-viz-port-valid-tenant",
            format!(
                "[server]\ntransport = \"stdio\"\n{base}[viz]\nenabled = true\nport = 65535\n\
                 tenant_id = \"00000000-0000-0000-0000-000000000001\"\n"
            ),
        ),
    ]
}

/// Spawn the binary with `config`, wait up to `grace`, and return the classified
/// outcome: `Some(reason)` if it CRASHED (signal death / panic exit 101),
/// `None` if it exited cleanly or was still running (started) at the deadline.
fn boot_and_classify(binary: &str, name: &str, config: &str, grace: Duration) -> Option<String> {
    let dir = std::env::temp_dir();
    let cfg_path = dir.join(format!("fmem-bootfuzz-{name}.toml"));
    let err_path = dir.join(format!("fmem-bootfuzz-{name}.stderr"));
    std::fs::write(&cfg_path, config).expect("write temp config");
    let err_file = File::create(&err_path).expect("create stderr capture file");

    let mut child = Command::new(binary)
        .env("FERROSA_MEMORY_CONFIG", &cfg_path)
        // Never mutate the shared cluster schema from a fuzzed boot.
        .env("FERROSA_MIGRATIONS_ENABLED", "false")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err_file))
        .spawn()
        .expect("spawn ferrosa-memory-mcp");

    let deadline = Instant::now() + grace;
    let status = loop {
        match child.try_wait().expect("try_wait on child") {
            Some(s) => break Some(s),
            None if Instant::now() >= deadline => {
                // Still alive = it started (server loop) or is retrying a bad
                // connection. Either way it did not crash. Reap it.
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(200)),
        }
    };

    let outcome = status.and_then(|s| {
        let signal = s.signal();
        let code = s.code();
        let crashed = signal.is_some() || code == Some(101);
        crashed.then(|| {
            let mut stderr = String::new();
            let _ = File::open(&err_path).and_then(|mut f| f.read_to_string(&mut stderr));
            let tail = stderr
                .lines()
                .rev()
                .take(15)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "config '{name}' CRASHED on startup (signal={signal:?}, exit_code={code:?}).\n\
                 --- stderr tail ---\n{tail}\n--- config ---\n{config}"
            )
        })
    });

    let _ = std::fs::remove_file(&cfg_path);
    let _ = std::fs::remove_file(&err_path);
    outcome
}

/// Tier B: no curated config may crash (signal death / panic) the MCP server on
/// startup. A clean exit (validation error, connect failure) or a started server
/// both pass — only a hard crash fails.
#[test]
#[ignore = "requires live test cluster + built ferrosa-memory-mcp; run with --ignored"]
fn config_boot_never_crashes_against_sandbox() {
    let binary = mcp_binary();
    let endpoint = cql_endpoint();
    let keyspace = test_keyspace();

    let crashes: Vec<String> = boot_configs(&endpoint, &keyspace)
        .into_iter()
        .filter_map(|(name, config)| {
            boot_and_classify(&binary, name, &config, Duration::from_secs(8))
        })
        .collect();

    assert!(
        crashes.is_empty(),
        "{} config(s) crashed ferrosa-memory-mcp on startup (signal death or Rust panic exit 101) \
         — these are real robustness bugs:\n\n{}",
        crashes.len(),
        crashes.join("\n\n========================================\n\n")
    );
}
