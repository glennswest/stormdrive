//! The write gate and the DriveOperation controller (#45) against a stand-in
//! apiserver: the real daemon, a stub that answers TokenReview,
//! SubjectAccessReview and the storage.storm.io objects the way rustkube
//! would, and records what stormdrive writes back.
//!
//! - alice (group storage-admins) may; bob may not; anything else is no one.
//! - Three DriveOperations name this node: one with no requester stamp, one
//!   stamped bob, one stamped alice (selecting a model no drive has); a
//!   fourth names another node and must be left alone.

use std::collections::HashMap;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde_json::{json, Value};

#[derive(Default)]
struct Seen {
    status: HashMap<String, Value>,
    events: Vec<Value>,
    reviews: Vec<Value>,
}

type S = Arc<Mutex<Seen>>;

fn user_of(token: &str) -> Option<Value> {
    match token {
        "alice-token" => Some(json!({ "username": "alice", "groups": ["storage-admins", "system:authenticated"] })),
        "bob-token" => Some(json!({ "username": "bob", "groups": ["system:authenticated"] })),
        _ => None,
    }
}

async fn token_review(Json(b): Json<Value>) -> Json<Value> {
    let t = b["spec"]["token"].as_str().unwrap_or("");
    Json(match user_of(t) {
        Some(u) => json!({ "kind": "TokenReview", "status": { "authenticated": true, "user": u } }),
        None => json!({ "kind": "TokenReview", "status": { "authenticated": false } }),
    })
}

async fn access_review(State(s): State<S>, Json(b): Json<Value>) -> Json<Value> {
    s.lock().unwrap().reviews.push(b.clone());
    let spec = &b["spec"];
    let admin = spec["groups"].as_array().is_some_and(|g| g.iter().any(|x| x == "storage-admins"));
    let ok = admin && spec["resourceAttributes"]["group"] == "storage.storm.io";
    Json(json!({ "kind": "SubjectAccessReview", "status": { "allowed": ok, "reason": if ok { "storage-admin" } else { "no role grants it" } } }))
}

fn op(name: &str, node: &str, requester: Option<(&str, &str)>) -> Value {
    let mut meta = json!({ "name": name, "uid": format!("uid-{name}"), "generation": 1 });
    if let Some((u, g)) = requester {
        meta["annotations"] = json!({ "storage.storm.io/requester": u, "storage.storm.io/requester-groups": g });
    }
    json!({
        "apiVersion": "storage.storm.io/v1", "kind": "DriveOperation", "metadata": meta,
        "spec": { "node": node, "select": { "model": "no-such-model" }, "steps": [{ "op": "format", "blockSize": 4096 }] },
    })
}

async fn operations() -> Json<Value> {
    Json(json!({ "kind": "DriveOperationList", "items": [
        op("unstamped", "harness-node", None),
        op("by-bob", "HARNESS-NODE", Some(("bob", "system:authenticated"))),
        op("by-alice", "harness-node", Some(("alice", "storage-admins,system:authenticated"))),
        op("elsewhere", "other-node", Some(("alice", "storage-admins"))),
    ] }))
}

async fn op_status(State(s): State<S>, Path(name): Path<String>, Json(b): Json<Value>) -> Json<Value> {
    s.lock().unwrap().status.insert(name, b["status"].clone());
    Json(json!({}))
}

async fn event(State(s): State<S>, Json(b): Json<Value>) -> Json<Value> {
    s.lock().unwrap().events.push(b);
    Json(json!({}))
}

async fn stub() -> (String, S) {
    let seen: S = Arc::default();
    let app = Router::new()
        .route("/apis/authentication.k8s.io/v1/tokenreviews", post(token_review))
        .route("/apis/authorization.k8s.io/v1/subjectaccessreviews", post(access_review))
        .route("/apis/storage.storm.io/v1/drives", get(|| async { Json(json!({ "items": [] })) }))
        .route("/apis/storage.storm.io/v1/driveoperations", get(operations))
        .route("/apis/storage.storm.io/v1/driveoperations/{name}/status", patch(op_status))
        .route("/api/v1/namespaces/default/events", post(event))
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (base, seen)
}

