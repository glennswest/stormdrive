//! The drive's own error and PI-check counters, from stormblock's card API
//! (#160, stormblock#648, v24.24.0+).
//!
//! stormblock samples every drive behind the card every 30 s: the drive's
//! error log pages (read/write/verify, non-medium, 0x2F exception), its grown
//! defect list (SATA: reallocated/pending/offline uncorrectable), and the
//! kernel's sense lines for it (PI guard/app/ref checks the drive raised,
//! EEDP checks the card raised, medium/recovered/hardware errors). It rates
//! them and sets a level: corrected-on-the-fly ECC never counts; `degraded`
//! is any retry-corrected, non-medium, recovered, hardware or medium error,
//! a PI check, or a defect grown in the last day; `failing` is an
//! uncorrected error, a PI check or medium error two samples running, or 10
//! defects grown in a day. Each row of `GET /api/v1/storage/hba` carries it
//! as `errors`.
//!
//! The fleet tick reads the rows every [`REFRESH`] and joins them by SAS
//! address: the Drive object carries the block as `status.health.errors`
//! and two conditions, `DriveErrorsDegraded` (degraded or failing) and
//! `DriveErrorsFailing`, with stormblock's `since` as their transition time.
//! The level is the engine's; stormdrive does not re-derive it.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::drive::DriveId;

/// How often the fleet tick reads the engine's rows (the engine samples
/// every 30 s).
pub const REFRESH: Duration = Duration::from_secs(60);

/// The engine's level for a drive's errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Settling,
    Ok,
    Degraded,
    Failing,
}

impl Level {
    pub fn parse(s: &str) -> Option<Level> {
        match s {
            "settling" => Some(Level::Settling),
            "ok" => Some(Level::Ok),
            "degraded" => Some(Level::Degraded),
            "failing" => Some(Level::Failing),
            _ => None,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Level::Settling => "settling",
            Level::Ok => "ok",
            Level::Degraded => "degraded",
            Level::Failing => "failing",
        }
    }
}

/// A level change worth an Event: into degraded or failing, or back to ok
/// from either. Settling and the first ok are not.
pub fn noteworthy(from: Option<Level>, to: Level) -> bool {
    match to {
        Level::Degraded | Level::Failing => from != Some(to),
        Level::Ok => matches!(from, Some(Level::Degraded | Level::Failing)),
        Level::Settling => false,
    }
}

const PI: [&str; 3] = ["pi_guard", "pi_app_tag", "pi_ref_tag"];
const EEDP: [&str; 3] = ["eedp_guard", "eedp_app_tag", "eedp_ref_tag"];

/// The engine's `errors` block as the Drive object carries it, camelCase,
/// with the sums the health summary has. `None` when it is not one (no
/// `level`).
pub fn project(e: &Value) -> Option<Value> {
    let level = Level::parse(e.get("level")?.as_str()?)?;
    let d = &e["drive"];
    let s = &e["sense"];
    let u = |v: &Value| v.as_u64();
    let page = |p: &Value| {
        if p.is_object() {
            json!({
                "correctedFast": p["corrected_fast"],
                "correctedDelayed": p["corrected_delayed"],
                "retries": p["retries"],
                "corrected": p["corrected"],
                "uncorrected": p["uncorrected"],
            })
        } else {
            Value::Null
        }
    };
    let sum = |ks: &[&str]| ks.iter().filter_map(|k| u(&s[*k])).sum::<u64>();
    let uncorrected = ["read", "write", "verify"].iter().filter_map(|p| u(&d[*p]["uncorrected"])).sum::<u64>();
    let per_min: serde_json::Map<String, Value> = e["per_min"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some((p.get(0)?.as_str()?.to_string(), p.get(1)?.clone())))
        .collect();
    let why: Vec<String> = e["why"].as_array().into_iter().flatten().filter_map(|w| w.as_str().map(str::to_string)).collect();
    Some(json!({
        "level": level.word(),
        "since": u(&e["since"]),
        "at": u(&e["at"]),
        "streak": u(&e["streak"]),
        "why": why,
        "piChecks": sum(&PI),
        "eedpChecks": sum(&EEDP),
        "uncorrected": uncorrected,
        "mediumErrors": u(&s["medium_error"]),
        "grownDefects": u(&d["grown_defects"]),
        "defectsPerDay": u(&e["defects_per_day"]),
        "pendingPerDay": u(&e["pending_per_day"]),
        "drive": {
            "read": page(&d["read"]),
            "write": page(&d["write"]),
            "verify": page(&d["verify"]),
            "nonMedium": d["non_medium"],
            "exception": d["exception"],
            "grownDefects": d["grown_defects"],
            "reallocated": d["reallocated"],
            "pending": d["pending"],
            "offlineUncorrectable": d["offline_uncorrectable"],
        },
        "sense": {
            "piGuard": s["pi_guard"],
            "piAppTag": s["pi_app_tag"],
            "piRefTag": s["pi_ref_tag"],
            "eedpGuard": s["eedp_guard"],
            "eedpAppTag": s["eedp_app_tag"],
            "eedpRefTag": s["eedp_ref_tag"],
            "mediumError": s["medium_error"],
            "recoveredError": s["recovered_error"],
            "hardwareError": s["hardware_error"],
        },
        "perMin": per_min,
    }))
}

