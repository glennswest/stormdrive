//! Client for the local stormblock management API (:9090).
//!
//! stormblock v11 closed the loop this daemon exists for (stormblock#70):
//! a drive is registered **with where it is** (`labels`) and **what it is**
//! (`uuid`), its slabs are listed by identity, a health report quarantines
//! it before anyone orders it out, and a drain empties it over HTTP with
//! progress. Everything here is that surface, and nothing here decides
//! policy — `fleet.rs` does.

use crate::config::StormBlockConfig;
use crate::drive::DriveKind;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone)]
pub struct StormBlockClient {
    cfg: StormBlockConfig,
    http: reqwest::Client,
}

/// What stormblock says about a drain (`GET /api/v1/drives/{id}/drain`).
#[derive(Debug, Clone, Deserialize)]
pub struct DrainStatus {
    pub drive: String,
    /// `running`, `empty`, `stuck`, `cancelled`.
    pub state: String,
    #[serde(default)]
    pub moved: u64,
    #[serde(default)]
    pub failed: u64,
    #[serde(default)]
    pub remaining: u64,
    #[serde(default)]
    pub errors: Vec<String>,
}

impl DrainStatus {
    pub fn is_empty(&self) -> bool {
        self.state == "empty"
    }
    pub fn is_running(&self) -> bool {
        self.state == "running"
    }
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

    fn drive_url(&self, id_or_path: &str, suffix: &str) -> String {
        self.url(&format!("/api/v1/drives/{}{suffix}", urlencode_path(id_or_path)))
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
                .remove("items")
                .or_else(|| o.remove("drives"))
                .and_then(|d| d.as_array().cloned())
                .unwrap_or_default(),
            _ => Vec::new(),
        })
    }

    /// POST /api/v1/drives {path, labels, uuid} — open a drive in stormblock
    /// with where it is and what it is. The labels become the failure
    /// domain of every slab on it; the uuid is our stable identity, so the
    /// engine's per-open uuid (stormblock#65) never has to be the one that
    /// matters.
    pub async fn add_drive(
        &self,
        path: &str,
        labels: &[(String, String)],
        uuid: Option<Uuid>,
    ) -> anyhow::Result<Value> {
        let labels: serde_json::Map<String, Value> = labels
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let mut body = serde_json::json!({ "path": path, "labels": labels });
        if let Some(u) = uuid {
            body["uuid"] = Value::String(u.to_string());
        }
        Ok(self
            .http
            .post(self.url("/api/v1/drives"))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// PUT /api/v1/drives/{id}/labels — relabel after the fact (a shelf
    /// resolved late, a drive moved bays). Every slab on it follows.
    pub async fn set_labels(
        &self,
        id_or_path: &str,
        labels: &[(String, String)],
        uuid: Option<Uuid>,
    ) -> anyhow::Result<()> {
        let mut map: serde_json::Map<String, Value> = labels
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        if let Some(u) = uuid {
            map.insert("drive".into(), Value::String(u.to_string()));
        }
        self.http
            .put(self.drive_url(id_or_path, "/labels"))
            .json(&serde_json::json!({ "labels": map }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// DELETE /api/v1/drives/{id} — id may be a UUID or a path.
    pub async fn delete_drive(&self, id_or_path: &str, force: bool) -> anyhow::Result<()> {
        let q = if force { "?force=true" } else { "" };
        self.http
            .delete(self.drive_url(id_or_path, q))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// GET /api/v1/drives/{id}/slabs — the slabs on this device, by
    /// identity (stormblock#70 item 2). Empty means nothing lives there.
    pub async fn drive_slabs(&self, id_or_path: &str) -> anyhow::Result<Vec<Value>> {
        let v: Value = self
            .http
            .get(self.drive_url(id_or_path, "/slabs"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(v.get("items").and_then(|i| i.as_array().cloned()).unwrap_or_default())
    }

    /// GET /api/v1/slabs — the whole pool, for the summary card.
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
                .remove("items")
                .or_else(|| o.remove("slabs"))
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

    /// POST /api/v1/drives/{id}/health — tell the engine what we concluded.
    /// `healthy` lifts a quarantine; `degraded`/`failing` quarantine the
    /// drive's slabs and make every redundant volume stop reading that leg;
    /// `failed`/`missing` also start a drain.
    pub async fn report_health(
        &self,
        id_or_path: &str,
        state: &str,
        reason: Option<&str>,
        drain: bool,
    ) -> anyhow::Result<Value> {
        Ok(self
            .http
            .post(self.drive_url(id_or_path, "/health"))
            .json(&serde_json::json!({ "state": state, "reason": reason, "drain": drain }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// POST /api/v1/drives/{id}/drain — empty every slab on the drive.
    pub async fn start_drain(&self, id_or_path: &str) -> anyhow::Result<DrainStatus> {
        Ok(self
            .http
            .post(self.drive_url(id_or_path, "/drain"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// GET /api/v1/drives/{id}/drain — where the drain is.
    pub async fn drain_status(&self, id_or_path: &str) -> anyhow::Result<Option<DrainStatus>> {
        let resp = self.http.get(self.drive_url(id_or_path, "/drain")).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(resp.error_for_status()?.json().await?))
    }

    /// DELETE /api/v1/drives/{id}/drain — stop a drain; what moved stays moved.
    pub async fn cancel_drain(&self, id_or_path: &str) -> anyhow::Result<()> {
        self.http
            .delete(self.drive_url(id_or_path, "/drain"))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
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

    #[test]
    fn drain_status_terminal_states() {
        let s: DrainStatus = serde_json::from_value(serde_json::json!({
            "drive": "/dev/sdb", "state": "empty", "moved": 12, "remaining": 0
        }))
        .unwrap();
        assert!(s.is_empty() && !s.is_running());
        let s: DrainStatus =
            serde_json::from_value(serde_json::json!({ "drive": "/dev/sdb", "state": "running" })).unwrap();
        assert!(s.is_running());
    }
}
