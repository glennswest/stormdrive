//! Which drives a suite may touch, and how. Pure functions over
//! `/api/v1/drives` records, unit-tested, because this is where a test on a
//! real machine is kept from harming real drives.
//!
//! The rules (#11):
//! - **Never destructive.** No join/leave, format, firmware, destructive
//!   test, forget, drain or `failed` designation is ever *meant* to run.
//! - **Refusals only where the refusal is certain.** A request the server
//!   must refuse is sent only to a drive whose record shows the guard the
//!   server checks *first* (in the fleet, busy, missing…), so even a broken
//!   later guard cannot let it start.
//! - **Writes are reversible and restored:** a designation of `spare` on an
//!   out-of-fleet drive that has none, an overcommit round-trip on a drive
//!   with no slabs (nothing is pushed to the engine), read-only tests.

use serde_json::Value;

pub fn s<'a>(d: &'a Value, k: &str) -> &'a str {
    d[k].as_str().unwrap_or_default()
}

pub fn present(d: &Value) -> bool {
    s(d, "activity") != "missing"
}

pub fn idle(d: &Value) -> bool {
    s(d, "activity") == "idle"
}

pub fn usable(d: &Value) -> bool {
    d["usable"].as_bool().unwrap_or(true)
}

pub fn in_fleet(d: &Value) -> bool {
    s(d, "membership") == "fleet"
}

pub fn in_use(d: &Value) -> bool {
    d["in_use_by"].as_str().is_some_and(|w| !w.is_empty())
}

fn has_slabs(d: &Value) -> bool {
    d["usage"]["slabs"].as_array().is_some_and(|a| !a.is_empty())
}

/// For reversible operator settings (designation, overcommit): out of the
/// fleet, holding nobody's data, idle, no designation, no slabs — the drive
/// nothing else is acting on, and whose settings reach no engine.
pub fn settings_candidate(drives: &[Value]) -> Option<&Value> {
    drives.iter().find(|d| {
        present(d) && idle(d) && !in_fleet(d) && !in_use(d) && s(d, "designation") == "none" && !has_slabs(d)
    })
}

/// For read-only tests (smoke, read scan + cancel): idle and readable.
/// Out-of-fleet drives first, so a fleet drive's I/O is left alone when
/// there is a choice; the tests only read either way.
pub fn read_candidate(drives: &[Value]) -> Option<&Value> {
    let ok = |d: &&Value| present(d) && idle(d) && usable(d);
    drives
        .iter()
        .filter(ok)
        .find(|d| !in_fleet(d) && !in_use(d))
        .or_else(|| drives.iter().find(ok))
}

/// For requests that must be refused: a drive in the fleet (the first guard
/// of join, format and the destructive test), else one holding stormblock
/// data (checked right after, and read off the disk again on the server).
pub fn guarded(drives: &[Value]) -> Option<&Value> {
    drives
        .iter()
        .find(|d| present(d) && in_fleet(d))
        .or_else(|| drives.iter().find(|d| present(d) && in_use(d)))
}

/// A drive DELETE must refuse: present (the first check is "only a missing
/// drive can be forgotten").
pub fn forget_refused(drives: &[Value]) -> Option<&Value> {
    drives.iter().find(|d| present(d) && !s(d, "id").is_empty())
}

/// A serial names one drive only when no other drive shares it (NVMe-oF
/// namespaces can).
pub fn serial_unique(drives: &[Value], serial: &str) -> bool {
    !serial.is_empty() && drives.iter().filter(|d| s(d, "serial") == serial).count() == 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn d(over: Value) -> Value {
        let mut base = json!({
            "id": "a", "name": "sda", "serial": "S1", "membership": "out", "designation": "none",
            "activity": "idle", "usable": true, "in_use_by": null, "usage": null,
        });
        for (k, v) in over.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    }

    #[test]
    fn settings_never_touch_fleet_or_data_drives() {
        let drives = vec![
            d(json!({"id": "fleet", "membership": "fleet"})),
            d(json!({"id": "sys", "in_use_by": "stormblock"})),
            d(json!({"id": "spare", "designation": "spare"})),
            d(json!({"id": "slabs", "usage": {"slabs": [{"id": "x"}]}})),
            d(json!({"id": "busy", "activity": "formatting"})),
            d(json!({"id": "gone", "activity": "missing"})),
        ];
        assert!(settings_candidate(&drives).is_none());
        let mut more = drives.clone();
        more.push(d(json!({"id": "free"})));
        assert_eq!(s(settings_candidate(&more).unwrap(), "id"), "free");
    }

    #[test]
    fn reads_prefer_out_of_fleet_and_skip_unusable() {
        let drives = vec![
            d(json!({"id": "fleet", "membership": "fleet"})),
            d(json!({"id": "520", "usable": false})),
            d(json!({"id": "out"})),
        ];
        assert_eq!(s(read_candidate(&drives).unwrap(), "id"), "out");
        assert_eq!(s(read_candidate(&drives[..2]).unwrap(), "id"), "fleet");
        assert!(read_candidate(&drives[1..2]).is_none());
    }

    #[test]
    fn refusals_only_on_drives_whose_first_guard_holds() {
        let out = d(json!({"id": "out"}));
        assert!(guarded(&[out.clone()]).is_none(), "a free drive is never sent a destructive request");
        let sys = d(json!({"id": "sys", "in_use_by": "stormblock"}));
        let fleet = d(json!({"id": "fleet", "membership": "fleet"}));
        assert_eq!(s(guarded(&[out.clone(), sys.clone(), fleet]).unwrap(), "id"), "fleet");
        assert_eq!(s(guarded(&[out, sys]).unwrap(), "id"), "sys");
        let gone = d(json!({"id": "gone", "activity": "missing"}));
        assert!(forget_refused(&[gone]).is_none(), "a missing drive would really be forgotten");
    }

    #[test]
    fn shared_serials_are_not_handles() {
        let drives = vec![d(json!({"serial": "X"})), d(json!({"serial": "X"})), d(json!({"serial": "Y"}))];
        assert!(!serial_unique(&drives, "X"));
        assert!(serial_unique(&drives, "Y"));
        assert!(!serial_unique(&drives, ""));
    }
}
