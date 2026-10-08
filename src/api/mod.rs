//! REST API on :9092, the embedded UI page, and the stormd dashboard card.
//! Error envelope matches stormblock's `{error, code}` shape.

use crate::config::Config;
use crate::drive::{Activity, Designation, DriveId, HealthStatus, Membership};
use crate::drivetest::{TestHandle, TestKind, TestState};
use crate::firmware::FwHandle;
use crate::format::FormatHandle;
use crate::events::{EventLog, Severity};
use crate::inventory::Inventory;
use crate::stormblock::StormBlockClient;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

pub mod kube;

// The page (#6): a Svelte + stormview build, committed as web/dist so a
// cargo-only build needs no node. Rebuilt on dev (README, "The page").
const INDEX_HTML: &str = include_str!("../../web/dist/index.html");
const APP_JS: &str = include_str!("../../web/dist/assets/app.js");
const APP_CSS: &str = include_str!("../../web/dist/assets/app.css");

pub struct AppState {
    pub config: Config,
    pub inventory: RwLock<Inventory>,
    pub events: RwLock<EventLog>,
    pub stormblock: StormBlockClient,
    pub tests: RwLock<HashMap<DriveId, Arc<TestHandle>>>,
    /// Sector-size reformats, one per drive, kept after they finish.
    pub formats: RwLock<HashMap<DriveId, Arc<FormatHandle>>>,
    /// Firmware updates, one per drive, kept after they finish.
    pub firmware: RwLock<HashMap<DriveId, Arc<FwHandle>>>,
    /// Fleet drives update firmware one at a time.
    pub fleet_firmware_lock: tokio::sync::Mutex<()>,
    /// Shelf (IOM) firmware runs by shelf key (#35).
    pub shelf_firmware: RwLock<crate::iomfw::ShelfRuns>,
    /// Latest SES scan: every shelf the node can talk to, by logical id.
    pub shelves: RwLock<crate::topology::Shelves>,
    /// Latest HBA scan: every PCIe SCSI controller, by PCIe address.
    pub hbas: RwLock<crate::hba::Hbas>,
    pub inventory_path: Option<PathBuf>,
    /// `<data_dir>/events.json` (#25), and the latest seq written there.
    pub events_path: Option<PathBuf>,
    pub events_persisted: tokio::sync::Mutex<u64>,
    pub node_name: String,
    /// Health reads: bounded, timed out, costed (#15).
    pub poller: crate::poller::Sampler,
    /// Hash of the inventory last written, so an unchanged one is not
    /// rewritten.
    /// Held across serialise + write, so two persists never land out of
    /// order.
    pub persisted: tokio::sync::Mutex<Option<u64>>,
    /// The drive worker's jobs and lanes (#5).
    pub worker: crate::worker::Worker,
    /// Who may write (#45): bearer reviews, the admin token, audit.
    pub gate: crate::kubeauth::Gate,
    /// The engine's own report of where the node's slabs run, as last read
    /// (#58); None until it answers with one.
    pub engine_slabs: RwLock<Option<crate::engine::EngineSlabs>>,
    /// Drive history and hardware assets in system-data (#64).
    pub history: Arc<crate::history::History>,
    /// This boot's hardware assets record, once taken (#64).
    pub assets: RwLock<Option<crate::assets::Assets>>,
}

impl AppState {
    /// Write the event log's tail when anything was added since the last
    /// write (#25). Runs with every inventory persist.
    pub async fn persist_events(&self) {
        let Some(path) = &self.events_path else { return };
        let mut written = self.events_persisted.lock().await;
        let (seq, bytes) = {
            let log = self.events.read().await;
            if log.latest_seq() == *written {
                return;
            }
            (log.latest_seq(), log.snapshot(crate::events::PERSIST_TAIL))
        };
        let path = path.clone();
        match tokio::task::spawn_blocking(move || crate::inventory::write_atomic(&path, &bytes)).await {
            Ok(Ok(())) => *written = seq,
            Ok(Err(e)) => tracing::error!("events persist failed: {e:#}"),
            Err(e) => tracing::error!("events persist failed: {e}"),
        }
    }

    pub async fn persist(&self) {
        self.persist_events().await;
        let Some(path) = &self.inventory_path else {
            return;
        };
        // Serialise under the lock, write outside it: at 160 drives the
        // write is the slow half, and every poll result waits on the lock.
        let mut last = self.persisted.lock().await;
        let bytes = match self.inventory.read().await.to_bytes() {
            Ok(b) => b,
            Err(e) => return tracing::error!("inventory persist failed: {e:#}"),
        };
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            bytes.hash(&mut h);
            h.finish()
        };
        if *last == Some(hash) {
            return;
        }
        let path = path.clone();
        match tokio::task::spawn_blocking(move || crate::inventory::write_atomic(&path, &bytes)).await {
            Ok(Ok(())) => *last = Some(hash),
            Ok(Err(e)) => tracing::error!("inventory persist failed: {e:#}"),
            Err(e) => tracing::error!("inventory persist failed: {e}"),
        }
    }
}

struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: msg.into(),
        }
    }
    fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "bad_request",
            message: msg.into(),
        }
    }
    fn conflict(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "conflict",
            message: msg.into(),
        }
    }
    fn upstream(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            code: "stormblock",
            message: msg.into(),
        }
    }
    fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal",
            message: msg.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({ "error": self.message, "code": self.code })),
        )
            .into_response()
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // The page computes its own API base (mkube's proxy-prefix pattern),
        // so it works served from any of these and through stormd's
        // /ui/proxy/stormdrive/ — no redirects, which a proxied iframe
        // could not follow.
        .route("/", get(ui_index))
        .route("/ui", get(ui_index))
        .route("/ui/", get(ui_index))
        // Asset URLs are relative, so "/" and "/ui" load /assets/… and
        // "/ui/" loads /ui/assets/…; through stormd's proxy both arrive as
        // /assets/….
        .route("/assets/{file}", get(ui_asset))
        .route("/ui/assets/{file}", get(ui_asset))
        .route("/api/v1/health", get(health))
        // The conventional probe path (#19): open on plain HTTP as health.
        .route("/healthz", get(health))
        .route("/api/v1/components", get(components_feed))
        .route("/ws/components", get(ws_components))
        .route("/api/v1/monitor", get(monitor_stats))
        .route("/metrics", get(metrics))
        .route("/api/v1/drives", get(list_drives))
        .route("/api/v1/drives/{id}", get(get_drive).delete(forget_drive))
        .route("/api/v1/drives/{id}/health", get(get_drive_health))
        .route("/api/v1/drives/{id}/slabs", get(get_drive_slabs))
        .route("/api/v1/drives/{id}/history", get(get_drive_history))
        // app-system-data (#64): drive history status, this boot's assets.
        .route("/api/v1/history", get(history_status))
        .route("/api/v1/assets", get(get_assets))
        .route("/api/v1/drives/{id}/locate", post(set_locate))
        // Parameter-less action routes: a stormview renderer invokes
        // method+path with no body, so every action needs a body-free form.
        .route("/api/v1/drives/{id}/locate/{state}", post(locate_by_path))
        .route("/api/v1/drives/{id}/fleet", post(fleet_action))
        .route("/api/v1/drives/{id}/drain", get(get_drain).post(start_drain).delete(cancel_drain))
        .route("/api/v1/drives/{id}/fleet/{action}", post(fleet_by_path))
        .route("/api/v1/drives/{id}/designation", post(set_designation))
        .route("/api/v1/drives/{id}/designation/{value}", post(designation_by_path))
        .route("/api/v1/drives/{id}/overcommit", get(get_overcommit).put(set_overcommit).post(set_overcommit))
        .route("/api/v1/drives/{id}/overcommit/{value}", post(overcommit_by_path))
        .route("/api/v1/drives/{id}/test", get(get_test).post(start_test))
        .route("/api/v1/drives/{id}/test/cancel", post(cancel_test))
        .route("/api/v1/drives/{id}/test/{kind}", post(test_by_path))
        // Sector-size reformat (FORMAT UNIT): one drive, many drives, or a
        // whole shelf's worth of 520-byte drives.
        .route("/api/v1/drives/{id}/enroll", post(enroll_drive))
        .route("/api/v1/drives/{id}/format", get(get_format).post(format_drive))
        .route("/api/v1/drives/{id}/format/{block_size}", post(format_drive_by_path))
        .route("/api/v1/format", get(list_formats).post(format_many))
        // Firmware: image store + updates (one, many, or by model).
        .route("/api/v1/firmware", get(list_firmware).post(firmware_many))
        .route("/api/v1/firmware/images", get(list_images))
        .route(
            "/api/v1/firmware/images/{name}",
            axum::routing::put(put_image).delete(delete_image).get(get_image),
        )
        .route("/api/v1/drives/{id}/firmware", get(get_firmware).post(firmware_drive))
        .route("/api/v1/shelves", get(list_shelves))
        .route("/api/v1/shelves/{key}", get(get_shelf))
        .route("/api/v1/shelves/{key}/locate", post(shelf_locate))
        .route("/api/v1/shelves/{key}/locate/{state}", post(shelf_locate_by_path))
        .route("/api/v1/shelves/{key}/format", post(format_shelf))
        .route("/api/v1/shelves/{key}/firmware", get(get_shelf_firmware).post(shelf_firmware))
        .route("/api/v1/shelves/{key}/format/{block_size}", post(format_shelf_by_path))
        .route("/api/v1/topology", get(topology))
        .route("/api/v1/hbas", get(list_hbas))
        .route("/api/v1/placement", get(placement))
        .route("/api/v1/placement/{id}", get(placement_one))
        // The drive worker (#5): prepare drives at fleet scale.
        .route("/api/v1/worker/jobs", get(list_jobs).post(create_job))
        .route("/api/v1/worker/jobs/{id}", get(get_job))
        .route("/api/v1/worker/jobs/{id}/cancel", post(cancel_job))
        .route("/api/v1/worker/jobs/{id}/resume", post(resume_job))
        .route("/api/v1/events", get(list_events))
        .route("/api/v1/summary", get(summary))
        // Kubernetes-shaped resources, served by this daemon (stormblock#80).
        .merge(kube::router())
        .layer(axum::middleware::from_fn_with_state(state.clone(), guard))
        .layer(axum::extract::DefaultBodyLimit::max(
            state.config.firmware.max_image_mib as usize * 1024 * 1024 + 4096,
        ))
        .with_state(state)
}

