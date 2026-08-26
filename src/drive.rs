//! The drive model: stable identity, kind, location, state, and health.
//!
//! Identity is the part stormblock got wrong (a fresh v4 UUID on every
//! open): a `DriveId` here is uuid5 of the WWID — or model+serial when the
//! device has no WWID — so it survives reboots, re-opens, and /dev path
//! changes.

use serde::{Deserialize, Serialize};
use std::time::SystemTime;
use uuid::Uuid;

/// Fixed namespace for uuid5 drive-id derivation. Never change this value:
/// every persisted DriveId depends on it.
pub const DRIVE_ID_NS: Uuid = Uuid::from_bytes([
    0x53, 0x74, 0x6f, 0x72, 0x6d, 0x44, 0x72, 0x69, 0x76, 0x65, 0x2e, 0x69, 0x64, 0x2e, 0x76,
    0x31,
]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DriveId(pub Uuid);

impl DriveId {
    /// Derive the stable id. WWID wins when present; model+serial is the
    /// fallback for devices that expose no WWID.
    pub fn derive(wwid: Option<&str>, model: &str, serial: &str) -> Self {
        let key = match wwid {
            Some(w) if !w.trim().is_empty() => w.trim().to_string(),
            _ => format!("{}:{}", model.trim(), serial.trim()),
        };
        DriveId(Uuid::new_v5(&DRIVE_ID_NS, key.as_bytes()))
    }
}

impl std::fmt::Display for DriveId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriveKind {
    NvmeSsd,
    SasSsd,
    SasHdd,
    SataSsd,
    SataHdd,
    Unknown,
}

impl DriveKind {
    /// Default stormblock slab tier for this kind of drive.
    pub fn default_tier(self) -> &'static str {
        match self {
            DriveKind::NvmeSsd => "hot",
            DriveKind::SasSsd | DriveKind::SataSsd => "warm",
            DriveKind::SasHdd | DriveKind::SataHdd => "cool",
            DriveKind::Unknown => "cool",
        }
    }

    pub fn is_ssd(self) -> bool {
        matches!(
            self,
            DriveKind::NvmeSsd | DriveKind::SasSsd | DriveKind::SataSsd
        )
    }
}

/// The HBA a drive hangs off. Multiple controllers per node is the normal
/// case on a shelf rig.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Controller {
    /// hostN (SCSI); the grouping key.
    pub scsi_host: Option<String>,
    pub pcie_addr: Option<String>,
    /// Kernel driver (mpt3sas, nvme, …).
    pub driver: Option<String>,
}

/// A SAS shelf (SES enclosure), e.g. a NetApp DS4246. Identity comes from
/// the SES processor's SCSI device; `serial` (VPD 0x80) is the canonical
/// shelf key — a dual-IOM shelf shows up as two enclosure devices with two
/// ids but one serial.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shelf {
    /// sysfs enclosure id (e.g. "1:0:8:0") — per-IOM, not canonical.
    pub id: Option<String>,
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub sas_address: Option<String>,
}

impl Shelf {
    /// The stable key for grouping: serial when known, else the sysfs id.
    pub fn key(&self) -> Option<String> {
        self.serial.clone().or_else(|| self.id.clone())
    }

    pub fn display(&self) -> String {
        match (&self.model, &self.serial, &self.id) {
            (Some(m), Some(s), _) => format!("{m} {s}"),
            (Some(m), None, Some(i)) => format!("{m} {i}"),
            (Some(m), None, None) => m.clone(),
            (None, _, Some(i)) => i.clone(),
            _ => "shelf".into(),
        }
    }
}

/// Where the drive physically is: controller → shelf → bay. Every field
/// optional — populated with whatever the platform exposes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    #[serde(default)]
    pub controller: Option<Controller>,
    #[serde(default)]
    pub shelf: Option<Shelf>,
    pub bay: Option<u32>,
    /// The drive's own SAS address.
    pub sas_address: Option<String>,
    /// NVMe drives: the namespace's controller BDF / physical slot.
    pub pcie_addr: Option<String>,
    pub pcie_slot: Option<String>,
}

