//! Where `fmem` gets its console URL and its root directory.
//!
//! Config comes from the environment first, then a file, then a built-in
//! default — and the resolved value is always REPORTED, with its source. A URL
//! that silently differs from what the operator expects points enrollment at the
//! wrong control plane, and enrollment is not undoable.
//!
//! Correctness: Correct when the effective console URL and where it came from
//! are both visible before anything is sent to it.
//! Last revised: 2026-08-22
//! Last changed: Initial settings resolution.

use std::path::{Path, PathBuf};

/// Environment variable naming the console origin.
pub const CONSOLE_URL_ENV: &str = "FMEM_CONSOLE_URL";

/// Environment variable naming the Ferrosa root.
pub const ROOT_ENV: &str = "FERROSA_HOME";

/// Console origin used when nothing says otherwise.
///
/// The dev deployment, not `dev.fmem.ai`: that domain is registered but still
/// parked, and defaulting to a host that does not answer would make a fresh
/// install fail with a DNS error instead of working. Change this when the
/// domain is pointed, or override it per host with [`CONSOLE_URL_ENV`].
pub const DEFAULT_CONSOLE_URL: &str = "https://maas-dev-console.fly.dev";

/// Where a setting came from, so it can be shown alongside its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// From the `--console` flag.
    Flag,
    /// From [`CONSOLE_URL_ENV`].
    Environment,
    /// From `config/fmem.toml`.
    ConfigFile,
    /// From [`DEFAULT_CONSOLE_URL`].
    BuiltIn,
}

impl Source {
    /// How to describe this in one short phrase.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Flag => "--console",
            Self::Environment => CONSOLE_URL_ENV,
            Self::ConfigFile => "config/fmem.toml",
            Self::BuiltIn => "built-in default",
        }
    }
}

/// A resolved setting and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub value: String,
    pub source: Source,
}

