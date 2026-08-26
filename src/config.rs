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
        }
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
    /// kind → slab tier overrides; DriveKind::default_tier() otherwise.
    pub tier_map: BTreeMap<String, String>,
}

impl Default for StormBlockConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            url: "http://127.0.0.1:9090".into(),
            auto_add: false,
            tier_map: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    /// Empty = no auth (current ecosystem posture on node LANs). Present in
    /// the schema from day one so enabling it is not a format change.
    pub api_token: String,
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
        let mut c = Config::default();
        c.listen_addr = "not-an-addr".into();
        assert!(c.validate().is_err());
    }
}
