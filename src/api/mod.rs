//! REST API on :9092, plus the stormd dashboard card.
//! Error envelope matches stormblock's `{error, code}` shape.

use crate::config::Config;
use crate::drive::{DriveState, HealthStatus};
use crate::events::EventLog;
use crate::inventory::Inventory;
use crate::stormblock::StormBlockClient;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct AppState {
    pub config: Config,
    pub inventory: RwLock<Inventory>,
    pub events: RwLock<EventLog>,
    pub stormblock: StormBlockClient,
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
        .route("/api/v1/health", get(health))
        .route("/api/v1/drives", get(list_drives))
        .route("/api/v1/drives/{id}", get(get_drive))
        .route("/api/v1/drives/{id}/health", get(get_drive_health))
        .route("/api/v1/drives/{id}/locate", post(set_locate))
        .route("/api/v1/drives/{id}/state", post(set_state))
        .route("/api/v1/events", get(list_events))
        .route("/api/v1/summary", get(summary))
        .with_state(state)
}

async fn health(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "version": crate::VERSION,
        "node": s.node_name,
    }))
}

async fn list_drives(State(s): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let inv = s.inventory.read().await;
    let mut drives: Vec<_> = inv.drives.values().cloned().collect();
    drives.sort_by(|a, b| a.name.cmp(&b.name));
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
    let name = {
        let inv = s.inventory.read().await;
        inv.resolve(&id)
            .ok_or_else(|| ApiError::not_found(format!("drive {id:?}")))?
            .name
            .clone()
    };
    let on = body.on;
    let n2 = name.clone();
    tokio::task::spawn_blocking(move || crate::topology::set_locate(&n2, on))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(|e| ApiError::bad_request(format!("{name}: {e}")))?;
    Ok(Json(json!({ "name": name, "locate": on })))
}

#[derive(Deserialize)]
struct StateBody {
    state: String,
}

async fn set_state(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<StateBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let target = match body.state.as_str() {
        "available" => DriveState::Available,
        "draining" => DriveState::Draining,
        "retired" => DriveState::Retired,
        other => {
            return Err(ApiError::bad_request(format!(
                "state {other:?} not settable; use available|draining|retired"
            )))
        }
    };
    let mut inv = s.inventory.write().await;
    let Some(d) = inv.resolve(&id).map(|d| d.id) else {
        return Err(ApiError::not_found(format!("drive {id:?}")));
    };
    let d = inv.drives.get_mut(&d).expect("resolved id present");
    let from = d.state;
    d.state = target;
    let name = d.name.clone();
    let did = d.id;
    drop(inv);
    s.events.write().await.push(
        Some(did),
        crate::events::Severity::Info,
        "state",
        format!("{name}: {from:?} → {target:?} (operator)"),
    );
    s.persist().await;
    Ok(Json(json!({ "id": did, "from": from, "to": target })))
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
    let mut active = 0u32;
    let mut warn = 0u32;
    let mut bad = 0u32;
    let mut hottest: Option<i32> = None;
    let mut worst_wear: Option<u8> = None;
    for d in inv.drives.values() {
        total += 1;
        match d.state {
            DriveState::Active => active += 1,
            DriveState::Failed | DriveState::Missing => bad += 1,
            DriveState::Draining => warn += 1,
            _ => {}
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
    let health = if bad > 0 {
        "error"
    } else if warn > 0 {
        "warn"
    } else if total == 0 {
        "idle"
    } else {
        "ok"
    };
    let detail = if total == 0 {
        "no drives discovered".to_string()
    } else {
        format!("{total} drives, {active} in stormblock, {warn} warning, {bad} failed/missing")
    };
    let mut metrics = vec![
        json!({ "label": "Drives", "value": total.to_string() }),
        json!({ "label": "Active", "value": active.to_string(), "tone": "accent" }),
    ];
    if warn + bad > 0 {
        metrics.push(json!({
            "label": "Attention",
            "value": (warn + bad).to_string(),
            "tone": if bad > 0 { "error" } else { "warn" },
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
