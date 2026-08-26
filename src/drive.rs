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

/// Where the drive physically is. Every field optional — populated with
/// whatever the platform exposes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub pcie_addr: Option<String>,
    pub pcie_slot: Option<String>,
    pub sas_address: Option<String>,
    pub enclosure: Option<String>,
    pub bay: Option<u32>,
    pub scsi_host: Option<String>,
}

impl Location {
    /// Failure-domain labels for placement, in stormblock topology shape.
    pub fn labels(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(e) = &self.enclosure {
            out.push(("enclosure".into(), e.clone()));
        }
        if let Some(b) = self.bay {
            out.push(("bay".into(), b.to_string()));
        }
        if let Some(h) = &self.scsi_host {
            out.push(("hba".into(), h.clone()));
        }
        if let Some(s) = &self.pcie_slot {
            out.push(("pcie_slot".into(), s.clone()));
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriveState {
    /// Seen and identified, not yet offered anywhere.
    Discovered,
    /// Qualified and eligible for stormblock.
    Available,
    /// Registered with stormblock (in its drive list and/or carrying a slab).
    Active,
    /// Being evacuated ahead of retirement or failure.
    Draining,
    /// Deliberately withdrawn.
    Retired,
    /// Health said so.
    Failed,
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
    /// Current /dev node. May change across boots; `id` does not.
    pub path: String,
    /// Kernel name (sda, nvme0n1).
    pub name: String,
    pub kind: DriveKind,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub wwid: Option<String>,
    pub capacity_bytes: u64,
    pub block_size: u32,
    #[serde(default)]
    pub location: Location,
    pub state: DriveState,
    #[serde(default)]
    pub health: HealthReport,
    pub first_seen: SystemTime,
    pub last_seen: SystemTime,
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

    #[test]
    fn location_labels() {
        let loc = Location {
            enclosure: Some("enc0".into()),
            bay: Some(7),
            ..Default::default()
        };
        let labels = loc.labels();
        assert!(labels.contains(&("enclosure".into(), "enc0".into())));
        assert!(labels.contains(&("bay".into(), "7".into())));
    }
}