/// Every request but health passes the gate (#19, #45, `kubeauth`):
/// - plain HTTP answers health only (unless `allow_anonymous`);
/// - a read needs the admin token, a node-CA client certificate, or a
///   bearer allowed `get` on `storage.storm.io`; the page's shell, asked
///   with no credential, is answered 401 with the page (it signs in);
/// - a write needs a storage-admin bearer or client certificate, or the
///   admin token. The requester rides into the handler as an extension (a
///   worker job keeps it for the re-check). A worker job that is only a dry
///   run is gated as a read.
async fn guard(
    State(s): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    use crate::kubeauth::{audit_line, classify, is_health, is_page_asset, is_page_shell, read_access, Credential, Decision};
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    if is_health(&path) {
        return next.run(req).await;
    }
    // Set by the listener (`tls::Peer`); absent = treated as plain HTTP.
    let peer = req.extensions().get::<axum::extract::ConnectInfo<crate::tls::Peer>>().map(|c| c.0.clone());
    let (tls, cert) = peer.map(|p| (p.tls, p.client)).unwrap_or((false, None));
    if !tls && !s.gate.anonymous() {
        return ApiError {
            status: StatusCode::FORBIDDEN,
            code: "tls_required",
            message: "plain HTTP answers /api/v1/health only: use https://, verified against the node CA".into(),
        }
        .into_response();
    }
    if is_page_asset(&path) {
        return next.run(req).await;
    }
    let bearer = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer ").or_else(|| h.strip_prefix("bearer ")))
        .map(str::to_string);
    let cred = Credential { bearer: bearer.as_deref(), cert: cert.as_ref() };
    let mut req = req;
    let write = match classify(&method, &path) {
        Some(access) if method == "POST" && path.trim_end_matches('/') == "/api/v1/worker/jobs" => {
            let (parts, body) = req.into_parts();
            let bytes = match axum::body::to_bytes(body, 4 << 20).await {
                Ok(b) => b,
                Err(e) => return ApiError::bad_request(format!("body: {e}")).into_response(),
            };
            let dry = serde_json::from_slice::<serde_json::Value>(&bytes).ok().is_some_and(|v| v["dry_run"] == json!(true) || v["dryRun"] == json!(true));
            req = axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes));
            (!dry).then_some(access)
        }
        other => other,
    };
    let Some(access) = write else {
        let access = read_access(&path);
        return match s.gate.check_read(cred, &access).await {
            Decision::Allowed(r) | Decision::AuditOnly(r, _) => {
                req.extensions_mut().insert(r);
                next.run(req).await
            }
            Decision::Refused(401, _, _) if is_page_shell(&path) && cred.bearer.is_none() => {
                (StatusCode::UNAUTHORIZED, Html(INDEX_HTML)).into_response()
            }
            Decision::Refused(code, _, why) => refused(code, why),
        };
    };
    if method == "POST" && path.trim_end_matches('/') == "/api/v1/worker/jobs" {
        let (parts, body) = req.into_parts();
        let bytes = match axum::body::to_bytes(body, 4 << 20).await {
            Ok(b) => b,
            Err(e) => return ApiError::bad_request(format!("body: {e}")).into_response(),
        };
        let dry = serde_json::from_slice::<serde_json::Value>(&bytes).ok().is_some_and(|v| v["dry_run"] == json!(true) || v["dryRun"] == json!(true));
        req = axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes));
        if dry {
            return next.run(req).await;
        }
    }
    let decision = s.gate.check(cred, &access).await;
    let (requester, verdict, reason) = match decision {
        Decision::Allowed(r) => (r, "allowed", String::new()),
        Decision::AuditOnly(r, why) => (r, "allowed-audit-only", why),
        Decision::Refused(code, who, why) => {
            audit(&s, &audit_line(&method, &path, &access, &who, "refused", &why, Some(code)), &access).await;
            return refused(code, why);
        }
    };
    let who = requester.who.clone();
    req.extensions_mut().insert(requester);
    let resp = next.run(req).await;
    audit(&s, &audit_line(&method, &path, &access, &who, verdict, &reason, Some(resp.status().as_u16())), &access).await;
    resp
}

/// The gate's refusal as the API's error envelope.
fn refused(code: u16, why: String) -> Response {
    let status = StatusCode::from_u16(code).unwrap_or(StatusCode::UNAUTHORIZED);
    let code = match code {
        403 => "forbidden",
        503 => "unavailable",
        _ => "unauthorized",
    };
    ApiError { status, code, message: why }.into_response()
}

/// One audit record: log + audit.log + the event ring (kind `audit`; a
/// drive operation or a refusal is a warning).
async fn audit(s: &AppState, line: &serde_json::Value, access: &crate::kubeauth::Access) {
    s.gate.audit(line);
    let decision = line["decision"].as_str().unwrap_or("");
    let drive = match &access.name {
        Some(n) if access.resource == "drives" => s.inventory.read().await.resolve(n).map(|d| d.id),
        _ => None,
    };
    let sev = if decision == "refused" || access.destructive() { Severity::Warning } else { Severity::Info };
    let msg = format!(
        "{} {} {} by {}: {}{} ({})",
        line["method"].as_str().unwrap_or(""),
        line["path"].as_str().unwrap_or(""),
        format_args!("[{} {}]", access.verb, access.resource),
        line["who"].as_str().unwrap_or("?"),
        decision,
        line["reason"].as_str().filter(|r| !r.is_empty()).map(|r| format!(" — {r}")).unwrap_or_default(),
        line["status"].as_u64().map(|c| c.to_string()).unwrap_or_default(),
    );
    s.events.write().await.push(drive, sev, "audit", msg);
}

#[derive(Deserialize, Default)]
struct PlacementQuery {
    since: Option<u64>,
}

/// Where every drive and shelf is (#10), for mirrors. `?since=G` or
/// `If-None-Match: "G"` answers 304 while the generation is still G.
async fn placement(
    State(s): State<Arc<AppState>>,
    Query(q): Query<PlacementQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    let v = {
        let inv = s.inventory.read().await;
        let shelves = s.shelves.read().await;
        crate::placement::view(inv.drives.values(), &shelves, &s.node_name)
    };
    let generation = v["generation"].as_u64().unwrap_or_default();
    let etag = format!("\"{generation}\"");
    let matched = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| h.split(',').any(|t| t.trim().trim_start_matches("W/") == etag || t.trim() == "*"));
    let hdr = [(axum::http::header::ETAG, etag)];
    if matched || q.since == Some(generation) {
        return (StatusCode::NOT_MODIFIED, hdr).into_response();
    }
    (hdr, Json(v)).into_response()
}

/// One drive's placement, by wwn, uuid, /dev path or name, or serial.
async fn placement_one(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let inv = s.inventory.read().await;
    let d = inv
        .resolve(&id)
        .ok_or_else(|| ApiError::not_found(format!("drive {id:?}")))?;
    let mut v = crate::placement::drive_record(d);
    v["node"] = json!(s.node_name);
    Ok(Json(v))
}

async fn ui_index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

fn asset(file: &str) -> Option<(&'static str, &'static str)> {
    match file {
        "app.js" => Some(("text/javascript; charset=utf-8", APP_JS)),
        "app.css" => Some(("text/css; charset=utf-8", APP_CSS)),
        _ => None,
    }
}