/// The Ferrosa root directory.
///
/// `FERROSA_HOME` first so a test, or a second install, can point somewhere
/// else without touching the operator's real one.
#[must_use]
pub fn root() -> PathBuf {
    if let Ok(dir) = std::env::var(ROOT_ENV) {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    home().join(".ferrosa")
}

fn home() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// Resolve the console origin, given an optional explicit override.
///
/// Precedence: the `--console` flag, then the environment, then
/// `config/fmem.toml`, then the built-in. The flag is separate from the
/// environment so a one-off command can point elsewhere without exporting
/// anything into a shell that later runs a different command.
#[must_use]
pub fn console_url(root: &Path, flag: Option<&str>) -> Resolved {
    if let Some(value) = flag.map(str::trim).filter(|v| !v.is_empty()) {
        return Resolved {
            value: normalize(value),
            source: Source::Flag,
        };
    }
    if let Some(value) = std::env::var(CONSOLE_URL_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        return Resolved {
            value: normalize(&value),
            source: Source::Environment,
        };
    }
    if let Some(value) = console_url_from_config(root) {
        return Resolved {
            value: normalize(&value),
            source: Source::ConfigFile,
        };
    }
    Resolved {
        value: normalize(DEFAULT_CONSOLE_URL),
        source: Source::BuiltIn,
    }
}

/// Read `[console] url` from `config/fmem.toml`.
fn console_url_from_config(root: &Path) -> Option<String> {
    let path = root.join("config").join("fmem.toml");
    let text = std::fs::read_to_string(path).ok()?;
    let parsed: toml::Value = toml::from_str(&text).ok()?;
    let url = parsed.get("console")?.get("url")?.as_str()?.trim();
    (!url.is_empty()).then(|| url.to_string())
}

/// Strip a trailing slash so `{base}/v1/...` never doubles it.
fn normalize(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flag beats everything, so one command can point elsewhere without
    /// changing the shell every later command inherits.
    #[test]
    fn an_explicit_flag_wins() {
        let tmp = tempfile::tempdir().expect("tmp");
        let got = console_url(tmp.path(), Some("https://flag.test/"));
        assert_eq!(got.value, "https://flag.test");
        assert_eq!(got.source, Source::Flag);
    }

    /// Every source describes ITSELF.
    ///
    /// The report exists so an operator can see where the URL came from before
    /// enrolling against it. A flag that reported "FMEM_CONSOLE_URL" would send
    /// someone to check an environment variable they never set — which is what
    /// this did until a live run put the wrong label on screen.
    #[test]
    fn each_source_describes_itself_accurately() {
        assert_eq!(Source::Flag.describe(), "--console");
        assert_eq!(Source::Environment.describe(), CONSOLE_URL_ENV);
        assert_eq!(Source::ConfigFile.describe(), "config/fmem.toml");
        assert_eq!(Source::BuiltIn.describe(), "built-in default");

        let all = [
            Source::Flag,
            Source::Environment,
            Source::ConfigFile,
            Source::BuiltIn,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                assert_ne!(a.describe(), b.describe(), "two sources read the same");
            }
        }
    }

    /// A config file beats the built-in.
    #[test]
    fn a_config_file_beats_the_built_in() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).expect("mkdir");
        std::fs::write(
            cfg.join("fmem.toml"),
            "[console]\nurl = \"https://dev.fmem.ai\"\n",
        )
        .expect("write");

        let got = console_url(tmp.path(), None);
        assert_eq!(got.value, "https://dev.fmem.ai");
        assert_eq!(got.source, Source::ConfigFile);
    }

    /// With nothing configured, the built-in is used AND is identifiable as
    /// such, so the operator can see they never chose it.
    #[test]
    fn the_built_in_is_reported_as_a_default() {
        let tmp = tempfile::tempdir().expect("tmp");
        let got = console_url(tmp.path(), None);
        assert_eq!(got.value, DEFAULT_CONSOLE_URL);
        assert_eq!(got.source, Source::BuiltIn);
        assert_eq!(got.source.describe(), "built-in default");
    }

    /// Trailing slashes are stripped everywhere, from every source.
    ///
    /// `{base}/v1/device-auth/start` with a trailing slash produces a double
    /// slash, which some proxies 404 and others silently rewrite — a difference
    /// that shows up only in deployment.
    #[test]
    fn a_trailing_slash_never_survives() {
        let tmp = tempfile::tempdir().expect("tmp");
        assert_eq!(
            console_url(tmp.path(), Some("https://x.test///")).value,
            "https://x.test"
        );

        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).expect("mkdir");
        std::fs::write(
            cfg.join("fmem.toml"),
            "[console]\nurl = \"https://y.test/\"\n",
        )
        .expect("write");
        assert_eq!(console_url(tmp.path(), None).value, "https://y.test");
    }

    /// An empty flag or an empty config value falls through instead of
    /// producing an empty base URL that would build nonsense requests.
    #[test]
    fn empty_values_fall_through() {
        let tmp = tempfile::tempdir().expect("tmp");
        assert_eq!(console_url(tmp.path(), Some("   ")).source, Source::BuiltIn);

        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).expect("mkdir");
        std::fs::write(cfg.join("fmem.toml"), "[console]\nurl = \"\"\n").expect("write");
        assert_eq!(console_url(tmp.path(), None).source, Source::BuiltIn);
    }

    /// A malformed config is skipped rather than fatal — the built-in still
    /// works and the operator is told which source was used.
    #[test]
    fn a_broken_config_falls_back_rather_than_failing() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cfg = tmp.path().join("config");
        std::fs::create_dir_all(&cfg).expect("mkdir");
        std::fs::write(cfg.join("fmem.toml"), "not [ valid").expect("write");

        assert_eq!(console_url(tmp.path(), None).source, Source::BuiltIn);
    }

    /// The default must not be the parked domain: a fresh install would fail
    /// with a DNS error rather than work.
    #[test]
    fn the_default_points_somewhere_that_answers() {
        assert!(!DEFAULT_CONSOLE_URL.contains("fmem.ai"));
        assert!(DEFAULT_CONSOLE_URL.starts_with("https://"));
        assert!(!DEFAULT_CONSOLE_URL.ends_with('/'));
    }
}
