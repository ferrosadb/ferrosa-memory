//! Module: Decide where diagnostics go, and start the subscriber that sends
//! them there.
//!
//! Correctness: Correct when an absent DSN starts nothing, when a debug log is
//! written on a testing build without being asked for, and when neither is
//! turned on by a value that cannot work.
//!
//! Last revised: 2026-08-28
//! Last changed: New.
//!
//! # Why the DSN is not in this file
//!
//! ferrosa-memory is a PUBLIC repository. A Sentry DSN is not a secret — it is
//! a write-only ingest key, designed to be embedded in a client — but a DSN in
//! a public repo can be used by anyone to send events into the project, and
//! rotating it then needs a release. It comes from the environment instead.
//!
//! # The two tiers
//!
//! Errors go to Sentry, when a DSN is configured. Debug detail goes to a file
//! on disk, and is on by default for a testing build, because the failures
//! worth catching in testing are the ones nobody was watching for.
//!
//! They are separate on purpose. A debug log is verbose, local, and cheap; an
//! error event leaves the machine and costs quota. Sending debug to Sentry
//! would bury the errors and empty the quota on noise.

use std::path::PathBuf;

/// Where this process should send diagnostics.
///
/// A plain value so the decision can be tested without a subscriber, a
/// filesystem or a network. Building the subscriber from it is mechanical;
/// deciding what to build is where the mistakes are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryPlan {
    /// The Sentry DSN, when one is configured and usable.
    pub sentry_dsn: Option<String>,
    /// Where to write the debug log, when one should be written.
    pub debug_log_dir: Option<PathBuf>,
}

/// Where a testing build writes its debug log when nobody named a directory.
pub const DEFAULT_DEBUG_LOG_DIR: &str = "logs";

impl TelemetryPlan {
    /// Work out the plan from configuration and the kind of build.
    ///
    /// `testing_build` is passed in rather than read from `cfg!(debug_assertions)`
    /// here, so the rule can be tested for both kinds of build from one place.
    pub fn resolve(dsn: Option<&str>, debug_log_dir: Option<&str>, testing_build: bool) -> Self {
        // A DSN that is empty or not a URL cannot work. Starting the SDK with
        // it would report a configuration error on every launch about a side
        // channel nobody asked for, so it is treated as absent.
        let sentry_dsn = dsn
            .map(str::trim)
            .filter(|value| !value.is_empty() && value.starts_with("https://"))
            .map(str::to_owned);

        let named = debug_log_dir
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);

        // An explicitly named directory always wins, including on a release
        // build: someone asking for a debug log is asking on purpose.
        // Otherwise a log is written when either is true:
        //
        // - this is a testing build, where the failures worth catching are the
        //   ones nobody was watching for; or
        // - no Sentry DSN is configured, so disk is the ONLY place a failure
        //   could be recorded. This repository is public and Sentry is the
        //   user's to configure, not ours to assume; a user who has not set one
        //   up must still end up with something to send when they report a bug.
        let needs_local_record = testing_build || sentry_dsn.is_none();
        let debug_log_dir =
            named.or_else(|| needs_local_record.then(|| PathBuf::from(DEFAULT_DEBUG_LOG_DIR)));

        Self {
            sentry_dsn,
            debug_log_dir,
        }
    }

    /// Whether anything at all is being sent off this machine.
    pub fn reports_errors(&self) -> bool {
        self.sentry_dsn.is_some()
    }

    /// Whether a debug log is being written.
    pub fn writes_debug_log(&self) -> bool {
        self.debug_log_dir.is_some()
    }
}

/// The commit this binary was built from, or `unknown`.
///
/// Set by build.rs, from `FERROSA_BUILD_SHA` if CI supplied it and otherwise
/// from git. `unknown` is honest: a stale SHA would be believed.
pub const BUILD_SHA: &str = env!("FERROSA_BUILD_SHA");

