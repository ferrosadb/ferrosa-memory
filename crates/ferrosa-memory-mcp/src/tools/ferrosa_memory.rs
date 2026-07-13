//! Native management CLI for local Ferrosa Memory installs.
//!
//! This binary owns user-facing install reconciliation (`setup`), health
//! inspection (`doctor`), and removal (`uninstall`). The serving path remains
//! `ferrosa-memory-mcp`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

mod native_setup_clients;
mod native_setup_plan;

use native_setup_clients::{
    ConfigScope, PreviewRequest, ToolProfile, preview_client_configs, supported_clients,
};
use native_setup_plan::{
    DEFAULT_CONTROL_PORT, DEFAULT_DATABASE_PORT, DataScope, InstallScope, NativeSetupOptions,
    NativeSetupPlan, plan_native_setup,
};

const MANAGED_PATH_BEGIN: &str = "# >>> ferrosa-memory >>>";
const MANAGED_PATH_END: &str = "# <<< ferrosa-memory <<<";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Setup,
    Doctor,
    Uninstall,
    ProvisionTenant,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cli {
    command: Command,
    yes: bool,
    dry_run: bool,
    system: bool,
    delete_data: bool,
    /// MCP config path for `provision-tenant`.
    config: Option<String>,
    /// HTTP auth file path for `provision-tenant`.
    auth_file: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let cli = parse_args(std::env::args().skip(1));
    match cli.command {
        Command::Setup => setup(&cli),
        Command::Doctor => doctor(&cli),
        Command::Uninstall => uninstall(&cli),
        Command::ProvisionTenant => provision_tenant(&cli),
        Command::Help => {
            print_help();
            Ok(())
        }
    }
}

fn parse_args<I, S>(args: I) -> Cli
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut command = Command::Help;
    let mut yes = false;
    let mut dry_run = false;
    let mut system = false;
    let mut delete_data = false;
    let mut config = None;
    let mut auth_file = None;

    let mut it = args.into_iter().map(Into::into);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "setup" => command = Command::Setup,
            "doctor" | "status" => command = Command::Doctor,
            "uninstall" => command = Command::Uninstall,
            "provision-tenant" => command = Command::ProvisionTenant,
            "-h" | "--help" | "help" => command = Command::Help,
            "-y" | "--yes" => yes = true,
            "--dry-run" => dry_run = true,
            "--system" => system = true,
            "--delete-data" => delete_data = true,
            "--config" => config = it.next(),
            "--auth-file" => auth_file = it.next(),
            _ => {
                eprintln!("unknown argument: {arg}");
                command = Command::Help;
            }
        }
    }

    Cli {
        command,
        yes,
        dry_run,
        system,
        delete_data,
        config,
        auth_file,
    }
}

fn print_help() {
    eprintln!(
        "ferrosa-memory\n\n\
         Usage:\n\
           ferrosa-memory setup [--system] [--dry-run] [--yes]\n\
           ferrosa-memory doctor\n\
           ferrosa-memory uninstall [--delete-data] [--dry-run] [--yes]\n\
           ferrosa-memory provision-tenant --config <path> [--auth-file <path>]\n"
    );
}

