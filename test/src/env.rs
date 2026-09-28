//! What the runner hands the container (stormcentral docs/test-standard.md).

use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Env {
    /// The node's stormdrive, `http://<STORM_NODE>:9092`.
    pub base: Option<String>,
    pub run_id: String,
    pub timeout: Duration,
    pub results: PathBuf,
    /// Bearer token, for when stormdrive's API gains auth (#19). Sent when
    /// set; today the API is open.
    pub token: Option<String>,
    /// Upper bound on the drives a long wave smoke-tests at once.
    pub wave_max: Option<usize>,
}

fn var(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

/// `host`, `host:port`, `v6`, `[v6]` or `[v6]:port` → `http://…` with
/// stormdrive's port (9092) when none is given.
pub fn with_port(host: &str) -> String {
    let host = host.trim_start_matches("http://").trim_end_matches('/');
    let has_port = if host.starts_with('[') {
        host.contains("]:")
    } else {
        host.matches(':').count() == 1
    };
    match (has_port, host.starts_with('['), host.contains(':')) {
        (true, _, _) => format!("http://{host}"),
        (false, true, _) => format!("http://{host}:9092"),
        (false, false, true) => format!("http://[{host}]:9092"),
        (false, false, false) => format!("http://{host}:9092"),
    }
}

pub fn default_timeout(suite: &str) -> Duration {
    Duration::from_secs(match suite {
        "short" => 120,
        "medium" => 1800,
        _ => 8 * 3600,
    })
}

impl Env {
    pub fn read(suite: &str) -> Env {
        // STORM_STORMDRIVE_URL overrides, for a run against a dev instance.
        let base = var("STORM_STORMDRIVE_URL")
            .map(|u| u.trim_end_matches('/').to_string())
            .or_else(|| var("STORM_NODE").map(|n| with_port(&n)));
        Env {
            base,
            run_id: var("STORM_RUN_ID").unwrap_or_else(|| format!("local-{}", std::process::id())),
            timeout: var("STORM_TIMEOUT")
                .and_then(|t| t.parse().ok())
                .map(Duration::from_secs)
                .unwrap_or_else(|| default_timeout(suite)),
            results: PathBuf::from(var("STORM_RESULTS").unwrap_or_else(|| "/results".into())),
            token: var("STORM_STORMDRIVE_TOKEN"),
            wave_max: var("STORM_WAVE_MAX").and_then(|v| v.parse().ok()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::with_port;

    #[test]
    fn node_addresses() {
        assert_eq!(with_port("10.0.0.5"), "http://10.0.0.5:9092");
        assert_eq!(with_port("node1:9999"), "http://node1:9999");
        assert_eq!(with_port("http://node1/"), "http://node1:9092");
        assert_eq!(with_port("fe80::1"), "http://[fe80::1]:9092");
        assert_eq!(with_port("[fe80::1]"), "http://[fe80::1]:9092");
        assert_eq!(with_port("[fe80::1]:9092"), "http://[fe80::1]:9092");
    }
}