/// How one build identifies itself to Sentry.
///
/// `service@version+sha`, e.g. `memory-mcp@0.29.0+a1b2c3d4e5f6`.
///
/// The SERVICE, not the crate: `sentry::release_name!()` expands to
/// CARGO_PKG_NAME at its call site, which is this crate — so all three
/// binaries would have reported `ferrosa-memory-core@0.29.0` and nothing would
/// have distinguished the MCP server from the sync CLI.
///
/// The SHA, not just the version: nightly cuts several builds per version, and
/// the first question about any report is whether it is the build that already
/// has the fix.
pub fn release_id(service: &str, version: &str, build_sha: &str) -> String {
    if build_sha.is_empty() || build_sha == "unknown" {
        format!("{service}@{version}")
    } else {
        format!("{service}@{version}+{build_sha}")
    }
}

/// Everything the process must hold onto for diagnostics to keep working.
///
/// Dropping this flushes and stops both sinks, so `main` must keep it alive for
/// the life of the process. Sentry's guard in particular sends nothing after it
/// is dropped, which is a quiet way to lose exactly the errors that happen
/// during shutdown.
#[must_use = "dropping this stops diagnostics; bind it for the life of main"]
pub struct TelemetryGuard {
    _sentry: Option<sentry::ClientInitGuard>,
    _debug_log: Option<tracing_appender::non_blocking::WorkerGuard>,
}

/// Read the plan from the environment, using this build's kind.
///
/// `FERROSA_MEMORY_SENTRY_DSN` and `FERROSA_MEMORY_DEBUG_LOG_DIR`. Both come
/// from the environment because the installer owns them: this repository is
/// public, and a DSN committed here could be used by anyone and could not be
/// rotated without a release.
pub fn plan_from_env() -> TelemetryPlan {
    plan_from_env_prefixed("FERROSA_MEMORY")
}

/// The same, for a component with its own variables.
///
/// `FERROSA_STREAMER_SENTRY_DSN`, `FERROSA_STREAMER_DEBUG_LOG_DIR`, and so on.
/// One Sentry project per component means one variable per component: a single
/// shared name would send every component's errors to whichever project the
/// installer happened to configure last.
pub fn plan_from_env_prefixed(prefix: &str) -> TelemetryPlan {
    TelemetryPlan::resolve(
        std::env::var(format!("{prefix}_SENTRY_DSN"))
            .ok()
            .as_deref(),
        std::env::var(format!("{prefix}_DEBUG_LOG_DIR"))
            .ok()
            .as_deref(),
        cfg!(debug_assertions),
    )
}

