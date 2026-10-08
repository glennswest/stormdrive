//! stormdrive's test container (#11), per stormcentral
//! `docs/test-standard.md`: `/test short|medium|long`.
//!
//! The suites drive the stormdrive running on the node under test
//! (`STORM_NODE:9092`) through its REST API. Exit 0 when every test passed,
//! 1 when one failed, 2 when the run could not happen. One JSON object per
//! test on stdout, a summary last. They never do anything destructive to a
//! drive: see `pick` for what may be touched and how.

pub mod api;
pub mod env;
pub mod long;
pub mod medium;
pub mod pick;
pub mod report;
pub mod short;

use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

use api::Api;
use env::Env;
use report::{Report, Why};

/// What this run changed and must put back: settings it set and tests it
/// started. Kept outside the suites so a suite that times out mid-change is
/// still undone.
#[derive(Default)]
pub struct Undo {
    /// (drive id, original designation)
    pub designation: Vec<(String, String)>,
    /// (drive id, original overcommit JSON)
    pub overcommit: Vec<(String, Value)>,
    /// Drives this run started a test on.
    pub tests: Vec<String>,
}

pub static UNDO: Mutex<Undo> = Mutex::new(Undo { designation: Vec::new(), overcommit: Vec::new(), tests: Vec::new() });

/// Put back everything in `UNDO`: cancel our tests, restore settings.
pub async fn cleanup(api: &Api) {
    let undo = std::mem::take(&mut *UNDO.lock().unwrap());
    for id in undo.tests {
        let _ = api.post_empty(&format!("api/v1/drives/{id}/test/cancel")).await;
    }
    for (id, v) in undo.designation.into_iter().rev() {
        let _ = api.post(&format!("api/v1/drives/{id}/designation"), json!({ "designation": v })).await;
    }
    for (id, v) in undo.overcommit.into_iter().rev() {
        let body = json!({ "enabled": v["enabled"].as_bool().unwrap_or(false), "ratio": v["ratio"] });
        let _ = api.post(&format!("api/v1/drives/{id}/overcommit"), body).await;
    }
}

/// Every drive the node knows, as `/api/v1/drives` lists it.
pub async fn drives(api: &Api) -> Result<Vec<Value>, Why> {
    let v = api.get("api/v1/drives").await?.json("GET /api/v1/drives")?;
    v["drives"].as_array().cloned().ok_or_else(|| Why::Fail("no drives array".into()))
}

pub async fn drive(api: &Api, id: &str) -> Result<Value, Why> {
    api.get(&format!("api/v1/drives/{id}")).await?.json("GET drive")
}

pub async fn latest_seq(api: &Api) -> Result<u64, Why> {
    let v = api.get("api/v1/events").await?.json("GET /api/v1/events")?;
    Ok(v["latest_seq"].as_u64().unwrap_or(0))
}

/// Poll a drive until its test is no longer running; the finished run.
pub async fn wait_test(api: &Api, id: &str, within: Duration) -> Result<Value, Why> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let v = api.get(&format!("api/v1/drives/{id}/test")).await?.json("GET test")?;
        let t = &v["test"];
        if !t.is_null() && t["state"] != "running" {
            return Ok(t.clone());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Why::Fail(format!("test on {id} still running after {}s", within.as_secs())));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Run one suite against `env.base` and report it; the exit code.
pub async fn run(suite: &str, env: &Env, r: &mut Report) -> i32 {
    let Some(base) = env.base.clone() else {
        eprintln!("could not run: neither STORM_NODE nor STORM_STORMDRIVE_URL is set");
        return 2;
    };
    let mut api = Api::new(&base, &env.tls, env.token.clone(), env.read_token.clone());
    // A node still on plain HTTP (no serving pair yet, `allow_anonymous`,
    // #19): found from STORM_NODE's https:// by asking health over http://.
    if base.starts_with("https://") && env.base_from_node && api.get("api/v1/health").await.is_err() {
        let plain = format!("http://{}", &base["https://".len()..]);
        let p = Api::new(&plain, &env.tls, env.token.clone(), env.read_token.clone());
        if p.get("api/v1/health").await.is_ok_and(|r| r.ok()) {
            eprintln!("note: {base} does not speak TLS; using {plain} (stormdrive before #19, or no serving pair)");
            api = p;
        }
    }
    let limit = env.timeout + Duration::from_secs(30);
    let result = match suite {
        "short" => tokio::time::timeout(limit, short::run(env, &api, r)).await,
        "medium" => tokio::time::timeout(limit, medium::run(env, &api, r)).await,
        _ => tokio::time::timeout(limit, long::run(env, &api, r)).await,
    };
    let code = match result {
        Ok(Ok(())) => r.exit_code(),
        Ok(Err(e)) => {
            eprintln!("could not run: {e}");
            2
        }
        Err(_) => {
            r.record("suite-timeout", &Err(Why::Fail(format!("{suite} ran past {}s", env.timeout.as_secs()))), 0);
            1
        }
    };
    cleanup(&api).await;
    code
}

/// The first test of every suite: the node's stormdrive answers. Not
/// answering is "could not run" (exit 2), never a pass or a fail.
pub async fn api_up(api: &Api, r: &mut Report) -> Result<(), String> {
    let t = std::time::Instant::now();
    let reply = api
        .get("api/v1/health")
        .await
        .map_err(|e| format!("stormdrive at {} did not answer: {}", api.url(""), e.message()))?;
    let outcome = (|| {
        let v = reply.json("GET /api/v1/health")?;
        report::ensure(v["status"] == "ok", format!("status {}", v["status"]))?;
        let version = v["version"].as_str().unwrap_or_default().to_string();
        let parsed = api::parse_version(&version).ok_or_else(|| Why::Fail(format!("version {version:?}")))?;
        *api.version.lock().unwrap() = Some(parsed);
        let node = v["node"].as_str().unwrap_or_default();
        report::ensure(!node.is_empty(), "no node name")?;
        Ok(format!("stormdrive {version} on {node}"))
    })();
    r.record("api-up", &outcome, t.elapsed().as_millis());
    Ok(())
}
