//! Where every drive physically is, for mirrors (#10): rustkube-node
//! attaches it to each PV next to stormblock's per-volume placement
//! (stormblock#136, rustkube-node#60).
//!
//! stormblock names a volume's drives by `wwn` (the raw sysfs `wwid`,
//! `naa.…`/`eui.…`) and `serial`; each record here carries both, so the
//! join needs no device name. Only placement goes in — shelf, bay, SAS
//! wiring, PCIe, fleet membership, designation, activity and health
//! *state* — never temperatures or counters, so `generation` moves when a
//! drive moves, a shelf appears, or a drive changes role, and not on every
//! health sample.
//!
//! `generation` is a 53-bit FNV-1a hash of the records (safe as a JSON
//! number anywhere): compare it for equality only. Being a hash of the
//! content, it survives a restart unchanged when nothing moved, and a
//! mirror holding it never misses a change.

use crate::drive::{Drive, Shelf};
use crate::topology::Shelves;
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn shelf_json(key: &str, sh: &Shelf) -> Value {
    json!({
        "key": key,
        "logical_id": sh.logical_id,
        "vendor": sh.vendor,
        "model": sh.model,
        "serial": sh.serial,
        "sas_address": sh.sas_address,
    })
}

/// One drive's placement record.
pub fn drive_record(d: &Drive) -> Value {
    let loc = &d.location;
    let labels: BTreeMap<String, String> = loc.labels().into_iter().collect();
    json!({
        "id": d.id.0.to_string(),
        "wwn": d.wwid,
        "serial": d.serial,
        "model": d.model,
        "kind": d.kind,
        "capacity_bytes": d.capacity_bytes,
        "path": d.path,
        "paths": d.paths,
        "shelf": loc.shelf.as_ref().and_then(|sh| sh.key().map(|k| shelf_json(&k, sh))),
        "bay": loc.bay,
        "sas_address": loc.sas_address,
        "sas_phy": loc.sas_phy,
        "expander": loc.expander,
        "hba": loc.controller,
        "pcie_addr": loc.pcie_addr,
        "pcie_slot": loc.pcie_slot,
        "labels": labels,
        "membership": d.membership,
        "designation": d.designation,
        "activity": d.activity,
        "health": d.health.status(),
        "in_use_by": d.in_use_by,
    })
}

/// The whole node: every drive, and every shelf the node can see — from
/// the SES scan, plus any shelf only a drive's sysfs names.
pub fn view<'a>(drives: impl IntoIterator<Item = &'a Drive>, shelves: &Shelves, node: &str) -> Value {
    let mut ds: Vec<&Drive> = drives.into_iter().collect();
    ds.sort_by_key(|d| d.id.0);
    let mut sh: BTreeMap<String, Value> = BTreeMap::new();
    for (key, rep) in shelves {
        let mut v = shelf_json(key, &rep.shelf);
        v["status"] = json!(rep.worst());
        v["esp_paths"] = json!(rep.esps.len());
        sh.insert(key.clone(), v);
    }
    for d in &ds {
        if let Some(s) = &d.location.shelf {
            if let Some(k) = s.key() {
                sh.entry(k.clone()).or_insert_with(|| shelf_json(&k, s));
            }
        }
    }
    for (key, v) in sh.iter_mut() {
        let bays: Vec<Value> = ds
            .iter()
            .filter(|d| d.location.shelf.as_ref().and_then(|s| s.key()).as_deref() == Some(key.as_str()))
            .map(|d| json!({ "bay": d.location.bay, "wwn": d.wwid, "serial": d.serial }))
            .collect();
        v["drives"] = Value::Array(bays);
    }
    let drives: Vec<Value> = ds.iter().map(|d| drive_record(d)).collect();
    let shelves: Vec<Value> = sh.into_values().collect();
    let generation = generation(&json!({ "node": node, "drives": drives, "shelves": shelves }));
    json!({ "node": node, "generation": generation, "drives": drives, "shelves": shelves })
}

/// FNV-1a over the serialized value, cut to 53 bits. serde_json writes an
/// object's keys in a fixed order, so equal content hashes equal.
pub fn generation(v: &Value) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in v.to_string().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h & ((1u64 << 53) - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::*;
    use std::time::SystemTime;

    fn drive(serial: &str, wwn: &str, bay: u32) -> Drive {
        let mut d: Drive = serde_json::from_value(json!({
            "id": DriveId::derive(Some(wwn), "M", serial),
            "path": "/dev/sdb", "name": "sdb", "kind": "sas_hdd", "model": "ST1200MM0098",
            "serial": serial, "firmware": "N004", "wwid": wwn, "capacity_bytes": 1u64 << 40,
            "block_size": 4096,
            "first_seen": SystemTime::UNIX_EPOCH, "last_seen": SystemTime::UNIX_EPOCH,
        }))
        .unwrap();
        d.location = Location {
            shelf: Some(Shelf { logical_id: Some("5000a098".into()), model: Some("DS224C".into()), ..Default::default() }),
            bay: Some(bay),
            sas_address: Some("0x5000c500a1b2c3d1".into()),
            sas_phy: Some(bay),
            expander: Some("0x500a09800abc0001".into()),
            ..Default::default()
        };
        d
    }

    #[test]
    fn records_join_on_wwn_and_serial_and_carry_the_place() {
        let d = drive("S1", "naa.5000c500a1b2c3d1", 4);
        let v = view([&d], &Shelves::new(), "node1");
        let r = &v["drives"][0];
        assert_eq!(r["wwn"], "naa.5000c500a1b2c3d1");
        assert_eq!(r["serial"], "S1");
        assert_eq!(r["shelf"]["key"], "5000a098");
        assert_eq!(r["bay"], 4);
        assert_eq!(r["sas_phy"], 4);
        assert_eq!(r["expander"], "0x500a09800abc0001");
        assert_eq!(r["labels"]["shelf"], "5000a098");
        assert_eq!(r["membership"], "out");
        // A shelf known only from its drives still appears, with its bays.
        assert_eq!(v["shelves"][0]["key"], "5000a098");
        assert_eq!(v["shelves"][0]["drives"][0]["bay"], 4);
        assert!(v["generation"].as_u64().unwrap() < 1 << 53);
    }

    #[test]
    fn generation_moves_with_placement_not_with_health_samples() {
        let d = drive("S1", "naa.5000c500a1b2c3d1", 4);
        let g = |d: &Drive| view([d], &Shelves::new(), "node1")["generation"].as_u64().unwrap();
        let g0 = g(&d);
        assert_eq!(g0, g(&d.clone()), "stable for the same content");

        let mut warm = d.clone();
        warm.health.temperature_c = Some(51);
        warm.health.power_on_hours = Some(12_345);
        assert_eq!(g(&warm), g0, "a health sample is not a placement change");

        let mut moved = d.clone();
        moved.location.bay = Some(9);
        assert_ne!(g(&moved), g0, "a re-bay is");

        let mut spare = d.clone();
        spare.designation = Designation::Spare;
        assert_ne!(g(&spare), g0, "a role change is");

        let mut failing = d.clone();
        failing.health.status = Some(HealthStatus::Failing);
        assert_ne!(g(&failing), g0, "a health state change is");
    }
}