/// Start diagnostics for one binary, and say on stderr where they are going.
///
/// `default_filter` applies when RUST_LOG is unset — the MCP server is quiet
/// by default and the others are not, so the caller supplies its own.
///
/// Replaces the bare `tracing_subscriber::fmt().with_env_filter(...).init()`
/// that each binary called separately — three initialisation sites is three
/// chances to drift, and none of them reported anything anywhere but the
/// console of a process nobody is watching.
pub fn init(service: &str, plan: &TelemetryPlan, default_filter: &str) -> TelemetryGuard {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let sentry_guard = plan.sentry_dsn.as_ref().map(|dsn| {
        // Built field by field: ClientOptions is #[non_exhaustive], so a
        // struct expression does not compile from outside the SDK.
        let mut options = sentry::ClientOptions::default();
        options.release = Some(release_id(service, env!("CARGO_PKG_VERSION"), BUILD_SHA).into());
        // Never. This process holds other people's memory, and the default
        // attaches the machine's IP and username.
        options.send_default_pii = false;
        sentry::init((dsn.as_str(), options))
    });

    let (debug_writer, debug_guard) = match plan.debug_log_dir.as_ref() {
        Some(dir) => {
            // Created here rather than assumed: a log directory that does not
            // exist is how a "log to disk" fallback writes nowhere.
            if let Err(error) = std::fs::create_dir_all(dir) {
                eprintln!(
                    "ferrosa: cannot create the debug log directory {}: {error}",
                    dir.display()
                );
                (None, None)
            } else {
                let appender = tracing_appender::rolling::daily(dir, format!("{service}.log"));
                let (writer, guard) = tracing_appender::non_blocking(appender);
                (Some(writer), Some(guard))
            }
        }
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter)),
        )
        // STDERR, always. ferrosa-memory-mcp speaks the MCP protocol on
        // STDOUT, and a log line written there corrupts the stream — the
        // client sees malformed JSON-RPC rather than a log it can ignore.
        // fmt::layer() defaults to stdout, so this is not optional.
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(debug_writer.map(|writer| {
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                // No colour: these are read with grep, and ANSI escapes make an
                // anchored pattern score zero on a file full of errors.
                .with_ansi(false)
        }))
        .with(sentry_guard.as_ref().map(|_| {
            sentry_tracing::layer().event_filter(|meta| match *meta.level() {
                tracing::Level::ERROR => sentry_tracing::EventFilter::Event,
                // Warnings become breadcrumbs, not events: they are context for
                // the error that follows, and as events they would bury it.
                tracing::Level::WARN => sentry_tracing::EventFilter::Breadcrumb,
                _ => sentry_tracing::EventFilter::Ignore,
            })
        }))
        .init();

    // Said out loud so nobody has to guess whether reporting is on. A side
    // channel that is silently off is how a fortnight of errors goes missing.
    match (&plan.sentry_dsn, &plan.debug_log_dir) {
        (Some(_), Some(dir)) => eprintln!(
            "ferrosa: errors report to Sentry; debug log at {}/{service}.log",
            dir.display()
        ),
        (Some(_), None) => eprintln!("ferrosa: errors report to Sentry"),
        (None, Some(dir)) => eprintln!(
            "ferrosa: no Sentry DSN configured; logging to {}/{service}.log",
            dir.display()
        ),
        (None, None) => eprintln!("ferrosa: diagnostics are off"),
    }

    TelemetryGuard {
        _sentry: sentry_guard,
        _debug_log: debug_guard,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identifier a report is traced back to.
    #[test]
    fn a_release_id_names_the_service_the_version_and_the_commit() {
        assert_eq!(
            release_id("memory-mcp", "0.29.0", "a1b2c3d4e5f6"),
            "memory-mcp@0.29.0+a1b2c3d4e5f6"
        );
    }

    /// The SERVICE, not the crate. `sentry::release_name!()` expands to
    /// CARGO_PKG_NAME at its call site — this crate — so all three binaries
    /// would have reported the same thing and nothing would have told the MCP
    /// server apart from the sync CLI.
    #[test]
    fn the_three_binaries_are_distinguishable() {
        let mcp = release_id("memory-mcp", "0.29.0", "abc123");
        let sync = release_id("memory-sync", "0.29.0", "abc123");
        assert_ne!(mcp, sync);
    }

    /// An unknown commit degrades to the version rather than shipping the word
    /// "unknown" into a release name, which would group every unstamped build
    /// of every version together.
    #[test]
    fn an_unknown_commit_falls_back_to_the_version() {
        assert_eq!(
            release_id("memory-mcp", "0.29.0", "unknown"),
            "memory-mcp@0.29.0"
        );
        assert_eq!(release_id("memory-mcp", "0.29.0", ""), "memory-mcp@0.29.0");
    }

    /// A dirty tree is a build nobody else can reproduce, and the SHA says so.
    #[test]
    fn a_dirty_build_is_marked_as_such() {
        assert!(release_id("memory-mcp", "0.29.0", "abc123-dirty").ends_with("+abc123-dirty"));
    }

    /// The stamp is present in this very build, so a build.rs that silently
    /// stopped running would fail here rather than in a report months later.
    #[test]
    fn this_build_carries_a_stamp() {
        assert!(
            !BUILD_SHA.is_empty(),
            "build.rs did not set FERROSA_BUILD_SHA"
        );
    }

    /// A release build with no Sentry still writes a local log.
    ///
    /// This repository is public and Sentry is the user's to configure. With no
    /// DSN, disk is the only place a failure can be recorded — and a user
    /// reporting a problem needs something to send.
    #[test]
    fn a_release_build_without_sentry_still_writes_to_disk() {
        let plan = TelemetryPlan::resolve(None, None, false);
        assert!(!plan.reports_errors(), "nothing leaves the machine");
        assert!(plan.writes_debug_log(), "but it is written down somewhere");
    }

    /// With Sentry configured, a release build does not ALSO write to disk by
    /// default: errors have somewhere to go, and an unbounded local log on
    /// every deployment is a cost nobody asked for.
    #[test]
    fn sentry_on_a_release_build_replaces_the_disk_fallback() {
        let plan = TelemetryPlan::resolve(Some("https://k@o1.ingest.us.sentry.io/2"), None, false);
        assert!(plan.reports_errors());
        assert!(!plan.writes_debug_log());
    }

    /// A testing build writes one regardless, even with Sentry configured.
    #[test]
    fn a_testing_build_writes_a_log_even_with_sentry() {
        let plan = TelemetryPlan::resolve(Some("https://k@o1.ingest.us.sentry.io/2"), None, true);
        assert!(plan.reports_errors() && plan.writes_debug_log());
    }

    /// The point of the debug tier: a testing build writes one without being
    /// asked, because the failures worth catching are the unwatched ones.
    #[test]
    fn a_testing_build_writes_a_debug_log_by_default() {
        let plan = TelemetryPlan::resolve(None, None, true);
        assert_eq!(
            plan.debug_log_dir,
            Some(PathBuf::from(DEFAULT_DEBUG_LOG_DIR))
        );
    }

    /// Asking for a debug log on a release build is a deliberate act and is
    /// honoured.
    #[test]
    fn a_named_directory_wins_on_a_release_build() {
        let plan = TelemetryPlan::resolve(None, Some("/var/log/ferrosa"), false);
        assert_eq!(plan.debug_log_dir, Some(PathBuf::from("/var/log/ferrosa")));
    }

    /// ...and overrides the testing default rather than being ignored.
    #[test]
    fn a_named_directory_overrides_the_testing_default() {
        let plan = TelemetryPlan::resolve(None, Some("/tmp/here"), true);
        assert_eq!(plan.debug_log_dir, Some(PathBuf::from("/tmp/here")));
    }

    /// A usable DSN turns error reporting on.
    #[test]
    fn a_usable_dsn_enables_error_reporting() {
        let plan = TelemetryPlan::resolve(Some("https://k@o1.ingest.us.sentry.io/2"), None, false);
        assert!(plan.reports_errors());
    }

    /// An empty or whitespace value is the shape an unset environment variable
    /// takes in a shell script, and must not be treated as configuration.
    #[test]
    fn an_empty_dsn_is_absent_not_broken() {
        assert!(!TelemetryPlan::resolve(Some(""), None, false).reports_errors());
        assert!(!TelemetryPlan::resolve(Some("   "), None, false).reports_errors());
    }

    /// A value that is not a URL cannot work. Starting the SDK with it would
    /// complain on every launch about something nobody asked for.
    #[test]
    fn a_dsn_that_is_not_a_url_is_refused() {
        assert!(!TelemetryPlan::resolve(Some("not-a-dsn"), None, false).reports_errors());
        assert!(
            !TelemetryPlan::resolve(Some("http://k@example.com/1"), None, false).reports_errors(),
            "plain http would send diagnostics in the clear"
        );
    }

    /// The two tiers are independent: errors can be reported with no debug
    /// log, and a debug log written with nowhere to report errors.
    #[test]
    fn the_two_tiers_are_independent() {
        let errors_only =
            TelemetryPlan::resolve(Some("https://k@o1.ingest.us.sentry.io/2"), None, false);
        assert!(errors_only.reports_errors() && !errors_only.writes_debug_log());

        let debug_only = TelemetryPlan::resolve(None, Some("/tmp/x"), false);
        assert!(!debug_only.reports_errors() && debug_only.writes_debug_log());
    }
}
