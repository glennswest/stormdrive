//! stormdrive.toml parsing. A missing file is not an error (stormblock
//! convention): defaults apply, CLI flags override the file.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub listen_addr: String,
    pub data_dir: Option<String>,
    pub node_name: Option<String>,
    pub discovery: DiscoveryConfig,
    pub monitor: MonitorConfig,
    pub stormblock: StormBlockConfig,
    pub api: ApiConfig,
    pub firmware: FirmwareConfig,
    pub worker: WorkerConfig,
    pub kubernetes: KubernetesConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:9092".into(),
            data_dir: None,
            node_name: None,
            discovery: DiscoveryConfig::default(),
            monitor: MonitorConfig::default(),
            stormblock: StormBlockConfig::default(),
            api: ApiConfig::default(),
            firmware: FirmwareConfig::default(),
            worker: WorkerConfig::default(),
            kubernetes: KubernetesConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FirmwareConfig {
    /// WRITE BUFFER / Firmware Image Download chunk size. 32 KiB is what
    /// the SAS vendors' own instructions use; the drive's READ BUFFER
    /// offset boundary rounds it up when larger.
    pub chunk_kib: u32,
    /// Largest image accepted by `PUT /api/v1/firmware/images/{name}`.
    pub max_image_mib: u32,
}

impl Default for FirmwareConfig {
    fn default() -> Self {
        Self {
            chunk_kib: 32,
            max_image_mib: 256,
        }
    }
}

/// The drive worker (#5).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkerConfig {
    /// Low-level steps (format, sanitize, partition) running at once behind
    /// one HBA (an NVMe drive is its own). The drive does the work; the
    /// host only polls, so a shelf's worth in parallel is the normal case.
    pub max_per_hba: usize,
    /// `enroll` steps running at once per failure domain (shelf, else HBA):
    /// what changes stormblock's pool goes one at a time per domain.
    pub enroll_per_domain: usize,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self { max_per_hba: 8, enroll_per_domain: 1 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoveryConfig {
    pub interval_secs: u64,
    /// Extra exclusion patterns ('*' wildcard) on the kernel name. The
    /// built-in exclusions (loop*, ram*, dm-*, md*, sr*, nbd*, ublkb*, …)
    /// always apply.
    pub exclude: Vec<String>,
    /// Explicit allow-list; empty means all eligible devices.
    pub include: Vec<String>,
    /// Manage drives that have mounted partitions. Off by default: a
    /// mounted drive is somebody's root/boot disk until proven otherwise.
    pub manage_mounted: bool,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            interval_secs: 30,
            exclude: Vec::new(),
            include: Vec::new(),
            manage_mounted: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorConfig {
    pub interval_secs: u64,
    pub temp_warn_c: i32,
    pub temp_crit_c: i32,
    pub spare_warn_pct: u8,
    pub spare_crit_pct: u8,
    pub wear_warn_pct: u8,
    pub wear_crit_pct: u8,
    /// Consecutive samples required before a *worsening* transition sticks.
    pub hysteresis: u32,
    /// Health reads in flight at once (#15): no thread per drive.
    pub max_concurrent: usize,
    /// A health read that has not answered by then counts as a failed
    /// sample; the drive is not read again until the stuck read returns.
    pub sample_timeout_secs: u64,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            interval_secs: 60,
            temp_warn_c: 55,
            temp_crit_c: 70,
            spare_warn_pct: 20,
            spare_crit_pct: 10,
            wear_warn_pct: 80,
            wear_crit_pct: 95,
            hysteresis: 3,
            max_concurrent: 8,
            sample_timeout_secs: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StormBlockConfig {
    pub enabled: bool,
    pub url: String,
    /// Phase 4: register qualified drives with stormblock automatically.
    /// Explicit opt-in; discovery/monitoring never depends on it.
    pub auto_add: bool,
    /// Format a slab on a drive auto-add registers (tier from `tier_map`).
    #[serde(default = "yes")]
    pub auto_format_slab: bool,
    /// Push our health conclusions to stormblock: Failing/Failed quarantine
    /// the drive's slabs and make redundant volumes stop reading that leg.
    #[serde(default = "yes")]
    pub push_health: bool,
    /// Start a stormblock drain on our own when a fleet drive goes
    /// Failing/Failed, and retire it (leave the fleet, locate LED on) when
    /// the drain reports empty. Off does not stop the engine draining a
    /// drive reported `failed` (#43).
    #[serde(default = "yes")]
    pub drain_on_failing: bool,
    /// kind → slab tier overrides; DriveKind::default_tier() otherwise.
    pub tier_map: BTreeMap<String, String>,
    /// The engine's bearer token, named explicitly (stormblock#107). Empty
    /// = `$STORMBLOCK_API_TOKEN`, then `token_file`.
    pub api_token: String,
    /// Where the engine minted its token. Empty = `$STORMBLOCK_TOKEN_FILE`,
    /// then `/run/stormblock/engine/api_token`, `/etc/stormblock/api_token`,
    /// `/var/lib/stormblock/api_token`. Re-read while absent and on a 401.
    pub token_file: String,
    /// Destructive verbs (`DELETE`) need the engine's `admin_token` when a
    /// node sets one. Empty = `$STORMBLOCK_ADMIN_TOKEN`, then `api_token`.
    pub admin_token: String,
}

fn yes() -> bool {
    true
}

impl Default for StormBlockConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            url: "http://127.0.0.1:9090".into(),
            auto_add: false,
            auto_format_slab: true,
            push_health: true,
            drain_on_failing: true,
            tier_map: BTreeMap::new(),
            api_token: String::new(),
            token_file: String::new(),
            admin_token: String::new(),
        }
    }
}

