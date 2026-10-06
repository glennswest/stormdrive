//! :9092's transport and read gate (#19) on the real daemon: TLS from a
//! node CA minted here, plain HTTP for health only, and every other request
//! with a credential — a node-CA client certificate, the admin token, or a
//! bearer a stand-in apiserver reviews.
//!
//! - alice-token: alice (storage-viewers) may get, not create;
//! - bob-token: bob, who may do nothing;
//! - a client certificate CN=carol O=storage-admins may write; one from
//!   another CA never finishes the handshake.

mod common;

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

async fn token_review(Json(b): Json<Value>) -> Json<Value> {
    let user = match b["spec"]["token"].as_str().unwrap_or("") {
        "alice-token" => Some(json!({ "username": "alice", "groups": ["storage-viewers", "system:authenticated"] })),
        "bob-token" => Some(json!({ "username": "bob", "groups": ["system:authenticated"] })),
        _ => None,
    };
    Json(match user {
        Some(u) => json!({ "kind": "TokenReview", "status": { "authenticated": true, "user": u } }),
        None => json!({ "kind": "TokenReview", "status": { "authenticated": false } }),
    })
}

async fn access_review(Json(b): Json<Value>) -> Json<Value> {
    let spec = &b["spec"];
    let in_group = |g: &str| spec["groups"].as_array().is_some_and(|gs| gs.iter().any(|x| x == g));
    let verb = spec["resourceAttributes"]["verb"].as_str().unwrap_or("");
    let ours = spec["resourceAttributes"]["group"] == "storage.storm.io";
    let ok = ours && (in_group("storage-admins") || (in_group("storage-viewers") && matches!(verb, "get" | "list" | "watch")));
    Json(json!({ "kind": "SubjectAccessReview", "status": { "allowed": ok, "reason": if ok { "rbac" } else { "no role grants it" } } }))
}

async fn apiserver() -> String {
    let app = Router::new()
        .route("/apis/authentication.k8s.io/v1/tokenreviews", post(token_review))
        .route("/apis/authorization.k8s.io/v1/subjectaccessreviews", post(access_review));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    base
}