/// Provision a unique per-install tenant + credentials, idempotently.
///
/// Reads the MCP config (and auth file if present), resolves the tenant
/// (reusing an existing real one, else generating a fresh UUID), and writes it
/// consistently into the HTTP auth principals and `[viz].tenant_id`; stdio
/// configs also receive `[server].tenant_id`. HTTP mode deliberately removes
/// that legacy fallback because the authenticated principal is authoritative.
/// If the auth file is absent it generates one with random
/// per-install credentials. Emits `KEY=VALUE` lines on stdout for the
/// installer to thread into the hook env (the plaintext password is shown
/// once here and is otherwise unrecoverable).
fn provision_tenant(cli: &Cli) -> anyhow::Result<()> {
    use ferrosa_memory_core::tenant_provision as tp;
    use uuid::Uuid;

    let config_path = cli
        .config
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("provision-tenant requires --config <path>"))?;
    let config_doc = fs::read_to_string(config_path)
        .map_err(|e| anyhow::anyhow!("reading config {config_path}: {e}"))?;

    // The ferrosa_user principal's tenant in an existing auth file is the
    // authoritative existing identity; fall back to the config's [server].
    let auth_path = cli.auth_file.as_deref();
    let existing_auth = auth_path.and_then(|p| fs::read_to_string(p).ok());
    let existing = existing_auth
        .as_deref()
        .and_then(first_principal_tenant)
        .or_else(|| section_value(&config_doc, "server", "tenant_id"))
        .and_then(|s| Uuid::parse_str(&s).ok());

    let tenant = tp::resolve_tenant(existing, Uuid::new_v4());

    // HTTP derives tenant identity only from the authenticated principal. An
    // old release may have left the now-invalid server fallback behind; repair
    // it during provisioning so setup is idempotent and self-healing.
    let is_http = section_value(&config_doc, "server", "transport")
        .is_some_and(|transport| transport.eq_ignore_ascii_case("http"));
    let mut new_config = if is_http {
        tp::remove_from_section(&config_doc, "server", "tenant_id")
    } else {
        tp::set_in_section(&config_doc, "server", "tenant_id", &tenant.to_string())
    };
    new_config = tp::set_in_section(&new_config, "viz", "tenant_id", &tenant.to_string());

    // Auth file: update existing principals, or generate fresh credentials.
    let mut generated: Vec<tp::GeneratedCredential> = Vec::new();
    if let Some(path) = auth_path {
        if let Some(doc) = existing_auth {
            let updated = tp::set_each_principal_tenant(&doc, &tenant.to_string());
            write_if_changed(Path::new(path), &updated)?;
        } else {
            for username in ["ferrosa_admin", "ferrosa_user"] {
                generated.push(tp::GeneratedCredential {
                    username: username.to_string(),
                    // 128 bits of entropy from a v4 UUID; unique per install.
                    password: Uuid::new_v4().simple().to_string(),
                    tenant_id: tenant,
                });
            }
            fs::write(path, tp::render_auth_file(&generated))
                .map_err(|e| anyhow::anyhow!("writing auth file {path}: {e}"))?;
            // Point the config at the generated auth file.
            new_config = tp::set_in_section(&new_config, "server", "auth_file", path);
        }
    }

    write_if_changed(Path::new(config_path), &new_config)?;

    // Machine-readable output for the installer.
    println!("FERROSA_MEMORY_TENANT_ID={tenant}");
    if let Some(user) = generated.iter().find(|c| c.username == "ferrosa_user") {
        println!("FERROSA_MEMORY_MCP_USER={}", user.username);
        println!("FERROSA_MEMORY_MCP_PASSWORD={}", user.password);
    }
    eprintln!("provisioned tenant {tenant} (idempotent: re-runs preserve it)");
    Ok(())
}

/// First `tenant_id` value under the first `[[principal]]` of an auth doc.
fn first_principal_tenant(doc: &str) -> Option<String> {
    let mut in_principal = false;
    for line in doc.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_principal = trimmed == "[[principal]]";
            continue;
        }
        if in_principal && let Some(v) = line_value(trimmed, "tenant_id") {
            return Some(v);
        }
    }
    None
}

/// Value of `key` inside `[section]` of a TOML doc (first occurrence).
fn section_value(doc: &str, section: &str, key: &str) -> Option<String> {
    let header = format!("[{section}]");
    let mut in_section = false;
    for line in doc.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == header;
            continue;
        }
        if in_section && let Some(v) = line_value(trimmed, key) {
            return Some(v);
        }
    }
    None
}

/// Parse `key = "value"` (uncommented), returning the unquoted value.
fn line_value(trimmed: &str, key: &str) -> Option<String> {
    if trimmed.starts_with('#') {
        return None;
    }
    let rest = trimmed.strip_prefix(key)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim();
    Some(rest.trim_matches('"').to_string())
}

