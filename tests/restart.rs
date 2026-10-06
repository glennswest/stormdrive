//! #39: a restart in the middle of a format, a test or a firmware update
//! that was not a worker job. The real daemon starts on an inventory.json
//! whose drives read busy; afterwards none may still read busy, the
//! records say what happened, and each drive has a warning event.
//!
//! The drives do not exist on the build box, so the SCSI format re-attach
//! cannot open its device and ends `failed` (after a restart) — the path
//! a drive that went away takes.

use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

fn drive(id: &str, name: &str, kind: &str, activity: &str, extra: Value) -> Value {
    let mut d = json!({
        "id": id, "path": format!("/dev/{name}"), "name": name, "paths": [format!("/dev/{name}")],
        "kind": kind, "model": "M", "serial": name, "firmware": "N003", "wwid": null,
        "capacity_bytes": 1_000_000_000u64, "block_size": 520, "activity": activity,
        "first_seen": {"secs_since_epoch": 0, "nanos_since_epoch": 0},
        "last_seen": {"secs_since_epoch": 0, "nanos_since_epoch": 0},
    });
    for (k, v) in extra.as_object().unwrap() {
        d[k] = v.clone();
    }
    d
}

const SAS_FMT: &str = "00000000-0000-4000-8000-000000000001";
const NVME_FMT: &str = "00000000-0000-4000-8000-000000000002";
const TEST: &str = "00000000-0000-4000-8000-000000000003";
const FW: &str = "00000000-0000-4000-8000-000000000004";
const DRAIN: &str = "00000000-0000-4000-8000-000000000005";

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_leaves_no_drive_stuck_busy() {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let dir = std::env::temp_dir().join(format!("stormdrive-restart-{}-{port}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let fmt = json!({"format": {"from_block_size": 520, "to_block_size": 4096, "state": "running", "started": null, "finished": null}});
    let drives = [
        drive(SAS_FMT, "sdzz1", "sas_hdd", "formatting", fmt.clone()),
        drive(NVME_FMT, "nvme99n1", "nvme_ssd", "formatting", fmt),
        drive(TEST, "sdzz3", "sas_hdd", "testing", json!({})),
        drive(FW, "sdzz4", "sas_hdd", "updating_firmware", json!({"firmware_update": {
            "image": "x.lod", "from_version": "N003", "to_version": null, "state": "running", "started": null, "finished": null}})),
        drive(DRAIN, "sdzz5", "sas_hdd", "draining", json!({})),
    ];
    let inv = json!({"drives": drives.iter().map(|d| (d["id"].as_str().unwrap().to_string(), d.clone())).collect::<serde_json::Map<_, _>>()});
    std::fs::write(dir.join("inventory.json"), serde_json::to_vec(&inv).unwrap()).unwrap();
    let cfg = dir.join("stormdrive.toml");
    std::fs::write(&cfg, "node_name = \"harness-node\"\n[discovery]\ninclude = [\"stormdrive-harness-no-such-disk\"]\n[stormblock]\nenabled = false\n").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_stormdrive"))
        .args(["--config", cfg.to_str().unwrap(), "--listen", &format!("127.0.0.1:{port}")])
        .args(["--data-dir", dir.to_str().unwrap()])
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .spawn()
        .expect("start stormdrive");
    let base = format!("http://127.0.0.1:{port}");

    let get = |p: String| async move { reqwest::get(p).await.ok()?.json::<Value>().await.ok() };
    let mut last = Value::Null;
    let mut ok = false;
    // The re-attached format fails on its own task; give it a moment.
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let Some(sas) = get(format!("{base}/api/v1/drives/{SAS_FMT}")).await else { continue };
        last = sas.clone();
        if sas["format"]["state"] == "failed" {
            ok = true;
            break;
        }
    }
    assert!(ok, "the re-attached SCSI format never finished: {last}");

    let mut busy = vec![];
    for id in [SAS_FMT, NVME_FMT, TEST, FW, DRAIN] {
        let d = get(format!("{base}/api/v1/drives/{id}")).await.expect(id);
        let a = d["activity"].as_str().unwrap_or("").to_string();
        if ["formatting", "testing", "updating_firmware", "sanitizing"].contains(&a.as_str()) {
            busy.push(format!("{id}: {a}"));
        }
        match id {
            SAS_FMT => assert!(d["format"]["error"].as_str().unwrap().contains("after a restart"), "{d}"),
            NVME_FMT => assert_eq!(d["format"]["state"], "interrupted", "{d}"),
            FW => assert_eq!(d["firmware_update"]["state"], "interrupted", "{d}"),
            // A drain is the fleet loop's to pick up again; missing (no
            // such device here) is the monitor's word, never "idle".
            DRAIN => assert!(["draining", "missing"].contains(&a.as_str()), "{d}"),
            _ => {}
        }
    }
    let events = get(format!("{base}/api/v1/events")).await.unwrap();
    let kinds: Vec<(String, String)> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["drive_id"].as_str().unwrap_or("").to_string(), e["kind"].as_str().unwrap_or("").to_string()))
        .collect();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(busy.is_empty(), "still busy after the restart: {busy:?}");
    for id in [NVME_FMT, TEST, FW] {
        assert!(kinds.contains(&(id.to_string(), "restart".to_string())), "no restart event for {id}: {kinds:?}");
    }
    assert!(kinds.contains(&(SAS_FMT.to_string(), "format".to_string())), "{kinds:?}");
}
