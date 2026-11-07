//! Is a drive in use, and by what (#123)?
//!
//! The answer is read from what the drive record already holds — the slabs
//! found in its partition table (`slab_parts`, with the role from the
//! partition type GUID), a stormraid superblock (`in_use_by`), fleet
//! membership, a mounted partition, and the engine's own view (its volumes
//! on the drive, `usage.volumes`; which halves it runs from the network,
//! `engine_finding`) — never from the free-text `contents`.
//!
//! Every destructive verb refuses a drive in use and says why with
//! [`InUse::reason`]: format, sanitize, security erase, re-partition, the
//! destructive test, and the designations that contradict the use.

use crate::drive::{Drive, Membership};
use serde::Serialize;

/// What a drive is used for: the first that holds, in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Use {
    /// It holds this node's system slab (the root runs from it).
    System,
    /// A stormraid RAID set member.
    RaidMember,
    /// It holds a stormblock data slab.
    Data,
    /// The engine holds it otherwise: in the fleet, or a slab on the whole
    /// drive.
    SlabMember,
    /// A partition of it is mounted.
    Mounted,
    Free,
}

/// One slab on the drive, by name and role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UseSlab {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partition: Option<usize>,
    pub name: String,
    /// `system`, `data`, or `unknown` (a whole-drive slab).
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InUse {
    pub in_use: bool,
    #[serde(rename = "use")]
    pub use_: Use,
    pub slabs: Vec<UseSlab>,
    /// What stops working if the drive is destroyed: the node's root, its
    /// data half, the engine's volumes on it, a RAID set.
    pub depends_on: Vec<String>,
    /// The sentence a refusal carries: "sda holds this node's system slab
    /// (partition 2 'stormblock'); the node runs from it". None when free.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

fn slab_label(s: &UseSlab) -> String {
    match (s.partition, s.name.is_empty()) {
        (Some(p), false) => format!("partition {p} '{}'", s.name),
        (Some(p), true) => format!("partition {p}"),
        (None, _) => "the whole drive".into(),
    }
}

/// The drive's use (#123).
pub fn of(d: &Drive) -> InUse {
    let slabs: Vec<UseSlab> = d
        .slab_parts
        .iter()
        .map(|p| UseSlab { partition: p.partition, name: p.name.clone(), role: p.role.clone() })
        .collect();
    let raid = d.in_use_by.as_deref().is_some_and(|w| w.starts_with("stormraid"));
    let remote = |role: &str| d.engine_finding.as_ref().is_some_and(|f| f.roles.iter().any(|r| r == role));
    let of_role = |role: &str| slabs.iter().filter(|s| s.role == role).map(slab_label).collect::<Vec<_>>();
    let system = of_role("system");
    let data = of_role("data");

    let mut depends_on = vec![];
    if !system.is_empty() {
        depends_on.push(if remote("system") {
            "this node's system slab (the engine runs the system half from the network today)".to_string()
        } else {
            "this node's root (the system slab)".to_string()
        });
    }
    if !data.is_empty() {
        depends_on.push(if remote("data") {
            "this node's data slab (the engine runs the data half from the network today)".to_string()
        } else {
            "this node's data half (the data slab)".to_string()
        });
    }
    if raid {
        depends_on.push("a stormraid RAID set".into());
    }
    if let Some(vols) = d.usage.as_ref().and_then(|u| u.volumes.as_ref()).filter(|v| !v.is_empty()) {
        let names: Vec<&str> = vols.iter().take(3).map(|v| v.name.as_str()).collect();
        let more = if vols.len() > names.len() { format!(", … {} more", vols.len() - names.len()) } else { String::new() };
        depends_on.push(format!("{} volume(s): {}{more}", vols.len(), names.join(", ")));
    }

    let plural = |v: &[String]| if v.len() == 1 { "" } else { "s" };
    let (use_, reason) = if !system.is_empty() {
        let runs = if remote("system") { "the engine runs the system half from the network today, but the slab is the node's" } else { "the node runs from it" };
        (Use::System, Some(format!("{} holds this node's system slab ({}); {runs}", d.name, system.join(", "))))
    } else if raid {
        (Use::RaidMember, Some(format!("{} is a member of a stormraid RAID set", d.name)))
    } else if !data.is_empty() {
        (Use::Data, Some(format!("{} holds this node's data slab{} ({})", d.name, plural(&data), data.join(", "))))
    } else if d.membership == Membership::Fleet || d.in_use_by.is_some() {
        let what = if d.membership == Membership::Fleet { "is in the fleet: the engine places data on it".to_string() } else { format!("holds data for {}", d.in_use_by.as_deref().unwrap_or("stormblock")) };
        (Use::SlabMember, Some(format!("{} {what}", d.name)))
    } else if d.mounted {
        (Use::Mounted, Some(format!("{} has mounted partitions", d.name)))
    } else {
        (Use::Free, None)
    };
    let mut reason = reason;
    if let Some(r) = reason.as_mut() {
        let vols = depends_on.iter().find(|x| x.contains("volume(s)"));
        if let Some(v) = vols {
            r.push_str(&format!("; {v}"));
        }
    }
    InUse { in_use: use_ != Use::Free, use_, slabs, depends_on, reason }
}

/// What a fresh read of the disk found (`contents::holds`), as a use: a
/// slab or a RAID superblock on it is in use even before the record knows.
pub fn fresh_reason(name: &str, holds: Option<&str>) -> Option<String> {
    let h = holds?;
    if h.contains("stormblock slab") {
        return Some(format!("{name} holds a stormblock slab ({h})"));
    }
    if h.contains("stormraid member") {
        return Some(format!("{name} is a member of a stormraid RAID set ({h})"));
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::contents::SlabPart;
    use crate::drive::Designation;

    fn part(n: usize, name: &str, role: &str) -> SlabPart {
        SlabPart { partition: Some(n), name: name.into(), role: role.into(), offset_bytes: n as u64 * (1 << 30) }
    }

    /// forge1 as it read live on 0.39.0 (2026-11-07): sda the system drive
    /// (ESP, system slab, two data slabs, in the fleet), sdb–sdd blank and
    /// out of the fleet. Serials are made up.
    pub(crate) fn forge1() -> Vec<Drive> {
        let mut sda = Drive::test_fixture("sda");
        sda.membership = Membership::Fleet;
        sda.in_use_by = Some("stormblock (slabs in partitions 2 'stormblock', 3 'stormblock-data', 4 'stormblock-bulk')".into());
        sda.contents = Some("vfat in partition 1 'EFI', stormblock slab in partition 2 'stormblock', stormblock slab in partition 3 'stormblock-data', stormblock slab in partition 4 'stormblock-bulk'".into());
        sda.slab_parts = vec![part(2, "stormblock", "system"), part(3, "stormblock-data", "data"), part(4, "stormblock-bulk", "data")];
        let mut out = vec![sda];
        for n in ["sdb", "sdc", "sdd"] {
            out.push(Drive::test_fixture(n));
        }
        out
    }

    #[test]
    fn forge1_sda_is_the_system_drive_and_the_rest_are_free() {
        let f = forge1();
        let u = of(&f[0]);
        assert!(u.in_use);
        assert_eq!(u.use_, Use::System);
        assert_eq!(u.slabs.len(), 3);
        assert_eq!(u.slabs[0], UseSlab { partition: Some(2), name: "stormblock".into(), role: "system".into() });
        assert_eq!(
            u.reason.as_deref(),
            Some("sda holds this node's system slab (partition 2 'stormblock'); the node runs from it")
        );
        assert_eq!(u.depends_on, vec!["this node's root (the system slab)", "this node's data half (the data slab)"]);
        let v = serde_json::to_value(&u).unwrap();
        assert_eq!(v["use"], "system");
        assert_eq!(v["in_use"], true);
        for d in &f[1..] {
            let u = of(d);
            assert!(!u.in_use, "{}", d.name);
            assert_eq!(u.use_, Use::Free);
            assert!(u.reason.is_none() && u.slabs.is_empty() && u.depends_on.is_empty());
        }
    }

    #[test]
    fn each_use_in_order() {
        let mut d = Drive::test_fixture("sdx");
        d.slab_parts = vec![part(3, "stormblock-data", "data")];
        assert_eq!(of(&d).use_, Use::Data);
        assert!(of(&d).reason.unwrap().contains("data slab (partition 3 'stormblock-data')"));
        d.in_use_by = Some(crate::contents::STORMRAID_HOLDER.into());
        assert_eq!(of(&d).use_, Use::RaidMember, "a RAID member before a data slab");
        let mut w = Drive::test_fixture("sdy");
        w.in_use_by = Some("stormblock (a slab on the whole drive)".into());
        w.slab_parts = vec![SlabPart { partition: None, name: String::new(), role: "unknown".into(), offset_bytes: 0 }];
        assert_eq!(of(&w).use_, Use::SlabMember);
        assert!(of(&w).reason.unwrap().contains("holds data for stormblock"));
        let mut fl = Drive::test_fixture("sdz");
        fl.membership = Membership::Fleet;
        assert_eq!(of(&fl).use_, Use::SlabMember);
        let mut m = Drive::test_fixture("sdm");
        m.mounted = true;
        assert_eq!(of(&m).use_, Use::Mounted);
        // A designation alone is not a use: a blank drive designated system
        // carries nothing yet.
        let mut s = Drive::test_fixture("sds");
        s.designation = Designation::System;
        assert_eq!(of(&s).use_, Use::Free);
    }

    #[test]
    fn remote_halves_and_fresh_reads() {
        let mut d = forge1().remove(0);
        d.engine_finding = Some(crate::engine::EngineFinding {
            severity: crate::drive::HealthStatus::Warning,
            roles: vec!["system".into(), "data".into()],
            message: String::new(),
        });
        let u = of(&d);
        assert_eq!(u.use_, Use::System, "still the system drive: its slab is the node's");
        assert!(u.reason.unwrap().contains("from the network"));
        assert_eq!(fresh_reason("sdb", None), None);
        assert_eq!(fresh_reason("sdb", Some("xfs (whole drive)")), None, "foreign data is the destroy rule's");
        assert!(fresh_reason("sdb", Some("stormblock slab in partition 2")).unwrap().contains("stormblock slab"));
        assert!(fresh_reason("sdb", Some("stormraid member (whole drive)")).unwrap().contains("RAID"));
    }
}
