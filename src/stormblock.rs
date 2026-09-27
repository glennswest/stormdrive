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

#[derive(Clone)]
pub struct StormBlockClient {
    cfg: StormBlockConfig,
    http: reqwest::Client,
    /// The engine's bearer token. The engine mints it at boot, possibly
    /// after we start, and may rotate it: `None` is re-read on every call,
    /// and a 401 re-reads it once.
    token: Arc<RwLock<Option<String>>>,
    /// Named explicitly (config or env) for destructive verbs; `None` =
    /// the ordinary token covers them.
    admin_token: Option<String>,
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
        let admin_token = non_empty(&cfg.admin_token).or_else(|| {
            std::env::var("STORMBLOCK_ADMIN_TOKEN").ok().as_deref().and_then(non_empty)
        });
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
            admin_token,
        }
    }

    /// Re-read the engine token and remember it; the new value.
    fn reload_token(&self) -> Option<String> {
        let t = TokenSource::from_config(&self.cfg).read();
        *self.token.write().unwrap_or_else(|e| e.into_inner()) = t.clone();
        t
    }

    /// The token for this call. An absent token is looked up again every
    /// time: the engine writes it at boot, maybe after we start.
    fn bearer(&self, admin: bool) -> Option<String> {
        if admin {
            if let Some(t) = &self.admin_token {
                return Some(t.clone());
            }
        }
        let cached = self.token.read().unwrap_or_else(|e| e.into_inner()).clone();
        cached.or_else(|| self.reload_token())
    }

    /// Every engine call goes through here (stormblock#107: all of
    /// `/api/v1` needs `Authorization: Bearer`). A 401 re-reads the token
    /// and, when it changed, retries once.
    async fn send(
        &self,
        req: reqwest::RequestBuilder,
        admin: bool,
    ) -> anyhow::Result<reqwest::Response> {
        let retry = req.try_clone();
        let used = self.bearer(admin);
        let resp = with_bearer(req, used.as_deref()).send().await?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }
        if admin && self.admin_token.is_some() {
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
            .send(self.http.get(self.url("/api/v1/drives")), false)
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
            .send(self.http.post(self.url("/api/v1/drives")).json(&body), false)
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
        self.send(req, false)
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
            .send(self.http.put(self.drive_url(path, "/overcommit")).json(&body), false)
            .await?;
        if matches!(resp.status(), reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED) {
            return Ok(false);
        }
        resp.error_for_status()?;
        Ok(true)
    }

    /// DELETE /api/v1/drives/{id} — id may be a UUID or a path.
    pub async fn delete_drive(&self, id_or_path: &str, force: bool) -> anyhow::Result<()> {
        let q = if force { "?force=true" } else { "" };
        self.send(self.http.delete(self.drive_url(id_or_path, q)), true)
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// GET /api/v1/drives/{id}/slabs — the slabs on this device, by
    /// identity (stormblock#70 item 2). Empty means nothing lives there.
    pub async fn drive_slabs(&self, id_or_path: &str) -> anyhow::Result<Vec<Value>> {
        let v: Value = self
            .send(self.http.get(self.drive_url(id_or_path, "/slabs")), false)
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(v.get("items").and_then(|i| i.as_array().cloned()).unwrap_or_default())
    }

    /// GET /api/v1/slabs — the whole pool, for the summary card.
    pub async fn list_slabs(&self) -> anyhow::Result<Vec<Value>> {
        let v: Value = self
            .send(self.http.get(self.url("/api/v1/slabs")), false)
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
        let req = self
            .http
            .post(self.url("/api/v1/slabs"))
            .json(&serde_json::json!({ "device_path": device_path, "tier": tier }));
        Ok(self
            .send(req, false)
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
            .send(req, false)
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// POST /api/v1/drives/{id}/drain — empty every slab on the drive.
    pub async fn start_drain(&self, id_or_path: &str) -> anyhow::Result<DrainStatus> {
        Ok(self
            .send(self.http.post(self.drive_url(id_or_path, "/drain")), false)
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// GET /api/v1/drives/{id}/drain — where the drain is.
    pub async fn drain_status(&self, id_or_path: &str) -> anyhow::Result<Option<DrainStatus>> {
        let resp = self
            .send(self.http.get(self.drive_url(id_or_path, "/drain")), false)
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(resp.error_for_status()?.json().await?))
    }

    /// DELETE /api/v1/drives/{id}/drain — stop a drain; what moved stays moved.
    pub async fn cancel_drain(&self, id_or_path: &str) -> anyhow::Result<()> {
        self.send(self.http.delete(self.drive_url(id_or_path, "/drain")), true)
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
fn with_bearer(req: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
    match token {
        Some(t) => req.bearer_auth(t),
        None => req,
    }
}

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

    #[test]
    fn admin_token_is_used_only_for_destructive_verbs() {
        let cfg = StormBlockConfig {
            api_token: "ops".into(),
            admin_token: "root".into(),
            ..Default::default()
        };
        let c = StormBlockClient::new(cfg);
        assert_eq!(c.bearer(false).as_deref(), Some("ops"));
        assert_eq!(c.bearer(true).as_deref(), Some("root"));
        let c = StormBlockClient::new(StormBlockConfig { api_token: "ops".into(), ..Default::default() });
        assert_eq!(c.bearer(true).as_deref(), Some("ops"));
    }
}
