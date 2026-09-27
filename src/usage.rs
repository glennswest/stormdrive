//! How much of a drive is used and how much is left (#12): "look at a
//! drive and know how much storage is left."
//!
//! stormblock owns the numbers — its `/api/v1/slabs` lists every slab with
//! total/free/allocated slots and, since v17.1 (stormblock#136), the drive
//! it sits on by the identity we use (`drive {serial, wwn, model, path}`;
//! for a slab in a partition, the disk). This joins them to our drives:
//! WWN first (NVMe-oF namespaces can share a serial), else serial, else
//! /dev path.
//!
//! - `used` = what stormblock has allocated out of the drive's slabs.
//! - `free_in_slabs` = slab space stormblock can still hand out.
//! - `outside_slabs` = capacity no slab's slot area covers: the partition
//!   table, each slab's own metadata region, unpartitioned space, other
//!   partitions. Not all of it can become slab space; most of it on a
//!   stormcos disk is metadata.
//! - `free` = capacity − used, the owner's "how much is left".

use crate::drive::Drive;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlabUsage {
    pub id: String,
    /// `system` or `data`.
    pub role: String,
    pub tier: String,
    pub slot_size: u64,
    pub total_bytes: u64,
    pub allocated_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub capacity_bytes: u64,
    pub slabs: Vec<SlabUsage>,
    /// Σ slab total: the slot area stormblock manages on this drive.
    pub in_slabs_bytes: u64,
    pub used_bytes: u64,
    pub free_in_slabs_bytes: u64,
    pub outside_slabs_bytes: u64,
    /// capacity − used.
    pub free_bytes: u64,
    /// When stormblock last answered for this drive.
    pub collected_at: SystemTime,
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

fn u64_of(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or_default()
}

/// Is this slab (a `/api/v1/slabs` item) on this drive? A slab that names
/// a WWN is matched on it alone, so namespaces sharing a serial stay apart.
pub fn slab_on(slab: &Value, d: &Drive) -> bool {
    let Some(r) = slab.get("drive") else { return false };
    let wwn = str_of(r, "wwn");
    if !wwn.is_empty() {
        return d.wwid.as_deref().is_some_and(|w| w.eq_ignore_ascii_case(wwn));
    }
    let serial = str_of(r, "serial");
    if !serial.is_empty() {
        return serial == d.serial;
    }
    let path = str_of(r, "path");
    !path.is_empty() && (path == d.path || d.paths.iter().any(|p| p == path))
}

fn slab_usage(slab: &Value) -> SlabUsage {
    let slot_size = u64_of(slab, "slot_size");
    let total_bytes = u64_of(slab, "total_bytes");
    let free_bytes = u64_of(slab, "free_bytes");
    let allocated_bytes = match slab.get("allocated_slots").and_then(Value::as_u64) {
        Some(n) if slot_size > 0 => n * slot_size,
        _ => total_bytes.saturating_sub(free_bytes),
    };
    SlabUsage {
        id: str_of(slab, "id").to_string(),
        role: str_of(slab, "role").to_string(),
        tier: str_of(slab, "tier").to_string(),
        slot_size,
        total_bytes,
        allocated_bytes,
        free_bytes,
    }
}

/// This drive's usage out of stormblock's whole slab listing. A drive
/// with no slab reads as all outside, all free.
pub fn compute(d: &Drive, slabs: &[Value], now: SystemTime) -> Usage {
    let mut mine: Vec<SlabUsage> = slabs.iter().filter(|s| slab_on(s, d)).map(slab_usage).collect();
    mine.sort_by(|a, b| (&a.role, &a.id).cmp(&(&b.role, &b.id)));
    let capacity = d.capacity_bytes;
    let in_slabs: u64 = mine.iter().map(|s| s.total_bytes).sum();
    let used: u64 = mine.iter().map(|s| s.allocated_bytes).sum();
    let free_in_slabs: u64 = mine.iter().map(|s| s.free_bytes).sum();
    Usage {
        capacity_bytes: capacity,
        slabs: mine,
        in_slabs_bytes: in_slabs,
        used_bytes: used,
        free_in_slabs_bytes: free_in_slabs,
        outside_slabs_bytes: capacity.saturating_sub(in_slabs),
        free_bytes: capacity.saturating_sub(used),
        collected_at: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::*;
    use serde_json::json;

    const GB: u64 = 1_000_000_000;
    const SLOT: u64 = 1 << 30;

    fn drive(serial: &str, wwn: Option<&str>, path: &str, capacity: u64) -> Drive {
        serde_json::from_value(json!({
            "id": DriveId::derive(wwn, "WDC WD20EFAX-68F", serial),
            "path": path, "name": path.trim_start_matches("/dev/"), "paths": [path],
            "kind": "sata_hdd", "model": "WDC WD20EFAX-68F", "serial": serial,
            "firmware": "0A82", "wwid": wwn, "capacity_bytes": capacity, "block_size": 512,
            "first_seen": SystemTime::UNIX_EPOCH, "last_seen": SystemTime::UNIX_EPOCH,
        }))
        .unwrap()
    }

    fn slab(id: &str, role: &str, total_slots: u64, allocated: u64, drive: Value) -> Value {
        json!({
            "id": id, "tier": "cool", "role": role, "domain": "drive=…", "slot_size": SLOT,
            "total_slots": total_slots, "free_slots": total_slots - allocated,
            "allocated_slots": allocated, "total_bytes": total_slots * SLOT,
            "free_bytes": (total_slots - allocated) * SLOT, "drive": drive,
        })
    }

    /// The R230 (C2NR0Q2) as the issue describes it: a 2 TB WD with a
    /// system and a data slab, a few GB used in each.
    #[test]
    fn the_r230_system_disk_adds_up() {
        let d = drive("WD-WX11D28JFS6T", Some("naa.50014ee2bab11f8d"), "/dev/sda", 2_000_398_934_016);
        let on_it = json!({ "serial": "WD-WX11D28JFS6T", "wwn": "naa.50014ee2bab11f8d",
                            "model": "WDC WD20EFAX-68F", "path": "/dev/sda" });
        let elsewhere = json!({ "serial": "OTHER", "wwn": "naa.5000c500deadbeef", "path": "/dev/sdb" });
        let slabs = vec![
            slab("data-1", "data", 1_741, 6, on_it.clone()),
            slab("sys-1", "system", 108, 8, on_it),
            slab("x", "data", 500, 400, elsewhere),
        ];
        let u = compute(&d, &slabs, SystemTime::UNIX_EPOCH);
        assert_eq!(u.slabs.len(), 2);
        assert_eq!(u.slabs[0].role, "data", "sorted by role");
        assert_eq!(u.in_slabs_bytes, 1_849 * SLOT);
        assert_eq!(u.used_bytes, 14 * SLOT);
        assert_eq!(u.free_in_slabs_bytes, 1_835 * SLOT);
        assert_eq!(u.outside_slabs_bytes, d.capacity_bytes - 1_849 * SLOT);
        assert_eq!(u.free_bytes, d.capacity_bytes - 14 * SLOT);
        // ~15 GB used, ~1.98 TB left, ~15 GB in no slab.
        assert!(u.used_bytes / GB == 15 && u.free_bytes / GB == 1_985, "{u:?}");
        assert!(u.outside_slabs_bytes / GB == 15, "{u:?}");
    }

    #[test]
    fn joins_on_wwn_then_serial_then_path() {
        let a = drive("SB010A", Some("uuid.aaaa"), "/dev/nvme1n1", 100 * SLOT);
        let b = drive("SB010A", Some("uuid.bbbb"), "/dev/nvme1n2", 100 * SLOT);
        let on_b = slab("s", "data", 10, 1, json!({ "serial": "SB010A", "wwn": "UUID.BBBB", "path": "/dev/nvme1n2" }));
        assert!(!slab_on(&on_b, &a), "a WWN is decisive even when the serial matches");
        assert!(slab_on(&on_b, &b));

        let c = drive("S1", None, "/dev/sdc", 100 * SLOT);
        assert!(slab_on(&slab("s", "data", 1, 0, json!({ "serial": "S1", "path": "/dev/sdz" })), &c));
        assert!(slab_on(&slab("s", "data", 1, 0, json!({ "serial": "", "path": "/dev/sdc" })), &c));
        assert!(!slab_on(&slab("s", "data", 1, 0, json!({ "serial": "S2", "path": "/dev/sdc" })), &c));
        assert!(!slab_on(&json!({ "id": "pre-v17.1, no drive" }), &c));
    }

    #[test]
    fn a_drive_without_slabs_is_all_free() {
        let d = drive("S1", None, "/dev/sdc", 4 * SLOT);
        let u = compute(&d, &[], SystemTime::UNIX_EPOCH);
        assert_eq!((u.used_bytes, u.free_bytes, u.outside_slabs_bytes), (0, 4 * SLOT, 4 * SLOT));
        assert!(u.slabs.is_empty());
    }
}
