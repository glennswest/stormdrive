//! Kubernetes-shaped resources, served by stormdrive itself (stormblock#80).
//!
//! `/apis/storage.storm.io/v1/drives` and `…/enclosures` — the physical
//! truth this daemon owns, in the `apiVersion/kind/metadata/spec/status`
//! shape kubectl, an aggregated API server and stormview read. API discovery
//! at `/apis` and `/apis/storage.storm.io/v1`; `?watch=1` streams
//! newline-delimited `{type, object}` events. stormblock serves the same
//! group with its `Volume`/`Slab`/`Node` (and the engine's own `Drive`);
//! the two are told apart by `metadata.labels["storm.io/component"]`.
//!
//! No second store: every object is a projection of the inventory, and the
//! `spec` fields that can be written — `designation`, `fleet`, `drain`,
//! `locate` — map one-to-one onto verbs the REST API already has.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use super::AppState;
use crate::drive::{Activity, Designation, Drive, DriveId, Membership};
use crate::events::Severity;

pub const GROUP: &str = "storage.storm.io";
pub const VERSION: &str = "v1";

fn api_version() -> String {
    format!("{GROUP}/{VERSION}")
}

fn status_error(code: StatusCode, reason: &str, message: impl Into<String>) -> Response {
    (
        code,
        Json(json!({
            "apiVersion": "v1", "kind": "Status", "status": "Failure",
            "message": message.into(), "reason": reason, "code": code.as_u16(),
        })),
    )
        .into_response()
}

fn fingerprint(v: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.to_string().hash(&mut h);
    h.finish()
}

// ------------------------------------------------------------- discovery

fn group_json() -> Value {
    json!({
        "name": GROUP,
        "versions": [{ "groupVersion": api_version(), "version": VERSION }],
        "preferredVersion": { "groupVersion": api_version(), "version": VERSION },
    })
}

async fn apis() -> Json<Value> {
    Json(json!({ "kind": "APIGroupList", "apiVersion": "v1", "groups": [group_json()] }))
}

async fn api_group() -> Json<Value> {
    let mut g = group_json();
    g["kind"] = json!("APIGroup");
    g["apiVersion"] = json!("v1");
    Json(g)
}

async fn api_resources() -> Json<Value> {
    let res = |name: &str, kind: &str, verbs: &[&str]| {
        json!({ "name": name, "singularName": kind.to_lowercase(), "namespaced": false, "kind": kind, "verbs": verbs })
    };
    Json(json!({
        "kind": "APIResourceList", "apiVersion": "v1", "groupVersion": api_version(),
        "resources": [
            res("drives", "Drive", &["get", "list", "watch", "patch"]),
            res("enclosures", "Enclosure", &["get", "list", "watch"]),
        ],
    }))
}

// ---------------------------------------------------------------- drives

fn drive_object(d: &Drive, node: &str) -> Value {
    let key = d.id.0.to_string();
    let mut labels = BTreeMap::new();
    labels.insert("storm.io/component".to_string(), "stormdrive".to_string());
    labels.insert("storm.io/node".to_string(), node.to_string());
    labels.insert("storm.io/path".to_string(), d.path.clone());
    labels.insert("storm.io/kind".to_string(), format!("{:?}", d.kind).to_lowercase());
    for (k, v) in d.location.labels() {
        labels.insert(format!("storm.io/{k}"), v);
    }
    let fleet = match d.membership {
        Membership::Fleet => "fleet",
        Membership::Out => "out",
    };
    json!({
        "apiVersion": api_version(),
        "kind": "Drive",
        "metadata": { "name": key, "uid": key, "labels": labels },
        "spec": {
            "designation": d.designation,
            "fleet": fleet,
            "drain": d.drain.as_ref().is_some_and(|r| r.state == "running"),
        },
        "status": {
            "node": node,
            "path": d.path,
            "paths": d.paths,
            "name": d.name,
            "kind": d.kind,
            "model": d.model,
            "serial": d.serial,
            "firmware": d.firmware,
            "wwid": d.wwid,
            "capacityBytes": d.capacity_bytes,
            "blockSize": d.block_size,
            "physicalBlockSize": d.physical_block_size,
            "usable": d.usable,
            "needsReformat": d.needs_reformat(),
            "location": d.location,
            "membership": d.membership,
            "activity": d.activity,
            "health": d.health,
            "drain": d.drain,
            "pushedLabels": d.pushed_labels,
            "firstSeen": d.first_seen.duration_since(std::time::UNIX_EPOCH).map(|x| x.as_secs()).unwrap_or(0),
            "lastSeen": d.last_seen.duration_since(std::time::UNIX_EPOCH).map(|x| x.as_secs()).unwrap_or(0),
        },
    })
}

