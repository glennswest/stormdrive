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

const INDEX_HTML: &str = include_str!("../ui/index.html");

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
    /// Latest SES scan: every shelf the node can talk to, by logical id.
    pub shelves: RwLock<crate::topology::Shelves>,
    pub inventory_path: Option<PathBuf>,
    pub node_name: String,
}

impl AppState {
    pub async fn persist(&self) {
        let Some(path) = &self.inventory_path else {
            return;
        };
        let inv = self.inventory.read().await;
        if let Err(e) = inv.save(path) {
            tracing::error!("inventory persist failed: {e:#}");
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
        .route("/api/v1/health", get(health))
        .route("/api/v1/components", get(components_feed))
        .route("/ws/components", get(ws_components))
        .route("/api/v1/drives", get(list_drives))
        .route("/api/v1/drives/{id}", get(get_drive))
        .route("/api/v1/drives/{id}/health", get(get_drive_health))
        .route("/api/v1/drives/{id}/locate", post(set_locate))
        // Parameter-less action routes: a stormview renderer invokes
        // method+path with no body, so every action needs a body-free form.
        .route("/api/v1/drives/{id}/locate/{state}", post(locate_by_path))
        .route("/api/v1/drives/{id}/fleet", post(fleet_action))
        .route("/api/v1/drives/{id}/drain", get(get_drain).post(start_drain).delete(cancel_drain))
        .route("/api/v1/drives/{id}/fleet/{action}", post(fleet_by_path))
        .route("/api/v1/drives/{id}/designation", post(set_designation))
        .route("/api/v1/drives/{id}/designation/{value}", post(designation_by_path))
        .route("/api/v1/drives/{id}/test", get(get_test).post(start_test))
        .route("/api/v1/drives/{id}/test/cancel", post(cancel_test))
        .route("/api/v1/drives/{id}/test/{kind}", post(test_by_path))
        // Sector-size reformat (FORMAT UNIT): one drive, many drives, or a
        // whole shelf's worth of 520-byte drives.
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
        .route("/api/v1/shelves/{key}/format/{block_size}", post(format_shelf_by_path))
        .route("/api/v1/topology", get(topology))
        .route("/api/v1/events", get(list_events))
        .route("/api/v1/summary", get(summary))
        // Kubernetes-shaped resources, served by this daemon (stormblock#80).
        .merge(kube::router())
        .layer(axum::extract::DefaultBodyLimit::max(
            state.config.firmware.max_image_mib as usize * 1024 * 1024 + 4096,
        ))
        .with_state(state)
}

async fn ui_index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn health(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "version": crate::VERSION,
        "node": s.node_name,
    }))
}

/// Resolve an API handle to a DriveId under a read lock.
async fn resolve_id(s: &AppState, handle: &str) -> Result<DriveId, ApiError> {
    let inv = s.inventory.read().await;
    inv.resolve(handle)
        .map(|d| d.id)
        .ok_or_else(|| ApiError::not_found(format!("drive {handle:?}")))
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
    Ok(Json(serde_json::to_value(d).map_err(|e| ApiError::internal(e.to_string()))?))
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
                    .drive_slabs(&drive.path)
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
                .delete_drive(&drive.path, body.force)
                .await
                .map_err(|e| ApiError::upstream(format!("remove drive: {e:#}")))?;
            {
                let mut inv = s.inventory.write().await;
                if let Some(d) = inv.drives.get_mut(&did) {
                    d.membership = Membership::Out;
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
        let path = s.inventory.read().await.drives.get(&did).map(|d| d.path.clone()).unwrap_or_default();
        if let Err(e) = s.stormblock.report_health(&path, "failed", Some("operator designation"), false).await {
            tracing::warn!(drive = %name, "failed designation not reported to stormblock: {e:#}");
        } else if let Some(d) = s.inventory.write().await.drives.get_mut(&did) {
            d.pushed_health = Some("failed".into());
        }
        if s.config.stormblock.drain_on_failing {
            match crate::fleet::start_drain(&s, did, "operator", true).await {
                Ok(rec) => drain = Some(rec),
                Err(e) => {
                    s.events.write().await.push(
                        Some(did),
                        Severity::Error,
                        "drain",
                        format!("{name}: marked failed but the drain did not start: {e:#}"),
                    );
                }
            }
        }
    }
    s.persist().await;
    Ok(Json(json!({ "id": did, "from": from, "to": body.designation, "drain": drain })))
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
            json!({ "key": key, "controller": ctrl, "shelves": shelves, "direct": direct })
        })
        .collect();
    Json(json!({ "controllers": controllers, "unlocated": unlocated }))
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
        let h = crate::firmware::start(s.clone(), d, image_name.clone(), image.clone()).await;
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
    Json(json!({ "latest_seq": log.latest_seq(), "events": log.since(q.since) }))
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
            Activity::Testing | Activity::Formatting | Activity::UpdatingFirmware => testing += 1,
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