impl Location {
    /// Failure-domain labels for placement, in stormblock topology shape.
    pub fn labels(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(sh) = &self.shelf {
            if let Some(k) = sh.key() {
                out.push(("shelf".into(), k));
            }
        }
        if let Some(b) = self.bay {
            out.push(("bay".into(), b.to_string()));
        }
        if let Some(c) = &self.controller {
            if let Some(h) = &c.scsi_host {
                out.push(("hba".into(), h.clone()));
            }
        }
        if let Some(s) = &self.pcie_slot {
            out.push(("pcie_slot".into(), s.clone()));
        }
        out
    }
}

/// Is the drive handed to stormblock? Orthogonal to designation and
/// activity: a spare or even an operator-failed drive can still be in the
/// fleet (until drained), and a reserved drive can sit out of it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Membership {
    /// Not registered with stormblock.
    #[default]
    Out,
    /// Registered with stormblock (in its drive list and/or carrying a slab).
    Fleet,
}

/// Operator-set label. Applies both in fleet and out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Designation {
    #[default]
    None,
    /// Held back on purpose — not to be joined or consumed.
    Reserved,
    /// Standing by as a replacement.
    Spare,
    /// Operator declared it bad (health can also conclude this on its own).
    Failed,
}

/// What the drive is doing right now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    #[default]
    Idle,
    /// A drive test is running (see test.rs).
    Testing,
    /// Being evacuated ahead of removal (needs stormblock#70 to automate).
    Draining,
    /// Inventory remembers it; the node cannot see it.
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Unknown,
    Good,
    Warning,
    Failing,
    Failed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HealthReport {
    pub status: Option<HealthStatus>,
    pub temperature_c: Option<i32>,
    pub power_on_hours: Option<u64>,
    pub media_errors: u64,
    pub available_spare_pct: Option<u8>,
    /// NVMe percentage-used / SSD endurance-used. May exceed 100 per spec.
    pub wear_pct: Option<u8>,
    /// NVMe critical-warning bitfield; 0 for non-NVMe.
    pub critical_warning: u8,
    pub messages: Vec<String>,
    pub collected_at: Option<SystemTime>,
}