async fn all_drives(s: &AppState) -> Vec<Value> {
    let inv = s.inventory.read().await;
    let mut sorted: Vec<&Drive> = inv.drives.values().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    sorted.iter().map(|d| drive_object(d, &s.node_name)).collect()
}

async fn get_drive(State(s): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let inv = s.inventory.read().await;
    match inv.resolve(&name) {
        Some(d) => Json(drive_object(d, &s.node_name)).into_response(),
        None => status_error(StatusCode::NOT_FOUND, "NotFound", format!("drives \"{name}\" not found")),
    }
}

#[derive(Deserialize, Default)]
struct DrivePatch {
    #[serde(default)]
    spec: Option<DriveSpecPatch>,
}

#[derive(Deserialize, Default)]
struct DriveSpecPatch {
    #[serde(default)]
    designation: Option<Designation>,
    /// `fleet` or `out`.
    #[serde(default)]
    fleet: Option<String>,
    /// `true` drains (and retires once empty); `false` cancels.
    #[serde(default)]
    drain: Option<bool>,
    /// Locate LED.
    #[serde(default)]
    locate: Option<bool>,
}

/// `PATCH /apis/storage.storm.io/v1/drives/{name}` — the writes the REST API
/// already offers, as a merge-patch on `spec`.
async fn patch_drive(
    State(s): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(patch): Json<DrivePatch>,
) -> Response {
    let drive = {
        let inv = s.inventory.read().await;
        match inv.resolve(&name) {
            Some(d) => d.clone(),
            None => return status_error(StatusCode::NOT_FOUND, "NotFound", format!("drives \"{name}\" not found")),
        }
    };
    let did: DriveId = drive.id;
    let Some(spec) = patch.spec else {
        return get_drive(State(s), Path(name)).await;
    };

    if let Some(des) = spec.designation {
        let from = {
            let mut inv = s.inventory.write().await;
            let d = inv.drives.get_mut(&did).expect("resolved id present");
            let from = d.designation;
            d.designation = des;
            from
        };
        s.events.write().await.push(
            Some(did),
            Severity::Info,
            "designation",
            format!("{}: {from:?} → {des:?} (kube)", drive.name),
        );
        if des == Designation::Failed && drive.membership == Membership::Fleet && s.stormblock.enabled() {
            let _ = s.stormblock.report_health(&drive.path, "failed", Some("operator designation"), false).await;
            if let Some(d) = s.inventory.write().await.drives.get_mut(&did) {
                d.pushed_health = Some("failed".into());
            }
            if s.config.stormblock.drain_on_failing {
                if let Err(e) = crate::fleet::start_drain(&s, did, "operator", true).await {
                    return status_error(StatusCode::BAD_GATEWAY, "Upstream", format!("drain: {e:#}"));
                }
            }
        }
    }

    if let Some(fleet) = spec.fleet {
        if !s.stormblock.enabled() {
            return status_error(StatusCode::BAD_REQUEST, "BadRequest", "stormblock integration is disabled");
        }
        match fleet.as_str() {
            "fleet" | "join" => {
                if let Some(why) = drive.fleet_join_blocker() {
                    if why != "already in the fleet" {
                        return status_error(StatusCode::CONFLICT, "Conflict", format!("{}: {why}", drive.name));
                    }
                } else {
                    let labels = drive.stormblock_labels();
                    if let Err(e) = crate::fleet::join(
                        &s, did, &drive.name, &drive.path, &labels, drive.kind,
                        s.config.stormblock.auto_format_slab, None,
                    )
                    .await
                    {
                        return status_error(StatusCode::BAD_GATEWAY, "Upstream", format!("join: {e:#}"));
                    }
                    s.events.write().await.push(Some(did), Severity::Info, "fleet", format!("{}: joined the fleet (kube)", drive.name));
                }
            }
            "out" | "leave" => {
                if drive.membership == Membership::Fleet {
                    // Leaving through the resource always drains first: a
                    // declarative `fleet: out` must not strand data.
                    if let Err(e) = crate::fleet::start_drain(&s, did, "leave", true).await {
                        return status_error(StatusCode::BAD_GATEWAY, "Upstream", format!("drain: {e:#}"));
                    }
                }
            }
            other => return status_error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid", format!("spec.fleet {other:?}: use fleet or out")),
        }
    }

    match spec.drain {
        Some(true) => {
            if let Err(e) = crate::fleet::start_drain(&s, did, "operator", false).await {
                return status_error(StatusCode::BAD_GATEWAY, "Upstream", format!("drain: {e:#}"));
            }
        }
        Some(false) if drive.activity == Activity::Draining => {
            if let Err(e) = crate::fleet::cancel_drain(&s, did).await {
                return status_error(StatusCode::BAD_GATEWAY, "Upstream", format!("cancel drain: {e:#}"));
            }
        }
        Some(false) => {}
        None => {}
    }

    if let Some(on) = spec.locate {
        let shelves = s.shelves.read().await.clone();
        if let Err(e) = crate::topology::set_locate(&drive.name, &drive.location, &shelves, on) {
            return status_error(StatusCode::CONFLICT, "Conflict", format!("locate LED: {e}"));
        }
    }

    s.persist().await;
    get_drive(State(s), Path(name)).await
}