struct Daemon {
    child: Child,
    dir: PathBuf,
    base: String,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn start(apiserver: &str) -> Daemon {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let dir = std::env::temp_dir().join(format!("stormdrive-kube-{}-{port}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("stormdrive.toml");
    std::fs::write(
        &cfg,
        format!(
            "node_name = \"harness-node\"\n[discovery]\ninclude = [\"stormdrive-harness-no-such-disk\"]\n[stormblock]\nenabled = false\n[kubernetes]\napi_url = \"{apiserver}\"\ninterval_secs = 1\n"
        ),
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_stormdrive"))
        .args(["--config", cfg.to_str().unwrap(), "--listen", &format!("127.0.0.1:{port}")])
        .args(["--data-dir", dir.to_str().unwrap()])
        .env("RUST_LOG", "warn")
        .env_remove("STORMDRIVE_ADMIN_TOKEN")
        .env_remove("STORMDRIVE_KUBE_API")
        .stdout(Stdio::null())
        .spawn()
        .expect("start stormdrive");
    let d = Daemon { child, dir, base: format!("http://127.0.0.1:{port}") };
    for _ in 0..100 {
        if reqwest::get(format!("{}/api/v1/health", d.base)).await.is_ok_and(|r| r.status().is_success()) {
            return d;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("stormdrive did not answer on {}", d.base);
}

async fn write(d: &Daemon, path: &str, bearer: Option<&str>) -> (u16, Value) {
    let mut r = reqwest::Client::new().post(format!("{}{path}", d.base));
    if let Some(b) = bearer {
        r = r.bearer_auth(b);
    }
    let r = r.send().await.unwrap();
    let code = r.status().as_u16();
    (code, r.json().await.unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_and_operations_are_decided_by_the_apiserver() {
    let (api, seen) = stub().await;
    let d = start(&api).await;

    let h: Value = reqwest::get(format!("{}/api/v1/health", d.base)).await.unwrap().json().await.unwrap();
    assert_eq!(h["writes"], json!({ "gate": "enforce", "apiserver": true, "admin_token": false }));

    // The gate.
    let path = "/api/v1/drives/no-such-drive/designation/spare";
    let (c, b) = write(&d, path, None).await;
    assert_eq!((c, b["code"].as_str()), (401, Some("unauthorized")), "{b}");
    assert_eq!(write(&d, path, Some("forged")).await.0, 401);
    let (c, b) = write(&d, path, Some("bob-token")).await;
    assert_eq!((c, b["code"].as_str()), (403, Some("forbidden")), "{b}");
    assert!(b["error"].as_str().unwrap().contains("bob may not update drives"), "{b}");
    assert_eq!(write(&d, path, Some("alice-token")).await.0, 404, "alice gets through the gate to the 404");
    assert_eq!(write(&d, "/api/v1/format", Some("bob-token")).await.0, 403);
    {
        let s = seen.lock().unwrap();
        let ops: Vec<&Value> = s.reviews.iter().map(|r| &r["spec"]["resourceAttributes"]).collect();
        assert!(ops.iter().any(|a| a["resource"] == "drives" && a["verb"] == "update" && a["name"] == "no-such-drive"));
        assert!(ops.iter().any(|a| a["resource"] == "driveoperations" && a["verb"] == "create"));
    }
    // A dry run is a read.
    let r = reqwest::Client::new()
        .post(format!("{}/api/v1/worker/jobs", d.base))
        .json(&json!({ "select": { "model": "x" }, "steps": [{ "op": "partition" }], "dry_run": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400, "a dry run reaches the worker (and matches no drive)");

    // The controller.
    let mut statuses = HashMap::new();
    for _ in 0..100 {
        statuses = seen.lock().unwrap().status.clone();
        if statuses.len() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let phase = |n: &str| (statuses[n]["phase"].as_str().unwrap_or("").to_string(), statuses[n]["message"].as_str().unwrap_or("").to_string());
    let (p, m) = phase("unstamped");
    assert_eq!(p, "Refused");
    assert!(m.contains("no requester stamped"), "{m}");
    let (p, m) = phase("by-bob");
    assert_eq!(p, "Refused");
    assert!(m.contains("bob may no longer create driveoperations"), "{m}");
    let (p, m) = phase("by-alice");
    assert_eq!(p, "Refused", "alice passes the re-check; the worker refuses the selection");
    assert!(m.contains("matches no drive"), "{m}");
    assert_eq!(statuses["by-alice"]["requester"], "kubernetes:alice");
    assert!(!statuses.contains_key("elsewhere"), "another node's operation is not ours");

    let s = seen.lock().unwrap();
    assert!(s.events.iter().any(|e| e["involvedObject"]["name"] == "by-bob" && e["reason"] == "Refused" && e["type"] == "Warning"));
    assert!(s.events.iter().all(|e| e["involvedObject"]["kind"] == "DriveOperation"));
    drop(s);

    let audit = std::fs::read_to_string(d.dir.join("audit.log")).unwrap();
    assert!(audit.lines().any(|l| l.contains("\"who\":\"kubernetes:bob\"") && l.contains("\"decision\":\"refused\"")), "{audit}");
    assert!(audit.lines().any(|l| l.contains("\"who\":\"kubernetes:alice\"") && l.contains("\"decision\":\"allowed\"") && l.contains("\"status\":404")), "{audit}");
    assert!(audit.lines().any(|l| l.contains("by-bob")), "the controller's refusal is audited too");
}