struct Daemon {
    child: Child,
    dir: PathBuf,
    port: u16,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Daemon {
    fn https(&self, path: &str) -> String {
        format!("https://127.0.0.1:{}{path}", self.port)
    }
    fn http(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

/// stormdrive with its pair at `crt`/`key` (which need not exist yet).
async fn start(tag: &str, api: &str, ca: &Path, crt: &Path, key: &Path, dir: PathBuf) -> Daemon {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cfg = dir.join("stormdrive.toml");
    std::fs::write(
        &cfg,
        format!(
            "node_name = \"tls-{tag}\"\n[discovery]\ninclude = [\"stormdrive-harness-no-such-disk\"]\n[stormblock]\nenabled = false\n\
             [kubernetes]\napi_url = \"{api}\"\ncontroller = false\n[api]\nadmin_token = \"root-token\"\n{}",
            common::api_toml(ca, crt, key)
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
    let d = Daemon { child, dir, port };
    let plain = reqwest::Client::new();
    for _ in 0..100 {
        if plain.get(d.http("/api/v1/health")).send().await.is_ok_and(|r| r.status().is_success()) {
            return d;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("stormdrive did not answer on {}", d.http("/api/v1/health"));
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("stormdrive-tls-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn status(c: &reqwest::Client, url: &str, bearer: Option<&str>) -> u16 {
    let mut r = c.get(url);
    if let Some(b) = bearer {
        r = r.bearer_auth(b);
    }
    r.send().await.unwrap_or_else(|e| panic!("GET {url}: {e}")).status().as_u16()
}

async fn post_status(c: &reqwest::Client, url: &str, bearer: Option<&str>) -> u16 {
    let mut r = c.post(url);
    if let Some(b) = bearer {
        r = r.bearer_auth(b);
    }
    r.send().await.unwrap_or_else(|e| panic!("POST {url}: {e}")).status().as_u16()
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_answers_anonymously_but_health() {
    let api = apiserver().await;
    let dir = scratch("gate");
    let ca = common::Ca::new("test node CA");
    let (ca_file, crt, key) = common::node_certs(&dir, &ca);
    let d = start("gate", &api, &ca_file, &crt, &key, dir).await;

    // Plain HTTP: health, and nothing else.
    let plain = reqwest::Client::new();
    assert_eq!(status(&plain, &d.http("/api/v1/health"), None).await, 200);
    assert_eq!(status(&plain, &d.http("/healthz"), None).await, 200);
    for p in ["/api/v1/drives", "/metrics", "/", "/assets/app.js"] {
        assert_eq!(status(&plain, &d.http(p), Some("root-token")).await, 403, "plain {p}");
    }
    let r = plain.get(d.http("/api/v1/drives")).send().await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["code"], "tls_required");
    let h: Value = plain.get(d.http("/api/v1/health")).send().await.unwrap().json().await.unwrap();
    assert_eq!(h["reads"]["anonymous"], false);

    // TLS, no credential: health, the page's code, and a 401 page that signs in.
    let anon = common::client(&ca, None);
    assert_eq!(status(&anon, &d.https("/api/v1/health"), None).await, 200);
    assert_eq!(status(&anon, &d.https("/assets/app.js"), None).await, 200);
    let shell = anon.get(d.https("/")).send().await.unwrap();
    assert_eq!(shell.status().as_u16(), 401);
    assert!(shell.headers()["content-type"].to_str().unwrap().starts_with("text/html"));
    for p in ["/api/v1/drives", "/metrics", "/api/v1/placement", "/api/v1/components", "/api/v1/worker/jobs", "/apis/storage.storm.io/v1/drives", "/api/v1/summary"] {
        let r = anon.get(d.https(p)).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 401, "anonymous {p}");
        assert_eq!(r.json::<Value>().await.unwrap()["code"], "unauthorized", "{p}");
    }

    // The admin token reads and writes (the write reaches the handler: 404).
    assert_eq!(status(&anon, &d.https("/api/v1/drives"), Some("root-token")).await, 200);
    assert_eq!(post_status(&anon, &d.https("/api/v1/drives/nope/designation/spare"), Some("root-token")).await, 404);

    // Bearers the apiserver reviews: a viewer reads, and may not write.
    assert_eq!(status(&anon, &d.https("/api/v1/drives"), Some("alice-token")).await, 200);
    assert_eq!(status(&anon, &d.https("/metrics"), Some("alice-token")).await, 200);
    assert_eq!(status(&anon, &d.https("/"), Some("alice-token")).await, 200);
    assert_eq!(post_status(&anon, &d.https("/api/v1/drives/nope/designation/spare"), Some("alice-token")).await, 403);
    assert_eq!(status(&anon, &d.https("/api/v1/drives"), Some("bob-token")).await, 403);
    assert_eq!(status(&anon, &d.https("/api/v1/drives"), Some("made-up")).await, 401);
    // A bad bearer on the page is the API's 401, not the sign-in page.
    let r = anon.get(d.https("/")).bearer_auth("made-up").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 401);
    assert_eq!(r.json::<Value>().await.unwrap()["code"], "unauthorized");

    // A node-CA client certificate: reads; writes as its user (SAR).
    let console = common::client(&ca, Some(&ca.issue("stormconsole", &["storm:services"], false)));
    assert_eq!(status(&console, &d.https("/api/v1/drives"), None).await, 200);
    assert_eq!(status(&console, &d.https("/metrics"), None).await, 200);
    assert_eq!(post_status(&console, &d.https("/api/v1/drives/nope/designation/spare"), None).await, 403);
    let carol = common::client(&ca, Some(&ca.issue("carol", &["storage-admins"], false)));
    assert_eq!(post_status(&carol, &d.https("/api/v1/drives/nope/designation/spare"), None).await, 404);
    let audit = std::fs::read_to_string(d.dir.join("audit.log")).unwrap();
    assert!(audit.lines().any(|l| l.contains("\"who\":\"cert:carol\"") && l.contains("\"decision\":\"allowed\"")), "{audit}");
    assert!(audit.lines().any(|l| l.contains("\"who\":\"cert:stormconsole\"") && l.contains("\"decision\":\"refused\"")), "{audit}");

    // A certificate from another CA never gets a connection.
    let other = common::Ca::new("not the node CA");
    let stranger = common::client(&ca, Some(&other.issue("mallory", &["storage-admins"], false)));
    assert!(stranger.get(d.https("/api/v1/drives")).send().await.is_err(), "a foreign client certificate was accepted");
    // Nor does a client that does not trust the node CA (the server is checked).
    let naive = common::client(&other, None);
    assert!(naive.get(d.https("/api/v1/health")).send().await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pair_that_appears_later_is_served_without_a_restart() {
    let api = apiserver().await;
    let dir = scratch("late");
    let ca = common::Ca::new("late node CA");
    let (ca_file, crt, key) = (dir.join("ca.crt"), dir.join("stormdrive.crt"), dir.join("stormdrive.key"));
    std::fs::write(&ca_file, ca.pem()).unwrap();
    let d = start("late", &api, &ca_file, &crt, &key, dir).await;
    let c = common::client(&ca, None);
    // No pair: the handshake fails, plain health still answers.
    assert!(c.get(d.https("/api/v1/health")).send().await.is_err());
    // stormcert-agent writes it: the next handshake has it.
    let (pem, k) = ca.issue("stormdrive", &[], true);
    std::fs::write(&key, k).unwrap();
    std::fs::write(&crt, pem).unwrap();
    assert_eq!(status(&c, &d.https("/api/v1/drives"), Some("root-token")).await, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn allow_anonymous_is_the_old_behaviour_with_credentials_still_checked() {
    let api = apiserver().await;
    let dir = scratch("anon");
    let ca = common::Ca::new("anon node CA");
    let (ca_file, crt, key) = common::node_certs(&dir, &ca);
    // The same daemon with the transition flag.
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cfg = dir.join("stormdrive.toml");
    std::fs::write(
        &cfg,
        format!(
            "[discovery]\ninclude = [\"stormdrive-harness-no-such-disk\"]\n[stormblock]\nenabled = false\n\
             [kubernetes]\napi_url = \"{api}\"\ncontroller = false\n[api]\nallow_anonymous = true\n{}",
            common::api_toml(&ca_file, &crt, &key)
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
        .unwrap();
    let d = Daemon { child, dir, port };
    let plain = reqwest::Client::new();
    let mut up = false;
    for _ in 0..100 {
        if plain.get(d.http("/api/v1/health")).send().await.is_ok_and(|r| r.status().is_success()) {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(up);
    let tls = common::client(&ca, None);
    for c in [&plain, &tls] {
        let base = if std::ptr::eq(c, &plain) { d.http("") } else { d.https("") };
        assert_eq!(status(c, &format!("{base}/api/v1/drives"), None).await, 200);
        assert_eq!(status(c, &format!("{base}/"), None).await, 200);
        assert_eq!(status(c, &format!("{base}/api/v1/drives"), Some("made-up")).await, 401);
        assert_eq!(status(c, &format!("{base}/api/v1/drives"), Some("bob-token")).await, 403);
        // Writes keep the #45 gate.
        assert_eq!(post_status(c, &format!("{base}/api/v1/drives/nope/designation/spare"), None).await, 401);
    }
    let h: Value = plain.get(d.http("/api/v1/health")).send().await.unwrap().json().await.unwrap();
    assert_eq!(h["reads"]["anonymous"], true);
}