// ------------------------------------------------------------ enclosures

async fn all_enclosures(s: &AppState) -> Vec<Value> {
    let ses = s.shelves.read().await.clone();
    let inv = s.inventory.read().await;
    let mut shelves: BTreeMap<String, (Value, Vec<Value>, Vec<String>)> = BTreeMap::new();
    for (key, r) in &ses {
        let sh = &r.shelf;
        shelves.entry(key.clone()).or_insert_with(|| {
            (
                json!({ "id": sh.id, "vendor": sh.vendor, "model": sh.model, "serial": sh.serial, "sasAddress": sh.sas_address, "logicalId": sh.logical_id, "display": sh.display() }),
                Vec::new(),
                Vec::new(),
            )
        });
    }
    let mut sorted: Vec<&Drive> = inv.drives.values().collect();
    sorted.sort_by(|a, b| (a.location.bay, &a.name).cmp(&(b.location.bay, &b.name)));
    for d in sorted {
        let Some(sh) = &d.location.shelf else { continue };
        let Some(key) = sh.key() else { continue };
        let entry = shelves.entry(key).or_insert_with(|| {
            (
                json!({ "id": sh.id, "vendor": sh.vendor, "model": sh.model, "serial": sh.serial, "sasAddress": sh.sas_address, "logicalId": sh.logical_id, "display": sh.display() }),
                Vec::new(),
                Vec::new(),
            )
        });
        entry.1.push(json!({
            "drive": d.id.0.to_string(), "name": d.name, "bay": d.location.bay,
            "health": d.health.status(), "membership": d.membership, "kind": d.kind,
            "blockSize": d.block_size, "usable": d.usable, "needsReformat": d.needs_reformat(),
        }));
        if let Some(c) = &d.location.controller {
            if let Some(h) = &c.scsi_host {
                if !entry.2.contains(h) {
                    entry.2.push(h.clone());
                }
            }
        }
    }
    shelves
        .into_iter()
        .map(|(key, (shelf, drives, hbas))| {
            let mut labels = BTreeMap::new();
            labels.insert("storm.io/component".to_string(), "stormdrive".to_string());
            labels.insert("storm.io/node".to_string(), s.node_name.clone());
            labels.insert("storm.io/shelf".to_string(), key.clone());
            let worst = drives
                .iter()
                .filter_map(|d| d["health"].as_str().map(|h| h.to_string()))
                .max()
                .unwrap_or_else(|| "unknown".into());
            let enclosure = ses.get(&key).map(|r| {
                use crate::ses::{ET_COOLING, ET_POWER_SUPPLY, ET_TEMPERATURE};
                let elems: Vec<Value> = r
                    .elements
                    .iter()
                    .filter(|e| !e.overall)
                    .filter(|e| matches!(e.element_type, ET_COOLING | ET_POWER_SUPPLY | ET_TEMPERATURE))
                    .map(|e| json!({
                        "type": e.type_name, "index": e.index, "name": e.name, "status": e.status,
                        "temperatureC": e.temperature_c, "rpm": e.rpm, "flags": e.flags,
                    }))
                    .collect();
                json!({
                    "status": r.worst(),
                    "paths": r.esps.len(),
                    "maxTemperatureC": r.max_temperature_c(),
                    "powerSupplies": { "ok": r.count(ET_POWER_SUPPLY).0, "total": r.count(ET_POWER_SUPPLY).1 },
                    "fans": { "ok": r.count(ET_COOLING).0, "total": r.count(ET_COOLING).1 },
                    "elements": elems,
                })
            });
            let needs = drives.iter().filter(|d| d["needsReformat"] == json!(true)).count();
            json!({
                "apiVersion": api_version(),
                "kind": "Enclosure",
                "metadata": { "name": key, "uid": key, "labels": labels },
                "spec": {},
                "status": {
                    "shelf": shelf,
                    "hbas": hbas,
                    "driveCount": drives.len(),
                    "drives": drives,
                    "worstHealth": worst,
                    "needsReformat": needs,
                    "enclosure": enclosure,
                },
            })
        })
        .collect()
}

