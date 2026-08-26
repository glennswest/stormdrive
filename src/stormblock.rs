//! Client for the local stormblock management API (:9090). Phase 1 uses it
//! read-only (reconcile Active state); phase 4 adds the register/drain
//! flows behind `stormblock.auto_add`.

use crate::config::StormBlockConfig;
use crate::drive::DriveKind;
use serde_json::Value;
use std::time::Duration;

#[derive(Clone)]
pub struct StormBlockClient {
    cfg: StormBlockConfig,
    http: reqwest::Client,
}

impl StormBlockClient {
    pub fn new(cfg: StormBlockConfig) -> Self {
        Self {
            cfg,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("reqwest client"),
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.cfg.url.trim_end_matches('/'))
    }

    /// GET /api/v1/drives — stormblock's view of its open drives.
    pub async fn list_drives(&self) -> anyhow::Result<Vec<Value>> {
        let v: Value = self
            .http
            .get(self.url("/api/v1/drives"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(match v {
            Value::Array(a) => a,
            Value::Object(mut o) => o
                .remove("drives")
                .and_then(|d| d.as_array().cloned())
                .unwrap_or_default(),
            _ => Vec::new(),
        })
    }

    /// POST /api/v1/drives {path} — open a drive in stormblock.
    pub async fn add_drive(&self, path: &str) -> anyhow::Result<Value> {
        Ok(self
            .http
            .post(self.url("/api/v1/drives"))
            .json(&serde_json::json!({ "path": path }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// DELETE /api/v1/drives/{id} — id may be a UUID or a path.
    pub async fn delete_drive(&self, id_or_path: &str, force: bool) -> anyhow::Result<()> {
        let q = if force { "?force=true" } else { "" };
        self.http
            .delete(self.url(&format!(
                "/api/v1/drives/{}{q}",
                urlencode_path(id_or_path)
            )))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// GET /api/v1/slabs — used as a best-effort guard on fleet leave: a
    /// slab whose device path matches the drive means data still lives
    /// there (stormblock#70 will make this association first-class).
    pub async fn list_slabs(&self) -> anyhow::Result<Vec<Value>> {
        let v: Value = self
            .http
            .get(self.url("/api/v1/slabs"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(match v {
            Value::Array(a) => a,
            Value::Object(mut o) => o
                .remove("slabs")
                .and_then(|d| d.as_array().cloned())
                .unwrap_or_default(),
            _ => Vec::new(),
        })
    }

    /// POST /api/v1/slabs {device_path, tier} — format the drive as a slab.
    pub async fn format_slab(&self, device_path: &str, tier: &str) -> anyhow::Result<Value> {
        Ok(self
            .http
            .post(self.url("/api/v1/slabs"))
            .json(&serde_json::json!({ "device_path": device_path, "tier": tier }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// The slab tier a drive of this kind should get: config override first,
    /// then the kind's default.
    pub fn tier_for(&self, kind: DriveKind) -> String {
        let key = match kind {
            DriveKind::NvmeSsd => "nvme_ssd",
            DriveKind::SasSsd => "sas_ssd",
            DriveKind::SasHdd => "sas_hdd",
            DriveKind::SataSsd => "sata_ssd",
            DriveKind::SataHdd => "sata_hdd",
            DriveKind::Unknown => "unknown",
        };
        self.cfg
            .tier_map
            .get(key)
            .cloned()
            .unwrap_or_else(|| kind.default_tier().to_string())
    }
}

/// Percent-encode the path-segment characters that matter for a /dev path
/// used as a URL path parameter.
fn urlencode_path(s: &str) -> String {
    s.replace('%', "%25").replace('/', "%2F")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_paths_encode_for_url_segments() {
        assert_eq!(urlencode_path("/dev/sda"), "%2Fdev%2Fsda");
    }

    #[test]
    fn tier_map_overrides_defaults() {
        let mut cfg = StormBlockConfig::default();
        cfg.tier_map.insert("sas_hdd".into(), "cold".into());
        let c = StormBlockClient::new(cfg);
        assert_eq!(c.tier_for(DriveKind::SasHdd), "cold");
        assert_eq!(c.tier_for(DriveKind::NvmeSsd), "hot");
    }
}
