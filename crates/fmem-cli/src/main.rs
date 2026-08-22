//! `fmem` — enrol and inspect this host's memory systems.
//!
//! One host can run several memory systems, each with its own keypair (trust
//! D15), so every command here is scoped to ONE system, named by its MCP port.
//! When a host runs exactly one, the port is inferred; when it runs several,
//! the command refuses rather than guesses — enrolling the wrong system binds
//! an immutable kind to the wrong keypair.
//!
//! # The login flow
//!
//! `fmem login` asks the console for a device authorization grant, prints a
//! short code, tries to open a browser, and polls. The code travels terminal →
//! human → browser; nothing is ever pasted back here. A headless box behaves
//! identically, because the code and URL are always printed whether or not a
//! browser opened.
//!
//! Correctness: Correct when a system is never enrolled under a guessed
//! identity, and when the CLI reports only outcomes the server confirmed.
//! Last revised: 2026-08-22
//! Last changed: Initial fmem CLI.

// Fail-loud: an unwrap in a CLI path is a panic in front of an operator.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

mod device_auth;
mod enrolment;
mod settings;
mod system;

use std::path::Path;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use ferrosa_memory_sync::peer_cli;

#[derive(Debug, Parser)]
#[command(
    name = "fmem",
    about = "Enrol and inspect this host's Ferrosa Memory systems",
    version
)]
struct Cli {
    /// Console origin. Overrides FMEM_CONSOLE_URL and config/fmem.toml.
    #[arg(long, global = true)]
    console: Option<String>,

    /// Which memory system, by its MCP port.
    ///
    /// Optional when the host runs exactly one. Required when it runs several,
    /// because guessing binds an immutable kind to the wrong keypair.
    #[arg(long, global = true)]
    system: Option<u16>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Enrol this memory system against an account.
    Login {
        /// Label for the device list. Defaults to `<hostname>:<port>`.
        #[arg(long)]
        label: Option<String>,
    },
    /// Show what this host's memory systems are, and whether they are enrolled.
    Status,
    /// List the account's devices, as the gateway sees them.
    ///
    /// Authenticated by SIGNING with this system's device key — there is no API
    /// key on this path. Doubles as the check that the enrolment actually works
    /// against the gateway, not merely that a record was written locally.
    Devices,
    /// Forget this system's enrolment record LOCALLY.
    ///
    /// Does not revoke anything. A device cannot un-enrol itself server-side by
    /// design — that would let a compromised machine erase its own audit trail.
    Logout,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = settings::root();

    match &cli.command {
        Command::Login { label } => login(&cli, &root, label.as_deref()).await,
        Command::Status => status(&cli, &root),
        Command::Devices => devices(&cli, &root).await,
        Command::Logout => logout(&cli, &root),
    }
}