async fn get_enclosure(State(s): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    match all_enclosures(&s).await.into_iter().find(|e| e["metadata"]["name"] == name) {
        Some(e) => Json(e).into_response(),
        None => status_error(StatusCode::NOT_FOUND, "NotFound", format!("enclosures \"{name}\" not found")),
    }
}

// -------------------------------------------------------- list and watch

#[derive(Deserialize, Default)]
struct ListQuery {
    #[serde(default)]
    watch: Option<String>,
    #[serde(default, rename = "labelSelector")]
    label_selector: Option<String>,
}

fn selected(item: &Value, selector: &Option<String>) -> bool {
    let Some(sel) = selector else { return true };
    let labels = &item["metadata"]["labels"];
    sel.split(',').filter(|s| !s.trim().is_empty()).all(|pair| match pair.split_once('=') {
        Some((k, v)) => labels.get(k.trim()).and_then(|x| x.as_str()) == Some(v.trim()),
        None => labels.get(pair.trim()).is_some(),
    })
}

async fn collect(s: &AppState, kind: &str) -> Vec<Value> {
    match kind {
        "Drive" => all_drives(s).await,
        "Enclosure" => all_enclosures(s).await,
        _ => Vec::new(),
    }
}

async fn list_or_watch(s: Arc<AppState>, kind: &'static str, q: ListQuery) -> Response {
    let items: Vec<Value> = collect(&s, kind).await.into_iter().filter(|i| selected(i, &q.label_selector)).collect();
    if !matches!(q.watch.as_deref(), Some("1") | Some("true")) {
        let rv = fingerprint(&Value::Array(items.clone()));
        return Json(json!({
            "apiVersion": api_version(), "kind": format!("{kind}List"),
            "metadata": { "resourceVersion": rv.to_string() }, "items": items,
        }))
        .into_response();
    }
    let selector = q.label_selector.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(64);
    tokio::spawn(async move {
        let mut seen: BTreeMap<String, u64> = BTreeMap::new();
        for it in &items {
            let name = it["metadata"]["name"].as_str().unwrap_or_default().to_string();
            seen.insert(name, fingerprint(it));
            if tx.send(bytes::Bytes::from(format!("{}\n", json!({ "type": "ADDED", "object": it })))).await.is_err() {
                return;
            }
        }
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if tx.is_closed() {
                return;
            }
            let now: Vec<Value> = collect(&s, kind).await.into_iter().filter(|i| selected(i, &selector)).collect();
            let mut present: BTreeMap<String, u64> = BTreeMap::new();
            for it in &now {
                let name = it["metadata"]["name"].as_str().unwrap_or_default().to_string();
                let fp = fingerprint(it);
                present.insert(name.clone(), fp);
                let ev = match seen.get(&name) {
                    None => Some("ADDED"),
                    Some(old) if *old != fp => Some("MODIFIED"),
                    _ => None,
                };
                if let Some(t) = ev {
                    if tx.send(bytes::Bytes::from(format!("{}\n", json!({ "type": t, "object": it })))).await.is_err() {
                        return;
                    }
                }
            }
            for name in seen.keys() {
                if !present.contains_key(name) {
                    let obj = json!({ "apiVersion": api_version(), "kind": kind, "metadata": { "name": name } });
                    if tx.send(bytes::Bytes::from(format!("{}\n", json!({ "type": "DELETED", "object": obj })))).await.is_err() {
                        return;
                    }
                }
            }
            seen = present;
        }
    });
    let stream = futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx)).map(Ok::<_, std::io::Error>);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from_stream(stream))
        .unwrap()
}