fn setup(cli: &Cli) -> anyhow::Result<()> {
    let plan = setup_plan(cli)?;
    let paths = install_paths(&plan);
    let config = &plan.config_toml;

    println!("Ferrosa Memory setup plan\n");
    println!("Install root: {}", paths.root.display());
    println!("Binaries:     {}", paths.bin.display());
    println!("Config:       {}", paths.config_file.display());
    println!("Data:         {}", paths.data.display());
    println!("Logs:         {}", paths.logs.display());
    println!("Run state:    {}", paths.run.display());
    println!("Database:     127.0.0.1:{}", plan.database_port);
    println!("Control:      127.0.0.1:{}", plan.control_port);
    if plan.database_port != DEFAULT_DATABASE_PORT {
        println!(
            "Note: default database port {DEFAULT_DATABASE_PORT} is busy; planned {} instead.",
            plan.database_port
        );
    }
    if plan.control_port != DEFAULT_CONTROL_PORT {
        println!(
            "Note: default control port {DEFAULT_CONTROL_PORT} is busy; planned {} instead.",
            plan.control_port
        );
    }
    println!();
    println!("Managed config preview:\n");
    println!("{config}");
    println!("{}", path_block(&paths.bin));
    println!("MCP client preview snippets:\n");
    print_client_previews(&paths)?;

    if cli.dry_run {
        println!("Dry run only; no files changed.");
        return Ok(());
    }
    if !cli.yes && !confirm("Apply setup changes?")? {
        println!("No changes applied.");
        return Ok(());
    }

    ensure_layout(&paths)?;
    write_if_changed(&paths.config_file, config)?;
    reconcile_shell_path(&paths.bin, cli.yes)?;
    println!();
    println!("Setup reconciled. Run `ferrosa-memory doctor` to verify the install.");
    println!(
        "Client snippets above are previews only. Client writers and hook reconciliation will be enabled in the next setup phase."
    );
    Ok(())
}

fn doctor(cli: &Cli) -> anyhow::Result<()> {
    let plan = setup_plan(cli)?;
    let paths = install_paths(&plan);
    println!("Ferrosa Memory Doctor\n");
    check_path("install root", &paths.root);
    check_path("bin directory", &paths.bin);
    check_path("config directory", &paths.config);
    check_path("data directory", &paths.data);
    check_path("config file", &paths.config_file);
    check_path("MCP binary", &paths.mcp_binary);
    let path_ok = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|p| p == paths.bin))
        .unwrap_or(false);
    print_check(path_ok, &format!("{} is on PATH", paths.bin.display()));
    Ok(())
}

fn uninstall(cli: &Cli) -> anyhow::Result<()> {
    let plan = setup_plan(cli)?;
    let paths = install_paths(&plan);
    println!("Ferrosa Memory uninstall plan\n");
    println!("Will remove managed binaries from {}", paths.bin.display());
    println!("Will preserve data at {}", paths.data.display());
    println!("Will preserve config at {}", paths.config.display());
    println!("Will preserve logs at {}", paths.logs.display());
    if cli.delete_data {
        println!("Will DELETE data at {}", paths.data.display());
    }

    if cli.dry_run {
        println!("Dry run only; no files changed.");
        return Ok(());
    }
    if !cli.yes && !confirm("Apply uninstall changes?")? {
        println!("No changes applied.");
        return Ok(());
    }

    for name in ["ferrosa-memory", "ferrosa-memory-mcp", "ferrosa"] {
        let path = paths.bin.join(name);
        if path.exists() {
            fs::remove_file(&path)?;
            println!("removed {}", path.display());
        }
    }
    if cli.delete_data {
        if !cli.yes && !confirm("Type yes to permanently delete Ferrosa Memory data")? {
            println!("Data preserved at {}", paths.data.display());
        } else if paths.data.exists() {
            fs::remove_dir_all(&paths.data)?;
            println!("deleted {}", paths.data.display());
        }
    } else {
        println!("Data preserved at {}", paths.data.display());
    }
    Ok(())
}

fn setup_plan(cli: &Cli) -> anyhow::Result<NativeSetupPlan> {
    let install_scope = if cli.system {
        InstallScope::System
    } else {
        InstallScope::User
    };
    let data_scope = if cli.system {
        DataScope::System
    } else {
        DataScope::User
    };
    plan_native_setup(NativeSetupOptions {
        install_scope,
        data_scope,
        ..NativeSetupOptions::default()
    })
    .map_err(Into::into)
}

#[derive(Debug, Clone)]
struct InstallPaths {
    root: PathBuf,
    bin: PathBuf,
    config: PathBuf,
    data: PathBuf,
    logs: PathBuf,
    run: PathBuf,
    config_file: PathBuf,
    mcp_binary: PathBuf,
}