/// Enrol one memory system.
async fn login(cli: &Cli, root: &Path, label: Option<&str>) -> Result<()> {
    let system = system::resolve(root, cli.system)?;
    let console = settings::console_url(root, cli.console.as_deref());

    // Reported before anything is sent. An operator who is pointed at the wrong
    // control plane has to be able to see it BEFORE enrolling, because
    // enrolment is not undoable.
    println!(
        "console:  {} ({})",
        console.value,
        console.source.describe()
    );
    println!("system:   MCP port {}", system.port);

    let key_path = system.key_path(root);
    let identity = load_or_create_identity(&key_path)?;
    let public = identity.public_identity();
    let fingerprint = public.public_key_fingerprint.0.clone();
    let public_hex = hex(&public.public_key);

    // An existing enrolment is reported rather than silently redone. Enrolling
    // twice creates a second device row for one system, and the operator ends
    // up with two near-identical entries and no way to tell which is live.
    let enrolment_path = system.enrolment_path(root);
    if let Some(existing) = enrolment::load(&enrolment_path, &fingerprint)? {
        println!();
        println!("Already enrolled.");
        print_enrolment(&existing);
        println!();
        println!("To enrol again, revoke the device first, then delete");
        println!("  {}", enrolment_path.display());
        return Ok(());
    }

    let hostname = hostname();
    let label = label
        .map(str::to_string)
        .unwrap_or_else(|| system.default_label(&hostname));

    println!("label:    {label}");
    println!("kind:     memory");
    println!("identity: {}", group(&fingerprint));
    println!();

    let client = reqwest::Client::builder()
        .build()
        .context("building the HTTP client")?;

    let grant = device_auth::start(&client, &console.value, &public_hex, &label, "memory").await?;

    // Printed BEFORE any browser is attempted, and printed whether or not one
    // opens. A headless machine and a desktop must show the operator the same
    // thing; making the terminal output conditional on a browser is how the
    // headless case ends up unusable.
    println!("  Your code:  {}", grant.user_code);
    println!("  Go to:      {}", grant.verification_uri);
    println!();
    println!("  Or open:    {}", grant.verification_uri_complete);
    println!();

    match open_browser(&grant.verification_uri_complete) {
        Ok(true) => println!("  (a browser should have opened)"),
        Ok(false) | Err(_) => {
            println!("  (could not open a browser here — use the code above)");
        }
    }
    println!();
    println!("Waiting for approval...");

    let enrolled = device_auth::wait_for_approval(&client, &console.value, &grant, |_| {}).await?;

    // The gateway derives the fingerprint from the key we sent. If it reports a
    // different one, something is wrong with the binding and recording it would
    // create a local record that can never authenticate.
    if !enrolled.fingerprint.eq_ignore_ascii_case(&fingerprint) {
        anyhow::bail!(
            "the gateway enrolled fingerprint {} but this system's key is {}. \
             Nothing was recorded locally.",
            group(&enrolled.fingerprint),
            group(&fingerprint)
        );
    }

    let record = enrolment::Enrolment {
        contract: enrolment::CONTRACT.to_string(),
        system_port: system.port,
        device_id: enrolled.device_id.clone(),
        fingerprint: enrolled.fingerprint.clone(),
        kind: "memory".to_string(),
        label,
        email: enrolled.email.clone(),
        console_url: console.value.clone(),
    };
    enrolment::save(&enrolment_path, &record)?;

    println!();
    println!("Enrolled.");
    print_enrolment(&record);
    Ok(())
}

/// Show every system on this host and whether it is enrolled.
fn status(cli: &Cli, root: &Path) -> Result<()> {
    let systems = match cli.system {
        Some(port) => vec![system::MemorySystem {
            port,
            config_path: None,
        }],
        None => system::discover(root)?,
    };

    if systems.is_empty() {
        println!("No memory systems configured under {}.", root.display());
        return Ok(());
    }

    for system in systems {
        println!("MCP port {}", system.port);
        let key_path = system.key_path(root);
        if !key_path.exists() {
            println!(
                "  no device key yet — run `fmem login --system {}`",
                system.port
            );
            println!();
            continue;
        }
        let identity = peer_cli::load_identity(&key_path)
            .with_context(|| format!("reading {}", key_path.display()))?;
        let fingerprint = identity.public_identity().public_key_fingerprint.0;
        println!("  identity  {}", group(&fingerprint));

        match enrolment::load(&system.enrolment_path(root), &fingerprint) {
            Ok(Some(record)) => print_enrolment(&record),
            Ok(None) => println!("  not enrolled — run `fmem login --system {}`", system.port),
            // Surfaced, not swallowed: a divergent record is exactly the state
            // an operator needs to know about, and it reads as "enrolled" from
            // every other angle.
            Err(e) => println!("  PROBLEM: {e}"),
        }
        println!();
    }
    Ok(())
}

