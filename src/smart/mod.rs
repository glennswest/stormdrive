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
    /// Errors the drive itself counts against its media: NVMe log 0x02,
    /// ATA attribute 187 (reported uncorrectable). 0 when it reports none.
    pub media_errors: u64,
    /// Commands the kernel saw fail on the device since boot (sysfs
    /// `ioerr_cnt`, SCSI/SATA). Resets and aborts count too, so this is
    /// not media errors and its growth alone is no warning (#58).
    pub io_errors: Option<u64>,
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
    /// A SATA drive's whole SMART attribute table (#64): kept in the drive
    /// history, not served with health.
    pub ata_attributes: Vec<AtaAttribute>,
}

/// One ATA SMART attribute as the drive reports it (#64).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AtaAttribute {
    pub id: u8,
    /// Pre-fail (else old-age).
    pub prefail: bool,
    pub value: u8,
    pub worst: u8,
    /// READ THRESHOLDS; 0 when the drive gave none.
    pub threshold: u8,
    /// The 48-bit raw value.
    pub raw: u64,
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
    /// ATA 187: errors the drive could not correct on a host read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_uncorrectable: Option<u64>,
    /// ATA 199: interface (UDMA) CRC errors — cabling or backplane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crc_errors: Option<u64>,
    /// SAS error counter log pages (#64): Write (0x02), Read (0x03) and
    /// Verify (0x05) errors, as the drive counts them over its life.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_errors: Option<ErrorCounters>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_errors: Option<ErrorCounters>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_errors: Option<ErrorCounters>,
    /// SAS: entries in the grown defect list (READ DEFECT DATA(12), GLIST):
    /// blocks the drive has remapped since it left the factory (#81).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grown_defects: Option<u64>,
}

/// One SCSI error counter page (SBC: parameters 0003h, 0005h, 0006h).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ErrorCounters {
    /// Total errors corrected (0003h).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corrected: Option<u64>,
    /// Total errors the drive could not correct (0006h): media errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncorrected: Option<u64>,
    /// Total bytes processed (0005h).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
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
    /// Host read / write commands completed (#64).
    #[serde(default)]
    pub host_read_commands: u64,
    #[serde(default)]
    pub host_write_commands: u64,
    /// Minutes the controller was busy with I/O.
    #[serde(default)]
    pub controller_busy_minutes: u64,
    /// Minutes over the warning / critical composite temperature.
    #[serde(default)]
    pub warning_temp_minutes: u64,
    #[serde(default)]
    pub critical_temp_minutes: u64,
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
