//! The last write-test matrix run on a drive, from stormblock's card API
//! (#146, stormblock#627; owner: "can you compare drive settings/options
//! between drives").
//!
//! stormblock runs the matrix (`POST /api/v1/storage/hba/{sas}/write-test`,
//! admin only): WRPROTECT 0 and 1 at LBA 0 and the last LBA, read back with
//! RDPROTECT 0 and 1, the card's link after each step, the mode and log
//! pages before and after. It keeps the last run per drive for the engine's
//! life and answers it at `GET …/write-test` (404: none since the engine
//! started). The fleet tick asks for every drive with a SAS address
//! ([`REFRESH`]) and the Drive object carries the run as `status.writeTest`,
//! so forge1's four drives compare in one `kubectl get drives -o wide`.
//!
//! Only the run's summary is kept: what the drive is, the steps, the lines,
//! pass or not. The pages before and after stay on the engine (the drive's
//! own pages are `status.pages`, #147).

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::drive::DriveId;

/// How often the fleet tick asks the engine again.
pub const REFRESH: Duration = Duration::from_secs(300);

/// stormblock's run as the Drive object carries it. `None` when the answer
/// is not a run (no `steps` and no `lines`).
pub fn project(v: &Value) -> Option<Value> {
    if !v.is_object() || (v.get("steps").is_none() && v.get("lines").is_none()) {
        return None;
    }
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let steps: Vec<Value> = v["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|st| {
            json!({
                "lba": st["lba"],
                "wrprotect": st["wrprotect"],
                "write": st["write"],
                "read0": st["read0"],
                "read1": st["read1"],
                "readyAfter": st["ready_after"],
                "linkAfter": st["link_after"],
            })
        })
        .collect();
    let lines: Vec<String> = v["lines"].as_array().into_iter().flatten().filter_map(|l| l.as_str().map(str::to_string)).collect();
    Some(json!({
        "sasAddress": s("sas_address"),
        "at": v["at"].as_u64(),
        "passed": v["passed"].as_bool(),
        "stopped": v.get("stopped").cloned().unwrap_or(Value::Null),
        "via": s("via"),
        "vendor": s("vendor"),
        "product": s("product"),
        "revision": s("revision"),
        "capacity": v.get("capacity").cloned().unwrap_or(Value::Null),
        "linkBefore": v["link_before"].as_str(),
        "steps": steps,
        "lines": lines,
    }))
}

static BY_DRIVE: std::sync::OnceLock<Mutex<HashMap<DriveId, Value>>> = std::sync::OnceLock::new();

fn by_drive() -> &'static Mutex<HashMap<DriveId, Value>> {
    BY_DRIVE.get_or_init(Default::default)
}

/// The last run the engine had for a drive, for its Drive object.
pub fn get(id: &DriveId) -> Option<Value> {
    by_drive().lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned()
}

/// What the engine answered: a run (kept), or none (a 404: forgotten — the
/// engine restarted and the run is no longer its).
pub fn set(id: DriveId, run: Option<Value>) {
    let mut m = by_drive().lock().unwrap_or_else(|e| e.into_inner());
    match run {
        Some(v) => m.insert(id, v),
        None => m.remove(&id),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// forge1 bay 3's run, shaped as stormblock v24.14+ answers it.
    fn bay3() -> Value {
        json!({
            "via": "sg", "vendor": "NETAPP", "product": "X378_HLBRE10TA07", "revision": "NA02",
            "capacity": {"blocks": 2441609216u64, "block_len": 4096, "prot_en": true, "p_type": 1},
            "mode_before": [{"page": "0x0A", "hex": "0a0a…", "said": "control: ATO 1"}],
            "log_before": [["0x15", "15000000"]],
            "steps": [
                {"lba": 0, "wrprotect": 0, "write": "ok", "read0": "ok", "read1": "ok", "ready_after": "ready", "link_after": "12.0 Gbit (ready, present)"},
                {"lba": 0, "wrprotect": 1, "write": "check condition 0b/47/03", "read0": "ok", "read1": "-", "ready_after": "ready", "link_after": null}
            ],
            "mode_after": [], "log_after": [], "capacity_after": null,
            "link_after": [], "stopped": "the link went down",
            "sas_address": "0x5000cca26a819e69", "at": 1763400000u64,
            "link_before": "12.0 Gbit (ready, present)",
            "lines": ["LBA 0 WRPROTECT 0: write ok, read ok", "LBA 0 WRPROTECT 1: write check condition 0b/47/03"],
            "passed": false
        })
    }

    #[test]
    fn the_run_projected_for_the_drive_object() {
        let p = project(&bay3()).unwrap();
        assert_eq!(p["sasAddress"], "0x5000cca26a819e69");
        assert_eq!(p["at"], 1763400000u64);
        assert_eq!(p["passed"], false);
        assert_eq!(p["stopped"], "the link went down");
        assert_eq!(p["revision"], "NA02");
        assert_eq!(p["capacity"]["block_len"], 4096);
        assert_eq!(p["linkBefore"], "12.0 Gbit (ready, present)");
        assert_eq!(p["steps"].as_array().unwrap().len(), 2);
        assert_eq!(p["steps"][1]["write"], "check condition 0b/47/03");
        assert_eq!(p["steps"][0]["readyAfter"], "ready");
        assert_eq!(p["steps"][0]["linkAfter"], "12.0 Gbit (ready, present)");
        assert_eq!(p["lines"].as_array().unwrap().len(), 2);
        // The pages before and after stay on the engine.
        assert!(p.get("mode_before").is_none() && p.get("modeBefore").is_none() && p.get("log_after").is_none());
    }

    #[test]
    fn not_a_run_is_nothing() {
        assert_eq!(project(&json!({"error": "no write test run on this drive since the engine started"})), None);
        assert_eq!(project(&json!([])), None);
        // A run that stopped before its first step still has its lines.
        let p = project(&json!({"lines": ["no sg node, the card refused"], "passed": false})).unwrap();
        assert_eq!(p["steps"], json!([]));
        assert_eq!(p["stopped"], Value::Null);
    }

    #[test]
    fn kept_and_forgotten_by_drive() {
        let id = DriveId(uuid::Uuid::from_u128(0x146));
        assert!(get(&id).is_none());
        set(id, project(&bay3()));
        assert_eq!(get(&id).unwrap()["revision"], "NA02");
        set(id, None);
        assert!(get(&id).is_none());
    }
}