/// :9092's transport and its callers (#19, #45, stormcos#250): TLS from a
/// stormcert pair, nothing anonymous but health. A read needs a node-CA
/// client certificate, a Kubernetes bearer the apiserver allows `get` on
/// `storage.storm.io` (`storage-viewer`), or the admin token; a write needs
/// `storage-admin` (bearer or client certificate) or the admin token.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    /// A break-glass bearer accepted for every write, for a node with no
    /// apiserver. Empty = `$STORMDRIVE_ADMIN_TOKEN`, then `admin_token_file`.
    /// (`api_token`, parsed and never enforced before 0.18.0, is read as
    /// this.)
    #[serde(alias = "api_token")]
    pub admin_token: String,
    /// Where that token is kept (root-only, never mounted into other
    /// services). Empty = none.
    pub admin_token_file: String,
    /// `enforce`: a write without an allowed bearer is refused. `audit`: it
    /// goes through and is logged as one `enforce` would refuse — for
    /// rolling the gate out, never for running open.
    pub admin_gate: String,
    /// The serving certificate (PEM, chain first) for :9092 (#19) — what
    /// `stormcert-agent serving --cn stormdrive` writes. Re-read when it
    /// changes; while there is none a TLS handshake fails and plain HTTP
    /// answers health only.
    pub tls_cert_file: String,
    pub tls_key_file: String,
    /// The CAs a client certificate is verified against: the node CA. A
    /// file that is not there is skipped; with none, only bearers
    /// authenticate.
    pub client_ca_files: Vec<String>,
    /// **Transition only** (#19): serve plain HTTP and reads with no
    /// credential as before, so a release can carry TLS before every caller
    /// presents one. A credential that is sent is still checked, and writes
    /// keep the #45 gate.
    pub allow_anonymous: bool,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            admin_token: String::new(),
            admin_token_file: String::new(),
            admin_gate: "enforce".into(),
            tls_cert_file: "/data/stormcert/stormdrive.crt".into(),
            tls_key_file: "/data/stormcert/stormdrive.key".into(),
            client_ca_files: vec!["/data/stormcert/ca.crt".into()],
            allow_anonymous: false,
        }
    }
}

/// The cluster's apiserver (#45): reviews bearers (TokenReview +
/// SubjectAccessReview) and holds the `Drive` and `DriveOperation` objects.
/// Unset `api_url` = no apiserver: Kubernetes bearers are refused and the
/// controller does not run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KubernetesConfig {
    /// `https://<apiserver>:6443`. Empty = `$STORMDRIVE_KUBE_API`, then the
    /// in-cluster service (`$KUBERNETES_SERVICE_HOST`) when a service
    /// account token is mounted.
    pub api_url: String,
    /// The apiserver's CA (PEM). Empty = `$STORMDRIVE_KUBE_CA`, then the
    /// service account's `ca.crt`.
    pub ca_file: String,
    /// stormdrive's own credential: may create `tokenreviews` and
    /// `subjectaccessreviews` (`system:auth-delegator`) and holds the
    /// `stormdrive-controller` role (deploy/rbac.yaml). Empty =
    /// `$STORMDRIVE_KUBE_TOKEN_FILE`, then the service account's token.
    pub token_file: String,
    /// Skip TLS verification (a lab apiserver with no CA at hand).
    pub insecure: bool,
    /// Keep `Drive` objects and run `DriveOperation`s for this node.
    pub controller: bool,
    /// Between controller passes.
    pub interval_secs: u64,
}