use futures_util::StreamExt as _;

async fn list_drives(State(s): State<Arc<AppState>>, Query(q): Query<ListQuery>) -> Response {
    list_or_watch(s, "Drive", q).await
}
async fn list_enclosures(State(s): State<Arc<AppState>>, Query(q): Query<ListQuery>) -> Response {
    list_or_watch(s, "Enclosure", q).await
}

/// Routes only; the caller's `with_state` applies to these too.
pub fn router() -> Router<Arc<AppState>> {
    let gv = format!("/apis/{GROUP}/{VERSION}");
    Router::new()
        .route("/apis", get(apis))
        .route(&format!("/apis/{GROUP}"), get(api_group))
        .route(&gv, get(api_resources))
        .route(&format!("{gv}/drives"), get(list_drives))
        .route(&format!("{gv}/drives/{{name}}"), get(get_drive).patch(patch_drive))
        .route(&format!("{gv}/enclosures"), get(list_enclosures))
        .route(&format!("{gv}/enclosures/{{name}}"), get(get_enclosure))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::{Controller, DriveKind, HealthReport, HealthStatus, Location, Shelf};

    fn drive(name: &str, bay: u32) -> Drive {
        Drive {
            id: DriveId::derive(None, "M", name),
            path: format!("/dev/{name}"),
            name: name.into(),
            paths: vec![format!("/dev/{name}")],
            kind: DriveKind::SasSsd,
            model: "M".into(),
            serial: name.into(),
            firmware: "1".into(),
            wwid: None,
            capacity_bytes: 1 << 30,
            block_size: 512,
            physical_block_size: 512,
            usable: true,
            format: None,
            firmware_update: None,
            location: Location {
                controller: Some(Controller { scsi_host: Some("host3".into()), ..Default::default() }),
                shelf: Some(Shelf { serial: Some("SHELF1".into()), ..Default::default() }),
                bay: Some(bay),
                ..Default::default()
            },
            membership: Membership::Out,
            designation: Designation::None,
            activity: Activity::Idle,
            health: HealthReport { status: Some(HealthStatus::Good), ..Default::default() },
            first_seen: std::time::SystemTime::now(),
            last_seen: std::time::SystemTime::now(),
            pushed_labels: Vec::new(),
            pushed_health: None,
            drain: None,
        }
    }

    #[test]
    fn a_drive_renders_as_a_resource_with_location_labels() {
        let d = drive("sda", 7);
        let v = drive_object(&d, "n1");
        assert_eq!(v["kind"], "Drive");
        assert_eq!(v["apiVersion"], "storage.storm.io/v1");
        assert_eq!(v["metadata"]["name"], d.id.0.to_string());
        assert_eq!(v["metadata"]["labels"]["storm.io/bay"], "7");
        assert_eq!(v["metadata"]["labels"]["storm.io/hba"], "host3");
        assert_eq!(v["metadata"]["labels"]["storm.io/component"], "stormdrive");
        assert_eq!(v["spec"]["fleet"], "out");
        assert_eq!(v["spec"]["drain"], false);
        assert_eq!(v["status"]["health"]["status"], "good");
        assert!(selected(&v, &Some("storm.io/hba=host3".into())));
        assert!(!selected(&v, &Some("storm.io/hba=host9".into())));
    }
}
