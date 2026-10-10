//! The test container's suites (test/, #11) against the real daemon, on the
//! build box, on every sc-build: this binary on a free local port, with
//! stormblock off and discovery matching no disk (the build box's drives are
//! not ours to test, and a harness must not depend on them). What it proves
//! is the API contract the suites check — errors, refusals, the feed, kube,
//! placement, a long wave — and that the suites report pass/skip
//! correctly and exit 0. On a test machine the same suites meet real drives.
//! Served over TLS from a CA minted here (#19), as on a node.

mod common;

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use stormdrive_test::env::{Env, Tls};
use stormdrive_test::report::Report;

struct Daemon {
    child: Child,
    dir: PathBuf,
    base: String,
    ca: String,
    client_pair: (String, String),
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn start() -> Daemon {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let dir = std::env::temp_dir().join(format!("stormdrive-harness-{}-{port}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("stormdrive.toml");
    let ca = common::Ca::new("harness node CA");
    let (ca_file, crt, key) = common::node_certs(&dir, &ca);
    // A system-data directory (#64): the short suite's `system-data` check
    // reads the history and this machine's assets back.
    let sysdata = dir.join("system-data");
    std::fs::create_dir_all(&sysdata).unwrap();
    std::fs::write(
        &cfg,
        format!(
            "[discovery]\ninclude = [\"stormdrive-harness-no-such-disk\"]\n[stormblock]\nenabled = false\n[api]\nadmin_token = \"harness-admin\"\n{}[history]\ndir = \"{}\"\n",
            common::api_toml(&ca_file, &crt, &key),
            sysdata.display()
        ),
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_stormdrive"))
        .args(["--config", cfg.to_str().unwrap(), "--listen", &format!("127.0.0.1:{port}")])
        .args(["--data-dir", dir.to_str().unwrap()])
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .spawn()
        .expect("start stormdrive");
    let base = format!("https://127.0.0.1:{port}");
    let client_pair = ca.issue("stormdrive-test", &["storm:tests"], false);
    let http = common::client(&ca, None);
    let d = Daemon { child, dir, base, ca: ca.pem(), client_pair };
    for _ in 0..100 {
        if http.get(format!("{}/api/v1/health", d.base)).send().await.is_ok_and(|r| r.status().is_success()) {
            return d;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("stormdrive did not answer on {}", d.base);
}

fn env(d: &Daemon, suite: &str, secs: u64) -> Env {
    let (c, k) = &d.client_pair;
    Env {
        base: Some(d.base.clone()),
        base_from_node: false,
        // The node CA, and a client pair from it: reads by certificate,
        // writes by the admin token (a bearer wins over the certificate).
        tls: Tls { ca: Some(d.ca.clone().into_bytes()), identity: Some(format!("{c}\n{k}").into_bytes()) },
        read_token: None,
        run_id: format!("harness-{suite}"),
        timeout: Duration::from_secs(secs),
        results: d.dir.join("results"),
        // The node-local admin token (#45): the suites' writes get past the
        // gate, and writes-need-storage-admin checks the gate itself.
        token: Some("harness-admin".into()),
        wave_max: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_suite_passes_against_the_daemon() {
    let d = start().await;
    for (suite, secs) in [("short", 120), ("medium", 300), ("long", 20)] {
        let mut r = Report::new(&d.dir.join("results"));
        let code = stormdrive_test::run(suite, &env(&d, suite, secs), &mut r).await;
        r.summary();
        assert_eq!((code, r.fail), (0, 0), "{suite}: exit {code}, {} failed (see the JSON lines above)", r.fail);
        assert!(r.pass >= 3, "{suite}: only {} passed", r.pass);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_node_is_could_not_run() {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let dir = std::env::temp_dir().join(format!("stormdrive-harness-down-{port}"));
    let env = Env {
        base: Some(format!("https://127.0.0.1:{port}")),
        base_from_node: false,
        tls: Tls::default(),
        read_token: None,
        run_id: "harness-down".into(),
        timeout: Duration::from_secs(30),
        results: dir.clone(),
        token: None,
        wave_max: None,
    };
    let mut r = Report::new(&dir);
    assert_eq!(stormdrive_test::run("short", &env, &mut r).await, 2);
    let _ = std::fs::remove_dir_all(dir);
}
