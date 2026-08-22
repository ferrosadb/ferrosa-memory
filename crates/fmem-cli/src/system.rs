//! Which memory system a command is acting for.
//!
//! One host can run several memory systems — that is the point of per-role
//! keypairs (trust D15), and it is why every command here is scoped to one
//! rather than to the machine.
//!
//! A system is named by its **MCP port**, because that is the thing that is
//! already unique per instance, already in each instance's config, and already
//! what an operator uses to tell two of them apart.
//!
//! Correctness: Correct when an ambiguous host refuses rather than guesses, and
//! when each system's identity lands in its own file.
//! Last revised: 2026-08-22
//! Last changed: Initial per-system resolution.

use std::path::{Path, PathBuf};

/// Errors resolving which system to act for. All fail closed.
#[derive(Debug, thiserror::Error)]
pub enum SystemError {
    /// Nothing to act for.
    #[error(
        "no memory system found under {root}. Start one, or name its MCP port \
         explicitly with --system <port>."
    )]
    NoneFound { root: PathBuf },

    /// Several, and no way to know which was meant.
    ///
    /// REFUSED rather than defaulted. Enrolling the wrong system binds an
    /// immutable kind to the wrong keypair, and the only remedy is revoking the
    /// device and starting again — so a wrong guess here is expensive in a way
    /// that asking is not.
    #[error(
        "several memory systems found ({}). Name one with --system <port>.",
        .ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", ")
    )]
    Ambiguous { ports: Vec<u16> },

    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// One memory system on this host, identified by its MCP port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySystem {
    /// The MCP port the daemon serves on. The system's local name.
    pub port: u16,
    /// Config file this was discovered from, for error messages that point at
    /// something the operator can open.
    pub config_path: Option<PathBuf>,
}

impl MemorySystem {
    /// Where this system's device key lives.
    ///
    /// Per system, never per machine. The workbench keeps its own identity in
    /// `config/device.json`; a memory system must not share it, because they
    /// are different kinds with different keypairs and only one of them may be
    /// offered a peer channel.
    #[must_use]
    pub fn key_path(&self, root: &Path) -> PathBuf {
        root.join("config")
            .join("devices")
            .join(format!("{}.json", self.port))
    }

    /// Where this system's enrollment record lives.
    ///
    /// Beside the key rather than inside it: the key file keeps the shape
    /// `memory-sync p2p-keygen` writes and `load_identity` reads, so the same
    /// file still works for `control-listen` and friends. Widening that struct
    /// would fork the format for no gain.
    #[must_use]
    ///
    /// Falls back to the pre-rename `<port>.enrolment.json` when only that
    /// exists, for the same reason the legacy contract is accepted: a record on
    /// disk describes a live device, and ignoring it would enroll it twice.
    pub fn enrollment_path(&self, root: &Path) -> PathBuf {
        let current = root
            .join("config")
            .join("devices")
            .join(format!("{}.enrollment.json", self.port));
        if current.exists() {
            return current;
        }
        let legacy = root
            .join("config")
            .join("devices")
            .join(format!("{}.enrolment.json", self.port));
        if legacy.exists() { legacy } else { current }
    }

    /// The label this system enrolls under when the operator does not choose.
    ///
    /// Host AND port, because several memory systems on one machine would
    /// otherwise arrive in the device list with identical names, and the list
    /// is exactly where an operator has to tell them apart.
    #[must_use]
    pub fn default_label(&self, hostname: &str) -> String {
        format!("{hostname}:{}", self.port)
    }
}

/// Find the memory systems configured under `root`.
///
/// Reads `config/*.toml` for a `[server] http_port`, which is what the daemon
/// itself is configured by. Discovering from config rather than from listening
/// sockets is deliberate: a system that is installed but stopped still needs to
/// be enrollable, and a port that happens to be open is not evidence that a
/// memory system owns it.
pub fn discover(root: &Path) -> Result<Vec<MemorySystem>, SystemError> {
    let config_dir = root.join("config");
    let entries = match std::fs::read_dir(&config_dir) {
        Ok(e) => e,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(source) => {
            return Err(SystemError::Io {
                path: config_dir,
                source,
            });
        }
    };

    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(port) = http_port_from_toml(&text) {
            found.push(MemorySystem {
                port,
                config_path: Some(path),
            });
        }
    }
    found.sort_by_key(|s| s.port);
    found.dedup_by_key(|s| s.port);
    Ok(found)
}

/// Pull `[server] http_port` out of a config file.
///
/// Returns `None` for a file that parses but is not a memory-system config —
/// the config directory holds ferrosa node configs and http-auth too, and a
/// missing `[server]` table is the normal way to tell them apart, not an error.
#[must_use]
pub fn http_port_from_toml(text: &str) -> Option<u16> {
    let parsed: toml::Value = toml::from_str(text).ok()?;
    let port = parsed.get("server")?.get("http_port")?.as_integer()?;
    u16::try_from(port).ok()
}