fn install_paths(plan: &NativeSetupPlan) -> InstallPaths {
    let dirs = &plan.directories;
    InstallPaths {
        root: dirs.root.clone(),
        mcp_binary: dirs.bin.join("ferrosa-memory-mcp"),
        config_file: dirs.config.join("ferrosa-memory.toml"),
        bin: dirs.bin.clone(),
        config: dirs.config.clone(),
        data: dirs.data.clone(),
        logs: dirs.logs.clone(),
        run: dirs.run.clone(),
    }
}

fn print_client_previews(paths: &InstallPaths) -> anyhow::Result<()> {
    let request = PreviewRequest {
        install_root: &paths.root,
        config_path: &paths.config_file,
        binary_path: &paths.mcp_binary,
        scope: ConfigScope::Global,
        tool_profile: ToolProfile::Recommended,
    };
    for preview in preview_client_configs(supported_clients(), &request)? {
        println!("--- {} ({}) ---", preview.display_name, preview.client_id);
        println!("{}", preview.snippet.trim_end());
        println!("{}", preview.note);
        println!();
    }
    Ok(())
}

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))
}

fn ensure_layout(paths: &InstallPaths) -> anyhow::Result<()> {
    for dir in [
        &paths.bin,
        &paths.config,
        &paths.data,
        &paths.logs,
        &paths.run,
    ] {
        fs::create_dir_all(dir)?;
    }
    Ok(())
}