/// Every row's `errors`, projected, by normalized SAS address.
pub fn by_sas(report: &Value) -> HashMap<String, Value> {
    report["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let sas = crate::ses::normalize_sas(r.get("sas_address")?.as_str()?);
            Some((sas, project(r.get("errors")?)?))
        })
        .collect()
}

/// `DriveErrorsDegraded` (degraded or failing) and `DriveErrorsFailing`.
/// None while the engine has no level yet, or is settling with nothing
/// before it (a settling sample keeps what was last said).
pub fn conditions(e: &Value) -> Vec<Value> {
    let Some(level) = e["level"].as_str().and_then(Level::parse) else { return Vec::new() };
    if level == Level::Settling {
        return Vec::new();
    }
    let why = e["why"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join("; ");
    let since = e["since"].as_u64().map(crate::controller::rfc3339);
    let c = |kind: &str, on: bool, reason_on: &str| {
        let (reason, message) = if on {
            (reason_on.to_string(), if why.is_empty() { format!("drive errors {}", level.word()) } else { why.clone() })
        } else {
            ("DriveErrorsOk".to_string(), format!("drive errors {}", level.word()))
        };
        json!({
            "type": kind,
            "status": if on { "True" } else { "False" },
            "reason": reason,
            "message": message,
            "lastTransitionTime": since,
        })
    };
    vec![
        c("DriveErrorsDegraded", matches!(level, Level::Degraded | Level::Failing), "ErrorsMoving"),
        c("DriveErrorsFailing", level == Level::Failing, "ErrorsFailing"),
    ]
}

static BY_DRIVE: std::sync::OnceLock<Mutex<HashMap<DriveId, Value>>> = std::sync::OnceLock::new();

fn by_drive() -> &'static Mutex<HashMap<DriveId, Value>> {
    BY_DRIVE.get_or_init(Default::default)
}

/// The engine's last word on a drive's errors, projected.
pub fn get(id: &DriveId) -> Option<Value> {
    by_drive().lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned()
}

/// What the engine answered for a drive: a block (kept), or none (it no
/// longer lists the drive, or has no sample of it: forgotten).
pub fn set(id: DriveId, errors: Option<Value>) {
    let mut m = by_drive().lock().unwrap_or_else(|e| e.into_inner());
    match errors {
        Some(v) => m.insert(id, v),
        None => m.remove(&id),
    };
}

/// `status.health` with `errors` joined in, and the two conditions.
pub fn status(id: &DriveId, health: Value) -> (Value, Vec<Value>) {
    let Some(e) = get(id) else { return (health, Vec::new()) };
    let conds = conditions(&e);
    let mut h = health;
    if !h.is_object() {
        h = json!({});
    }
    h["errors"] = e;
    (h, conds)
}

