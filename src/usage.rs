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
//!
//! And what the drive may promise (#13, stormblock#152):
//!
//! - `promisable` = slab space × the drive's overcommit ratio (× 1 when
//!   overcommit is off). Only slab space can hold volumes.
//! - `committed` = Σ the virtual size of the volumes placed in the drive's
//!   slabs, as stormblock reports it per slab (`committed_bytes`). Null
//!   until the engine reports it — stormblock#152 adds it.
//! - `headroom` = promisable − committed: what a new claim can still get.
//!
//! And which volumes the drive holds (#26, stormconsole#29): "which volumes
//! am I about to lose if this drive goes". stormblock v17.1 answers it per
//! volume (`GET /api/v1/volumes?placement=true`: the slabs and drives
//! holding each leg, each slab's state); [`volumes_on`] turns that inside
//! out for one drive, the same reduction as stormconsole's
//! `plugins/stormblock/src/placement.rs`. A console reads every node's
//! stormdrive but only its own node's engine, so this is how it sees them
//! everywhere.

use crate::drive::{Drive, Overcommit};
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
    /// Virtual size promised out of this slab (stormblock#152); None while
    /// the engine does not report it.
    #[serde(default)]
    pub committed_bytes: Option<u64>,
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
    /// in_slabs × the overcommit factor (#13).
    #[serde(default)]
    pub promisable_bytes: u64,
    /// Σ slab committed, when every slab on the drive reports it.
    #[serde(default)]
    pub committed_bytes: Option<u64>,
    /// promisable − committed (0 when over).
    #[serde(default)]
    pub headroom_bytes: Option<u64>,
    /// When stormblock last answered for this drive.
    pub collected_at: SystemTime,
    /// The volumes with legs on this drive, largest first (#26). Absent —
    /// not empty — while the engine reports no placement (before v17.1) or
    /// has not answered yet: "not reported" is not "nothing here".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volumes: Option<Vec<DriveVolume>>,
    /// When the engine's volume placement last answered. A failed read keeps
    /// the last answer, and this says how old it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volumes_collected_at: Option<SystemTime>,
}

/// Who uses a volume, as the engine reports it (stormblock v18.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consumer {
    /// `PersistentVolumeClaim`, `VirtualMachineInstance`, `Mount`, …
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub namespace: String,
    pub name: String,
}

/// One volume with legs on a drive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveVolume {
    pub id: String,
    pub name: String,
    /// The engine's word: `volume`, `golden`, `blank`, `snapshot`, …
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer: Option<Consumer>,
    /// The volume's bytes on this drive.
    pub bytes: u64,
    /// Its data legs on this drive.
    pub legs: u64,
    /// Of those, legs shared with another volume (a clone and its golden):
    /// here, but not this volume's alone to lose.
    pub shared_legs: u64,
    /// The worst state of its slabs on this drive: `ok`, `draining`,
    /// `quarantined`, `failed` or `missing`.
    pub state: String,
    /// `none`, `needed`, `queued` or `running`.
    pub rebuild: String,
    /// The redundancy policy (`mirror2`, …) and its health, volume-wide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
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

/// A placement entry (a drive or a slab of a volume) on this drive. A
/// fabric drive (`nvme-tcp://host/…`) is another node's, whatever its serial.
fn placed_on(entry: &Value, d: &Drive) -> bool {
    let path = entry.get("drive").map(|r| str_of(r, "path")).unwrap_or_default();
    !path.contains("://") && slab_on(entry, d)
}

/// The worst first: one failed leg here is the fact that matters.
const WORST: [&str; 4] = ["missing", "failed", "quarantined", "draining"];

