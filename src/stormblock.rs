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
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use uuid::Uuid;

/// Where the engine mints its token when nothing names one (stormblock#107;
/// the first is where stormcos mounts it into this container, stormcos#104).
const DEFAULT_TOKEN_FILES: &[&str] = &[
    "/run/stormblock/engine/api_token",
    "/etc/stormblock/api_token",
    "/var/lib/stormblock/api_token",
];

/// Where the engine mints its admin token (stormblock#274: never under
/// `/run/stormblock`, which every service mounts).
const DEFAULT_ADMIN_TOKEN_FILE: &str = "/run/stormblock-admin/admin_token";

#[derive(Clone)]
pub struct StormBlockClient {
    cfg: StormBlockConfig,
    http: reqwest::Client,
    /// The engine's bearer token. The engine mints it at boot, possibly
    /// after we start, and may rotate it: `None` is re-read on every call,
    /// and a 401 re-reads it once.
    token: Arc<RwLock<Option<String>>>,
    /// The engine's admin token for destructive verbs (stormblock#274).
    admin: TokenSource,
    /// stormdrive's own Kubernetes credential (the `[kubernetes]` token file
    /// or the service account's): the engine reviews it for `storage-admin`
    /// when no admin token is at hand (#46).
    kube_token_file: Option<PathBuf>,
}

/// Where the engine token comes from, in stormblock's own CLI order: a
/// named token first, then the first readable non-empty file.
#[derive(Debug, Clone, PartialEq)]
struct TokenSource {
    explicit: Option<String>,
    files: Vec<PathBuf>,
}

impl TokenSource {
    fn from_config(cfg: &StormBlockConfig) -> Self {
        Self::resolve(
            &cfg.api_token,
            std::env::var("STORMBLOCK_API_TOKEN").ok(),
            &cfg.token_file,
            std::env::var("STORMBLOCK_TOKEN_FILE").ok(),
            DEFAULT_TOKEN_FILES,
        )
    }

    fn resolve(
        cfg_token: &str,
        env_token: Option<String>,
        cfg_file: &str,
        env_file: Option<String>,
        defaults: &[&str],
    ) -> Self {
        let explicit = non_empty(cfg_token).or_else(|| env_token.as_deref().and_then(non_empty));
        let mut files: Vec<PathBuf> = Vec::new();
        let named = [non_empty(cfg_file), env_file.as_deref().and_then(non_empty)];
        for f in named.into_iter().flatten().chain(defaults.iter().map(|d| d.to_string())) {
            let f = PathBuf::from(f);
            if !files.contains(&f) {
                files.push(f);
            }
        }
        Self { explicit, files }
    }