async fn ui_asset(Path(file): Path<String>) -> Response {
    match asset(&file) {
        Some((ty, body)) => ([(axum::http::header::CONTENT_TYPE, ty)], body).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed build is whole: the page names the two assets, relative
    /// (so it works under a proxy prefix), and both are served.
    #[test]
    fn page_and_assets_are_embedded() {
        assert!(INDEX_HTML.contains("src=\"./assets/app.js\""));
        assert!(INDEX_HTML.contains("href=\"./assets/app.css\""));
        assert!(asset("app.js").is_some_and(|(t, b)| t.starts_with("text/javascript") && !b.is_empty()));
        assert!(asset("app.css").is_some_and(|(t, b)| t.starts_with("text/css") && !b.is_empty()));
        assert!(asset("../Cargo.toml").is_none());
    }
}

async fn health(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "version": crate::VERSION,
        "node": s.node_name,
        "writes": s.gate.describe(),
        // #19: whether reads with no credential (and plain HTTP) are served.
        "reads": { "anonymous": s.gate.anonymous() },
    }))
}

/// Resolve an API handle to a DriveId under a read lock.
async fn resolve_id(s: &AppState, handle: &str) -> Result<DriveId, ApiError> {
    let inv = s.inventory.read().await;
    inv.resolve(handle)
        .map(|d| d.id)
        .ok_or_else(|| ApiError::not_found(format!("drive {handle:?}")))
}

/// What health polling costs on this node, and which drives are stuck.
/// Prometheus text (#18): drives, shelves, the poller. A read like any
/// other (#19): a scraper presents a node-CA client certificate or a
/// bearer. Built from cached state only.
async fn metrics(State(s): State<Arc<AppState>>) -> impl IntoResponse {
    let page = {
        let inv = s.inventory.read().await;
        let shelves = s.shelves.read().await;
        crate::metrics::render(crate::VERSION, &s.node_name, inv.drives.values(), &shelves, &s.poller.stats())
    };
    ([(axum::http::header::CONTENT_TYPE, crate::metrics::CONTENT_TYPE)], page)
}

async fn monitor_stats(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::to_value(s.poller.stats()).unwrap_or_default())
}

async fn list_drives(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let inv = s.inventory.read().await;
    let tests = s.tests.read().await;
    let formats = s.formats.read().await;
    let fws = s.firmware.read().await;
    let mut drives: Vec<serde_json::Value> = Vec::new();
    let mut sorted: Vec<_> = inv.drives.values().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for d in sorted {
        let mut v = serde_json::to_value(d).unwrap_or_default();
        if let Some(h) = tests.get(&d.id) {
            let run = h.run.lock().unwrap();
            v["test"] = serde_json::to_value(&*run).unwrap_or_default();
        }
        if let Some(h) = formats.get(&d.id) {
            let run = h.run.lock().unwrap();
            v["format_run"] = serde_json::to_value(&*run).unwrap_or_default();
        }
        if let Some(h) = fws.get(&d.id) {
            let run = h.run.lock().unwrap();
            v["firmware_run"] = serde_json::to_value(&*run).unwrap_or_default();
        }
        v["needs_reformat"] = json!(d.needs_reformat());
        v["owner"] = json!(d.owner());
        v["prep"] = prep_of(&s, d);
        drives.push(v);
    }
    Json(json!({ "drives": drives }))
}

async fn get_drive(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let inv = s.inventory.read().await;
    let d = inv
        .resolve(&id)
        .ok_or_else(|| ApiError::not_found(format!("drive {id:?}")))?;
    let mut v = serde_json::to_value(d).map_err(|e| ApiError::internal(e.to_string()))?;
    v["owner"] = json!(d.owner());
    v["prep"] = prep_of(&s, d);
    Ok(Json(v))
}

/// Where a drive is on its way into the fleet (#5), with the progress of
/// whatever low-level step is running on it.
pub(crate) fn prep_of(s: &AppState, d: &crate::drive::Drive) -> serde_json::Value {
    let pct = s.worker.progress_of(d.id).and_then(|(_, p)| p).or_else(|| {
        s.formats.try_read().ok().and_then(|f| f.get(&d.id).and_then(|h| h.run.lock().unwrap().progress_pct))
    });
    crate::worker::prep(d, pct)
}

async fn create_job(
    State(s): State<Arc<AppState>>,
    who: Option<axum::Extension<crate::kubeauth::Requester>>,
    Json(req): Json<crate::worker::Request>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let dry = req.dry_run;
    match crate::worker::submit(&s, req, who.map(|w| w.0)).await {
        Ok(v) => Ok(Json(v)),
        Err(e) if e.starts_with("nothing to run") => Err(ApiError::conflict(e)),
        Err(e) if e.contains("not found") => Err(ApiError::not_found(e)),
        Err(e) => Err(ApiError::bad_request(if dry { format!("dry run: {e}") } else { e })),
    }
}

#[derive(Debug, serde::Deserialize)]
struct EnrollQuery {
    tier: Option<String>,
}

/// One action for an offered drive (#42): a worker job that partitions it
/// and enrolls it as a data slab, of `?tier=` or the tier its kind gets.
/// Refused unless the drive is offered (blank, healthy, out of the fleet,
/// undesignated, usable sectors); the worker's guards and the requester's
/// re-check apply as to any job.
async fn enroll_drive(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<EnrollQuery>,
    who: Option<axum::Extension<crate::kubeauth::Requester>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let d = s.inventory.read().await.resolve(&id).cloned().ok_or_else(|| ApiError::not_found(format!("drive {id:?}")))?;
    if let Some(why) = d.offer_blocker(0) {
        return Err(ApiError::conflict(format!("{}: not enrolable — {why}", d.name)));
    }
    let tier = q.tier.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    if let Some(t) = &tier {
        if !crate::policy::TIERS.contains(&t.as_str()) {
            return Err(ApiError::bad_request(format!("tier {t:?}: one of {}", crate::policy::TIERS.join(", "))));
        }
    }
    let role = crate::worker::Role::Data;
    let req = crate::worker::Request {
        select: crate::worker::Select { drives: vec![d.id.0.to_string()], ..Default::default() },
        steps: vec![crate::worker::Step::Partition { role }, crate::worker::Step::Enroll { tier, role }],
        destroy: vec![],
        dry_run: false,
    };
    match crate::worker::submit(&s, req, who.map(|w| w.0)).await {
        Ok(v) => Ok(Json(v)),
        Err(e) if e.starts_with("nothing to run") => Err(ApiError::conflict(e)),
        Err(e) => Err(ApiError::bad_request(e)),
    }
}

async fn list_jobs(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({ "jobs": s.worker.list() }))
}