/// Decide which system a command acts for.
///
/// An explicit `--system` always wins and is never validated against discovery:
/// an operator naming a port that is configured elsewhere, or not yet
/// configured at all, is doing something reasonable, and refusing would make
/// the flag useless in exactly the cases it exists for.
pub fn resolve(root: &Path, requested: Option<u16>) -> Result<MemorySystem, SystemError> {
    if let Some(port) = requested {
        return Ok(MemorySystem {
            port,
            config_path: None,
        });
    }
    let mut found = discover(root)?;
    match found.len() {
        0 => Err(SystemError::NoneFound {
            root: root.to_path_buf(),
        }),
        1 => Ok(found.remove(0)),
        _ => Err(SystemError::Ambiguous {
            ports: found.into_iter().map(|s| s.port).collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).expect("write config");
    }

    fn memory_config(port: u16) -> String {
        format!("[server]\ntransport = \"http\"\nhttp_port = {port}\n")
    }

    #[test]
    fn a_single_configured_system_resolves_without_asking() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).expect("mkdir");
        write(&cfg, "ferrosa-memory.toml", &memory_config(43971));

        let system = resolve(tmp.path(), None).expect("resolves");
        assert_eq!(system.port, 43971);
    }

    /// The rule that matters. Guessing binds an IMMUTABLE kind to the wrong
    /// keypair, and the only fix is revoke-and-re-enroll.
    #[test]
    fn several_systems_refuse_rather_than_guess() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).expect("mkdir");
        write(&cfg, "one.toml", &memory_config(43971));
        write(&cfg, "two.toml", &memory_config(43972));

        match resolve(tmp.path(), None) {
            Err(SystemError::Ambiguous { ports }) => assert_eq!(ports, vec![43971, 43972]),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// The error has to name the ports, or the operator cannot act on it.
    #[test]
    fn the_ambiguous_error_names_what_it_found() {
        let err = SystemError::Ambiguous {
            ports: vec![43971, 43972],
        };
        let message = err.to_string();
        assert!(message.contains("43971"), "{message}");
        assert!(message.contains("43972"), "{message}");
        assert!(message.contains("--system"), "{message}");
    }

    /// An empty host says so, and says what to do about it.
    #[test]
    fn no_system_is_an_actionable_error() {
        let tmp = tempfile::tempdir().expect("tmp");
        let err = resolve(tmp.path(), None).expect_err("nothing configured");
        let message = err.to_string();
        assert!(message.contains("--system"), "{message}");
    }

    /// An explicit port is taken at face value, including one that is not
    /// configured here — that is the case the flag exists for.
    #[test]
    fn an_explicit_port_wins_over_discovery() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).expect("mkdir");
        write(&cfg, "one.toml", &memory_config(43971));
        write(&cfg, "two.toml", &memory_config(43972));

        let system = resolve(tmp.path(), Some(5555)).expect("explicit wins");
        assert_eq!(system.port, 5555);
    }

    /// Config files that are not memory systems must not be counted.
    ///
    /// The real config directory holds ferrosa node configs and http-auth
    /// alongside ferrosa-memory.toml. Counting those would make every host look
    /// ambiguous and refuse every command.
    #[test]
    fn non_memory_configs_are_ignored() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).expect("mkdir");
        write(&cfg, "ferrosa-memory.toml", &memory_config(43971));
        write(
            &cfg,
            "ferrosa-node1.toml",
            "[cluster]\nseed = \"127.0.0.1\"\n",
        );
        write(&cfg, "http-auth.toml", "[users]\nben = \"x\"\n");
        write(&cfg, "not-toml.json", "{}");

        let found = discover(tmp.path()).expect("discovers");
        assert_eq!(found.len(), 1, "found: {found:?}");
        assert_eq!(found[0].port, 43971);
    }

    /// A config that does not parse is skipped, not fatal.
    ///
    /// A half-written file in the config directory must not stop an operator
    /// enrolling a system that is fine.
    #[test]
    fn an_unparseable_config_is_skipped() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).expect("mkdir");
        write(&cfg, "broken.toml", "this is not [ valid toml");
        write(&cfg, "ferrosa-memory.toml", &memory_config(43971));

        let found = discover(tmp.path()).expect("discovers");
        assert_eq!(found.len(), 1);
    }

    /// A missing root is an empty host, not an error.
    #[test]
    fn a_missing_config_directory_is_empty_not_fatal() {
        let tmp = tempfile::tempdir().expect("tmp");
        let found = discover(&tmp.path().join("nope")).expect("no error");
        assert!(found.is_empty());
    }

    /// Each system gets its own files, and neither collides with the
    /// workbench's `config/device.json`.
    #[test]
    fn each_system_has_its_own_paths() {
        let root = Path::new("/root");
        let a = MemorySystem {
            port: 43971,
            config_path: None,
        };
        let b = MemorySystem {
            port: 43972,
            config_path: None,
        };

        assert_ne!(a.key_path(root), b.key_path(root));
        assert_ne!(a.key_path(root), a.enrollment_path(root));
        assert_eq!(a.key_path(root), root.join("config/devices/43971.json"));
        assert_ne!(a.key_path(root), root.join("config/device.json"));
    }

    /// Labels distinguish systems on one host, because the device list is
    /// exactly where they have to be told apart.
    #[test]
    fn the_default_label_carries_host_and_port() {
        let system = MemorySystem {
            port: 43971,
            config_path: None,
        };
        assert_eq!(system.default_label("studio"), "studio:43971");
        assert_ne!(
            system.default_label("studio"),
            MemorySystem {
                port: 43972,
                config_path: None
            }
            .default_label("studio")
        );
    }

    #[test]
    fn a_port_is_read_from_the_server_table() {
        assert_eq!(
            http_port_from_toml("[server]\nhttp_port = 43971\n"),
            Some(43971)
        );
        assert_eq!(http_port_from_toml("[cluster]\nhttp_port = 43971\n"), None);
        assert_eq!(http_port_from_toml("[server]\nbind_addr = \"x\"\n"), None);
        // Out of u16 range must not silently truncate.
        assert_eq!(http_port_from_toml("[server]\nhttp_port = 99999\n"), None);
    }
}