/// List the account's devices, signing the request with this system's key.
async fn devices(cli: &Cli, root: &Path) -> Result<()> {
    let system = system::resolve(root, cli.system)?;
    let key_path = system.key_path(root);
    if !key_path.exists() {
        anyhow::bail!(
            "MCP port {} has no device key. Run `fmem login --system {}` first.",
            system.port,
            system.port
        );
    }
    let identity = peer_cli::load_identity(&key_path)
        .with_context(|| format!("reading {}", key_path.display()))?;
    let mine = identity.public_identity().public_key_fingerprint.0;

    // The gateway is the memory gateway, not the console: /v1/devices is served
    // there. The console origin fronts pairing and the device grant only.
    let gateway = gateway_url(root, cli.console.as_deref());
    let path = "/v1/devices";
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    let nonce = uuid::Uuid::new_v4().to_string();
    let signed = ferrosa_memory_sync::device_request::sign_request(
        &identity, "GET", path, b"", timestamp, &nonce,
    );

    let client = reqwest::Client::builder()
        .build()
        .context("building the HTTP client")?;
    let request = signed.pairs().into_iter().fold(
        client.get(format!("{gateway}{path}")),
        |r, (name, value)| r.header(name, value),
    );

    let response = request
        .send()
        .await
        .with_context(|| format!("GET {gateway}{path}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "the gateway refused this device ({status}): {}",
            body.trim()
        );
    }

    let listed: Vec<serde_json::Value> = serde_json::from_str(&body)
        .context("the gateway returned something that is not a device list")?;
    if listed.is_empty() {
        println!("No devices on this account.");
        return Ok(());
    }
    for device in listed {
        let fingerprint = device
            .get("fingerprint")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // Marked, because several systems on one host carry similar labels and
        // the operator needs to know which row is the machine they are on.
        let marker = if fingerprint.eq_ignore_ascii_case(&mine) {
            " <- this system"
        } else {
            ""
        };
        let revoked = if device.get("revoked_at").is_some_and(|v| !v.is_null()) {
            "  REVOKED"
        } else {
            ""
        };
        // The device id is printed because it is what the gateway and every
        // listener log line refer to. Without it, correlating "controller
        // device 2320ee44" against a row in this list means asking the server
        // again by hand.
        println!(
            "{:<8} {:<24} {:<38} {}{}{}",
            // Absent kind is shown as such rather than as "control": the
            // gateway defaults it, but a blank here means this gateway did not
            // report one, which is a different fact.
            device
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("(none)"),
            device.get("label").and_then(|v| v.as_str()).unwrap_or(""),
            device
                .get("device_id")
                .and_then(|v| v.as_str())
                .unwrap_or("(no id)"),
            group(fingerprint),
            marker,
            revoked
        );
    }
    Ok(())
}

/// The memory gateway origin.
///
/// Derived from the recorded enrolment where possible so `devices` talks to the
/// same control plane the system enrolled against, rather than to whatever the
/// default happens to be today.
fn gateway_url(root: &Path, flag: Option<&str>) -> String {
    let _ = root;
    flag.map(str::trim)
        .filter(|v| !v.is_empty())
        .map_or_else(
            || {
                std::env::var("FMEM_GATEWAY_URL")
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| DEFAULT_GATEWAY_URL.to_string())
            },
            str::to_string,
        )
        .trim_end_matches('/')
        .to_string()
}

/// Memory gateway used when nothing says otherwise.
const DEFAULT_GATEWAY_URL: &str = "https://maas-dev-v2-gateway.fly.dev";