async fn get_job(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Result<Json<serde_json::Value>, ApiError> {
    s.worker.get(&id).map(Json).ok_or_else(|| ApiError::not_found(format!("job {id:?}")))
}

async fn cancel_job(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Result<Json<serde_json::Value>, ApiError> {
    crate::worker::cancel(&s, &id).await.map(Json).map_err(ApiError::not_found)
}

async fn resume_job(
    State(s): State<Arc<AppState>>,
    who: Option<axum::Extension<crate::kubeauth::Requester>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::worker::resume(&s, &id, who.map(|w| w.0)).await.map(Json).map_err(|e| {
        if e.contains("not found") {
            ApiError::not_found(e)
        } else {
            ApiError::conflict(e)
        }
    })
}

/// Forget a drive that is gone (#15): at 160 bays, pulled drives would
/// otherwise pile up in the inventory forever. Only a missing drive, and
/// not one stormblock still holds — leave the fleet (or drain) first.
async fn forget_drive(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let did = resolve_id(&s, &id).await?;
    let (name, serial) = {
        let mut inv = s.inventory.write().await;
        let d = inv.drives.get(&did).expect("resolved id present");
        if d.activity != Activity::Missing {
            return Err(ApiError::conflict(format!("{} is present; only a missing drive can be forgotten", d.name)));
        }
        if d.membership == Membership::Fleet {
            return Err(ApiError::conflict(format!("{} is still in the fleet; leave the fleet first", d.name)));
        }
        let out = (d.name.clone(), d.serial.clone());
        inv.drives.remove(&did);
        inv.trends.remove(&did);
        out
    };
    s.events.write().await.push(Some(did), Severity::Info, "forgotten", format!("{name} ({serial}): forgotten (operator)"));
    s.persist().await;
    Ok(Json(json!({ "id": did, "forgotten": true })))
}

async fn get_drive_health(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let inv = s.inventory.read().await;
    let d = inv
        .resolve(&id)
        .ok_or_else(|| ApiError::not_found(format!("drive {id:?}")))?;
    let trend = inv.trends.get(&d.id).cloned().unwrap_or_default();
    Ok(Json(json!({ "health": d.health, "trend": trend })))
}

/// The slabs on a drive three ways (#58): what is on the disk (partitions,
/// role, offset), what the engine's slab listing puts on it (usage), and
/// the engine's own report of where the node's halves run, with the
/// finding when those disagree.
async fn get_drive_slabs(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let inv = s.inventory.read().await;
    let d = inv
        .resolve(&id)
        .ok_or_else(|| ApiError::not_found(format!("drive {id:?}")))?;
    let engine = s.engine_slabs.read().await.clone();
    Ok(Json(json!({
        "drive": d.id,
        "name": d.name,
        "on_disk": d.slab_parts,
        "in_engine": d.usage.as_ref().map(|u| &u.slabs),
        "engine": engine,
        "finding": d.engine_finding,
    })))
}

#[derive(Deserialize)]
struct HistoryQuery {
    limit: Option<usize>,
}

/// `GET /api/v1/drives/{id}/history?limit=` — the drive's records from
/// system-data (#64), oldest first, the newest `limit` (default 100, at
/// most 10000). Across installs: the file is the drive's, not this run's.
async fn get_drive_history(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let d = {
        let inv = s.inventory.read().await;
        inv.resolve(&id).cloned().ok_or_else(|| ApiError::not_found(format!("drive {id:?}")))?
    };
    let limit = q.limit.unwrap_or(100).clamp(1, 10_000);
    let h = s.history.clone();
    let d2 = d.clone();
    let (active, records) = tokio::task::spawn_blocking(move || (h.available(), h.read(&d2, limit)))
        .await
        .map_err(|e| ApiError::internal(format!("history read: {e}")))?;
    Ok(Json(json!({
        "drive": d.id,
        "name": d.name,
        "key": crate::history::key(&d),
        "active": active,
        "records": records,
    })))
}

/// `GET /api/v1/history` — where drive history and assets go, and whether
/// they are being written (#64).
async fn history_status(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let h = s.history.clone();
    let st = tokio::task::spawn_blocking(move || {
        h.available();
        h.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    })
    .await
    .unwrap_or_default();
    Json(json!(st))
}

/// `GET /api/v1/assets` — this boot's hardware record (#64): every item,
/// and what changed since the previous boot. 404 until it is taken (or
/// when system-data is not mounted).
async fn get_assets(State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiError> {
    match s.assets.read().await.clone() {
        Some(a) => Ok(Json(json!(a))),
        None => Err(ApiError::not_found("no assets record this boot (see /api/v1/history)".to_string())),
    }
}

#[derive(Deserialize)]
struct LocateBody {
    on: bool,
}

async fn set_locate(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<LocateBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (name, loc) = {
        let inv = s.inventory.read().await;
        let d = inv
            .resolve(&id)
            .ok_or_else(|| ApiError::not_found(format!("drive {id:?}")))?;
        (d.name.clone(), d.location.clone())
    };
    let shelves = s.shelves.read().await.clone();
    let on = body.on;
    let n2 = name.clone();
    tokio::task::spawn_blocking(move || crate::topology::set_locate(&n2, &loc, &shelves, on))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(|e| ApiError::bad_request(format!("{name}: {e}")))?;
    Ok(Json(json!({ "name": name, "locate": on })))
}

#[derive(Deserialize)]
struct FleetBody {
    action: String,
    /// join only: also format a slab on the drive (DESTRUCTIVE — explicit
    /// opt-in, the UI asks for confirmation).
    #[serde(default)]
    format_slab: bool,
    /// join only: override the tier derived from the drive kind.
    #[serde(default)]
    tier: Option<String>,
    /// leave only: move everything off first, and leave once empty.
    #[serde(default)]
    drain: bool,
    /// leave only: skip the slab-in-use guard.
    #[serde(default)]
    force: bool,
}

async fn fleet_action(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<FleetBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let did = resolve_id(&s, &id).await?;
    let drive = {
        let inv = s.inventory.read().await;
        inv.drives.get(&did).cloned().expect("resolved id present")
    };
    if !s.stormblock.enabled() {
        return Err(ApiError::bad_request("stormblock integration is disabled"));
    }
    match body.action.as_str() {
        "join" => {
            if let Some(why) = drive.fleet_join_blocker() {
                return Err(ApiError::conflict(format!("{}: {why}", drive.name)));
            }
            if let Some(who) = crate::contents::probe(&drive.path) {
                return Err(ApiError::conflict(format!("{}: holds data for {who}", drive.name)));
            }
            let labels = drive.stormblock_labels();
            let slab_tier = crate::fleet::join(
                &s,
                did,
                &drive.name,
                &drive.path,
                &labels,
                drive.kind,
                body.format_slab,
                body.tier.clone(),
            )
            .await
            .map_err(|e| ApiError::upstream(format!("join: {e:#}")))?;
            s.events.write().await.push(
                Some(did),
                Severity::Info,
                "fleet",
                match &slab_tier {
                    Some(t) => format!("{}: joined the fleet, slab formatted ({t}), labels {labels:?}", drive.name),
                    None => format!("{}: joined the fleet (no slab formatted), labels {labels:?}", drive.name),
                },
            );
            s.persist().await;
            Ok(Json(json!({ "membership": "fleet", "slab_tier": slab_tier, "labels": labels })))
        }
        "leave" => {
            if drive.membership != Membership::Fleet {
                return Err(ApiError::conflict(format!("{}: not in the fleet", drive.name)));
            }
            if !body.force {
                // A slab with anything on it means data lives here. Asked to
                // drain, the drive leaves by itself once it is empty; not
                // asked, this refuses rather than strand the data.
                let slabs = s
                    .stormblock
                    .drive_slabs(&drive.stormblock_path())
                    .await
                    .map_err(|e| ApiError::upstream(format!("stormblock unreachable: {e:#}")))?;
                let occupied = slabs.iter().any(|sl| {
                    let total = sl.get("total_slots").and_then(|v| v.as_u64()).unwrap_or(0);
                    let free = sl.get("free_slots").and_then(|v| v.as_u64()).unwrap_or(0);
                    total > free
                });
                if occupied {
                    if body.drain {
                        let rec = crate::fleet::start_drain(&s, did, "leave", true)
                            .await
                            .map_err(|e| ApiError::upstream(format!("drain: {e:#}")))?;
                        return Ok(Json(json!({
                            "membership": "fleet",
                            "draining": true,
                            "drain": rec,
                            "note": "the drive leaves the fleet on its own once the drain reports empty",
                        })));
                    }
                    return Err(ApiError::conflict(format!(
                        "{}: data lives on this drive — leave with \"drain\": true to move it off first, or pass force",
                        drive.name
                    )));
                }
            }
            s.stormblock
                .delete_drive(&drive.stormblock_path(), body.force)
                .await
                .map_err(|e| ApiError::upstream(format!("remove drive: {e:#}")))?;
            {
                let mut inv = s.inventory.write().await;
                if let Some(d) = inv.drives.get_mut(&did) {
                    d.membership = Membership::Out;
                    d.fleet_partition = None;
                }
            }
            s.events.write().await.push(
                Some(did),
                Severity::Info,
                "fleet",
                format!("{}: left the fleet{}", drive.name, if body.force { " (forced)" } else { "" }),
            );
            s.persist().await;
            Ok(Json(json!({ "membership": "out" })))
        }
        other => Err(ApiError::bad_request(format!(
            "action {other:?}: use join or leave"
        ))),
    }
}

/// `POST /api/v1/drives/{id}/drain` — move everything off a fleet drive.
/// `?leave=true` retires it once empty (out of the fleet, locate LED on).
async fn start_drain(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let did = resolve_id(&s, &id).await?;
    if !s.stormblock.enabled() {
        return Err(ApiError::bad_request("stormblock integration is disabled"));
    }
    let leave = q.get("leave").is_some_and(|v| v == "true" || v == "1");
    let rec = crate::fleet::start_drain(&s, did, "operator", leave)
        .await
        .map_err(|e| ApiError::upstream(format!("drain: {e:#}")))?;
    Ok(Json(json!({ "id": did, "drain": rec, "then_leave": leave })))
}

/// `GET /api/v1/drives/{id}/drain` — what we know about the drain.
async fn get_drain(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let did = resolve_id(&s, &id).await?;
    let inv = s.inventory.read().await;
    let d = inv.drives.get(&did).expect("resolved id present");
    match &d.drain {
        Some(rec) => Ok(Json(json!({ "id": did, "activity": d.activity, "drain": rec }))),
        None => Err(ApiError::not_found(format!("{}: no drain has been asked for", d.name))),
    }
}

/// `DELETE /api/v1/drives/{id}/drain` — stop it; what moved stays moved.
async fn cancel_drain(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let did = resolve_id(&s, &id).await?;
    crate::fleet::cancel_drain(&s, did)
        .await
        .map_err(|e| ApiError::upstream(format!("cancel drain: {e:#}")))?;
    Ok(Json(json!({ "id": did, "cancelled": true })))
}

#[derive(Deserialize)]
struct DesignationBody {
    designation: Designation,
}

async fn set_designation(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<DesignationBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let did = resolve_id(&s, &id).await?;
    let (name, from, membership) = {
        let mut inv = s.inventory.write().await;
        let d = inv.drives.get_mut(&did).expect("resolved id present");
        let from = d.designation;
        d.designation = body.designation;
        (d.name.clone(), from, d.membership)
    };
    let mut log = s.events.write().await;
    log.push(
        Some(did),
        Severity::Info,
        "designation",
        format!("{name}: {from:?} → {:?} (operator)", body.designation),
    );
    drop(log);
    let mut drain = None;
    if body.designation == Designation::Failed && membership == Membership::Fleet && s.stormblock.enabled() {
        // Operator says failed: the engine stops trusting it now, and it is
        // drained and retired without waiting for a health poll to agree.
        let path = s.inventory.read().await.drives.get(&did).map(|d| d.stormblock_path()).unwrap_or_default();
        if let Err(e) = s.stormblock.report_health(&path, "failed", Some("operator designation"), false).await {
            tracing::warn!(drive = %name, "failed designation not reported to stormblock: {e:#}");
        } else if let Some(d) = s.inventory.write().await.drives.get_mut(&did) {
            d.pushed_health = Some("failed".into());
        }
        if s.config.stormblock.drain_on_failing {
            match crate::fleet::request_drain(&s, did, "operator", true).await {
                Ok(rec) => drain = Some(rec),
                Err(e) => {
                    s.events.write().await.push(
                        Some(did),
                        Severity::Error,
                        "drain",
                        format!("{name}: marked failed; the drain has not started yet (pending, retried each tick): {e:#}"),
                    );
                }
            }
        }
    }
    s.persist().await;
    Ok(Json(json!({ "id": did, "from": from, "to": body.designation, "drain": drain })))
}

#[derive(Deserialize)]
struct OvercommitBody {
    enabled: bool,
    #[serde(default)]
    ratio: Option<f64>,
}

async fn get_overcommit(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let did = resolve_id(&s, &id).await?;
    let inv = s.inventory.read().await;
    let d = inv.drives.get(&did).expect("resolved id present");
    Ok(Json(overcommit_json(d)))
}

fn overcommit_json(d: &crate::drive::Drive) -> serde_json::Value {
    let u = d.usage.as_ref();
    json!({
        "id": d.id, "overcommit": d.overcommit,
        // stormblock enforces it when a claim binds (stormblock#152); this
        // says whether the engine has accepted the current setting.
        "pushed": d.pushed_overcommit == Some(d.overcommit),
        "promisable_bytes": u.map(|u| u.promisable_bytes),
        "committed_bytes": u.and_then(|u| u.committed_bytes),
        "written_bytes": u.map(|u| u.used_bytes),
        "headroom_bytes": u.and_then(|u| u.headroom_bytes),
    })
}

/// Set a drive's overcommit (#13): `{"enabled": false}` or
/// `{"enabled": true, "ratio": 2.0}`. Taken here and pushed to stormblock
/// by the fleet loop on its next tick.
async fn set_overcommit(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<OvercommitBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let oc = crate::drive::Overcommit::new(body.enabled, body.ratio).map_err(ApiError::bad_request)?;
    let did = resolve_id(&s, &id).await?;
    let (name, from, out) = {
        let mut inv = s.inventory.write().await;
        let d = inv.drives.get_mut(&did).expect("resolved id present");
        let from = d.overcommit;
        d.overcommit = oc;
        d.usage = d.usage.take().map(|u| u.priced(oc));
        (d.name.clone(), from, overcommit_json(d))
    };
    if from != oc {
        s.events.write().await.push(
            Some(did),
            Severity::Info,
            "overcommit",
            format!("{name}: overcommit {} → {} (operator)", from.word(), oc.word()),
        );
        s.persist().await;
    }
    Ok(Json(out))
}

/// Body-free form for renderers: `off`, or a ratio (`2`, `1.5`).
async fn overcommit_by_path(
    State(s): State<Arc<AppState>>,
    Path((id, value)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let body = match value.as_str() {
        "off" => OvercommitBody { enabled: false, ratio: None },
        v => {
            let ratio: f64 = v
                .trim_end_matches(['x', '×'])
                .parse()
                .map_err(|_| ApiError::bad_request(format!("overcommit {v:?}: off or a ratio")))?;
            OvercommitBody { enabled: true, ratio: Some(ratio) }
        }
    };
    set_overcommit(State(s), Path(id), Json(body)).await
}

#[derive(Deserialize)]
struct TestBody {
    kind: TestKind,
}

async fn start_test(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<TestBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let did = resolve_id(&s, &id).await?;
    let drive = {
        let inv = s.inventory.read().await;
        inv.drives.get(&did).cloned().expect("resolved id present")
    };
    if drive.activity == Activity::Testing {
        return Err(ApiError::conflict(format!("{}: a test is already running", drive.name)));
    }
    if drive.activity != Activity::Idle {
        return Err(ApiError::conflict(format!(
            "{}: activity is {:?}",
            drive.name, drive.activity
        )));
    }
    if body.kind.is_destructive() {
        if let Some(why) = drive.destructive_test_blocker() {
            return Err(ApiError::conflict(format!("{}: {why}", drive.name)));
        }
        // Re-check right now — the world may have moved since discovery.
        if crate::discovery::is_mounted(&drive.name) {
            return Err(ApiError::conflict(format!(
                "{}: has mounted partitions — refusing a destructive test",
                drive.name
            )));
        }
        if let Some(who) = crate::contents::probe(&drive.path) {
            return Err(ApiError::conflict(format!(
                "{}: holds data for {who} — refusing a destructive test",
                drive.name
            )));
        }
    }
    let handle = crate::drivetest::start(s.clone(), drive, body.kind).await;
    let run = handle.run.lock().unwrap().clone();
    Ok(Json(serde_json::to_value(run).map_err(|e| ApiError::internal(e.to_string()))?))
}

async fn get_test(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let did = resolve_id(&s, &id).await?;
    let tests = s.tests.read().await;
    let Some(h) = tests.get(&did) else {
        return Ok(Json(json!({ "test": null })));
    };
    let run = h.run.lock().unwrap().clone();
    Ok(Json(json!({ "test": run })))
}

async fn cancel_test(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let did = resolve_id(&s, &id).await?;
    let tests = s.tests.read().await;
    let Some(h) = tests.get(&did) else {
        return Err(ApiError::not_found("no test for this drive"));
    };
    let running = h.run.lock().unwrap().state == TestState::Running;
    if !running {
        return Err(ApiError::conflict("test is not running"));
    }
    h.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(Json(json!({ "cancelling": true })))
}

/// The controller → shelf → drive tree, for shelf rigs (NetApp DS-series
/// and friends). Shelves are keyed by serial so a dual-IOM shelf appears
/// once; drives not behind any shelf land under the controller's `direct`
/// list; drives with no controller at all land in `unlocated`.
async fn topology(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    use std::collections::BTreeMap;
    let inv = s.inventory.read().await;
    let ses = s.shelves.read().await.clone();
    let hbas = s.hbas.read().await.clone();

    fn drive_leaf(d: &crate::drive::Drive) -> serde_json::Value {
        json!({
            "id": d.id,
            "name": d.name,
            "bay": d.location.bay,
            "kind": d.kind,
            "model": d.model,
            "serial": d.serial,
            "capacity_bytes": d.capacity_bytes,
            "membership": d.membership,
            "designation": d.designation,
            "activity": d.activity,
            "health": d.health.status(),
            "paths": d.paths,
            "block_size": d.block_size,
            "usable": d.usable,
            "needs_reformat": d.needs_reformat(),
        })
    }

    // controller key → (controller json, shelf key → (shelf json, drives), direct drives)
    type ShelfEntry = (serde_json::Value, Vec<serde_json::Value>);
    type ControllerEntry = (
        serde_json::Value,
        BTreeMap<String, ShelfEntry>,
        Vec<serde_json::Value>,
    );
    let mut controllers: BTreeMap<String, ControllerEntry> = BTreeMap::new();
    let mut unlocated: Vec<serde_json::Value> = Vec::new();

    let mut sorted: Vec<_> = inv.drives.values().collect();
    sorted.sort_by(|a, b| (a.location.bay, &a.name).cmp(&(b.location.bay, &b.name)));
    for d in sorted {
        let leaf = drive_leaf(d);
        let Some(ctrl) = &d.location.controller else {
            unlocated.push(leaf);
            continue;
        };
        let ckey = ctrl
            .scsi_host
            .clone()
            .or_else(|| ctrl.pcie_addr.clone())
            .unwrap_or_else(|| "unknown".into());
        let entry = controllers.entry(ckey).or_insert_with(|| {
            (
                serde_json::to_value(ctrl).unwrap_or_default(),
                BTreeMap::new(),
                Vec::new(),
            )
        });
        match &d.location.shelf {
            Some(sh) => {
                let skey = sh.key().unwrap_or_else(|| "unknown".into());
                let shelf_entry = entry
                    .1
                    .entry(skey)
                    .or_insert_with(|| (serde_json::to_value(sh).unwrap_or_default(), Vec::new()));
                shelf_entry.1.push(leaf);
            }
            None => entry.2.push(leaf),
        }
    }

    // A card with nothing on it yet is still hardware on the node.
    for h in hbas.values() {
        let covered = controllers.values().any(|(c, _, _)| c["pcie_addr"] == h.pcie_addr.as_str());
        if !covered {
            let key = h.scsi_hosts.first().cloned().unwrap_or_else(|| h.pcie_addr.clone());
            let ctrl = crate::drive::Controller {
                scsi_host: h.scsi_hosts.first().cloned(),
                pcie_addr: Some(h.pcie_addr.clone()),
                driver: h.driver.clone(),
            };
            controllers.insert(key, (serde_json::to_value(ctrl).unwrap_or_default(), BTreeMap::new(), Vec::new()));
        }
    }

    let controllers: Vec<serde_json::Value> = controllers
        .into_iter()
        .map(|(key, (ctrl, shelves, direct))| {
            let shelves: Vec<serde_json::Value> = shelves
                .into_iter()
                .map(|(key, (sh, drives))| {
                    let ses_summary = ses.get(&key).map(|r| {
                        json!({
                            "status": r.worst(),
                            "max_temperature_c": r.max_temperature_c(),
                            "power_supplies": { "ok": r.count(crate::ses::ET_POWER_SUPPLY).0, "total": r.count(crate::ses::ET_POWER_SUPPLY).1 },
                            "fans": { "ok": r.count(crate::ses::ET_COOLING).0, "total": r.count(crate::ses::ET_COOLING).1 },
                            "paths": r.esps.len(),
                        })
                    });
                    json!({ "key": key, "shelf": sh, "ses": ses_summary, "drives": drives })
                })
                .collect();
            let hba = ctrl["pcie_addr"].as_str().and_then(|a| hbas.get(a));
            json!({ "key": key, "controller": ctrl, "hba": hba, "shelves": shelves, "direct": direct })
        })
        .collect();
    Json(json!({ "controllers": controllers, "unlocated": unlocated }))
}

/// `GET /api/v1/hbas` — every PCIe SCSI controller with its firmware,
/// option-ROM BIOS and NVDATA versions (inventory only; #2).
async fn list_hbas(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let hbas: Vec<crate::hba::Hba> = s.hbas.read().await.values().cloned().collect();
    Json(json!({ "hbas": hbas }))
}

async fn components_feed(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let feed = crate::components::collect(&s).await;
    Json(serde_json::to_value(feed).unwrap_or_default())
}

/// Full-snapshot pushes, stormd-style: every 2 s, send the feed when it
/// changed. No delta protocol on purpose.
async fn ws_components(
    ws: axum::extract::ws::WebSocketUpgrade,
    State(s): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |mut sock| async move {
        let mut last = String::new();
        loop {
            let feed = crate::components::collect(&s).await;
            let json = serde_json::to_string(&feed).unwrap_or_default();
            if json != last {
                if sock
                    .send(axum::extract::ws::Message::Text(json.clone().into()))
                    .await
                    .is_err()
                {
                    return;
                }
                last = json;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    })
}

// --------------------------------------------------------------- shelves

fn shelf_json(r: &crate::ses::ShelfReport, drives: &[serde_json::Value]) -> serde_json::Value {
    let (psu_ok, psu_n) = r.count(crate::ses::ET_POWER_SUPPLY);
    let (fan_ok, fan_n) = r.count(crate::ses::ET_COOLING);
    let (slot_ok, slot_n) = {
        let a = r.count(crate::ses::ET_ARRAY_DEVICE_SLOT);
        let b = r.count(crate::ses::ET_DEVICE_SLOT);
        (a.0 + b.0, a.1 + b.1)
    };
    json!({
        "key": r.key,
        "shelf": r.shelf,
        "display": r.shelf.display(),
        "esps": r.esps,
        "paths": r.esps.len(),
        "status": r.worst(),
        "critical": r.critical,
        "noncritical": r.noncritical,
        "unrecoverable": r.unrecoverable,
        "generation": r.generation,
        "max_temperature_c": r.max_temperature_c(),
        "power_supplies": { "ok": psu_ok, "total": psu_n },
        "fans": { "ok": fan_ok, "total": fan_n },
        "slots": { "ok": slot_ok, "total": slot_n },
        "elements": r.elements,
        "slot_addresses": r.slots,
        "drives": drives,
        "collected_at": r.collected_at,
    })
}

/// Drives that sit in this shelf, as short leaves (bay order).
async fn shelf_drives(s: &AppState, key: &str) -> Vec<serde_json::Value> {
    let inv = s.inventory.read().await;
    let mut ds: Vec<&crate::drive::Drive> = inv
        .drives
        .values()
        .filter(|d| d.location.shelf.as_ref().and_then(|sh| sh.key()).as_deref() == Some(key))
        .collect();
    ds.sort_by(|a, b| (a.location.bay, &a.name).cmp(&(b.location.bay, &b.name)));
    ds.iter()
        .map(|d| {
            json!({
                "id": d.id, "name": d.name, "bay": d.location.bay, "model": d.model, "serial": d.serial,
                "block_size": d.block_size, "usable": d.usable, "needs_reformat": d.needs_reformat(),
                "capacity_bytes": d.capacity_bytes, "membership": d.membership, "designation": d.designation,
                "activity": d.activity, "health": d.health.status(),
            })
        })
        .collect()
}

async fn list_shelves(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let shelves = s.shelves.read().await.clone();
    let mut out = Vec::new();
    for (key, r) in &shelves {
        let drives = shelf_drives(&s, key).await;
        out.push(shelf_json(r, &drives));
    }
    Json(json!({ "shelves": out }))
}

/// Resolve a shelf handle: logical id, serial, sysfs id, or ESP SCSI id.
async fn resolve_shelf(s: &AppState, handle: &str) -> Result<crate::ses::ShelfReport, ApiError> {
    let shelves = s.shelves.read().await;
    let h = crate::ses::normalize_sas(handle);
    shelves
        .values()
        .find(|r| {
            r.key == h
                || r.key == handle
                || r.shelf.serial.as_deref() == Some(handle)
                || r.shelf.id.as_deref() == Some(handle)
                || r.esps.iter().any(|e| e.scsi_id == handle)
        })
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("shelf {handle:?}")))
}

async fn get_shelf(
    State(s): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let r = resolve_shelf(&s, &key).await?;
    let drives = shelf_drives(&s, &r.key).await;
    Ok(Json(shelf_json(&r, &drives)))
}

#[derive(Deserialize)]
struct ShelfFirmwareBody {
    image: String,
    /// Go ahead although a drive serving data loses its only path while an
    /// IOM restarts.
    #[serde(default)]
    allow_path_loss: bool,
}

/// Update the shelf's IOM firmware from an image in the store, one IOM at a
/// time (#35). Never automatic.
async fn shelf_firmware(
    State(s): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<ShelfFirmwareBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let r = resolve_shelf(&s, &key).await?;
    if !crate::firmware::valid_image_name(&body.image) {
        return Err(ApiError::bad_request(format!("image name {:?}", body.image)));
    }
    let path = image_dir(&s)?.join(&body.image);
    let data = tokio::fs::read(&path)
        .await
        .map_err(|_| ApiError::not_found(format!("image {:?} is not in the store", body.image)))?;
    if data.is_empty() {
        return Err(ApiError::bad_request(format!("image {:?} is empty", body.image)));
    }
    let h = crate::iomfw::start(s.clone(), r, body.image, Arc::new(data), body.allow_path_loss)
        .await
        .map_err(ApiError::conflict)?;
    Ok(Json(json!(h.view())))
}

async fn get_shelf_firmware(State(s): State<Arc<AppState>>, Path(key): Path<String>) -> Result<Json<serde_json::Value>, ApiError> {
    let r = resolve_shelf(&s, &key).await?;
    let run = s.shelf_firmware.read().await.get(&r.key).map(|h| h.view());
    Ok(Json(json!({
        "shelf": r.key,
        "ioms": r.esps.iter().map(|e| json!({ "scsi_id": e.scsi_id, "serial": e.serial, "revision": e.revision })).collect::<Vec<_>>(),
        "run": run,
    })))
}

#[derive(Deserialize)]
struct ShelfLocateBody {
    on: bool,
    /// A bay in this shelf instead of the shelf's own IDENT.
    #[serde(default)]
    bay: Option<u32>,
}

async fn shelf_locate(
    State(s): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<ShelfLocateBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let r = resolve_shelf(&s, &key).await?;
    let (on, bay) = (body.on, body.bay);
    let r2 = r.clone();
    tokio::task::spawn_blocking(move || crate::ses::set_ident(&r2, bay, on))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    s.events.write().await.push(
        None,
        Severity::Info,
        "shelf",
        match bay {
            Some(b) => format!("shelf {}: bay {b} locate {}", r.shelf.display(), if on { "on" } else { "off" }),
            None => format!("shelf {}: locate {}", r.shelf.display(), if on { "on" } else { "off" }),
        },
    );
    Ok(Json(json!({ "key": r.key, "bay": bay, "locate": on })))
}

async fn shelf_locate_by_path(
    State(s): State<Arc<AppState>>,
    Path((key, state)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let on = match state.as_str() {
        "on" => true,
        "off" => false,
        other => return Err(ApiError::bad_request(format!("locate {other:?}: use on|off"))),
    };
    shelf_locate(State(s), Path(key), Json(ShelfLocateBody { on, bay: None })).await
}

// ---------------------------------------------------------------- format

#[derive(Deserialize)]
struct FormatBody {
    /// 512 or 4096.
    #[serde(default = "default_block_size")]
    block_size: u32,
}

fn default_block_size() -> u32 {
    4096
}

#[derive(Deserialize)]
struct FormatManyBody {
    /// Drive handles: id, name, path, serial or wwid.
    drives: Vec<String>,
    #[serde(default = "default_block_size")]
    block_size: u32,
}

#[derive(Deserialize)]
struct FormatShelfBody {
    #[serde(default = "default_block_size")]
    block_size: u32,
    /// Every out-of-fleet drive in the shelf, not only those the kernel
    /// cannot use.
    #[serde(default)]
    all: bool,
}

/// Validate every drive first, start none if any is blocked: an operator
/// asking for 24 drives gets 24 or a reason, never 17 and a surprise.
async fn start_formats(
    s: &Arc<AppState>,
    ids: Vec<DriveId>,
    block_size: u32,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !crate::format::valid_target(block_size) {
        return Err(ApiError::bad_request(format!("block_size {block_size}: use 512 or 4096")));
    }
    if ids.is_empty() {
        return Err(ApiError::bad_request("no drives to format"));
    }
    let mut drives = Vec::new();
    let mut blocked = Vec::new();
    {
        let inv = s.inventory.read().await;
        for id in &ids {
            let Some(d) = inv.drives.get(id) else {
                blocked.push(json!({ "id": id, "reason": "unknown drive" }));
                continue;
            };
            if let Some(why) = d.format_blocker() {
                blocked.push(json!({ "id": id, "name": d.name, "reason": why }));
                continue;
            }
            if crate::discovery::is_mounted(&d.name) {
                blocked.push(json!({ "id": id, "name": d.name, "reason": "has mounted partitions" }));
                continue;
            }
            if let Some(who) = crate::contents::probe(&d.path) {
                blocked.push(json!({ "id": id, "name": d.name, "reason": format!("holds data for {who}") }));
                continue;
            }
            drives.push(d.clone());
        }
    }
    if !blocked.is_empty() {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            code: "conflict",
            message: format!(
                "not started: {}",
                blocked
                    .iter()
                    .map(|b| format!(
                        "{} ({})",
                        b["name"].as_str().unwrap_or_else(|| b["id"].as_str().unwrap_or("?")),
                        b["reason"].as_str().unwrap_or("")
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        });
    }
    let mut started = Vec::new();
    for d in drives {
        let h = crate::format::start(s.clone(), d, block_size).await;
        let run = h.run.lock().unwrap().clone();
        started.push(serde_json::to_value(run).unwrap_or_default());
    }
    Ok(Json(json!({ "block_size": block_size, "started": started })))
}

async fn format_drive(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<FormatBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let did = resolve_id(&s, &id).await?;
    start_formats(&s, vec![did], body.block_size).await
}

async fn format_drive_by_path(
    State(s): State<Arc<AppState>>,
    Path((id, bs)): Path<(String, u32)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    format_drive(State(s), Path(id), Json(FormatBody { block_size: bs })).await
}

async fn format_many(
    State(s): State<Arc<AppState>>,
    Json(body): Json<FormatManyBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut ids = Vec::new();
    for h in &body.drives {
        let id = resolve_id(&s, h).await?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    start_formats(&s, ids, body.block_size).await
}

async fn format_shelf(
    State(s): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<FormatShelfBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let r = resolve_shelf(&s, &key).await?;
    let ids: Vec<DriveId> = {
        let inv = s.inventory.read().await;
        inv.drives
            .values()
            .filter(|d| d.location.shelf.as_ref().and_then(|sh| sh.key()).as_deref() == Some(r.key.as_str()))
            .filter(|d| d.membership == Membership::Out)
            .filter(|d| body.all || d.needs_reformat() || d.block_size != body.block_size && !d.usable)
            .map(|d| d.id)
            .collect()
    };
    if ids.is_empty() {
        return Err(ApiError::conflict(format!(
            "shelf {}: no out-of-fleet drives need a reformat (pass \"all\": true to format every out-of-fleet drive)",
            r.shelf.display()
        )));
    }
    start_formats(&s, ids, body.block_size).await
}

async fn format_shelf_by_path(
    State(s): State<Arc<AppState>>,
    Path((key, bs)): Path<(String, u32)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    format_shelf(State(s), Path(key), Json(FormatShelfBody { block_size: bs, all: false })).await
}

async fn get_format(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let did = resolve_id(&s, &id).await?;
    let formats = s.formats.read().await;
    let run = formats.get(&did).map(|h| h.run.lock().unwrap().clone());
    let record = s.inventory.read().await.drives.get(&did).and_then(|d| d.format.clone());
    Ok(Json(json!({ "run": run, "last": record })))
}

async fn list_formats(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let formats = s.formats.read().await;
    let mut runs: Vec<crate::format::FormatRun> = formats.values().map(|h| h.run.lock().unwrap().clone()).collect();
    runs.sort_by(|a, b| a.name.cmp(&b.name));
    let running = runs.iter().filter(|r| r.state == crate::format::FormatState::Running).count();
    Json(json!({ "running": running, "formats": runs }))
}

// -------------------------------------------------------------- firmware

fn image_dir(s: &AppState) -> Result<PathBuf, ApiError> {
    crate::firmware::image_dir(s.config.data_dir.as_deref())
        .ok_or_else(|| ApiError::bad_request("no data_dir configured — the firmware image store needs one"))
}

async fn list_images(State(s): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiError> {
    let dir = image_dir(&s)?;
    let imgs = tokio::task::spawn_blocking(move || crate::firmware::list_images(&dir))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(json!({ "images": imgs })))
}

async fn get_image(
    State(s): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let dir = image_dir(&s)?;
    let imgs = tokio::task::spawn_blocking(move || crate::firmware::list_images(&dir))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    imgs.into_iter()
        .find(|i| i.name == name)
        .map(|i| Json(serde_json::to_value(i).unwrap_or_default()))
        .ok_or_else(|| ApiError::not_found(format!("image {name:?}")))
}

/// Raw upload: `PUT /api/v1/firmware/images/<name>` with the file as the
/// body. Written to a temp file and renamed, so a half-upload never
/// becomes an image.
async fn put_image(
    State(s): State<Arc<AppState>>,
    Path(name): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !crate::firmware::valid_image_name(&name) {
        return Err(ApiError::bad_request(format!("image name {name:?}: letters, digits, . _ - + only")));
    }
    if body.is_empty() {
        return Err(ApiError::bad_request("empty image"));
    }
    let dir = image_dir(&s)?;
    let data = body.to_vec();
    let n2 = name.clone();
    let img = tokio::task::spawn_blocking(move || -> std::io::Result<crate::firmware::Image> {
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!(".{n2}.upload"));
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, dir.join(&n2))?;
        Ok(crate::firmware::Image {
            name: n2,
            size: data.len() as u64,
            sha256: crate::firmware::sha256_hex(&data),
            modified: Some(std::time::SystemTime::now()),
        })
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
    .map_err(|e| ApiError::internal(format!("store image: {e}")))?;
    s.events.write().await.push(
        None,
        Severity::Info,
        "firmware",
        format!("image {} stored ({} bytes, sha256 {}…)", img.name, img.size, &img.sha256[..12]),
    );
    Ok(Json(serde_json::to_value(img).unwrap_or_default()))
}

async fn delete_image(
    State(s): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !crate::firmware::valid_image_name(&name) {
        return Err(ApiError::bad_request(format!("image name {name:?}")));
    }
    let dir = image_dir(&s)?;
    let p = dir.join(&name);
    if !p.is_file() {
        return Err(ApiError::not_found(format!("image {name:?}")));
    }
    std::fs::remove_file(&p).map_err(|e| ApiError::internal(format!("remove: {e}")))?;
    Ok(Json(json!({ "deleted": name })))
}

#[derive(Deserialize)]
struct FirmwareBody {
    image: String,
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
struct FirmwareManyBody {
    image: String,
    /// Drive handles (id, name, path, serial, wwid) …
    #[serde(default)]
    drives: Vec<String>,
    /// … and/or every drive of this model (exact match on the INQUIRY
    /// product / NVMe model string).
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    force: bool,
}

/// Validate every drive and read the image once; start none if any is
/// blocked. Fleet drives queue on the node-wide lock inside the engine.
async fn start_firmware(
    s: &Arc<AppState>,
    ids: Vec<DriveId>,
    image_name: String,
    force: bool,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !crate::firmware::valid_image_name(&image_name) {
        return Err(ApiError::bad_request(format!("image name {image_name:?}")));
    }
    if ids.is_empty() {
        return Err(ApiError::bad_request("no drives to update"));
    }
    let dir = image_dir(s)?;
    let path = dir.join(&image_name);
    let data = tokio::fs::read(&path)
        .await
        .map_err(|_| ApiError::not_found(format!("image {image_name:?} is not in the store")))?;
    if data.is_empty() {
        return Err(ApiError::bad_request(format!("image {image_name:?} is empty")));
    }
    let image = Arc::new(data);
    let mut drives = Vec::new();
    let mut blocked = Vec::new();
    {
        let inv = s.inventory.read().await;
        for id in &ids {
            let Some(d) = inv.drives.get(id) else {
                blocked.push(format!("{id} (unknown drive)"));
                continue;
            };
            if let Some(why) = d.firmware_blocker(force) {
                blocked.push(format!("{} ({why})", d.name));
                continue;
            }
            drives.push(d.clone());
        }
    }
    if !blocked.is_empty() {
        return Err(ApiError::conflict(format!("not started: {}", blocked.join("; "))));
    }
    let mut started = Vec::new();
    for d in drives {
        let h = crate::firmware::start(s.clone(), d, image_name.clone(), image.clone(), force).await;
        let run = h.run.lock().unwrap().clone();
        started.push(serde_json::to_value(run).unwrap_or_default());
    }
    Ok(Json(json!({ "image": image_name, "started": started })))
}

async fn firmware_drive(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<FirmwareBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let did = resolve_id(&s, &id).await?;
    start_firmware(&s, vec![did], body.image, body.force).await
}

async fn firmware_many(
    State(s): State<Arc<AppState>>,
    Json(body): Json<FirmwareManyBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut ids = Vec::new();
    for h in &body.drives {
        let id = resolve_id(&s, h).await?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if let Some(model) = &body.model {
        let inv = s.inventory.read().await;
        for d in inv.drives.values() {
            if d.model.trim() == model.trim() && !ids.contains(&d.id) {
                ids.push(d.id);
            }
        }
        if ids.is_empty() {
            return Err(ApiError::not_found(format!("no drives of model {model:?}")));
        }
    }
    start_firmware(&s, ids, body.image, body.force).await
}

async fn get_firmware(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let did = resolve_id(&s, &id).await?;
    let fws = s.firmware.read().await;
    let run = fws.get(&did).map(|h| h.run.lock().unwrap().clone());
    let (version, last) = {
        let inv = s.inventory.read().await;
        let d = inv.drives.get(&did);
        (d.map(|d| d.firmware.clone()), d.and_then(|d| d.firmware_update.clone()))
    };
    Ok(Json(json!({ "firmware": version, "run": run, "last": last })))
}

async fn list_firmware(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let fws = s.firmware.read().await;
    let mut runs: Vec<crate::firmware::FwRun> = fws.values().map(|h| h.run.lock().unwrap().clone()).collect();
    runs.sort_by(|a, b| a.name.cmp(&b.name));
    let active = runs
        .iter()
        .filter(|r| matches!(r.state, crate::firmware::FwState::Running | crate::firmware::FwState::Queued))
        .count();
    Json(json!({ "active": active, "updates": runs }))
}

// --- Parameter-less action wrappers (stormview renderers POST with no body) ---

async fn locate_by_path(
    State(s): State<Arc<AppState>>,
    Path((id, state)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let on = match state.as_str() {
        "on" => true,
        "off" => false,
        other => return Err(ApiError::bad_request(format!("locate {other:?}: use on|off"))),
    };
    set_locate(State(s), Path(id), Json(LocateBody { on })).await
}

async fn fleet_by_path(
    State(s): State<Arc<AppState>>,
    Path((id, action)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if action != "join" && action != "leave" {
        return Err(ApiError::bad_request(format!("fleet {action:?}: use join|leave")));
    }
    // The body-free join never formats a slab; the JSON endpoint stays the
    // door for that explicitly destructive choice.
    fleet_action(
        State(s),
        Path(id),
        Json(FleetBody {
            action,
            format_slab: false,
            tier: None,
            drain: false,
            force: false,
        }),
    )
    .await
}

async fn designation_by_path(
    State(s): State<Arc<AppState>>,
    Path((id, value)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let designation: Designation = serde_json::from_value(serde_json::Value::String(value.clone()))
        .map_err(|_| ApiError::bad_request(format!("designation {value:?}")))?;
    set_designation(State(s), Path(id), Json(DesignationBody { designation })).await
}

async fn test_by_path(
    State(s): State<Arc<AppState>>,
    Path((id, kind)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let kind: TestKind = serde_json::from_value(serde_json::Value::String(kind.clone()))
        .map_err(|_| ApiError::bad_request(format!("test kind {kind:?}")))?;
    start_test(State(s), Path(id), Json(TestBody { kind })).await
}

#[derive(Deserialize)]
struct SinceQuery {
    #[serde(default)]
    since: u64,
}

async fn list_events(
    State(s): State<Arc<AppState>>,
    Query(q): Query<SinceQuery>,
) -> Json<serde_json::Value> {
    let log = s.events.read().await;
    let started = log.started().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    Json(json!({
        "latest_seq": log.latest_seq(),
        // When this process started (#25): the newest events before it
        // were restored from events.json, so seq continued.
        "started": started,
        "persisted": s.events_path.is_some(),
        "events": log.since(q.since),
    }))
}

/// The stormd dashboard card (RemoteSummary shape). Must answer inside
/// stormd's 400 ms timeout, so it only reads cached state.
async fn summary(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let inv = s.inventory.read().await;
    let mut total = 0u32;
    let mut fleet = 0u32;
    let mut spares = 0u32;
    let mut testing = 0u32;
    let mut unusable = 0u32;
    let mut warn = 0u32;
    let mut bad = 0u32;
    let mut hottest: Option<i32> = None;
    let mut worst_wear: Option<u8> = None;
    for d in inv.drives.values() {
        total += 1;
        if d.membership == Membership::Fleet {
            fleet += 1;
        }
        match d.designation {
            Designation::Spare => spares += 1,
            Designation::Failed => bad += 1,
            _ => {}
        }
        match d.activity {
            Activity::Testing | Activity::Formatting | Activity::Sanitizing | Activity::UpdatingFirmware => testing += 1,
            Activity::Missing => bad += 1,
            Activity::Draining => warn += 1,
            Activity::Idle => {}
        }
        if !d.usable && d.activity == Activity::Idle {
            unusable += 1;
        }
        match d.health.status() {
            HealthStatus::Warning => warn += 1,
            HealthStatus::Failing | HealthStatus::Failed => bad += 1,
            _ => {}
        }
        if let Some(t) = d.health.temperature_c {
            hottest = Some(hottest.map_or(t, |h| h.max(t)));
        }
        if let Some(w) = d.health.wear_pct {
            worst_wear = Some(worst_wear.map_or(w, |x| x.max(w)));
        }
    }
    let shelves = s.shelves.read().await;
    let shelf_bad = shelves.values().filter(|r| r.worst().is_bad()).count() as u32;
    for r in shelves.values() {
        if let Some(t) = r.max_temperature_c() {
            hottest = Some(hottest.map_or(t, |h| h.max(t)));
        }
    }
    let health = if bad > 0 || shelf_bad > 0 {
        "error"
    } else if warn > 0 || unusable > 0 {
        "warn"
    } else if total == 0 {
        "idle"
    } else {
        "ok"
    };
    let detail = if total == 0 {
        "no drives discovered".to_string()
    } else {
        let mut d = format!("{total} drives, {fleet} fleet, {spares} spare, {testing} busy, {warn} warn, {bad} bad");
        if unusable > 0 {
            d.push_str(&format!(", {unusable} need reformat"));
        }
        if !shelves.is_empty() {
            d.push_str(&format!(", {} shelves", shelves.len()));
            if shelf_bad > 0 {
                d.push_str(&format!(" ({shelf_bad} degraded)"));
            }
        }
        d
    };
    let mut metrics = vec![
        json!({ "label": "Drives", "value": total.to_string() }),
        json!({ "label": "Fleet", "value": fleet.to_string(), "tone": "accent" }),
    ];
    if spares > 0 {
        metrics.push(json!({ "label": "Spare", "value": spares.to_string(), "tone": "muted" }));
    }
    if warn + bad > 0 {
        metrics.push(json!({
            "label": "Attention",
            "value": (warn + bad).to_string(),
            "tone": if bad > 0 { "error" } else { "warn" },
        }));
    }
    if unusable > 0 {
        metrics.push(json!({ "label": "Reformat", "value": unusable.to_string(), "tone": "warn" }));
    }
    if !shelves.is_empty() {
        metrics.push(json!({
            "label": "Shelves",
            "value": shelves.len().to_string(),
            "tone": if shelf_bad > 0 { "error" } else { "muted" },
        }));
    }
    if let Some(t) = hottest {
        metrics.push(json!({ "label": "Hottest", "value": t.to_string(), "unit": "°C" }));
    }
    if let Some(w) = worst_wear {
        metrics.push(json!({ "label": "Worst wear", "value": w.to_string(), "unit": "%" }));
    }
    Json(json!({ "health": health, "detail": detail, "metrics": metrics }))
}