impl Default for KubernetesConfig {
    fn default() -> Self {
        Self { api_url: String::new(), ca_file: String::new(), token_file: String::new(), insecure: false, controller: true, interval_secs: 5 }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(toml::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(?path, "no config file, using defaults");
                Ok(Self::default())
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        self.listen_addr
            .parse::<std::net::SocketAddr>()
            .map_err(|e| anyhow::anyhow!("listen_addr {:?}: {e}", self.listen_addr))?;
        if self.monitor.interval_secs == 0 || self.discovery.interval_secs == 0 {
            anyhow::bail!("intervals must be non-zero");
        }
        if self.monitor.max_concurrent == 0 || self.monitor.sample_timeout_secs == 0 {
            anyhow::bail!("monitor.max_concurrent and monitor.sample_timeout_secs must be non-zero");
        }
        if self.worker.max_per_hba == 0 || self.worker.enroll_per_domain == 0 {
            anyhow::bail!("worker.max_per_hba and worker.enroll_per_domain must be non-zero");
        }
        if !matches!(self.api.admin_gate.as_str(), "enforce" | "audit") {
            anyhow::bail!("api.admin_gate {:?}: use enforce or audit", self.api.admin_gate);
        }
        if self.api.tls_cert_file.trim().is_empty() != self.api.tls_key_file.trim().is_empty() {
            anyhow::bail!("api.tls_cert_file and api.tls_key_file go together");
        }
        if self.kubernetes.interval_secs == 0 {
            anyhow::bail!("kubernetes.interval_secs must be non-zero");
        }
        if self.monitor.hysteresis == 0 {
            anyhow::bail!("monitor.hysteresis must be >= 1");
        }
        Ok(())
    }

    pub fn node_name(&self) -> String {
        if let Some(n) = &self.node_name {
            if !n.is_empty() {
                return n.clone();
            }
        }
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unknown".into())
    }
}

/// Minimal '*' wildcard match, enough for device-name patterns.
pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    fn inner(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => inner(&p[1..], n) || (!n.is_empty() && inner(p, &n[1..])),
            (Some(pc), Some(nc)) if pc == nc => inner(&p[1..], &n[1..]),
            _ => false,
        }
    }
    inner(pattern.as_bytes(), name.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn parses_partial_toml() {
        let c: Config = toml::from_str(
            r#"
            listen_addr = "0.0.0.0:9192"
            [monitor]
            temp_warn_c = 50
            [stormblock]
            auto_add = true
            tier_map = { nvme_ssd = "hot" }
            "#,
        )
        .unwrap();
        assert_eq!(c.listen_addr, "0.0.0.0:9192");
        assert_eq!(c.monitor.temp_warn_c, 50);
        assert_eq!(c.monitor.temp_crit_c, 70, "unset fields keep defaults");
        assert!(c.stormblock.auto_add);
        assert_eq!(c.stormblock.tier_map["nvme_ssd"], "hot");
    }

    /// The shipped example claims every value in it is the default: keep
    /// it that way (#7). `data_dir` is the one it sets on purpose.
    #[test]
    fn example_config_is_the_defaults() {
        let mut c: Config = toml::from_str(include_str!("../deploy/stormdrive.example.toml")).unwrap();
        assert_eq!(c.data_dir.as_deref(), Some("/var/lib/stormdrive"));
        c.data_dir = None;
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            serde_json::to_value(Config::default()).unwrap()
        );
    }

    #[test]
    fn wildcard() {
        assert!(wildcard_match("sd*", "sda"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("nvme*n1", "nvme0n1"));
        assert!(!wildcard_match("sd*", "nvme0n1"));
        assert!(wildcard_match("sda", "sda"));
        assert!(!wildcard_match("sda", "sdab"));
    }

    #[test]
    fn bad_listen_addr_fails_validation() {
        let c = Config {
            listen_addr: "not-an-addr".into(),
            ..Config::default()
        };
        assert!(c.validate().is_err());
    }
}
