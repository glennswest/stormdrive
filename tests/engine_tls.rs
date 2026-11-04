//! The engine client against an engine serving TLS from a test CA (#101,
//! stormcos#374): verified against `stormblock.ca_file` and nothing else. The
//! stand-in takes TLS and plain on one port, as the engine does while
//! `tls_mode = "optional"` (stormblock#525).

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use stormdrive::config::StormBlockConfig;
use stormdrive::stormblock::{is_infra, StormBlockClient};
use stormdrive::tls;

struct Engine {
    port: u16,
    /// Requests that reached a handler: a refused handshake never does, so
    /// the token never left.
    drives: Arc<AtomicU32>,
    downgraded: Arc<AtomicU32>,
}

async fn engine(ca: &common::Ca, dir: &std::path::Path) -> Engine {
    let (crt, key) = ca.issue("stormblock", &[], true);
    let (crt_file, key_file) = (dir.join("stormblock.crt"), dir.join("stormblock.key"));
    std::fs::write(&crt_file, crt).unwrap();
    std::fs::write(&key_file, key).unwrap();
    let serving = tls::ServingCert::new(crt_file, key_file);
    let config = tls::server_config(serving, &[]).unwrap();
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = tcp.local_addr().unwrap().port();

    let (drives, downgraded) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
    let (d, g) = (drives.clone(), downgraded.clone());
    let app = Router::new()
        .route(
            "/api/v1/drives",
            get(move |h: HeaderMap| {
                let d = d.clone();
                async move {
                    d.fetch_add(1, Ordering::SeqCst);
                    if h.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer node-token") {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    Json(json!({ "items": [{ "path": "/dev/sdz" }] })).into_response()
                }
            }),
        )
        .route(
            "/api/v1/slabs",
            get(move || async move { Redirect::temporary(&format!("http://127.0.0.1:{port}/plain")) }),
        )
        .route(
            "/plain",
            get(move || {
                let g = g.clone();
                async move {
                    g.fetch_add(1, Ordering::SeqCst);
                    Json(json!({ "items": [] }))
                }
            }),
        );
    let listener = tls::Listener::new(tcp, config).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Engine { port, drives, downgraded }
}

fn client(url: String, ca_file: &std::path::Path) -> StormBlockClient {
    StormBlockClient::new(StormBlockConfig {
        url,
        ca_file: ca_file.to_str().unwrap().to_string(),
        api_token: "node-token".into(),
        ..Default::default()
    })
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("stormdrive-engine-tls-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn the_engine_client_verifies_https_against_the_ca_file_only() {
    let dir = scratch();
    let (ca, other) = (common::Ca::new("test node CA"), common::Ca::new("another CA"));
    let e = engine(&ca, &dir).await;
    let https = format!("https://127.0.0.1:{}", e.port);
    let (right, wrong, shifting) = (dir.join("ca.crt"), dir.join("other.crt"), dir.join("shifting.crt"));
    std::fs::write(&right, ca.pem()).unwrap();
    std::fs::write(&wrong, other.pem()).unwrap();

    // The engine's certificate from the CA we were given: works, token sent.
    let drives = client(https.clone(), &right).list_drives().await.unwrap();
    assert_eq!(drives[0]["path"], "/dev/sdz");

    // http:// keeps working, and does not need the CA file.
    let plain = format!("http://127.0.0.1:{}", e.port);
    assert_eq!(client(plain, &dir.join("no-such-ca")).list_drives().await.unwrap().len(), 1);

    // A certificate from another CA: refused in the handshake, so nothing
    // reached the engine; told apart as infrastructure, naming the reason.
    let before = e.drives.load(Ordering::SeqCst);
    let err = client(https.clone(), &wrong).list_drives().await.unwrap_err();
    assert!(is_infra(&err), "{err:#}");
    assert!(format!("{err:#}").to_lowercase().contains("certificate"), "{err:#}");
    assert_eq!(e.drives.load(Ordering::SeqCst), before, "the token must not reach an untrusted engine");

    // No CA named: refused, never a fallback to anything else.
    let none = StormBlockClient::new(StormBlockConfig {
        url: https.clone(),
        ca_file: String::new(),
        api_token: "node-token".into(),
        ..Default::default()
    });
    assert!(none.list_drives().await.is_err());
    assert_eq!(e.drives.load(Ordering::SeqCst), before);

    // One client, its CA file changing under it, no restart:
    let c = client(https.clone(), &shifting);
    assert!(c.list_drives().await.is_err(), "no file yet");
    assert_eq!(e.drives.load(Ordering::SeqCst), before);
    std::fs::write(&shifting, ca.pem()).unwrap();
    c.list_drives().await.expect("the CA appeared");
    // gone, or not a CA: the last good one holds;
    std::fs::remove_file(&shifting).unwrap();
    c.list_drives().await.expect("the file went away");
    std::fs::write(&shifting, "not a certificate").unwrap();
    c.list_drives().await.expect("the file is not a CA");
    // a new CA replaces the old one: the old one is no longer trusted.
    std::fs::write(&shifting, other.pem()).unwrap();
    assert!(c.list_drives().await.is_err(), "trust follows the file");
    std::fs::write(&shifting, ca.pem()).unwrap();
    c.list_drives().await.expect("back to the engine's CA");

    // An https engine cannot send us down to http.
    let c = client(https, &right);
    assert!(c.list_slabs().await.is_err());
    assert_eq!(e.downgraded.load(Ordering::SeqCst), 0, "followed a redirect to http");

    let _ = std::fs::remove_dir_all(&dir);
}
