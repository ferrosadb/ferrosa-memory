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
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cli {
    command: Command,
    yes: bool,
    dry_run: bool,
    system: bool,
    delete_data: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = parse_args(std::env::args().skip(1));
    match cli.command {
        Command::Setup => setup(&cli),
        Command::Doctor => doctor(&cli),
        Command::Uninstall => uninstall(&cli),
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

    for arg in args.into_iter().map(Into::into) {
        match arg.as_str() {
            "setup" => command = Command::Setup,
            "doctor" | "status" => command = Command::Doctor,
            "uninstall" => command = Command::Uninstall,
            "-h" | "--help" | "help" => command = Command::Help,
            "-y" | "--yes" => yes = true,
            "--dry-run" => dry_run = true,
            "--system" => system = true,
            "--delete-data" => delete_data = true,
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
    }
}

fn print_help() {
    eprintln!(
        "ferrosa-memory\n\n\
         Usage:\n\
           ferrosa-memory setup [--system] [--dry-run] [--yes]\n\
           ferrosa-memory doctor\n\
           ferrosa-memory uninstall [--delete-data] [--dry-run] [--yes]\n"
    );
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