/// The volumes with legs on this drive, out of the engine's listing with
/// placement, largest first. None when no volume carries a placement (an
/// engine before v17.1); an engine with no volumes at all is an empty list.
pub fn volumes_on(d: &Drive, volumes: &[Value]) -> Option<Vec<DriveVolume>> {
    let placed: Vec<(&Value, &Value)> = volumes
        .iter()
        .filter_map(|v| v.get("placement").filter(|p| p.is_object()).map(|p| (v, p)))
        .collect();
    if placed.is_empty() && !volumes.is_empty() {
        return None;
    }
    let list = |p: &'_ Value, k: &str| -> Vec<Value> {
        p.get(k).and_then(Value::as_array).cloned().unwrap_or_default()
    };
    let mut out = Vec::new();
    for (v, p) in placed {
        let here: Vec<Value> = list(p, "drives").into_iter().filter(|e| placed_on(e, d)).collect();
        if here.is_empty() {
            continue;
        }
        let slabs: Vec<Value> = list(p, "slabs").into_iter().filter(|s| placed_on(s, d)).collect();
        let state = WORST
            .into_iter()
            .find(|w| slabs.iter().any(|s| str_of(s, "state") == *w))
            .unwrap_or("ok");
        let id = str_of(v, "id").to_string();
        let name = Some(str_of(v, "name")).filter(|n| !n.is_empty()).map_or_else(|| id.clone(), str::to_string);
        let consumer = v.get("consumer").and_then(|c| {
            let (kind, name) = (str_of(c, "kind"), str_of(c, "name"));
            (!kind.is_empty() && !name.is_empty()).then(|| Consumer {
                kind: kind.to_string(),
                namespace: str_of(c, "namespace").to_string(),
                name: name.to_string(),
            })
        });
        let legs = p.get("legs");
        let word = |k: &str| legs.map(|l| str_of(l, k)).filter(|s| !s.is_empty()).map(str::to_string);
        out.push(DriveVolume {
            id,
            name,
            kind: Some(str_of(v, "kind")).filter(|k| !k.is_empty()).unwrap_or("volume").to_string(),
            consumer,
            bytes: here.iter().map(|e| u64_of(e, "bytes")).sum(),
            legs: here.iter().map(|e| u64_of(e, "legs")).sum(),
            shared_legs: slabs.iter().map(|s| u64_of(s, "shared_legs")).sum(),
            state: state.to_string(),
            rebuild: Some(str_of(p, "rebuild")).filter(|r| !r.is_empty()).unwrap_or("none").to_string(),
            policy: word("policy"),
            health: word("health"),
        });
    }
    // Largest first: the volume that loses most when this drive goes.
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.id.cmp(&b.id)));
    Some(out)
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
        committed_bytes: slab.get("committed_bytes").and_then(Value::as_u64),
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
    let committed: Option<u64> = mine.iter().map(|s| s.committed_bytes).sum();
    Usage {
        capacity_bytes: capacity,
        slabs: mine,
        in_slabs_bytes: in_slabs,
        used_bytes: used,
        free_in_slabs_bytes: free_in_slabs,
        outside_slabs_bytes: capacity.saturating_sub(in_slabs),
        free_bytes: capacity.saturating_sub(used),
        promisable_bytes: 0,
        committed_bytes: committed,
        headroom_bytes: None,
        collected_at: now,
        volumes: None,
        volumes_collected_at: None,
    }
    .priced(d.overcommit)
}

/// One monitor tick's usage: from the slab listing, with the volumes from
/// the placement listing when it answered (`volumes`), else the last
/// answer's volumes and their time.
pub fn refresh(d: &Drive, slabs: &[Value], volumes: Option<&[Value]>, last: Option<Usage>, now: SystemTime) -> Usage {
    let mut u = compute(d, slabs, now);
    match volumes {
        Some(vs) => {
            u.volumes = volumes_on(d, vs);
            u.volumes_collected_at = u.volumes.as_ref().map(|_| now);
        }
        None => {
            if let Some(last) = last {
                u.volumes = last.volumes;
                u.volumes_collected_at = last.volumes_collected_at;
            }
        }
    }
    u
}