impl HealthReport {
    pub fn status(&self) -> HealthStatus {
        self.status.unwrap_or(HealthStatus::Unknown)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Drive {
    pub id: DriveId,
    /// Primary /dev node. May change across boots; `id` does not.
    pub path: String,
    /// Kernel name of the primary path (sda, nvme0n1).
    pub name: String,
    /// Every /dev node this physical drive answers on. Dual-IOM shelves
    /// present two; `path` is always the first (sorted). Includes `path`.
    #[serde(default)]
    pub paths: Vec<String>,
    pub kind: DriveKind,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub wwid: Option<String>,
    pub capacity_bytes: u64,
    pub block_size: u32,
    #[serde(default)]
    pub location: Location,
    #[serde(default)]
    pub membership: Membership,
    #[serde(default)]
    pub designation: Designation,
    #[serde(default)]
    pub activity: Activity,
    #[serde(default)]
    pub health: HealthReport,
    pub first_seen: SystemTime,
    pub last_seen: SystemTime,
}

impl Drive {
    /// Why this drive cannot join the fleet right now, if it can't.
    pub fn fleet_join_blocker(&self) -> Option<String> {
        if self.membership == Membership::Fleet {
            return Some("already in the fleet".into());
        }
        if self.designation == Designation::Failed {
            return Some("designated failed".into());
        }
        if self.designation == Designation::Reserved {
            return Some("designated reserved".into());
        }
        if self.activity != Activity::Idle {
            return Some(format!("activity is {:?}", self.activity));
        }
        if self.health.status() >= HealthStatus::Failing {
            return Some(format!("health is {:?}", self.health.status()));
        }
        None
    }

    /// Destructive tests are only allowed on drives that are out of the
    /// fleet and present.
    pub fn destructive_test_blocker(&self) -> Option<String> {
        if self.membership == Membership::Fleet {
            return Some("in the fleet — destructive tests need an out-of-fleet drive".into());
        }
        if self.activity != Activity::Idle {
            return Some(format!("activity is {:?}", self.activity));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_id_is_stable_and_prefers_wwid() {
        let a = DriveId::derive(Some("naa.5000c500a1b2c3d4"), "X", "Y");
        let b = DriveId::derive(Some("naa.5000c500a1b2c3d4"), "OTHER", "OTHER");
        assert_eq!(a, b, "wwid alone determines the id");

        let c = DriveId::derive(None, "Micron_7450", "S1234");
        let d = DriveId::derive(None, "Micron_7450", "S1234");
        let e = DriveId::derive(None, "Micron_7450", "S9999");
        assert_eq!(c, d);
        assert_ne!(c, e);
    }

    #[test]
    fn drive_id_ignores_blank_wwid_and_whitespace() {
        let a = DriveId::derive(Some("  "), "M", "S");
        let b = DriveId::derive(None, " M ", " S ");
        assert_eq!(a, b);
    }

    #[test]
    fn default_tiers() {
        assert_eq!(DriveKind::NvmeSsd.default_tier(), "hot");
        assert_eq!(DriveKind::SasSsd.default_tier(), "warm");
        assert_eq!(DriveKind::SataHdd.default_tier(), "cool");
    }

    #[test]
    fn health_status_orders_by_severity() {
        assert!(HealthStatus::Failed > HealthStatus::Failing);
        assert!(HealthStatus::Failing > HealthStatus::Warning);
        assert!(HealthStatus::Warning > HealthStatus::Good);
    }

    fn base_drive() -> Drive {
        Drive {
            id: DriveId::derive(None, "M", "S"),
            path: "/dev/sdx".into(),
            name: "sdx".into(),
            paths: vec!["/dev/sdx".into()],
            kind: DriveKind::SataSsd,
            model: "M".into(),
            serial: "S".into(),
            firmware: "1".into(),
            wwid: None,
            capacity_bytes: 1 << 30,
            block_size: 512,
            location: Location::default(),
            membership: Membership::Out,
            designation: Designation::None,
            activity: Activity::Idle,
            health: HealthReport::default(),
            first_seen: SystemTime::now(),
            last_seen: SystemTime::now(),
        }
    }

    #[test]
    fn fleet_join_blockers() {
        assert!(base_drive().fleet_join_blocker().is_none());

        let mut d = base_drive();
        d.membership = Membership::Fleet;
        assert!(d.fleet_join_blocker().is_some());

        let mut d = base_drive();
        d.designation = Designation::Failed;
        assert!(d.fleet_join_blocker().is_some());
        d.designation = Designation::Reserved;
        assert!(d.fleet_join_blocker().is_some());
        d.designation = Designation::Spare;
        assert!(d.fleet_join_blocker().is_none(), "a spare may be pressed into service");

        let mut d = base_drive();
        d.activity = Activity::Testing;
        assert!(d.fleet_join_blocker().is_some());

        let mut d = base_drive();
        d.health.status = Some(HealthStatus::Failing);
        assert!(d.fleet_join_blocker().is_some());
        d.health.status = Some(HealthStatus::Warning);
        assert!(d.fleet_join_blocker().is_none());
    }

    #[test]
    fn destructive_test_blockers() {
        assert!(base_drive().destructive_test_blocker().is_none());
        let mut d = base_drive();
        d.membership = Membership::Fleet;
        assert!(d.destructive_test_blocker().is_some());
        let mut d = base_drive();
        d.activity = Activity::Missing;
        assert!(d.destructive_test_blocker().is_some());
    }

    #[test]
    fn location_labels() {
        let loc = Location {
            shelf: Some(Shelf {
                id: Some("1:0:8:0".into()),
                serial: Some("SHFSN1".into()),
                model: Some("DS4246".into()),
                ..Default::default()
            }),
            bay: Some(7),
            controller: Some(Controller {
                scsi_host: Some("host7".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let labels = loc.labels();
        assert!(labels.contains(&("shelf".into(), "SHFSN1".into())), "serial beats sysfs id");
        assert!(labels.contains(&("bay".into(), "7".into())));
        assert!(labels.contains(&("hba".into(), "host7".into())));
    }

    #[test]
    fn shelf_key_prefers_serial_and_display_reads_well() {
        let sh = Shelf {
            id: Some("1:0:8:0".into()),
            serial: Some("SN9".into()),
            model: Some("DS4246".into()),
            ..Default::default()
        };
        assert_eq!(sh.key(), Some("SN9".into()));
        assert_eq!(sh.display(), "DS4246 SN9");
        let bare = Shelf {
            id: Some("1:0:8:0".into()),
            ..Default::default()
        };
        assert_eq!(bare.key(), Some("1:0:8:0".into()));
        assert_eq!(bare.display(), "1:0:8:0");
    }
}