fn write_if_changed(path: &Path, content: &str) -> anyhow::Result<()> {
    if path.exists() && fs::read_to_string(path)? == content {
        println!("unchanged {}", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let backup = timestamped_backup_path(path);
        fs::copy(path, &backup)?;
        println!("backup {}", backup.display());
    }
    fs::write(path, content)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn timestamped_backup_path(path: &Path) -> PathBuf {
    let suffix = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("backup");
    path.with_file_name(format!("{file_name}.{suffix}.bak"))
}

fn reconcile_shell_path(bin: &Path, yes: bool) -> anyhow::Result<()> {
    let Some(shell_rc) = shell_rc_path()? else {
        println!(
            "No supported shell rc file found; add {} to PATH manually.",
            bin.display()
        );
        return Ok(());
    };
    let block = path_block(bin);
    let current = fs::read_to_string(&shell_rc).unwrap_or_default();
    let next = replace_managed_block(&current, &block);
    if current == next {
        println!("PATH already managed in {}", shell_rc.display());
        return Ok(());
    }
    println!("Will update shell PATH in {}", shell_rc.display());
    println!("{block}");
    if !yes && !confirm("Apply PATH change?")? {
        return Ok(());
    }
    write_if_changed(&shell_rc, &next)
}

fn shell_rc_path() -> anyhow::Result<Option<PathBuf>> {
    let home = home_dir()?;
    let shell = std::env::var("SHELL").unwrap_or_default();
    if shell.ends_with("zsh") {
        return Ok(Some(home.join(".zshrc")));
    }
    if shell.ends_with("bash") {
        return Ok(Some(home.join(".bashrc")));
    }
    Ok(None)
}

fn path_block(bin: &Path) -> String {
    format!(
        "{MANAGED_PATH_BEGIN}\nexport PATH=\"{}:$PATH\"\n{MANAGED_PATH_END}\n",
        bin.display()
    )
}

fn replace_managed_block(current: &str, block: &str) -> String {
    if let Some(start) = current.find(MANAGED_PATH_BEGIN)
        && let Some(end_rel) = current[start..].find(MANAGED_PATH_END)
    {
        let end = start + end_rel + MANAGED_PATH_END.len();
        let mut next = String::new();
        next.push_str(current[..start].trim_end());
        if !next.is_empty() {
            next.push_str("\n\n");
        }
        next.push_str(block.trim_end());
        let tail = current[end..].trim_start_matches(['\n', '\r']);
        if !tail.is_empty() {
            next.push_str("\n\n");
            next.push_str(tail);
        } else {
            next.push('\n');
        }
        return next;
    }
    let mut next = current.trim_end().to_string();
    if !next.is_empty() {
        next.push_str("\n\n");
    }
    next.push_str(block);
    next
}

fn check_path(label: &str, path: &Path) {
    print_check(path.exists(), &format!("{label}: {}", path.display()));
}

fn print_check(ok: bool, message: &str) {
    let mark = if ok { "✓" } else { "✗" };
    println!("{mark} {message}");
}

fn confirm(prompt: &str) -> anyhow::Result<bool> {
    eprint!("{prompt} [y/N] ");
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(matches!(input.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_defaults_to_help() {
        let cli = parse_args(std::iter::empty::<&str>());
        assert_eq!(cli.command, Command::Help);
    }

    #[test]
    fn parse_provision_tenant_with_value_args() {
        let cli = parse_args([
            "provision-tenant",
            "--config",
            "/etc/ferrosa/ferrosa-memory.toml",
            "--auth-file",
            "/etc/ferrosa/http-auth.toml",
        ]);
        assert_eq!(cli.command, Command::ProvisionTenant);
        assert_eq!(
            cli.config.as_deref(),
            Some("/etc/ferrosa/ferrosa-memory.toml")
        );
        assert_eq!(
            cli.auth_file.as_deref(),
            Some("/etc/ferrosa/http-auth.toml")
        );
    }

    #[test]
    fn toml_value_readers_parse_sections_and_principals() {
        let config = "[server]\ntransport = \"http\"\ntenant_id = \"abc\"\n";
        assert_eq!(
            section_value(config, "server", "tenant_id").as_deref(),
            Some("abc")
        );
        assert_eq!(section_value(config, "server", "missing"), None);
        let auth = "[[principal]]\nusername = \"u\"\ntenant_id = \"t-1\"\n";
        assert_eq!(first_principal_tenant(auth).as_deref(), Some("t-1"));
        // Commented assignments are not read as values.
        assert_eq!(line_value("# tenant_id = \"x\"", "tenant_id"), None);
    }

    #[test]
    fn provision_tenant_repairs_http_config_without_server_tenant_fallback() {
        let directory = std::env::temp_dir().join(format!(
            "ferrosa-memory-provision-tenant-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&directory).unwrap();
        let config_path = directory.join("ferrosa-memory.toml");
        let auth_path = directory.join("http-auth.toml");
        fs::write(
            &config_path,
            "[server]\ntransport = \"http\"\nauth_file = \"http-auth.toml\"\ntenant_id = \"00000000-0000-0000-0000-000000000001\"\n\n[viz]\nenabled = true\n",
        )
        .unwrap();
        fs::write(
            &auth_path,
            "[[principal]]\nusername = \"ferrosa_user\"\npassword_sha256 = \"hash\"\ntenant_id = \"22222222-2222-2222-2222-222222222222\"\n",
        )
        .unwrap();
        let cli = Cli {
            command: Command::ProvisionTenant,
            yes: false,
            dry_run: false,
            system: false,
            delete_data: false,
            config: Some(config_path.display().to_string()),
            auth_file: Some(auth_path.display().to_string()),
        };

        provision_tenant(&cli).unwrap();

        let config = fs::read_to_string(&config_path).unwrap();
        let auth = fs::read_to_string(&auth_path).unwrap();
        assert_eq!(section_value(&config, "server", "tenant_id"), None);
        let tenant = first_principal_tenant(&auth).unwrap();
        assert!(
            !ferrosa_memory_core::tenant_provision::is_placeholder_tenant(
                uuid::Uuid::parse_str(&tenant).unwrap()
            )
        );
        assert_eq!(section_value(&config, "viz", "tenant_id"), Some(tenant));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn parse_setup_flags() {
        let cli = parse_args(["setup", "--system", "--yes", "--dry-run"]);
        assert_eq!(cli.command, Command::Setup);
        assert!(cli.system);
        assert!(cli.yes);
        assert!(cli.dry_run);
    }

    #[test]
    fn replace_managed_block_is_idempotent() {
        let block = path_block(Path::new("/tmp/fmem/bin"));
        let once = replace_managed_block("export A=1\n", &block);
        let twice = replace_managed_block(&once, &block);
        assert_eq!(once, twice);
        assert!(once.contains("/tmp/fmem/bin"));
    }

    #[test]
    fn setup_plan_uses_native_defaults() {
        let cli = parse_args(["setup", "--yes"]);
        let plan = setup_plan(&cli).expect("setup plan");
        assert_eq!(plan.install_scope, InstallScope::User);
        assert_eq!(plan.data_scope, DataScope::User);
        assert_eq!(plan.directories.root, home_dir().unwrap().join(".ferrosa"));
        assert!(plan.config_toml.contains("[server]"));
    }
}