    fn read(&self) -> Option<String> {
        if let Some(t) = &self.explicit {
            return Some(t.clone());
        }
        self.files.iter().find_map(|f| read_token_file(f))
    }
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn read_token_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().as_deref().and_then(non_empty)
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
        let admin = TokenSource::resolve(
            &cfg.admin_token,
            std::env::var("STORMBLOCK_ADMIN_TOKEN").ok(),
            &cfg.admin_token_file,
            std::env::var("STORMBLOCK_ADMIN_TOKEN_FILE").ok(),
            &[DEFAULT_ADMIN_TOKEN_FILE],
        );
        let token = TokenSource::from_config(&cfg).read();
        if token.is_none() && cfg.enabled {
            tracing::warn!("no stormblock engine token yet; engine calls will retry the lookup");
        }
        Self {
            cfg,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("reqwest client"),
            token: Arc::new(RwLock::new(token)),
            admin,
            kube_token_file: None,
        }
    }

    /// Present this Kubernetes credential (a file, re-read every call) on
    /// destructive verbs when no admin token is readable.
    pub fn with_kube_token_file(mut self, file: Option<PathBuf>) -> Self {
        self.kube_token_file = file;
        self
    }

    /// Re-read the engine token and remember it; the new value.
    fn reload_token(&self) -> Option<String> {
        let t = TokenSource::from_config(&self.cfg).read();
        *self.token.write().unwrap_or_else(|e| e.into_inner()) = t.clone();
        t
    }

    /// The node token for an ordinary call. An absent token is looked up
    /// again every time: the engine writes it at boot, maybe after we start.
    fn bearer(&self) -> Option<String> {
        let cached = self.token.read().unwrap_or_else(|e| e.into_inner()).clone();
        cached.or_else(|| self.reload_token())
    }

    /// What a destructive call presents, in order (stormblock#274): the
    /// engine's admin token, then stormdrive's Kubernetes credential (allowed
    /// when bound to `storage-admin`), then the node token (an engine with
    /// `admin_gate = "audit"`, or one older than #274). Re-read every call:
    /// these are rare, and a token minted or rotated later is picked up.
    fn admin_bearers(&self) -> Vec<(&'static str, String)> {
        let mut out: Vec<(&'static str, String)> = Vec::new();
        let mut push = |who: &'static str, t: Option<String>| {
            if let Some(t) = t {
                if !out.iter().any(|(_, have)| *have == t) {
                    out.push((who, t));
                }
            }
        };
        push("admin token", self.admin.read());
        push("kubernetes bearer", self.kube_token_file.as_deref().and_then(read_token_file));
        push("node token", self.bearer());
        out
    }

    /// A destructive verb: each credential in turn until one is not refused
    /// (401/403). The last refusal is returned, and logged with what the
    /// engine wants.
    async fn send_admin(&self, req: reqwest::RequestBuilder) -> anyhow::Result<reqwest::Response> {
        let bearers = self.admin_bearers();
        let mut req = Some(req);
        let mut last = None;
        for (i, (who, t)) in bearers.iter().enumerate() {
            let Some(r) = req.take() else { break };
            if i + 1 < bearers.len() {
                req = r.try_clone();
            }
            let resp = r.bearer_auth(t).send().await?;
            let s = resp.status();
            if s != reqwest::StatusCode::UNAUTHORIZED && s != reqwest::StatusCode::FORBIDDEN {
                return Ok(resp);
            }
            tracing::debug!(credential = *who, status = s.as_u16(), "stormblock refused a destructive verb");
            last = Some(resp);
        }
        match last {
            Some(resp) => {
                tracing::warn!(
                    status = resp.status().as_u16(),
                    tried = ?bearers.iter().map(|(w, _)| *w).collect::<Vec<_>>(),
                    "stormblock refused a destructive verb (slab format / drive close): it needs the engine's \
                     admin token (stormblock.admin_token_file, default {DEFAULT_ADMIN_TOKEN_FILE}) or a \
                     Kubernetes bearer allowed storage.storm.io (storage-admin)"
                );
                Ok(resp)
            }
            None => Ok(req.expect("request unsent").send().await?),
        }
    }

    /// Every ordinary engine call goes through here (stormblock#107: all of
    /// `/api/v1` needs `Authorization: Bearer`). A 401 re-reads the token
    /// and, when it changed, retries once.
    async fn send(&self, req: reqwest::RequestBuilder) -> anyhow::Result<reqwest::Response> {
        let retry = req.try_clone();
        let used = self.bearer();
        let resp = with_bearer(req, used.as_deref()).send().await?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }
        let fresh = self.reload_token();
        match (retry, fresh) {
            (Some(req), Some(fresh)) if Some(&fresh) != used.as_ref() => {
                tracing::info!("stormblock refused our token; retrying with the re-read one");
                Ok(with_bearer(req, Some(fresh.as_str())).send().await?)
            }
            _ => {
                tracing::warn!(
                    have_token = used.is_some(),
                    "stormblock returned 401; set stormblock.token_file or $STORMBLOCK_TOKEN_FILE"
                );
                Ok(resp)
            }
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
            .send(self.http.get(self.url("/api/v1/drives")))
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
            .send(self.http.post(self.url("/api/v1/drives")).json(&body))
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
        let req = self
            .http
            .put(self.drive_url(id_or_path, "/labels"))
            .json(&serde_json::json!({ "labels": map }));
        self.send(req)
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// PUT /api/v1/drives/{id}/overcommit {enabled, ratio, drive} — the
    /// drive's overcommit setting (#13), for stormblock to enforce when a
    /// claim binds on its slabs (stormblock#152). `drive` names it by our
    /// identity, as the slab listing does, so a disk the engine holds as
    /// `file+…` slabs rather than an opened drive (stormblock#133) is still
    /// found. `Ok(false)`: the engine has no such route yet (404/405).
    pub async fn set_overcommit(
        &self,
        path: &str,
        oc: crate::drive::Overcommit,
        uuid: Uuid,
        wwn: Option<&str>,
        serial: &str,
    ) -> anyhow::Result<bool> {
        let body = serde_json::json!({
            "enabled": oc.enabled,
            "ratio": oc.ratio,
            "drive": { "uuid": uuid.to_string(), "wwn": wwn, "serial": serial, "path": path },
        });
        let resp = self
            .send(self.http.put(self.drive_url(path, "/overcommit")).json(&body))
            .await?;
        if matches!(resp.status(), reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED) {
            return Ok(false);
        }
        resp.error_for_status()?;
        Ok(true)
    }

    /// DELETE /api/v1/drives/{id} — id may be a UUID or a path. Destructive
    /// on the engine (stormblock#274): the admin token or a storage-admin bearer.
    pub async fn delete_drive(&self, id_or_path: &str, force: bool) -> anyhow::Result<()> {
        let q = if force { "?force=true" } else { "" };
        self.send_admin(self.http.delete(self.drive_url(id_or_path, q)))
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// GET /api/v1/drives/{id}/slabs — the slabs on this device, by
    /// identity (stormblock#70 item 2). Empty means nothing lives there.
    pub async fn drive_slabs(&self, id_or_path: &str) -> anyhow::Result<Vec<Value>> {
        let v: Value = self
            .send(self.http.get(self.drive_url(id_or_path, "/slabs")))
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(v.get("items").and_then(|i| i.as_array().cloned()).unwrap_or_default())
    }

    /// GET /api/v1/slabs — the whole pool, for the summary card.
    pub async fn list_slabs(&self) -> anyhow::Result<Vec<Value>> {
        let v: Value = self
            .send(self.http.get(self.url("/api/v1/slabs")))
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(items(v, "slabs"))
    }

    /// GET /api/v1/health — the engine's own health; its `slabs` says where
    /// the node's system and data halves run (stormblock#322/#344, #58).
    pub async fn node_health(&self) -> anyhow::Result<Value> {
        Ok(self.send(self.http.get(self.url("/api/v1/health"))).await?.error_for_status()?.json().await?)
    }

    /// GET /api/v1/arrays — the engine's RAID sets with their members'
    /// state, drive and labels (stormblock#252). None: an engine without
    /// arrays (404).
    pub async fn list_arrays(&self) -> anyhow::Result<Option<Vec<Value>>> {
        let resp = self.send(self.http.get(self.url("/api/v1/arrays"))).await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let v: Value = resp.error_for_status()?.json().await?;
        Ok(Some(items(v, "arrays")))
    }

    /// GET /api/v1/volumes?placement=true — every volume with the slabs and
    /// drives holding it (stormblock v17.1, #136) and its consumer (v18.1).
    /// The engine walks every volume's extent map for it, so it gets longer
    /// than the client's 5 s.
    pub async fn list_volumes_placed(&self) -> anyhow::Result<Vec<Value>> {
        let req = self
            .http
            .get(self.url("/api/v1/volumes?placement=true"))
            .timeout(Duration::from_secs(30));
        let v: Value = self.send(req).await?.error_for_status()?.json().await?;
        Ok(items(v, "volumes"))
    }

    /// POST /api/v1/slabs {device_path, tier} — format the drive as a slab.
    /// `role`: `data` or `system` (stormblock's default is `system`); None
    /// leaves it to the engine, as a whole-disk join always has. Destructive
    /// on the engine (stormblock#274): the admin token or a storage-admin bearer.
    pub async fn format_slab(&self, device_path: &str, tier: &str, role: Option<&str>) -> anyhow::Result<Value> {
        let mut body = serde_json::json!({ "device_path": device_path, "tier": tier });
        if let Some(r) = role {
            body["role"] = serde_json::json!(r);
        }
        let req = self.http.post(self.url("/api/v1/slabs")).json(&body);
        Ok(self
            .send_admin(req)
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
        let req = self
            .http
            .post(self.drive_url(id_or_path, "/health"))
            .json(&serde_json::json!({ "state": state, "reason": reason, "drain": drain }));
        Ok(self
            .send(req)
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// POST /api/v1/drives/{id}/drain — empty every slab on the drive.
    pub async fn start_drain(&self, id_or_path: &str) -> anyhow::Result<DrainStatus> {
        Ok(self
            .send(self.http.post(self.drive_url(id_or_path, "/drain")))
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// GET /api/v1/drives/{id}/drain — where the drain is.
    pub async fn drain_status(&self, id_or_path: &str) -> anyhow::Result<Option<DrainStatus>> {
        let resp = self
            .send(self.http.get(self.drive_url(id_or_path, "/drain")))
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(resp.error_for_status()?.json().await?))
    }

    /// DELETE /api/v1/drives/{id}/drain — stop a drain; what moved stays moved.
    /// Ordinary on the engine (detach-like): the node token.
    pub async fn cancel_drain(&self, id_or_path: &str) -> anyhow::Result<()> {
        self.send(self.http.delete(self.drive_url(id_or_path, "/drain")))
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

/// A listing's entries: a bare array, or `items` (or `alt`) of an object.
fn items(v: Value, alt: &str) -> Vec<Value> {
    match v {
        Value::Array(a) => a,
        Value::Object(mut o) => o
            .remove("items")
            .or_else(|| o.remove(alt))
            .and_then(|d| d.as_array().cloned())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn with_bearer(req: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
    match token {
        Some(t) => req.bearer_auth(t),
        None => req,
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

    #[test]
    fn token_lookup_follows_the_cli_order() {
        let t = TokenSource::resolve("", Some("env-tok".into()), "", None, DEFAULT_TOKEN_FILES);
        assert_eq!(t.explicit.as_deref(), Some("env-tok"));
        let t = TokenSource::resolve(" cfg-tok\n", Some("env-tok".into()), "", None, &[]);
        assert_eq!(t.explicit.as_deref(), Some("cfg-tok"));

        let t = TokenSource::resolve("", None, "/cfg/tok", Some("/env/tok".into()), DEFAULT_TOKEN_FILES);
        assert_eq!(t.explicit, None);
        let files: Vec<_> = t.files.iter().map(|p| p.to_str().unwrap()).collect();
        assert_eq!(
            files,
            [
                "/cfg/tok",
                "/env/tok",
                "/run/stormblock/engine/api_token",
                "/etc/stormblock/api_token",
                "/var/lib/stormblock/api_token"
            ]
        );
        // The stormcos unit names the default path: no duplicate.
        let t = TokenSource::resolve(
            "",
            None,
            "",
            Some("/run/stormblock/engine/api_token".into()),
            DEFAULT_TOKEN_FILES,
        );
        assert_eq!(t.files.len(), 3);
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("stormdrive-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn token_file_is_read_trimmed_and_skips_empty_files() {
        let d = scratch("files");
        let (a, b) = (d.join("a"), d.join("b"));
        let src = TokenSource { explicit: None, files: vec![a.clone(), b.clone()] };
        assert_eq!(src.read(), None);
        std::fs::write(&a, "\n").unwrap();
        std::fs::write(&b, "tok-b\n").unwrap();
        assert_eq!(src.read().as_deref(), Some("tok-b"));
        std::fs::write(&a, "tok-a").unwrap();
        assert_eq!(src.read().as_deref(), Some("tok-a"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A stand-in engine that answers only the bearer token in `want`.
    async fn fake_engine(want: Arc<RwLock<String>>) -> String {
        use axum::http::{HeaderMap, StatusCode};
        let app = axum::Router::new().route(
            "/api/v1/drives",
            axum::routing::get(move |h: HeaderMap| {
                let want = want.clone();
                async move {
                    let expect = format!("Bearer {}", want.read().unwrap());
                    let got = h.get("authorization").and_then(|v| v.to_str().ok());
                    if got == Some(expect.as_str()) {
                        (StatusCode::OK, axum::Json(serde_json::json!({ "items": [{ "path": "/dev/sdb" }] })))
                    } else {
                        (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({ "error": "auth" })))
                    }
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn token_absent_at_start_then_minted_then_rotated() {
        let d = scratch("engine");
        let file = d.join("api_token");
        let want = Arc::new(RwLock::new("first".to_string()));
        let url = fake_engine(want.clone()).await;
        let cfg = StormBlockConfig {
            url,
            token_file: file.to_str().unwrap().into(),
            ..Default::default()
        };
        let c = StormBlockClient::new(cfg);

        // The engine has not minted it yet: 401, and nothing cached.
        let e = c.list_drives().await.unwrap_err();
        assert!(e.to_string().contains("401"), "{e}");

        // Minted after we started: the next call finds it with no restart.
        std::fs::write(&file, "first\n").unwrap();
        assert_eq!(c.list_drives().await.unwrap().len(), 1);

        // Rotated: the cached token gets a 401, the re-read one succeeds.
        std::fs::write(&file, "second").unwrap();
        *want.write().unwrap() = "second".into();
        assert_eq!(c.list_drives().await.unwrap().len(), 1);
        assert_eq!(c.token.read().unwrap().as_deref(), Some("second"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The overcommit push (#13): what an engine with stormblock#152 gets,
    /// and how one without it answers.
    #[tokio::test]
    async fn overcommit_push_names_the_drive_and_tells_a_missing_route_apart() {
        let seen: Arc<RwLock<Option<Value>>> = Arc::default();
        let got = seen.clone();
        let app = axum::Router::new().route(
            "/api/v1/drives/{id}/overcommit",
            axum::routing::put(move |axum::extract::Path(id): axum::extract::Path<String>, axum::Json(v): axum::Json<Value>| {
                let got = got.clone();
                async move {
                    // Only sda is served, so sdb stands for an engine
                    // without the route.
                    if id != "/dev/sda" {
                        return axum::http::StatusCode::NOT_FOUND;
                    }
                    *got.write().unwrap() = Some(v);
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let c = StormBlockClient::new(StormBlockConfig { url, api_token: "t".into(), ..Default::default() });

        let oc = crate::drive::Overcommit::new(true, Some(2.0)).unwrap();
        let uuid = Uuid::nil();
        assert!(c.set_overcommit("/dev/sda", oc, uuid, Some("naa.5000"), "WD-1").await.unwrap());
        let v = seen.read().unwrap().clone().unwrap();
        assert_eq!(v["enabled"], true);
        assert_eq!(v["ratio"], 2.0);
        assert_eq!(v["drive"]["wwn"], "naa.5000");
        assert_eq!(v["drive"]["serial"], "WD-1");
        assert_eq!(v["drive"]["uuid"], uuid.to_string());

        // An engine without the route: not an error, just "not yet".
        assert!(!c.set_overcommit("/dev/sdb", oc, uuid, None, "S").await.unwrap());
    }

    /// A stand-in engine with stormblock#274's classes: slab format and drive
    /// close take the admin token `root` or the storage-admin bearer `sa`
    /// (`viewer` is a valid bearer without the role: 403), and the node token
    /// `node` only when `audit`; a drain cancel is ordinary. Records every
    /// bearer it was shown.
    async fn engine_274(audit: bool, seen: Arc<RwLock<Vec<String>>>) -> String {
        use axum::http::{HeaderMap, StatusCode};
        let bearer = |h: &HeaderMap| {
            h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").trim_start_matches("Bearer ").to_string()
        };
        let destructive = {
            let seen = seen.clone();
            move |h: HeaderMap| {
                let seen = seen.clone();
                async move {
                    let b = bearer(&h);
                    seen.write().unwrap().push(b.clone());
                    let code = match b.as_str() {
                        "root" | "sa" => StatusCode::OK,
                        "node" if audit => StatusCode::OK,
                        "viewer" => StatusCode::FORBIDDEN,
                        _ => StatusCode::UNAUTHORIZED,
                    };
                    (code, axum::Json(serde_json::json!({ "id": "slab-1" })))
                }
            }
        };
        let ordinary = {
            let seen = seen.clone();
            move |h: HeaderMap| {
                let seen = seen.clone();
                async move {
                    let b = bearer(&h);
                    seen.write().unwrap().push(b.clone());
                    if b == "node" { StatusCode::OK } else { StatusCode::UNAUTHORIZED }
                }
            }
        };
        let app = axum::Router::new()
            .route("/api/v1/slabs", axum::routing::post(destructive.clone()))
            .route("/api/v1/drives/{id}", axum::routing::delete(destructive))
            .route("/api/v1/drives/{id}/drain", axum::routing::delete(ordinary));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn destructive_verbs_present_the_admin_token_then_the_kube_bearer_then_the_node_token() {
        let d = scratch("admin274");
        let admin_file = d.join("admin_token");
        let kube_file = d.join("kube_token");
        let seen = Arc::new(RwLock::new(Vec::new()));
        let take = |seen: &Arc<RwLock<Vec<String>>>| std::mem::take(&mut *seen.write().unwrap());
        let client = |url: &str| {
            StormBlockClient::new(StormBlockConfig {
                url: url.into(),
                api_token: "node".into(),
                admin_token_file: admin_file.to_str().unwrap().into(),
                ..Default::default()
            })
            .with_kube_token_file(Some(kube_file.clone()))
        };

        // enforce, nothing but the node token: refused, and the error says so.
        let url = engine_274(false, seen.clone()).await;
        let c = client(&url);
        assert!(c.format_slab("/dev/sdb", "hdd", Some("data")).await.is_err());
        assert_eq!(take(&seen), ["node"]);

        // stormdrive's Kubernetes credential, bound to storage-admin.
        std::fs::write(&kube_file, "sa\n").unwrap();
        c.format_slab("/dev/sdb", "hdd", Some("data")).await.unwrap();
        assert_eq!(take(&seen), ["sa"]);

        // The engine's admin token, minted after we started: re-read, first.
        std::fs::write(&admin_file, "root\n").unwrap();
        c.delete_drive("/dev/sdb", false).await.unwrap();
        assert_eq!(take(&seen), ["root"]);

        // A drain cancel is ordinary: the node token, admin token or not.
        c.cancel_drain("/dev/sdb").await.unwrap();
        assert_eq!(take(&seen), ["node"]);

        // A bearer without the role (403) falls through to the node token,
        // which an audit-mode engine lets by; enforce refuses both.
        std::fs::remove_file(&admin_file).unwrap();
        std::fs::write(&kube_file, "viewer").unwrap();
        let audit = engine_274(true, seen.clone()).await;
        client(&audit).delete_drive("/dev/sdb", false).await.unwrap();
        assert_eq!(take(&seen), ["viewer", "node"]);
        assert!(c.delete_drive("/dev/sdb", false).await.is_err());
        assert_eq!(take(&seen), ["viewer", "node"]);

        // An explicit admin token wins over the file.
        let c = StormBlockClient::new(StormBlockConfig {
            url,
            api_token: "node".into(),
            admin_token: "root".into(),
            ..Default::default()
        });
        c.format_slab("/dev/sdb", "hdd", None).await.unwrap();
        assert_eq!(take(&seen), ["root"]);
    }
}