/// The errors level as a Drive object spells it.
pub fn level_of(drive_object: &Value) -> Option<Level> {
    drive_object["status"]["health"]["errors"]["level"].as_str().and_then(Level::parse)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row as stormblock v24.24.0 answers it (#648): a SAS drive with a
    /// PI guard check two samples running.
    fn report() -> Value {
        json!({
            "watching": true, "read_at": 1763900000u64, "every_secs": 30,
            "rows": [
                {
                    "sas_address": "0x5000CCA26A819E69", "link": {"level": "ok"},
                    "errors": {
                        "sas_address": "0x5000cca26a819e69", "at": 1763900000u64,
                        "drive": {
                            "write": {"corrected_fast": 10, "corrected_delayed": 0, "retries": 0, "corrected": 10, "uncorrected": 0},
                            "read": {"corrected_fast": 900, "corrected_delayed": 2, "retries": 2, "corrected": 902, "uncorrected": 1},
                            "verify": null, "non_medium": 3, "exception": "0x00/0x00", "grown_defects": 4,
                            "reallocated": null, "pending": null, "offline_uncorrectable": null, "errors": []
                        },
                        "sense": {"pi_guard": 2, "pi_app_tag": 0, "pi_ref_tag": 1, "eedp_guard": 1, "eedp_app_tag": 0, "eedp_ref_tag": 0,
                                  "medium_error": 1, "recovered_error": 0, "hardware_error": 0},
                        "per_min": [["pi_guard", 0.5], ["read_uncorrected", 0.03]],
                        "defects_per_day": 1, "pending_per_day": null,
                        "level": "failing", "since": 1763899940u64, "streak": 2,
                        "why": ["pi_guard moving 2 samples running", "read uncorrected 1"]
                    }
                },
                {"sas_address": "0x5000cca26a81aaaa"},
                {"sas_address": "0x5000cca26a81bbbb", "errors": {"level": "ok", "since": 1763800000u64, "drive": {}, "sense": {}, "per_min": []}}
            ],
            "changes": [], "errors": []
        })
    }

    #[test]
    fn the_rows_joined_by_sas_address() {
        let m = by_sas(&report());
        assert_eq!(m.len(), 2, "a row with no errors block has none");
        let e = &m["5000cca26a819e69"];
        assert_eq!(e["level"], "failing");
        assert_eq!(e["since"], 1763899940u64);
        assert_eq!(e["streak"], 2);
        assert_eq!(e["piChecks"], 3);
        assert_eq!(e["eedpChecks"], 1);
        assert_eq!(e["uncorrected"], 1);
        assert_eq!(e["mediumErrors"], 1);
        assert_eq!(e["grownDefects"], 4);
        assert_eq!(e["defectsPerDay"], 1);
        assert_eq!(e["drive"]["read"]["correctedDelayed"], 2);
        assert_eq!(e["drive"]["verify"], Value::Null);
        assert_eq!(e["drive"]["nonMedium"], 3);
        assert_eq!(e["sense"]["piRefTag"], 1);
        assert_eq!(e["perMin"]["pi_guard"], 0.5);
        assert_eq!(e["why"][0], "pi_guard moving 2 samples running");
        let ok = &m["5000cca26a81bbbb"];
        assert_eq!(ok["level"], "ok");
        assert_eq!(ok["piChecks"], 0);
        assert_eq!(ok["grownDefects"], Value::Null);
    }

    #[test]
    fn not_a_block_is_nothing() {
        assert_eq!(project(&json!({})), None);
        assert_eq!(project(&json!({"level": "weird"})), None);
        assert_eq!(project(&json!(null)), None);
    }

    #[test]
    fn conditions_follow_the_engine_level() {
        let m = by_sas(&report());
        let cs = conditions(&m["5000cca26a819e69"]);
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0]["type"], "DriveErrorsDegraded");
        assert_eq!(cs[0]["status"], "True");
        assert_eq!(cs[1]["type"], "DriveErrorsFailing");
        assert_eq!(cs[1]["status"], "True");
        assert_eq!(cs[1]["reason"], "ErrorsFailing");
        assert_eq!(cs[1]["message"], "pi_guard moving 2 samples running; read uncorrected 1");
        assert_eq!(cs[1]["lastTransitionTime"], crate::controller::rfc3339(1763899940));

        let cs = conditions(&m["5000cca26a81bbbb"]);
        assert_eq!(cs[0]["status"], "False");
        assert_eq!(cs[0]["reason"], "DriveErrorsOk");
        assert_eq!(cs[1]["status"], "False");

        let degraded = project(&json!({"level": "degraded", "since": 5, "why": ["recovered_error moving"]})).unwrap();
        let cs = conditions(&degraded);
        assert_eq!((cs[0]["status"].as_str(), cs[1]["status"].as_str()), (Some("True"), Some("False")));
        assert_eq!(cs[0]["message"], "recovered_error moving");

        assert!(conditions(&project(&json!({"level": "settling"})).unwrap()).is_empty());
    }

    #[test]
    fn health_carries_the_block() {
        let id = DriveId(uuid::Uuid::from_u128(0x160));
        let (h, cs) = status(&id, json!({"status": "ok"}));
        assert_eq!(h, json!({"status": "ok"}));
        assert!(cs.is_empty());
        set(id, by_sas(&report()).remove("5000cca26a819e69"));
        let (h, cs) = status(&id, json!({"status": "ok"}));
        assert_eq!(h["status"], "ok");
        assert_eq!(h["errors"]["level"], "failing");
        assert_eq!(cs.len(), 2);
        assert_eq!(level_of(&json!({"status": {"health": h}})), Some(Level::Failing));
        set(id, None);
        assert!(get(&id).is_none());
    }

    #[test]
    fn events_on_level_changes() {
        assert!(noteworthy(None, Level::Degraded));
        assert!(noteworthy(Some(Level::Degraded), Level::Failing));
        assert!(!noteworthy(Some(Level::Failing), Level::Failing));
        assert!(noteworthy(Some(Level::Failing), Level::Ok));
        assert!(!noteworthy(None, Level::Ok));
        assert!(!noteworthy(Some(Level::Ok), Level::Settling));
    }
}
