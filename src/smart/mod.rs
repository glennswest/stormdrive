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
}

/// Collect a sample for one drive. Blocking (ioctls, sysfs reads) — call
/// from `spawn_blocking`.
pub fn collect(drive: &Drive) -> Sample {
    match drive.kind {
        DriveKind::NvmeSsd => nvme::collect(&drive.path, &drive.name),
        _ => scsi::collect(&drive.name),
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
