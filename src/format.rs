//! Sector-size reformat: the way a NetApp 520-byte drive becomes a drive
//! Linux (and stormblock) can use.
//!
//! Per drive, on the blocking pool:
//! 1. READ CAPACITY(16) — where we start from.
//! 2. MODE SENSE(10) page 1 for the current block descriptor; MODE
//!    SELECT(10) with a descriptor carrying the new block length and a
//!    block count of 0 ("all"); MODE SELECT(6) if the drive rejects (10).
//! 3. FORMAT UNIT, FMTDATA + IMMED. If IMMED is refused, the blocking form
//!    with a day-long timeout.
//! 4. TEST UNIT READY every few seconds; the drive answers NOT READY
//!    04/04 with a progress indication until it is done.
//! 5. Kernel rescan (`device/rescan`; delete + targeted host scan if sd
//!    still sees 0 blocks), then READ CAPACITY again to verify.
//!
//! Many drives at once is the normal case (a shelf of them): each runs in
//! its own task, the drive does the work, the host only polls. There is
//! no cancel — a half-formatted drive is worse than a slow one.

use crate::api::AppState;
use crate::drive::{Activity, Drive, DriveId, FormatRecord};
use crate::events::Severity;
use crate::scsi::{self, Device, Error as ScsiError};
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FormatState {
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct FormatRun {
    pub drive: DriveId,
    pub name: String,
    pub from_block_size: u32,
    pub to_block_size: u32,
    pub state: FormatState,
    /// `prepare`, `mode_select`, `format`, `formatting`, `rescan`,
    /// `verify`, `done`.
    pub phase: String,
    /// 0..=100 while formatting, when the drive reports progress.
    pub progress_pct: Option<u8>,
    pub started: SystemTime,
    pub finished: Option<SystemTime>,
    pub error: Option<String>,
    /// Whether the drive took IMMED (progress is reportable).
    pub immediate: bool,
}

pub struct FormatHandle {
    pub run: Mutex<FormatRun>,
}

impl FormatHandle {
    fn set_phase(&self, phase: &str) {
        self.run.lock().unwrap().phase = phase.into();
    }
    fn set_progress(&self, pct: Option<u8>) {
        self.run.lock().unwrap().progress_pct = pct;
    }
}

/// Block sizes we will format to.
pub fn valid_target(block_size: u32) -> bool {
    crate::drive::USABLE_BLOCK_SIZES.contains(&block_size)
}

/// Progress from a TEST UNIT READY result while a format runs.
/// Ok(None) = done; Ok(Some(pct)) = still going; Err = the format failed
/// or the drive went away.
pub fn interpret_tur(r: &Result<(), ScsiError>) -> Result<Option<Option<u8>>, String> {
    match r {
        Ok(()) => Ok(None),
        Err(ScsiError::Sense(s)) if s.is_format_in_progress() => Ok(Some(s.progress_pct())),
        // NOT READY, becoming ready / in process of becoming ready.
        Err(ScsiError::Sense(s)) if s.key == 0x2 && s.asc == 0x04 && (s.ascq == 0x01 || s.ascq == 0x00) => {
            Ok(Some(None))
        }
        // Unit attention (reset, mode parameters changed) right after the
        // format is not a failure — ask again.
        Err(ScsiError::Sense(s)) if s.is_unit_attention() => Ok(Some(None)),
        // Format command failed / medium error: the drive says so with 31/01.
        Err(ScsiError::Sense(s)) if s.asc == 0x31 => Err(format!("format failed: {s}")),
        Err(e) => Err(format!("{e}")),
    }
}

const POLL: Duration = Duration::from_secs(5);
const MAX_WAIT: Duration = Duration::from_secs(48 * 60 * 60);

fn dev_path(drive: &Drive) -> String {
    scsi::sg_path_for_block(&drive.name).unwrap_or_else(|| drive.path.clone())
}

fn run_blocking(drive: &Drive, to: u32, handle: &FormatHandle) -> Result<u32, String> {
    handle.set_phase("prepare");
    let dev = Device::open(&dev_path(drive)).map_err(|e| format!("open: {e}"))?;
    // Clear a pending unit attention so it does not fail the first real
    // command.
    let _ = dev.test_unit_ready();
    let cap = dev.read_capacity16().map_err(|e| format!("read capacity: {e}"))?;
    if cap.block_len == to {
        tracing::info!(drive = %drive.name, "already {to}-byte; formatting anyway (operator asked)");
    }

    handle.set_phase("mode_select");
    let (medium, density) = match dev.mode_sense10(0x01) {
        Ok(h) => (
            h.medium_type,
            h.block_descriptor.as_ref().map(|bd| bd[0]).unwrap_or(0),
        ),
        Err(e) => {
            tracing::debug!(drive = %drive.name, "mode sense failed ({e}); using zeros");
            (0, 0)
        }
    };
    let ms10 = scsi::mode_select10_block_length(to, medium, density);
    if let Err(e10) = dev.mode_select10(&ms10) {
        let ms6 = scsi::mode_select6_block_length(to, medium, density);
        dev.mode_select6(&ms6)
            .map_err(|e6| format!("mode select rejected: (10) {e10}; (6) {e6}"))?;
    }

    handle.set_phase("format");
    let immediate = match dev.format_unit(true) {
        Ok(()) => true,
        Err(ScsiError::Sense(s)) if s.is_illegal_request() => {
            tracing::info!(drive = %drive.name, "IMMED refused ({s}); blocking format");
            handle.run.lock().unwrap().immediate = false;
            dev.format_unit(false).map_err(|e| format!("format unit: {e}"))?;
            false
        }
        Err(e) => return Err(format!("format unit: {e}")),
    };

    if immediate {
        handle.set_phase("formatting");
        let t0 = Instant::now();
        loop {
            std::thread::sleep(POLL);
            match interpret_tur(&dev.test_unit_ready())? {
                None => break,
                Some(p) => handle.set_progress(p),
            }
            if t0.elapsed() > MAX_WAIT {
                return Err("format did not finish within 48 h".into());
            }
        }
    }
    handle.set_progress(Some(100));

    handle.set_phase("rescan");
    for p in &drive.paths {
        let name = p.trim_start_matches("/dev/");
        rescan_block(name);
    }
    std::thread::sleep(Duration::from_secs(3));

    handle.set_phase("verify");
    let after = dev.read_capacity16().map_err(|e| format!("read capacity after format: {e}"))?;
    if after.block_len != to {
        return Err(format!(
            "drive reports {}-byte blocks after formatting to {to}",
            after.block_len
        ));
    }
    // sd may need a delete + re-add to drop its "unsupported sector size"
    // conclusion; do it per path when the rescan did not take.
    for p in &drive.paths {
        let name = p.trim_start_matches("/dev/");
        if sysfs_sectors(name) == 0 {
            readd_block(name);
        }
    }
    Ok(after.block_len)
}

#[cfg(target_os = "linux")]
fn sysfs_sectors(name: &str) -> u64 {
    std::fs::read_to_string(format!("/sys/block/{name}/size"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(not(target_os = "linux"))]
fn sysfs_sectors(_name: &str) -> u64 {
    0
}

#[cfg(target_os = "linux")]
fn rescan_block(name: &str) {
    if let Err(e) = std::fs::write(format!("/sys/block/{name}/device/rescan"), "1") {
        tracing::debug!(%name, "sd rescan: {e}");
    }
}

#[cfg(not(target_os = "linux"))]
fn rescan_block(_name: &str) {}

/// Delete the SCSI device and ask its host to scan that exact target
/// again. The drive comes back under whatever name is free; the
/// inventory follows it by WWID.
#[cfg(target_os = "linux")]
fn readd_block(name: &str) {
    let dev = std::path::PathBuf::from(format!("/sys/block/{name}/device"));
    let Ok(real) = std::fs::canonicalize(&dev) else { return };
    let Some(hctl) = real.file_name().map(|f| f.to_string_lossy().to_string()) else { return };
    let parts: Vec<&str> = hctl.split(':').collect();
    if parts.len() != 4 {
        return;
    }
    let (host, chan, target, lun) = (parts[0], parts[1], parts[2], parts[3]);
    tracing::info!(%name, %hctl, "sd still sees 0 blocks after format; re-adding the device");
    if let Err(e) = std::fs::write(dev.join("delete"), "1") {
        tracing::warn!(%name, "delete: {e}");
        return;
    }
    std::thread::sleep(Duration::from_secs(1));
    if let Err(e) = std::fs::write(
        format!("/sys/class/scsi_host/host{host}/scan"),
        format!("{chan} {target} {lun}"),
    ) {
        tracing::warn!(%name, "host scan: {e}");
    }
}

#[cfg(not(target_os = "linux"))]
fn readd_block(_name: &str) {}

/// Start a format on one drive. Caller has checked `format_blocker`.
pub async fn start(state: Arc<AppState>, drive: Drive, to: u32) -> Arc<FormatHandle> {
    let handle = Arc::new(FormatHandle {
        run: Mutex::new(FormatRun {
            drive: drive.id,
            name: drive.name.clone(),
            from_block_size: drive.block_size,
            to_block_size: to,
            state: FormatState::Running,
            phase: "queued".into(),
            progress_pct: None,
            started: SystemTime::now(),
            finished: None,
            error: None,
            immediate: true,
        }),
    });
    state.formats.write().await.insert(drive.id, handle.clone());
    {
        let mut inv = state.inventory.write().await;
        if let Some(d) = inv.drives.get_mut(&drive.id) {
            d.activity = Activity::Formatting;
            d.format = Some(FormatRecord {
                from_block_size: drive.block_size,
                to_block_size: to,
                state: "running".into(),
                started: Some(SystemTime::now()),
                finished: None,
                error: None,
            });
        }
    }
    state.events.write().await.push(
        Some(drive.id),
        Severity::Warning,
        "format",
        format!(
            "{}: FORMAT UNIT {} → {} bytes/sector started (all data destroyed)",
            drive.name, drive.block_size, to
        ),
    );
    state.persist().await;

    let h2 = handle.clone();
    let st2 = state.clone();
    tokio::spawn(async move {
        let h3 = h2.clone();
        let d2 = drive.clone();
        let result = tokio::task::spawn_blocking(move || run_blocking(&d2, to, &h3))
            .await
            .unwrap_or_else(|e| Err(format!("format task panicked: {e}")));
        let (state_word, err) = match &result {
            Ok(_) => (FormatState::Done, None),
            Err(e) => (FormatState::Failed, Some(e.clone())),
        };
        {
            let mut run = h2.run.lock().unwrap();
            run.state = state_word;
            run.finished = Some(SystemTime::now());
            run.error = err.clone();
            run.phase = if err.is_some() { "failed".into() } else { "done".into() };
        }
        {
            let mut inv = st2.inventory.write().await;
            if let Some(d) = inv.drives.get_mut(&drive.id) {
                if d.activity == Activity::Formatting {
                    d.activity = Activity::Idle;
                }
                if let Ok(bs) = &result {
                    d.block_size = *bs;
                    d.physical_block_size = *bs;
                    d.usable = true;
                    // Capacity in the new geometry; discovery corrects it
                    // on the next scan once sd has re-read it.
                    d.capacity_bytes = d.capacity_bytes / d.format.as_ref().map(|f| f.from_block_size.max(1) as u64).unwrap_or(1)
                        * (*bs as u64);
                }
                if let Some(f) = d.format.as_mut() {
                    f.state = if err.is_some() { "failed".into() } else { "done".into() };
                    f.finished = Some(SystemTime::now());
                    f.error = err.clone();
                }
            }
        }
        st2.events.write().await.push(
            Some(drive.id),
            if err.is_some() { Severity::Error } else { Severity::Info },
            "format",
            match &result {
                Ok(bs) => format!("{}: formatted to {bs}-byte sectors; kernel rescanned", drive.name),
                Err(e) => format!("{}: format FAILED: {e}", drive.name),
            },
        );
        st2.persist().await;
    });
    handle
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scsi::Sense;

    fn sense(key: u8, asc: u8, ascq: u8, progress: Option<u16>) -> Result<(), ScsiError> {
        Err(ScsiError::Sense(Sense {
            key,
            asc,
            ascq,
            progress,
        }))
    }

    #[test]
    fn tur_interpretation() {
        assert_eq!(interpret_tur(&Ok(())).unwrap(), None, "ready = done");
        assert_eq!(
            interpret_tur(&sense(2, 0x04, 0x04, Some(0x8000))).unwrap(),
            Some(Some(50))
        );
        assert_eq!(interpret_tur(&sense(2, 0x04, 0x04, None)).unwrap(), Some(None));
        assert_eq!(interpret_tur(&sense(2, 0x04, 0x01, None)).unwrap(), Some(None), "becoming ready");
        assert_eq!(interpret_tur(&sense(6, 0x29, 0x00, None)).unwrap(), Some(None), "unit attention");
        assert!(interpret_tur(&sense(3, 0x31, 0x01, None)).is_err(), "format command failed");
        assert!(interpret_tur(&Err(ScsiError::Unsupported("x"))).is_err());
    }

    #[test]
    fn targets() {
        assert!(valid_target(512));
        assert!(valid_target(4096));
        assert!(!valid_target(520));
        assert!(!valid_target(0));
    }
}
