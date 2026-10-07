//! Health collection. One `Sample` shape regardless of transport; the
//! threshold engine in `monitor` turns samples into `HealthStatus`.

pub mod nvme;
pub mod scsi;

use crate::drive::{Drive, DriveKind};

/// One raw health observation.
#[derive(Debug, Clone, Default)]
pub struct Sample {
    pub temperature_c: Option<i32>,
    pub power_on_hours: Option<u64>,
    pub media_errors: u64,
    pub available_spare_pct: Option<u8>,
    pub wear_pct: Option<u8>,
    /// NVMe critical-warning bitfield (0 for non-NVMe).
    pub critical_warning: u8,
    /// Kernel still considers the device usable (`device/state` running,
    /// identify/log reads succeed).
    pub kernel_ok: bool,
    pub messages: Vec<String>,
    /// The rest of the NVMe SMART/Health log (None for SAS/SATA).
    pub nvme: Option<NvmeCounters>,
    /// What a SAS/SATA drive says itself (#22): LOG SENSE or ATA SMART.
    pub smart: Option<SmartCounters>,
}

/// A SAS/SATA drive's own health (#22). The sector counters come from ATA
/// SMART (None from LOG SENSE, which this does not read them from).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SmartCounters {
    /// `log_sense` or `ata_smart`.
    pub source: String,
    /// The drive predicts its own failure: why, in its words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicted_failure: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reallocated_sectors: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_sectors: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offline_uncorrectable: Option<u64>,
}

/// NVMe SMART/Health log (0x02) counters beyond what health decides on;
/// served on /metrics (#18).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NvmeCounters {
    pub available_spare_threshold_pct: u8,
    /// Data units are 1000 × 512 bytes; these are bytes.
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub power_cycles: u64,
    pub unsafe_shutdowns: u64,
    /// Error Information Log entries over the controller's life.
    pub error_log_entries: u64,
}

/// Collect a sample for one drive. Blocking (ioctls, sysfs reads) — call
/// from `spawn_blocking`.
pub fn collect(drive: &Drive) -> Sample {
    match drive.kind {
        DriveKind::NvmeSsd => nvme::collect(&drive.path, &drive.name),
        _ => scsi::collect(&drive.name, drive.kind.is_ssd()),
    }
}

/// NVMe critical-warning bits (NVMe spec, SMART/Health log byte 0).
pub mod crit {
    pub const SPARE_BELOW_THRESHOLD: u8 = 1 << 0;
    pub const TEMPERATURE: u8 = 1 << 1;
    pub const RELIABILITY_DEGRADED: u8 = 1 << 2;
    pub const READ_ONLY: u8 = 1 << 3;
    pub const VOLATILE_BACKUP_FAILED: u8 = 1 << 4;
}