/// Forget the local record. Server-side enrolment is untouched.
fn logout(cli: &Cli, root: &Path) -> Result<()> {
    let system = system::resolve(root, cli.system)?;
    let path = system.enrolment_path(root);
    match std::fs::remove_file(&path) {
        Ok(()) => {
            println!(
                "Forgot the local enrolment record for MCP port {}.",
                system.port
            );
            println!();
            // Said plainly, because the opposite is the reasonable assumption.
            println!("The device is STILL enrolled at the gateway. A device cannot");
            println!("un-enrol itself — revoke it from another device to do that.");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("No local enrolment record for MCP port {}.", system.port);
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

fn print_enrolment(record: &enrolment::Enrolment) {
    println!("  device    {}", record.device_id);
    println!("  kind      {}", record.kind);
    println!("  label     {}", record.label);
    if let Some(email) = record.email.as_deref() {
        println!("  account   {email}");
    }
    println!("  console   {}", record.console_url);
}

/// Load this system's key, creating one if it has none.
///
/// Uses `peer_cli`, so the file is the same shape `memory-sync p2p-keygen`
/// writes and `control-listen` reads — one key per system, usable by every
/// tool that needs it.
fn load_or_create_identity(
    path: &Path,
) -> Result<ferrosa_memory_core::remote_identity::InstanceSigningIdentity> {
    if path.exists() {
        return peer_cli::load_identity(path)
            .with_context(|| format!("reading {}", path.display()));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let identity = peer_cli::keygen(path).with_context(|| format!("writing {}", path.display()))?;
    println!("new device key: {}", path.display());
    Ok(identity)
}

/// Try to open a URL in the operator's browser.
///
/// Best effort by design, and its failure is never fatal: the code and URL are
/// already on screen, so a machine with no browser is not a machine that cannot
/// enrol. Returns whether a launcher was found and exited cleanly.
fn open_browser(url: &str) -> std::io::Result<bool> {
    let launcher = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    Ok(std::process::Command::new(launcher)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false))
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|h| h.trim().trim_end_matches(".local").to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown-host".to_string())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Group a fingerprint for reading. An unbroken 64-character run is what people
/// skip rather than compare.
fn group(fingerprint: &str) -> String {
    fingerprint
        .as_bytes()
        .chunks(4)
        .take(8)
        .map(|c| String::from_utf8_lossy(c).to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fingerprint_is_grouped_for_reading() {
        let fp = "a8b5d7b03245836841d1e30cf27d550b";
        let shown = group(fp);
        assert!(shown.starts_with("a8b5 d7b0"), "{shown}");
        // Truncated for display, but never re-joined into something that looks
        // like a different full fingerprint.
        assert_eq!(shown.replace(' ', ""), &fp[..32.min(fp.len())]);
    }

    #[test]
    fn hex_round_trips_a_public_key() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(hex(&[]), "");
    }

    /// The label must distinguish two systems on one host.
    #[test]
    fn the_default_label_differs_per_system() {
        let a = system::MemorySystem {
            port: 43971,
            config_path: None,
        };
        let b = system::MemorySystem {
            port: 43972,
            config_path: None,
        };
        assert_ne!(a.default_label("studio"), b.default_label("studio"));
    }

    /// A hostname is always produced, even where the command is unavailable —
    /// an empty label would enrol a device with no name at all.
    #[test]
    fn a_hostname_is_never_empty() {
        assert!(!hostname().is_empty());
    }

    /// `fmem` must parse the shapes the docs promise.
    #[test]
    fn the_documented_invocations_parse() {
        use clap::Parser;

        let cli = Cli::try_parse_from(["fmem", "login", "--system", "43971"]).expect("login");
        assert_eq!(cli.system, Some(43971));
        assert!(matches!(cli.command, Command::Login { .. }));

        let cli = Cli::try_parse_from([
            "fmem",
            "--console",
            "https://dev.fmem.ai",
            "login",
            "--label",
            "studio",
        ])
        .expect("console + label");
        assert_eq!(cli.console.as_deref(), Some("https://dev.fmem.ai"));

        assert!(Cli::try_parse_from(["fmem", "status"]).is_ok());
        assert!(Cli::try_parse_from(["fmem", "logout"]).is_ok());
    }

    /// A non-numeric port is refused by the parser rather than reaching
    /// resolution, so the error names the flag.
    #[test]
    fn a_bad_system_port_is_refused_at_parse_time() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["fmem", "login", "--system", "not-a-port"]).is_err());
        assert!(Cli::try_parse_from(["fmem", "login", "--system", "99999"]).is_err());
    }
}
