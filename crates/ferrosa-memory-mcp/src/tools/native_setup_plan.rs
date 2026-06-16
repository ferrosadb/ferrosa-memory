//! Module: Plan native Ferrosa Memory service layout without mutating the host.
//! Correctness: Correct when scope defaults resolve to stable directories, selected localhost ports are available at planning time, and generated config references those selected ports.
//! Last revised: 2026-06-15
//! Last changed: Added native setup planning helpers for service directories, localhost ports, and config TOML.

use std::io;
use std::net::TcpListener;
use std::path::PathBuf;

pub const DEFAULT_DATABASE_PORT: u16 = 18765;
pub const DEFAULT_CONTROL_PORT: u16 = 18766;

const LOCALHOST: &str = "127.0.0.1";
const LOCALHOST_CONFIG_HOST: &str = "localhost";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InstallScope {
    #[default]
    User,
    System,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DataScope {
    #[default]
    User,
    System,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeSetupOptions {
    pub install_scope: InstallScope,
    pub data_scope: DataScope,
    pub root: Option<PathBuf>,
    pub database_port: u16,
    pub control_port: u16,
}

impl Default for NativeSetupOptions {
    fn default() -> Self {
        Self {
            install_scope: InstallScope::User,
            data_scope: DataScope::User,
            root: None,
            database_port: DEFAULT_DATABASE_PORT,
            control_port: DEFAULT_CONTROL_PORT,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeSetupDirectories {
    pub root: PathBuf,
    pub bin: PathBuf,
    pub config: PathBuf,
    pub data: PathBuf,
    pub logs: PathBuf,
    pub run: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeSetupPlan {
    pub install_scope: InstallScope,
    pub data_scope: DataScope,
    pub directories: NativeSetupDirectories,
    pub database_port: u16,
    pub control_port: u16,
    pub config_toml: String,
}

pub fn plan_native_setup(options: NativeSetupOptions) -> io::Result<NativeSetupPlan> {
    let root = options
        .root
        .unwrap_or_else(|| default_root_for_data_scope(options.data_scope));
    let directories = native_setup_directories(root);
    let database_port = next_available_localhost_port(options.database_port)?;
    let control_port =
        next_available_localhost_port_excluding(options.control_port, &[database_port])?;

    Ok(NativeSetupPlan {
        install_scope: options.install_scope,
        data_scope: options.data_scope,
        directories,
        database_port,
        control_port,
        config_toml: native_setup_config_toml(database_port, control_port),
    })
}

pub fn native_setup_directories(root: PathBuf) -> NativeSetupDirectories {
    NativeSetupDirectories {
        bin: root.join("bin"),
        config: root.join("config"),
        data: root.join("data"),
        logs: root.join("logs"),
        run: root.join("run"),
        root,
    }
}

pub fn default_root_for_data_scope(scope: DataScope) -> PathBuf {
    match scope {
        DataScope::User => default_user_root(),
        DataScope::System => default_system_root(),
    }
}

pub fn default_user_root() -> PathBuf {
    home_dir()
        .map(|home| home.join(".ferrosa"))
        .unwrap_or_else(|| PathBuf::from("~/.ferrosa"))
}

pub fn default_system_root() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .map(|root| root.join("Ferrosa"))
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData\Ferrosa"))
    }

    #[cfg(not(windows))]
    {
        PathBuf::from("/var/lib/ferrosa")
    }
}

#[allow(dead_code)]
pub fn localhost_port_available(port: u16) -> bool {
    bind_localhost_port(port).is_ok()
}

pub fn next_available_localhost_port(start_port: u16) -> io::Result<u16> {
    next_available_localhost_port_excluding(start_port, &[])
}

pub fn native_setup_config_toml(database_port: u16, control_port: u16) -> String {
    format!(
        r#"[server]
transport = "http"
bind_addr = "{LOCALHOST}"
http_port = {control_port}
public_port = {control_port}

[ferrosa]
contact_points = ["{LOCALHOST_CONFIG_HOST}:{database_port}"]
keyspace = "agent_memory"
replication_factor = 1
consistency = "ONE"
username = "ferrosa"
password = "ferrosa"
"#
    )
}

fn next_available_localhost_port_excluding(start_port: u16, excluded: &[u16]) -> io::Result<u16> {
    if start_port == 0 {
        for _ in 0..64 {
            let listener = bind_localhost_port(0)?;
            let port = listener.local_addr()?.port();
            if !excluded.contains(&port) {
                return Ok(port);
            }
        }

        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "could not reserve a distinct ephemeral localhost port",
        ));
    }

    for port in start_port..=u16::MAX {
        if excluded.contains(&port) {
            continue;
        }
        if bind_localhost_port(port).is_ok() {
            return Ok(port);
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        format!("no available localhost port at or above {start_port}"),
    ))
}

fn bind_localhost_port(port: u16) -> io::Result<TcpListener> {
    TcpListener::bind((LOCALHOST, port))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planned_directories_are_root_children() {
        let root = PathBuf::from("/tmp/ferrosa-native-test");
        let dirs = native_setup_directories(root.clone());

        assert_eq!(dirs.root, root);
        assert_eq!(dirs.bin, PathBuf::from("/tmp/ferrosa-native-test/bin"));
        assert_eq!(
            dirs.config,
            PathBuf::from("/tmp/ferrosa-native-test/config")
        );
        assert_eq!(dirs.data, PathBuf::from("/tmp/ferrosa-native-test/data"));
        assert_eq!(dirs.logs, PathBuf::from("/tmp/ferrosa-native-test/logs"));
        assert_eq!(dirs.run, PathBuf::from("/tmp/ferrosa-native-test/run"));
    }

    #[test]
    fn user_scope_defaults_to_home_ferrosa_when_home_exists() {
        if let Some(home) = home_dir() {
            assert_eq!(default_user_root(), home.join(".ferrosa"));
        }
    }

    #[test]
    fn config_toml_uses_selected_localhost_ports() {
        let toml = native_setup_config_toml(19100, 19101);

        assert!(toml.contains("http_port = 19101"));
        assert!(toml.contains(r#"contact_points = ["localhost:19100"]"#));
    }

    #[test]
    fn port_fallback_skips_occupied_listener() {
        let occupied = TcpListener::bind((LOCALHOST, 0)).expect("bind occupied test port");
        let occupied_port = occupied.local_addr().unwrap().port();

        let chosen = next_available_localhost_port(occupied_port).expect("fallback port");

        assert_ne!(chosen, occupied_port);
        assert!(chosen > occupied_port);
        drop(occupied);
    }

    #[test]
    fn plan_uses_next_control_port_when_database_claims_preferred_control_port() {
        let probe = TcpListener::bind((LOCALHOST, 0)).expect("bind probe port");
        let preferred = probe.local_addr().unwrap().port();
        drop(probe);

        let plan = plan_native_setup(NativeSetupOptions {
            root: Some(PathBuf::from("/tmp/ferrosa-native-test")),
            database_port: preferred,
            control_port: preferred,
            ..NativeSetupOptions::default()
        })
        .expect("native setup plan");

        assert_eq!(plan.database_port, preferred);
        assert_ne!(plan.control_port, plan.database_port);
        assert!(plan.control_port > plan.database_port);
    }

    #[test]
    fn plan_renders_directories_ports_and_config() {
        let root = PathBuf::from("/tmp/ferrosa-native-test");
        let plan = plan_native_setup(NativeSetupOptions {
            root: Some(root.clone()),
            database_port: 0,
            control_port: 0,
            ..NativeSetupOptions::default()
        })
        .expect("native setup plan");

        assert_eq!(plan.install_scope, InstallScope::User);
        assert_eq!(plan.data_scope, DataScope::User);
        assert_eq!(plan.directories.root, root);
        assert_eq!(
            plan.directories.bin,
            PathBuf::from("/tmp/ferrosa-native-test/bin")
        );
        assert!(plan.database_port > 0);
        assert!(plan.control_port > 0);
        assert_ne!(plan.database_port, plan.control_port);
        assert!(
            plan.config_toml
                .contains(&format!("http_port = {}", plan.control_port))
        );
        assert!(
            plan.config_toml
                .contains(&format!("localhost:{}", plan.database_port))
        );
    }
}
