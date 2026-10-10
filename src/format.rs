//! Sector-size reformat: the way a NetApp 520-byte drive becomes a drive
//! Linux (and stormblock) can use.
//!
//! Per drive, on the blocking pool:
//! 1. READ CAPACITY(16) — where we start from.
//! 2. MODE SENSE(10) page 1 for the current block descriptor; MODE
//!    SELECT(10) with a descriptor carrying the new block length and a
//!    block count of 0 ("all"); MODE SELECT(6) if the drive rejects (10).
//! 3. FORMAT UNIT, FMTDATA + IMMED, FMTPINFO for protection information
//!    (#85: type 1 = 10b, PFU 000b). If IMMED is refused, the blocking
//!    form with a day-long timeout.
//! 4. TEST UNIT READY every 30 s; the drive answers NOT READY 04/04 with a
//!    progress indication until it is done. Meanwhile the drive's HBA
//!    (mpt3sas `ioc_reset_count`) is watched: a controller reset is
//!    recorded on the run, raised as an event, and no new format starts
//!    until stormdrive restarts (the format already on the drive goes on).
//! 5. Kernel rescan (`device/rescan`; delete + targeted host scan if sd
//!    still sees 0 blocks), then READ CAPACITY again to verify the block
//!    length **and** the protection (PROT_EN, P_TYPE).
//!
//! Many drives at once is the normal case (a shelf of them): each runs in
//! its own task, the drive does the work, the host only polls. There is
//! no cancel — a half-formatted drive is worse than a slow one.

use crate::api::AppState;
use crate::drive::{Activity, Drive, DriveId, FormatRecord};
use crate::pi::Protection;
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
    pub protection: Protection,
    /// PI type READ CAPACITY reports after (0 = off).
    pub prot_type_after: Option<u8>,
    /// Controller (IOC) resets seen on the drive's HBA while it ran.
    pub resets: Vec<String>,
}

/// Set once a controller reset is seen during a format: nothing new
/// starts until stormdrive restarts (owner, #85: "on any host or IOC
/// reset, stop starting anything").
static RESET_HOLD: Mutex<Option<String>> = Mutex::new(None);

pub fn reset_hold() -> Option<String> {
    RESET_HOLD.lock().unwrap().clone()
}

/// mpt3sas counts its IOC resets in sysfs; None for drivers without it.
fn ioc_resets(host: Option<&str>) -> Option<u64> {
    let host = host?;
    std::fs::read_to_string(format!("/sys/class/scsi_host/{host}/ioc_reset_count")).ok()?.trim().parse().ok()
}