impl Usage {
    /// Promisable and headroom under this overcommit setting — again when
    /// an operator changes it, without waiting for the engine.
    pub fn priced(mut self, oc: Overcommit) -> Self {
        self.promisable_bytes = (self.in_slabs_bytes as f64 * oc.factor()) as u64;
        self.headroom_bytes = self.committed_bytes.map(|c| self.promisable_bytes.saturating_sub(c));
        self
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
    fn promisable_follows_the_overcommit_ratio_and_committed_needs_every_slab() {
        let mut d = drive("S1", None, "/dev/sdc", 200 * SLOT);
        let on = json!({ "serial": "S1", "path": "/dev/sdc" });
        let mut a = slab("a", "data", 100, 10, on.clone());
        let mut b = slab("b", "data", 50, 5, on);
        let u = compute(&d, &[a.clone(), b.clone()], SystemTime::UNIX_EPOCH);
        assert_eq!(u.promisable_bytes, 150 * SLOT, "off: promise only what the slabs hold");
        assert_eq!((u.committed_bytes, u.headroom_bytes), (None, None), "no engine figure, no guess");

        a["committed_bytes"] = json!(200 * SLOT);
        let u = compute(&d, &[a.clone(), b.clone()], SystemTime::UNIX_EPOCH);
        assert_eq!(u.committed_bytes, None, "one slab without a figure leaves the total unknown");

        b["committed_bytes"] = json!(40 * SLOT);
        let u = compute(&d, &[a.clone(), b.clone()], SystemTime::UNIX_EPOCH);
        assert_eq!(u.committed_bytes, Some(240 * SLOT));
        assert_eq!(u.headroom_bytes, Some(0), "over-promised reads as no headroom, not a wrap");

        d.overcommit = Overcommit::new(true, Some(2.0)).unwrap();
        let u = compute(&d, &[a, b], SystemTime::UNIX_EPOCH);
        assert_eq!(u.promisable_bytes, 300 * SLOT);
        assert_eq!(u.headroom_bytes, Some(60 * SLOT));
        let off = u.priced(Overcommit::default());
        assert_eq!((off.promisable_bytes, off.headroom_bytes), (150 * SLOT, Some(0)));
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

    /// A volume as stormblock v17.1+ lists it with `?placement=true`.
    fn vol(id: &str, bytes: u64, drives: Value, slabs: Value) -> Value {
        json!({ "id": id, "name": format!("{id}-name"), "kind": "volume", "in_use": true,
                "consumer": { "kind": "PersistentVolumeClaim", "namespace": "shop", "name": "db" },
                "size_bytes": bytes,
                "placement": { "drives": drives, "slabs": slabs, "rebuild": "none",
                               "legs": { "policy": "mirror2", "health": "healthy", "extents": 4,
                                         "expected": 8, "missing": 0, "unreadable": 0, "failed_slabs": [] } } })
    }

    #[test]
    fn each_drive_lists_the_volumes_on_it_largest_first() {
        let a = drive("SN1", Some("naa.1"), "/dev/sda", 100 * SLOT);
        let b = drive("SN2", None, "/dev/sdb", 100 * SLOT);
        let sn1 = json!({ "serial": "SN1", "wwn": "naa.1", "model": "M", "path": "/dev/sda" });
        let sn2 = json!({ "serial": "SN2", "model": "M", "path": "/dev/sdb" });
        let big = vol(
            "big",
            8 * SLOT,
            json!([{ "drive": sn1, "node": "n1", "slabs": 1, "legs": 8, "bytes": 8 * SLOT },
                   { "drive": sn2, "node": "n1", "slabs": 1, "legs": 8, "bytes": 8 * SLOT }]),
            json!([{ "id": "s1", "drive": sn1, "state": "ok", "legs": 8, "shared_legs": 3, "bytes": 8 * SLOT },
                   { "id": "s2", "drive": sn2, "state": "draining", "legs": 8, "shared_legs": 0, "bytes": 8 * SLOT,
                     "drain": { "state": "running", "moved": 1, "remaining": 7, "failed": 0 } }]),
        );
        let mut small = vol(
            "small",
            SLOT,
            json!([{ "drive": sn1, "node": "n1", "slabs": 1, "legs": 1, "bytes": SLOT }]),
            json!([{ "id": "s1", "drive": sn1, "state": "ok", "legs": 1, "shared_legs": 0, "bytes": SLOT }]),
        );
        small.as_object_mut().unwrap().remove("consumer");
        let vs = [small, big];

        let on_a = volumes_on(&a, &vs).unwrap();
        assert_eq!(on_a.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), ["big", "small"]);
        let v = &on_a[0];
        assert_eq!((v.bytes, v.legs, v.shared_legs), (8 * SLOT, 8, 3));
        assert_eq!((v.name.as_str(), v.kind.as_str(), v.state.as_str()), ("big-name", "volume", "ok"));
        assert_eq!(v.consumer.as_ref().map(|c| (c.kind.as_str(), c.namespace.as_str(), c.name.as_str())),
                   Some(("PersistentVolumeClaim", "shop", "db")));
        assert_eq!((v.policy.as_deref(), v.health.as_deref(), v.rebuild.as_str()), (Some("mirror2"), Some("healthy"), "none"));
        assert_eq!(on_a[1].consumer, None);

        let on_b = volumes_on(&b, &vs).unwrap();
        assert_eq!(on_b.len(), 1);
        assert_eq!(on_b[0].state, "draining");

        let elsewhere = drive("SN3", None, "/dev/sdc", 100 * SLOT);
        assert_eq!(volumes_on(&elsewhere, &vs), Some(vec![]), "placement known, nothing here");
    }

    #[test]
    fn the_worst_slab_state_on_the_drive_is_the_volumes_state_there() {
        let d = drive("SN1", None, "/dev/sda", 100 * SLOT);
        let r = json!({ "serial": "SN1", "path": "/dev/sda" });
        let v = vol(
            "v",
            2,
            json!([{ "drive": r, "legs": 2, "bytes": 2 }]),
            json!([{ "id": "a", "drive": r, "state": "quarantined" }, { "id": "b", "drive": r, "state": "failed" }]),
        );
        assert_eq!(volumes_on(&d, &[v]).unwrap()[0].state, "failed");
    }

    #[test]
    fn no_placement_is_not_reported_and_no_volumes_is_empty() {
        let d = drive("SN1", None, "/dev/sda", 100 * SLOT);
        assert_eq!(volumes_on(&d, &[json!({ "id": "v1", "kind": "volume" })]), None, "engine before v17.1");
        assert_eq!(volumes_on(&d, &[]), Some(vec![]), "an engine with no volumes");
        let u = compute(&d, &[], SystemTime::UNIX_EPOCH);
        let j = serde_json::to_value(&u).unwrap();
        assert!(j.get("volumes").is_none(), "absent, not empty, until reported");
    }

    #[test]
    fn a_failed_placement_read_keeps_the_last_volumes_and_their_time() {
        let d = drive("SN1", None, "/dev/sda", 100 * SLOT);
        let r = json!({ "serial": "SN1", "path": "/dev/sda" });
        let vs = [vol("v", 5, json!([{ "drive": r, "legs": 1, "bytes": 5 }]), json!([]))];
        let t0 = SystemTime::UNIX_EPOCH;
        let t1 = t0 + std::time::Duration::from_secs(60);
        let first = refresh(&d, &[], Some(&vs), None, t0);
        assert_eq!(first.volumes.as_ref().map(Vec::len), Some(1));
        assert_eq!(first.volumes_collected_at, Some(t0));
        let kept = refresh(&d, &[], None, Some(first.clone()), t1);
        assert_eq!((kept.collected_at, kept.volumes_collected_at), (t1, Some(t0)), "the slabs are new, the volumes are not");
        assert_eq!(kept.volumes, first.volumes);
        let old_engine = refresh(&d, &[], Some(&[json!({ "id": "v" })]), Some(first), t1);
        assert_eq!((old_engine.volumes, old_engine.volumes_collected_at), (None, None));
    }

    #[test]
    fn a_fabric_drive_with_the_same_serial_is_another_nodes() {
        let d = drive("SN1", None, "/dev/nvme0n1", 100 * SLOT);
        let remote = json!({ "serial": "SN1", "path": "nvme-tcp://10.0.0.2:4420/nqn.x" });
        let v = vol("v", 1, json!([{ "drive": remote, "legs": 1, "bytes": 1 }]), json!([]));
        assert_eq!(volumes_on(&d, &[v]), Some(vec![]));
    }

    #[test]
    fn a_drive_without_slabs_is_all_free() {
        let d = drive("S1", None, "/dev/sdc", 4 * SLOT);
        let u = compute(&d, &[], SystemTime::UNIX_EPOCH);
        assert_eq!((u.used_bytes, u.free_bytes, u.outside_slabs_bytes), (0, 4 * SLOT, 4 * SLOT));
        assert!(u.slabs.is_empty());
    }
}
