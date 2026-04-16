//! Helper for integration tests that target the isolated test cluster
//! (started via `scripts/start-test-cluster.sh`).
//!
//! Tests use [`TestClusterConfig::from_env`] to read connection details
//! from `FERROSA_TEST_*` environment variables. When unset, tests should
//! skip — the pattern is `#[ignore]` on the test plus a check via
//! [`TestClusterConfig::from_env_or_skip`] inside the body.

/// Connection details for the isolated test Ferrosa cluster.
///
/// Defaults target the `docker-compose.test.yml` topology at
/// <http://localhost:17974> (graph) and `localhost:19542` (CQL).
#[derive(Debug, Clone)]
pub struct TestClusterConfig {
    pub cql_host: String,
    pub cql_port: u16,
    pub graph_url: String,
    pub keyspace: String,
    pub s3_endpoint: String,
}

impl TestClusterConfig {
    /// Construct from `FERROSA_TEST_*` env vars. Returns `None` when the
    /// required ones (currently just `FERROSA_TEST_CQL_PORT`) are unset,
    /// so tests can skip cleanly.
    pub fn from_env() -> Option<Self> {
        let port: u16 = std::env::var("FERROSA_TEST_CQL_PORT").ok()?.parse().ok()?;
        Some(Self {
            cql_host: std::env::var("FERROSA_TEST_CQL_HOST")
                .unwrap_or_else(|_| "localhost".into()),
            cql_port: port,
            graph_url: std::env::var("FERROSA_TEST_GRAPH_URL")
                .unwrap_or_else(|_| "http://localhost:17974".into()),
            keyspace: std::env::var("FERROSA_TEST_KEYSPACE")
                .unwrap_or_else(|_| "agent_memory_test".into()),
            s3_endpoint: std::env::var("FERROSA_TEST_S3_ENDPOINT")
                .unwrap_or_else(|_| "http://localhost:19500".into()),
        })
    }

    /// Same as [`from_env`], but prints a skip message to stderr when the
    /// cluster isn't wired. Call from the top of an `#[ignore]`d integration
    /// test body.
    pub fn from_env_or_skip() -> Option<Self> {
        let cfg = Self::from_env();
        if cfg.is_none() {
            eprintln!(
                "FERROSA_TEST_CQL_PORT unset; skipping live test. \
                 Start the test cluster: scripts/start-test-cluster.sh"
            );
        }
        cfg
    }

    /// Contact-point string suitable for cdrs-tokio.
    pub fn contact_point(&self) -> String {
        format!("{}:{}", self.cql_host, self.cql_port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contact_point_formats_correctly() {
        let c = TestClusterConfig {
            cql_host: "localhost".into(),
            cql_port: 19542,
            graph_url: "http://localhost:17974".into(),
            keyspace: "agent_memory_test".into(),
            s3_endpoint: "http://localhost:19500".into(),
        };
        assert_eq!(c.contact_point(), "localhost:19542");
    }

    #[test]
    fn from_env_returns_none_when_port_unset() {
        // SAFETY: This test explicitly unsets a scoped env var with no
        // other threads reading it. The serial_test crate would be
        // cleaner, but we're not pulling in an extra dep for one case.
        let prev = std::env::var("FERROSA_TEST_CQL_PORT").ok();
        unsafe {
            std::env::remove_var("FERROSA_TEST_CQL_PORT");
        }
        let result = TestClusterConfig::from_env();
        if let Some(p) = prev {
            unsafe {
                std::env::set_var("FERROSA_TEST_CQL_PORT", p);
            }
        }
        assert!(result.is_none());
    }
}