/// A reset seen now vs the count at the start → a line to record, once.
pub fn reset_seen(host: &str, before: Option<u64>, now: Option<u64>) -> Option<String> {
    match (before, now) {
        (Some(b), Some(n)) if n > b => Some(format!("{host}: IOC reset count {b} → {n}")),
        _ => None,
    }
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

const POLL: Duration = Duration::from_secs(30);
const MAX_WAIT: Duration = Duration::from_secs(48 * 60 * 60);

fn dev_path(drive: &Drive) -> String {
    scsi::sg_path_for_block(&drive.name).unwrap_or_else(|| drive.path.clone())
}

fn run_blocking(drive: &Drive, to: u32, prot: Protection, handle: &FormatHandle) -> Result<u32, String> {
    if let Some(why) = reset_hold() {
        return Err(format!("not started: {why}; no new format starts until stormdrive restarts"));
    }
    let host = drive.location.controller.as_ref().and_then(|c| c.scsi_host.clone());
    let resets0 = ioc_resets(host.as_deref());
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
    let immediate = match dev.format_unit_pi(true, prot.fmtpinfo()) {
        Ok(()) => true,
        Err(ScsiError::Sense(s)) if s.is_illegal_request() && prot != Protection::None => {
            // Refused with PI asked for: the drive does not do it — never
            // fall back to a format the operator did not choose.
            return Err(format!("format unit with {}: refused ({s})", prot.word()));
        }
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
        let mut seen = resets0;
        loop {
            std::thread::sleep(POLL);
            let now = ioc_resets(host.as_deref());
            if let Some(line) = reset_seen(host.as_deref().unwrap_or("?"), seen, now) {
                tracing::warn!(drive = %drive.name, "{line} during a format");
                *RESET_HOLD.lock().unwrap() = Some(format!("{line} during {}'s format", drive.name));
                handle.run.lock().unwrap().resets.push(line);
                seen = now;
            }
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
    handle.run.lock().unwrap().prot_type_after = Some(after.prot_type);
    if after.block_len != to {
        return Err(format!(
            "drive reports {}-byte blocks after formatting to {to}",
            after.block_len
        ));
    }
    if after.prot_type != prot.prot_type() {
        return Err(verify_prot_error(prot, after.prot_type));
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

/// Have the kernel re-read a drive's geometry after a low-level format
/// (also one stormdrive only watched the end of, after a restart).
pub fn rescan(name: &str) {
    rescan_block(name);
    if sysfs_sectors(name) == 0 {
        readd_block(name);
    }
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

/// READ CAPACITY after the format reports another protection than asked.
fn verify_prot_error(prot: Protection, got: u8) -> String {
    format!(
        "drive reports {} after formatting with {}",
        if got == 0 { "no PI".to_string() } else { format!("PI type {got}") },
        prot.word()
    )
}

/// Start a format on one drive. Caller has checked `format_blocker`.
pub async fn start(state: Arc<AppState>, drive: Drive, to: u32, prot: Protection) -> Arc<FormatHandle> {
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
            protection: prot,
            prot_type_after: None,
            resets: vec![],
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
                protection: prot,
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
            "{}: FORMAT UNIT {} → {} bytes/sector, {} started (all data destroyed)",
            drive.name,
            drive.block_size,
            to,
            prot.word()
        ),
    );
    state.persist().await;

    let h2 = handle.clone();
    tokio::spawn(async move {
        let h3 = h2.clone();
        let d2 = drive.clone();
        let result = tokio::task::spawn_blocking(move || run_blocking(&d2, to, prot, &h3))
            .await
            .unwrap_or_else(|e| Err(format!("format task panicked: {e}")));
        finish(&state, &h2, &drive, result).await;
    });
    handle
}

/// Record how a format ended: the handle, the drive's record and geometry,
/// its activity, an event.
async fn finish(state: &Arc<AppState>, handle: &FormatHandle, drive: &Drive, result: Result<u32, String>) {
    let err = result.as_ref().err().cloned();
    {
        let mut run = handle.run.lock().unwrap();
        run.state = if err.is_some() { FormatState::Failed } else { FormatState::Done };
        run.finished = Some(SystemTime::now());
        run.error = err.clone();
        run.phase = if err.is_some() { "failed".into() } else { "done".into() };
    }
    {
        let mut inv = state.inventory.write().await;
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
    let (prot, after, resets, from, to) = {
        let run = handle.run.lock().unwrap();
        (run.protection, run.prot_type_after, run.resets.clone(), run.from_block_size, run.to_block_size)
    };
    // Kept history (#68). A worker step records its own action (with the
    // requester); this is for the REST format routes and a re-attach.
    let worker_owned = state.worker.running_on(drive.id);
    if !worker_owned {
        crate::history::send(crate::history::Item::Action(
            drive.id,
            crate::history::Action {
                op: "format".into(),
                params: serde_json::json!({ "block_size": to, "protection": prot, "resets": resets }),
                result: if err.is_some() { "failed".into() } else { "done".into() },
                error: err.clone(),
                requester: None,
                job: None,
                before: serde_json::json!({ "block_size": from }),
                after: serde_json::json!({ "block_size": result.as_ref().ok(), "prot_type": after }),
            },
        ));
    }
    let mut events = state.events.write().await;
    for r in &resets {
        events.push(Some(drive.id), Severity::Warning, "format", format!("{}: controller reset during the format — {r}; no new format starts until stormdrive restarts", drive.name));
    }
    let read_back = match after {
        Some(0) => ", no PI read back".to_string(),
        Some(t) => format!(", PI type {t} read back"),
        None => String::new(),
    };
    events.push(
        Some(drive.id),
        if err.is_some() { Severity::Error } else { Severity::Info },
        "format",
        match &result {
            Ok(bs) => format!("{}: formatted to {bs}-byte sectors with {}{read_back}; kernel rescanned", drive.name, prot.word()),
            Err(e) => format!("{}: format FAILED: {e}", drive.name),
        },
    );
    drop(events);
    state.persist().await;
}

/// After a restart: a FORMAT UNIT (IMMED) we started keeps running on the
/// drive. Watch it to the end, rescan, and check the drive reports the
/// block size we asked for — a format that never reached the drive (we
/// died before FORMAT UNIT) shows up here as the old size, and fails.
pub async fn reattach(state: Arc<AppState>, drive: Drive) -> Arc<FormatHandle> {
    let rec = drive.format.clone().unwrap_or_default();
    let to = rec.to_block_size;
    let prot = rec.protection;
    let handle = Arc::new(FormatHandle {
        run: Mutex::new(FormatRun {
            drive: drive.id,
            name: drive.name.clone(),
            from_block_size: rec.from_block_size,
            to_block_size: to,
            state: FormatState::Running,
            phase: "formatting".into(),
            progress_pct: None,
            started: rec.started.unwrap_or_else(SystemTime::now),
            finished: None,
            error: None,
            immediate: true,
            protection: prot,
            prot_type_after: None,
            resets: vec![],
        }),
    });
    state.formats.write().await.insert(drive.id, handle.clone());
    state.events.write().await.push(
        Some(drive.id),
        Severity::Warning,
        "format",
        format!("{}: stormdrive restarted during a format to {to}-byte sectors; watching the drive to the end", drive.name),
    );
    let h2 = handle.clone();
    tokio::spawn(async move {
        let h3 = h2.clone();
        let d2 = drive.clone();
        let result = tokio::task::spawn_blocking(move || reattach_blocking(&d2, to, prot, &h3))
            .await
            .unwrap_or_else(|e| Err(format!("format task panicked: {e}")));
        finish(&state, &h2, &drive, result.map_err(|e| format!("{e} (after a restart)"))).await;
    });
    handle
}

fn reattach_blocking(drive: &Drive, to: u32, prot: Protection, handle: &FormatHandle) -> Result<u32, String> {
    let dev = Device::open(&dev_path(drive)).map_err(|e| format!("open: {e}"))?;
    let t0 = Instant::now();
    loop {
        match interpret_tur(&dev.test_unit_ready())? {
            None => break,
            Some(p) => handle.set_progress(p),
        }
        if t0.elapsed() > MAX_WAIT {
            return Err("format did not finish within 48 h".into());
        }
        std::thread::sleep(POLL);
    }
    handle.set_phase("rescan");
    for p in &drive.paths {
        rescan(p.trim_start_matches("/dev/"));
    }
    std::thread::sleep(Duration::from_secs(3));
    handle.set_phase("verify");
    let after = dev.read_capacity16().map_err(|e| format!("read capacity after format: {e}"))?;
    if after.block_len != to {
        return Err(format!(
            "drive reports {}-byte blocks, not the {to} it was being formatted to: the format did not complete",
            after.block_len
        ));
    }
    handle.run.lock().unwrap().prot_type_after = Some(after.prot_type);
    if after.prot_type != prot.prot_type() {
        return Err(verify_prot_error(prot, after.prot_type));
    }
    Ok(after.block_len)
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
    fn reset_lines_and_prot_errors() {
        assert_eq!(reset_seen("host0", Some(2), Some(3)).as_deref(), Some("host0: IOC reset count 2 → 3"));
        assert_eq!(reset_seen("host0", Some(2), Some(2)), None);
        assert_eq!(reset_seen("host0", None, Some(3)), None, "no counter at the start: nothing to compare");
        assert_eq!(verify_prot_error(Protection::Type1, 0), "drive reports no PI after formatting with PI type 1");
        assert_eq!(crate::scsi::cdb::format_unit_pi(Protection::Type1.fmtpinfo())[1], 0x90, "FMTPINFO 10b + FMTDATA");
        assert_eq!(crate::scsi::cdb::format_unit_pi(0)[1], 0x10);
    }

    #[test]
    fn targets() {
        assert!(valid_target(512));
        assert!(valid_target(4096));
        assert!(!valid_target(520));
        assert!(!valid_target(0));
    }
}
