//! What the runner hands the container (stormcentral docs/test-standard.md).

use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Env {
    /// The node's stormdrive, `https://<STORM_NODE>:9092` (#19).
    pub base: Option<String>,
    /// `base` came from `STORM_NODE` (not an explicit URL): a node that does
    /// not speak TLS yet is tried over plain HTTP.
    pub base_from_node: bool,
    /// How this run proves who it is and checks the node's certificate.
    pub tls: Tls,
    pub run_id: String,
    pub timeout: Duration,
    pub results: PathBuf,
    /// A storage-admin bearer (or the node's admin token), sent on every
    /// call. Since 0.18.0 (#45) writes need one; without it the checks that
    /// write are skipped.
    pub token: Option<String>,
    /// A bearer for reads when no `token` is given: the pod's service
    /// account token (a test Job runs in a pod), so a node with no client
    /// certificate for the run still answers its reads (`storage-viewer`).
    pub read_token: Option<String>,
    /// Upper bound on the drives a long wave smoke-tests at once.
    pub wave_max: Option<usize>,
}

/// TLS material (#19): the CA the node's serving certificate is checked
/// against, and a client pair (PEM, certificate then key) from the node CA.
#[derive(Clone, Debug, Default)]
pub struct Tls {
    pub ca: Option<Vec<u8>>,
    pub identity: Option<Vec<u8>>,
}

const SA_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

fn var(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

fn read(path: &str) -> Option<Vec<u8>> {
    std::fs::read(path).ok().filter(|b| !b.is_empty())
}

impl Tls {
    /// `STORM_STORMDRIVE_CA`, else the service account's `ca.crt` (the
    /// cluster's CA is the node CA on a stormcos node), else
    /// `/data/stormcert/ca.crt`. `STORM_STORMDRIVE_CERT` + `_KEY`: the
    /// client pair.
    pub fn from_env() -> Tls {
        let ca = var("STORM_STORMDRIVE_CA")
            .and_then(|p| read(&p))
            .or_else(|| read(&format!("{SA_DIR}/ca.crt")))
            .or_else(|| read("/data/stormcert/ca.crt"));
        let identity = match (var("STORM_STORMDRIVE_CERT"), var("STORM_STORMDRIVE_KEY")) {
            (Some(c), Some(k)) => match (read(&c), read(&k)) {
                (Some(mut c), Some(k)) => {
                    c.push(b'\n');
                    c.extend(k);
                    Some(c)
                }
                _ => None,
            },
            _ => None,
        };
        Tls { ca, identity }
    }
}

/// `host`, `host:port`, `v6`, `[v6]` or `[v6]:port` → `https://…` with
/// stormdrive's port (9092) when none is given. TLS since #19; a node still
/// on plain HTTP is found by `run`'s fallback.
pub fn with_port(host: &str) -> String {
    let host = host.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/');
    let has_port = if host.starts_with('[') {
        host.contains("]:")
    } else {
        host.matches(':').count() == 1
    };
    match (has_port, host.starts_with('['), host.contains(':')) {
        (true, _, _) => format!("https://{host}"),
        (false, true, _) => format!("https://{host}:9092"),
        (false, false, true) => format!("https://[{host}]:9092"),
        (false, false, false) => format!("https://{host}:9092"),
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
            base_from_node: var("STORM_STORMDRIVE_URL").is_none() && base.is_some(),
            base,
            run_id: var("STORM_RUN_ID").unwrap_or_else(|| format!("local-{}", std::process::id())),
            timeout: var("STORM_TIMEOUT")
                .and_then(|t| t.parse().ok())
                .map(Duration::from_secs)
                .unwrap_or_else(|| default_timeout(suite)),
            results: PathBuf::from(var("STORM_RESULTS").unwrap_or_else(|| "/results".into())),
            tls: Tls::from_env(),
            token: var("STORM_STORMDRIVE_TOKEN"),
            read_token: std::fs::read_to_string(format!("{SA_DIR}/token")).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()),
            wave_max: var("STORM_WAVE_MAX").and_then(|v| v.parse().ok()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::with_port;

    #[test]
    fn node_addresses() {
        assert_eq!(with_port("10.0.0.5"), "https://10.0.0.5:9092");
        assert_eq!(with_port("node1:9999"), "https://node1:9999");
        assert_eq!(with_port("http://node1/"), "https://node1:9092");
        assert_eq!(with_port("https://node1/"), "https://node1:9092");
        assert_eq!(with_port("fe80::1"), "https://[fe80::1]:9092");
        assert_eq!(with_port("[fe80::1]"), "https://[fe80::1]:9092");
        assert_eq!(with_port("[fe80::1]:9092"), "https://[fe80::1]:9092");
    }
}
