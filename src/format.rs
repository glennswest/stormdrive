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
//!    Only the block size and PI asked for (#130): a drive that refuses it
//!    (ILLEGAL REQUEST on MODE SELECT or FORMAT UNIT) gets its old block
//!    length back and nothing is formatted. The refusal is kept with why,
//!    and the formats it may take instead (`alternatives`) are offered for
//!    the person to choose, never run by themselves.
//! 4. REQUEST SENSE every 30 s, for the NOT READY 04/04 progress indication
//!    (it answers GOOD, so a 20 h format adds nothing to the drive's
//!    `io errs`); TEST UNIT READY when that does not say the format is
//!    running, which is also what says it ended. Percent, start time and
//!    time left go on the drive's persisted record, so they outlive a restart
//!    (#108). Meanwhile the drive's HBA (mpt3sas `ioc_reset_count`) is
//!    watched: a controller reset is recorded on the run, raised as an event,
//!    and no new format starts until stormdrive restarts (the format already
//!    on the drive goes on).
//! 5. Kernel rescan (`device/rescan`; delete + targeted host scan if sd
//!    still sees 0 blocks), then READ CAPACITY again to read back the block
//!    length **and** the protection (PROT_EN, P_TYPE). Not what was asked:
//!    the run fails saying what the drive reads, and no second format starts
//!    by itself.
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
    /// `prepare`, `format`, `formatting`, `rescan`, `verify`, `done`.
    pub phase: String,
    /// 0..=100 while formatting, when the drive reports progress.
    pub progress_pct: Option<u8>,
    /// When the drive last reported it.
    pub progress_at: Option<SystemTime>,
    /// Estimated seconds left at that moment.
    pub eta_secs: Option<u64>,
    /// The format the drive refused, with why (#130: then nothing ran).
    pub refused: Vec<String>,
    /// After a refusal: what the drive may take instead, for the person to
    /// choose (#130). Never started by stormdrive.
    pub alternatives: Vec<crate::pi::Choice>,
    /// READ CAPACITY block length after the format.
    pub block_size_after: Option<u32>,
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
    /// The drive's progress indication (0..=65535 of the whole). None keeps
    /// the last one: a poll that got none is not a step back.
    fn set_progress(&self, progress: Option<u16>) {
        let Some(p) = progress else { return };
        let mut run = self.run.lock().unwrap();
        let now = SystemTime::now();
        run.progress_pct = Some(((p as u32 * 100) / 65536) as u8);
        run.progress_at = Some(now);
        run.eta_secs = eta_secs(now.duration_since(run.started).unwrap_or_default().as_secs(), p);
    }
    fn set_finished_progress(&self) {
        let mut run = self.run.lock().unwrap();
        run.progress_pct = Some(100);
        run.progress_at = Some(SystemTime::now());
        run.eta_secs = Some(0);
    }
}

/// Seconds left, straight from the elapsed time and the drive's progress
/// (0..=65535 of the whole). None under 0.1%: too little to say anything.
pub fn eta_secs(elapsed_secs: u64, progress: u16) -> Option<u64> {
    let p = progress as u64;
    (p >= 66).then(|| elapsed_secs * (65536 - p) / p)
}

/// Block sizes we will format to.
pub fn valid_target(block_size: u32) -> bool {
    crate::drive::USABLE_BLOCK_SIZES.contains(&block_size)
}

/// Progress from a TEST UNIT READY result while a format runs.
/// Ok(None) = done; Ok(Some(pct)) = still going; Err = the format failed
/// or the drive went away.
pub fn interpret_tur(r: &Result<(), ScsiError>) -> Result<Option<Option<u8>>, String> {
    let raw = raw_progress(r)?;
    Ok(raw.map(|p| p.map(|p| ((p as u32 * 100) / 65536) as u8)))
}

/// One poll of a running format. REQUEST SENSE when it says the format is
/// running with a progress, or that it failed; otherwise TEST UNIT READY
/// decides (an idle drive's REQUEST SENSE says NO SENSE, not "done").
pub(crate) fn poll_format(
    request_sense: impl FnOnce() -> Result<Vec<u8>, ScsiError>,
    tur: impl FnOnce() -> Result<(), ScsiError>,
) -> Result<Option<Option<u16>>, String> {
    if let Some(s) = request_sense().ok().and_then(|raw| scsi::parse_sense(&raw)) {
        if (s.is_format_in_progress() && s.progress.is_some()) || s.asc == 0x31 {
            return raw_progress(&Err(ScsiError::Sense(s)));
        }
    }
    raw_progress(&tur())
}

fn raw_progress(r: &Result<(), ScsiError>) -> Result<Option<Option<u16>>, String> {
    match r {
        Ok(()) => Ok(None),
        Err(ScsiError::Sense(s)) if s.is_format_in_progress() => Ok(Some(s.progress)),
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

pub(crate) const POLL: Duration = Duration::from_secs(30);
pub(crate) const MAX_WAIT: Duration = Duration::from_secs(48 * 60 * 60);

fn dev_path(drive: &Drive) -> String {
    scsi::sg_path_for_block(&drive.name).unwrap_or_else(|| drive.path.clone())
}

/// What starting a format needs of a drive: a trait so the commands and
/// their order are tested without one.
pub(crate) trait Unit {
    /// MODE SENSE(10), long LBA descriptors allowed (LLBAA=1).
    fn mode_sense10(&self, page: u8) -> scsi::Result<scsi::ModeHeader10>;
    fn mode_select10(&self, data: &[u8]) -> scsi::Result<()>;
    fn mode_select6(&self, data: &[u8]) -> scsi::Result<()>;
    fn format_unit_pi(&self, immed: bool, fmtpinfo: u8) -> scsi::Result<()>;
    fn read_capacity16(&self) -> scsi::Result<scsi::Capacity>;
}

impl Unit for Device {
    fn mode_sense10(&self, page: u8) -> scsi::Result<scsi::ModeHeader10> {
        Device::mode_sense10_long(self, page)
    }
    fn mode_select10(&self, data: &[u8]) -> scsi::Result<()> {
        Device::mode_select10(self, data)
    }
    fn mode_select6(&self, data: &[u8]) -> scsi::Result<()> {
        Device::mode_select6(self, data)
    }
    fn format_unit_pi(&self, immed: bool, fmtpinfo: u8) -> scsi::Result<()> {
        Device::format_unit_pi(self, immed, fmtpinfo)
    }
    fn read_capacity16(&self) -> scsi::Result<scsi::Capacity> {
        Device::read_capacity16(self)
    }
}

/// The block length a format starts from (and puts back on a refusal):
/// READ CAPACITY's. A drive whose last format was cut off answers 31/00
/// MEDIUM FORMAT CORRUPTED instead (#131) — a format is what it needs, so
/// that is no reason to stop: the length on record stands in.
fn current_length(dev: &impl Unit, recorded: u32) -> Result<u32, String> {
    match dev.read_capacity16() {
        Ok(c) => Ok(c.block_len),
        Err(ScsiError::Sense(s)) if s.is_medium_format_corrupted() => {
            tracing::info!("read capacity: {s} (an interrupted format); starting from the recorded {recorded}-byte length");
            Ok(if recorded > 0 { recorded } else { 512 })
        }
        Err(e) => Err(format!("read capacity: {e}")),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Attempt {
    /// FORMAT UNIT accepted. `immediate`: it runs on, poll it; else it ended.
    Started { immediate: bool },
    /// ILLEGAL REQUEST: the drive will not do this one, and nothing is
    /// formatted. The next may be tried.
    Refused(String),
    /// Anything else (a transport error, a busy drive) is no answer about
    /// the format: the run stops.
    Failed(String),
}

fn refused_or_failed(e: &ScsiError, why: String) -> Attempt {
    match e {
        ScsiError::Sense(s) if s.is_illegal_request() => Attempt::Refused(why),
        _ => Attempt::Failed(why),
    }
}

/// MODE SELECT with the new block length in the drive's own block
/// descriptor (#129): MODE SENSE(10) with LLBAA, then MODE SELECT(10)
/// PF=1 SP=0 with what came back (long descriptor if the drive gave one),
/// count 0 and the new length; a refused long descriptor is tried short,
/// then MODE SELECT(6). An ILLEGAL REQUEST from any of them is the drive's
/// answer (refused, with the field it points at); only errors that answer
/// nothing (the (6) failing at the transport on a drive that has no (6))
/// leave it failed.
pub(crate) fn set_length(dev: &impl Unit, name: &str, to: u32) -> Result<(), Attempt> {
    let h = dev.mode_sense10(0x01).unwrap_or_else(|e| {
        tracing::debug!(drive = %name, "mode sense failed ({e}); a zero block descriptor");
        scsi::ModeHeader10 { medium_type: 0, device_specific: 0, long_lba: false, block_descriptor: None }
    });
    let long = h.long_lba && h.block_descriptor.as_ref().is_some_and(|bd| bd.len() >= 16);
    let mut lists: Vec<(&str, Vec<u8>, bool)> = vec![];
    if long {
        lists.push(("10, long", scsi::mode_select10_list(&h, to, true), true));
    }
    lists.push(("10", scsi::mode_select10_list(&h, to, false), true));
    lists.push(("6", scsi::mode_select6_list(&h, to), false));
    let mut errs = vec![];
    let mut refused = false;
    for (what, list, ten) in lists {
        let r = if ten { dev.mode_select10(&list) } else { dev.mode_select6(&list) };
        match r {
            Ok(()) => return Ok(()),
            Err(e) => {
                let hex: String = list.iter().map(|b| format!("{b:02x}")).collect();
                tracing::warn!(drive = %name, "mode select ({what}) of {to} refused: {e}; parameter list {hex}");
                refused |= matches!(&e, ScsiError::Sense(s) if s.is_illegal_request());
                errs.push(format!("({what}) {e}"));
            }
        }
    }
    let why = format!("mode select rejected: {}", errs.join("; "));
    Err(if refused { Attempt::Refused(why) } else { Attempt::Failed(why) })
}

/// The block length, then FORMAT UNIT.
fn begin(dev: &impl Unit, name: &str, to: u32, prot: Protection) -> Attempt {
    if let Err(a) = set_length(dev, name, to) {
        return a;
    }
    match dev.format_unit_pi(true, prot.fmtpinfo()) {
        Ok(()) => Attempt::Started { immediate: true },
        Err(ScsiError::Sense(s)) if s.is_illegal_request() && prot != Protection::None => {
            Attempt::Refused(format!("format unit with {}: refused ({s})", prot.word()))
        }
        Err(ScsiError::Sense(s)) if s.is_illegal_request() => {
            tracing::info!(drive = %name, "IMMED refused ({s}); blocking format");
            match dev.format_unit_pi(false, 0) {
                Ok(()) => Attempt::Started { immediate: false },
                Err(e) => refused_or_failed(&e, format!("format unit: {e}")),
            }
        }
        Err(e) => Attempt::Failed(format!("format unit: {e}")),
    }
}

/// Exactly the format asked for (#130): no fallback of our own. A refusal
/// puts the drive's old block length back and starts nothing; the line
/// says what was refused and why. Ok = whether it runs on (IMMED).
fn start_exact(dev: &impl Unit, name: &str, current: u32, (to, prot): (u32, Protection)) -> Result<bool, Attempt> {
    match begin(dev, name, to, prot) {
        Attempt::Started { immediate } => Ok(immediate),
        Attempt::Refused(why) => {
            let line = format!("{to} with {}: {why}", prot.word());
            tracing::warn!(drive = %name, "refused {line}");
            if let Err(Attempt::Refused(e) | Attempt::Failed(e)) = set_length(dev, name, current) {
                tracing::warn!(drive = %name, "putting block length {current} back: {e}");
            }
            Err(Attempt::Refused(line))
        }
        failed => Err(failed),
    }
}

/// The error of a refused format: nothing ran, and what the person may
/// choose instead.
fn refused_error(line: &str, alternatives: &[crate::pi::Choice]) -> String {
    let mut e = format!("the drive refused {line}; nothing was formatted");
    if !alternatives.is_empty() {
        let alts: Vec<String> = alternatives.iter().map(|c| format!("{} with {}", c.block_size, c.protection.word())).collect();
        e.push_str(&format!(". It may take instead: {} (format again choosing one)", alts.join(", ")));
    }
    e
}

fn run_blocking(drive: &Drive, (to, prot): (u32, Protection), alternatives: &[crate::pi::Choice], handle: &FormatHandle) -> Result<u32, String> {
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
    let current = current_length(&dev, drive.block_size)?;
    if current == to && drive.format_reason.is_none() {
        tracing::info!(drive = %drive.name, "already {to}-byte; formatting anyway (operator asked)");
    }

    // Blocking FORMAT UNIT: the phase reads `format` for as long as it takes.
    handle.set_phase("format");
    let immediate = match start_exact(&dev, &drive.name, current, (to, prot)) {
        Ok(immediate) => immediate,
        Err(Attempt::Refused(line)) => {
            let mut run = handle.run.lock().unwrap();
            run.refused.push(line.clone());
            run.alternatives = alternatives.to_vec();
            return Err(refused_error(&line, alternatives));
        }
        Err(Attempt::Failed(e)) => return Err(e),
        Err(Attempt::Started { .. }) => unreachable!("start_exact returns Ok for a started format"),
    };
    handle.run.lock().unwrap().immediate = immediate;

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
            match poll_format(|| dev.request_sense(), || dev.test_unit_ready())? {
                None => break,
                Some(p) => handle.set_progress(p),
            }
            if t0.elapsed() > MAX_WAIT {
                return Err("format did not finish within 48 h".into());
            }
        }
    }
    handle.set_finished_progress();

    handle.set_phase("rescan");
    for p in &drive.paths {
        let name = p.trim_start_matches("/dev/");
        rescan_block(name);
    }
    std::thread::sleep(Duration::from_secs(3));

    handle.set_phase("verify");
    let after = dev.read_capacity16().map_err(|e| format!("read capacity after format: {e}"))?;
    {
        let mut run = handle.run.lock().unwrap();
        run.prot_type_after = Some(after.prot_type);
        run.block_size_after = Some(after.block_len);
    }
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

/// An sg-only disk (#131) has no block node, so no sectors: once it is
/// formatted it is re-added, and sd makes it a block device.
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

/// The SCSI device directory of a disk: `/sys/block/<name>/device`, or for
/// one reached through its sg node only (#131) `/sys/class/scsi_generic/<sgN>/device`.
#[cfg(target_os = "linux")]
fn scsi_device_dir(name: &str) -> std::path::PathBuf {
    let block = std::path::PathBuf::from(format!("/sys/block/{name}/device"));
    if !block.exists() && name.starts_with("sg") {
        return std::path::PathBuf::from(format!("/sys/class/scsi_generic/{name}/device"));
    }
    block
}

#[cfg(target_os = "linux")]
fn rescan_block(name: &str) {
    if let Err(e) = std::fs::write(scsi_device_dir(name).join("rescan"), "1") {
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
    let dev = scsi_device_dir(name);
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
            progress_at: None,
            eta_secs: None,
            refused: vec![],
            alternatives: vec![],
            block_size_after: None,
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
    // Offered if the drive refuses; never run by themselves (#130).
    let alternatives: Vec<crate::pi::Choice> =
        drive.supports.as_ref().map(|s| crate::pi::alternatives(s, to, prot)).unwrap_or_default();
    {
        let mut inv = state.inventory.write().await;
        if let Some(d) = inv.drives.get_mut(&drive.id) {
            d.activity = Activity::Formatting;
            d.format = Some(FormatRecord {
                from_block_size: drive.block_size,
                to_block_size: to,
                protection: prot,
                target: crate::pi::target(to, prot),
                state: "running".into(),
                started: Some(SystemTime::now()),
                phase: "queued".into(),
                boot_id: current_boot_id(),
                ..Default::default()
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
        let task = tokio::task::spawn_blocking(move || run_blocking(&d2, (to, prot), &alternatives, &h3));
        let result = follow(&state, &h2, &drive, task).await;
        finish(&state, &h2, &drive, result).await;
    });
    handle
}

/// How often a run's state is copied to the drive's record.
const SYNC: Duration = Duration::from_secs(5);

/// A run's state, on the drive's record (what a restart and the Drive
/// object read).
fn copy_to_record(run: &FormatRun, f: &mut FormatRecord) {
    f.to_block_size = run.to_block_size;
    f.protection = run.protection;
    f.phase = run.phase.clone();
    f.progress_pct = run.progress_pct;
    f.progress_at = run.progress_at;
    f.eta_secs = run.eta_secs;
    f.refused = run.refused.clone();
    f.alternatives = run.alternatives.clone();
    f.target = crate::pi::target(run.to_block_size, run.protection);
    f.block_size_after = run.block_size_after;
    f.prot_type_after = run.prot_type_after;
}

/// Wait for the blocking run, keeping the drive's record up with it. Written
/// to disk when something moved that a restart must not lose: the phase, a
/// whole percent, a refusal and the new target. Each refusal is a warning
/// event as it happens: never a quiet fallback (owner, #82).
async fn follow(
    state: &Arc<AppState>,
    handle: &FormatHandle,
    drive: &Drive,
    mut task: tokio::task::JoinHandle<Result<u32, String>>,
) -> Result<u32, String> {
    let mut seen = handle.run.lock().unwrap().clone();
    loop {
        tokio::select! {
            r = &mut task => return r.unwrap_or_else(|e| Err(format!("format task panicked: {e}"))),
            _ = tokio::time::sleep(SYNC) => {}
        }
        let run = handle.run.lock().unwrap().clone();
        if let Some(d) = state.inventory.write().await.drives.get_mut(&drive.id) {
            if let Some(f) = d.format.as_mut() {
                copy_to_record(&run, f);
            }
        }
        if run.refused.len() > seen.refused.len() {
            let mut events = state.events.write().await;
            for line in &run.refused[seen.refused.len()..] {
                events.push(Some(drive.id), Severity::Warning, "format", format!("{}: the drive refused {line}", drive.name));
            }
        }
        let moved = run.phase != seen.phase
            || run.progress_pct != seen.progress_pct
            || run.refused.len() != seen.refused.len()
            || (run.to_block_size, run.protection) != (seen.to_block_size, seen.protection);
        if moved {
            state.persist().await;
        }
        seen = run;
    }
}

fn current_boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map(|s| s.trim().to_string()).unwrap_or_default()
}

/// The disks under `sys_block` (`/sys/block`) with this WWID: one per path.
fn names_by_wwid(sys_block: &std::path::Path, wwid: &str) -> Vec<String> {
    let Ok(dir) = std::fs::read_dir(sys_block) else { return vec![] };
    let mut names: Vec<String> = dir
        .flatten()
        .filter(|e| std::fs::read_to_string(e.path().join("device/wwid")).is_ok_and(|w| w.trim() == wwid))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

/// How long a node that restarted is given to bring a formatting drive back.
const REAPPEAR: Duration = Duration::from_secs(10 * 60);

/// The drive as it is named now. The inventory's names are from before the
/// restart, and after a node restart they may be another drive's: found by
/// WWID. Not found in the same boot: the names we have (no WWID, or a test).
/// Not found after a node restart: an error, never another drive's name.
fn as_named_now(drive: &Drive, other_boot: bool) -> Result<Drive, String> {
    let t0 = Instant::now();
    let names = loop {
        let names = drive.wwid.as_deref().map(|w| names_by_wwid(std::path::Path::new("/sys/block"), w)).unwrap_or_default();
        if !names.is_empty() || !other_boot || t0.elapsed() > REAPPEAR {
            break names;
        }
        std::thread::sleep(Duration::from_secs(10));
    };
    if names.is_empty() {
        return if other_boot && drive.wwid.is_some() {
            Err(format!("the node restarted and no disk has WWID {} after {} min", drive.wwid.as_deref().unwrap_or_default(), REAPPEAR.as_secs() / 60))
        } else {
            Ok(drive.clone())
        };
    }
    let mut d = drive.clone();
    d.name = names[0].clone();
    d.path = format!("/dev/{}", names[0]);
    d.paths = names.iter().map(|n| format!("/dev/{n}")).collect();
    Ok(d)
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
                copy_to_record(&handle.run.lock().unwrap(), f);
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
            progress_pct: rec.progress_pct,
            progress_at: rec.progress_at,
            eta_secs: rec.eta_secs,
            refused: rec.refused.clone(),
            alternatives: rec.alternatives.clone(),
            block_size_after: None,
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
    let other_boot = !rec.boot_id.is_empty() && rec.boot_id != current_boot_id();
    let h2 = handle.clone();
    tokio::spawn(async move {
        let h3 = h2.clone();
        let d2 = drive.clone();
        let task = tokio::task::spawn_blocking(move || reattach_blocking(&d2, to, prot, other_boot, &h3));
        let result = follow(&state, &h2, &drive, task).await;
        finish(&state, &h2, &drive, result.map_err(|e| format!("{e} (after a restart)"))).await;
    });
    handle
}

fn reattach_blocking(drive: &Drive, to: u32, prot: Protection, other_boot: bool, handle: &FormatHandle) -> Result<u32, String> {
    let drive = &as_named_now(drive, other_boot)?;
    let dev = Device::open(&dev_path(drive)).map_err(|e| format!("open: {e}"))?;
    let t0 = Instant::now();
    loop {
        match poll_format(|| dev.request_sense(), || dev.test_unit_ready())? {
            None => break,
            Some(p) => handle.set_progress(p),
        }
        if t0.elapsed() > MAX_WAIT {
            return Err("format did not finish within 48 h".into());
        }
        std::thread::sleep(POLL);
    }
    handle.set_finished_progress();
    handle.set_phase("rescan");
    for p in &drive.paths {
        rescan_block(p.trim_start_matches("/dev/"));
    }
    std::thread::sleep(Duration::from_secs(3));
    handle.set_phase("verify");
    let after = dev.read_capacity16().map_err(|e| format!("read capacity after format: {e}"));
    // A device still at 0 blocks (or with no block node, #131) is re-added
    // only now: the delete takes the open sg node with it.
    for p in &drive.paths {
        let name = p.trim_start_matches("/dev/");
        if sysfs_sectors(name) == 0 {
            readd_block(name);
        }
    }
    let after = after?;
    {
        let mut run = handle.run.lock().unwrap();
        run.prot_type_after = Some(after.prot_type);
        run.block_size_after = Some(after.block_len);
    }
    if after.block_len != to {
        return Err(format!(
            "drive reports {}-byte blocks, not the {to} it was being formatted to: the format did not complete",
            after.block_len
        ));
    }
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
            field: None,
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

    /// A drive that answers each command from a script and logs what it got.
    #[derive(Default)]
    struct FakeUnit {
        log: std::cell::RefCell<Vec<String>>,
        refuse_select: Vec<u32>,
        refuse_fmtpinfo: Vec<u8>,
        transport_error_on_format: bool,
        /// forge1's NetApp X378 (#129): 10 TB at 512, so past 2^32 blocks.
        /// MODE SENSE gives the long descriptor when LLBAA is asked; a short
        /// descriptor in MODE SELECT, or a non-zero block count, is 26/00
        /// with the field pointer; MODE SELECT(6) dies at the transport.
        netapp_x378: bool,
        /// forge1 sdb–sdd (#131): a format cut off by a power-off; READ
        /// CAPACITY answers 3/31/00 MEDIUM FORMAT CORRUPTED.
        corrupted: bool,
    }

    const X378_BLOCKS: u64 = 19_532_873_728;

    fn invalid_field(byte: u16) -> scsi::Result<()> {
        Err(ScsiError::Sense(Sense {
            field: Some(scsi::FieldPointer { in_cdb: false, bit: None, byte }),
            ..Sense::new(5, 0x26, 0)
        }))
    }

    impl Unit for FakeUnit {
        fn mode_sense10(&self, _: u8) -> scsi::Result<scsi::ModeHeader10> {
            if self.netapp_x378 {
                let mut bd = vec![0u8; 16];
                bd[0..8].copy_from_slice(&X378_BLOCKS.to_be_bytes());
                bd[12..16].copy_from_slice(&512u32.to_be_bytes());
                return Ok(scsi::ModeHeader10 { medium_type: 0, device_specific: 0x10, long_lba: true, block_descriptor: Some(bd) });
            }
            // The HGST beside it: a short descriptor, its count FFFFFFFFh.
            let bd = vec![0xff, 0xff, 0xff, 0xff, 0, 0, 0x02, 0x00];
            Ok(scsi::ModeHeader10 { medium_type: 0, device_specific: 0x10, long_lba: false, block_descriptor: Some(bd) })
        }
        fn mode_select10(&self, data: &[u8]) -> scsi::Result<()> {
            let long = data[4] & 1 != 0;
            let (len, count_zero) = if long {
                (u32::from_be_bytes(data[20..24].try_into().unwrap()), data[8..16].iter().all(|b| *b == 0))
            } else {
                (u32::from_be_bytes([0, data[13], data[14], data[15]]), data[8..12].iter().all(|b| *b == 0))
            };
            self.log.borrow_mut().push(format!("select10{} {len}", if long { "L" } else { "" }));
            assert_eq!(&data[0..2], &[0, 0], "mode data length is reserved on select");
            assert_eq!(data[3], 0, "device-specific is zero on select");
            if self.netapp_x378 && !long {
                return invalid_field(7);
            }
            if !count_zero {
                return invalid_field(8);
            }
            if self.refuse_select.contains(&len) {
                return sense(5, 0x26, 0, None);
            }
            Ok(())
        }
        fn mode_select6(&self, data: &[u8]) -> scsi::Result<()> {
            let len = u32::from_be_bytes([0, data[9], data[10], data[11]]);
            self.log.borrow_mut().push(format!("select6 {len}"));
            if self.netapp_x378 {
                return Err(ScsiError::Transport { status: 2, host: 7, driver: 8 });
            }
            if self.refuse_select.contains(&len) {
                return sense(5, 0x26, 0, None);
            }
            Ok(())
        }
        fn format_unit_pi(&self, immed: bool, fmtpinfo: u8) -> scsi::Result<()> {
            self.log.borrow_mut().push(format!("format immed={immed} fmtpinfo={fmtpinfo}"));
            if self.transport_error_on_format {
                return Err(ScsiError::Unsupported("gone"));
            }
            if self.refuse_fmtpinfo.contains(&fmtpinfo) {
                return sense(5, 0x24, 0, None);
            }
            Ok(())
        }
        fn read_capacity16(&self) -> scsi::Result<scsi::Capacity> {
            self.log.borrow_mut().push("read capacity".into());
            if self.corrupted {
                return Err(ScsiError::Sense(Sense::new(3, 0x31, 0)));
            }
            Ok(scsi::Capacity { last_lba: (1 << 31) - 1, block_len: 512, lbppbe: 3, prot_type: 1 })
        }
    }

    const PI: (u32, Protection) = (4096, Protection::Type1);

    fn run_fake(u: &FakeUnit) -> Result<bool, Attempt> {
        start_exact(u, "sdz", 512, PI)
    }

    fn refused(r: Result<bool, Attempt>) -> String {
        match r {
            Err(Attempt::Refused(line)) => line,
            other => panic!("not a refusal: {other:?}"),
        }
    }

    fn formats_sent(u: &FakeUnit) -> Vec<String> {
        u.log.borrow().iter().filter(|l| l.starts_with("format")).cloned().collect()
    }

    #[test]
    fn the_first_format_goes_mode_select_then_format_unit_with_fmtpinfo() {
        let u = FakeUnit::default();
        assert!(run_fake(&u).unwrap());
        assert_eq!(*u.log.borrow(), ["select10 4096", "format immed=true fmtpinfo=2"]);
    }

    /// #131: forge1 sdb–sdd after a power-off cut their formats short. READ
    /// CAPACITY answers 3/31/00 (and sd gave them no block node); that is
    /// the drive that needs a format, so it gets the 4096 + PI1 asked for.
    #[test]
    fn an_interrupted_format_is_formatted_at_4096_with_pi() {
        let u = FakeUnit { corrupted: true, ..Default::default() };
        let current = current_length(&u, 512).expect("31/00 is no reason to stop");
        assert_eq!(current, 512, "the recorded length stands in");
        assert!(start_exact(&u, "sg2", current, PI).unwrap());
        assert_eq!(*u.log.borrow(), ["read capacity", "select10 4096", "format immed=true fmtpinfo=2"]);
        // Any other failure still stops the run before a command is sent.
        struct Gone;
        impl Unit for Gone {
            fn mode_sense10(&self, _: u8) -> scsi::Result<scsi::ModeHeader10> { unreachable!() }
            fn mode_select10(&self, _: &[u8]) -> scsi::Result<()> { unreachable!() }
            fn mode_select6(&self, _: &[u8]) -> scsi::Result<()> { unreachable!() }
            fn format_unit_pi(&self, _: bool, _: u8) -> scsi::Result<()> { unreachable!() }
            fn read_capacity16(&self) -> scsi::Result<scsi::Capacity> { Err(ScsiError::Sense(Sense::new(4, 0x44, 0))) }
        }
        assert!(current_length(&Gone, 512).unwrap_err().starts_with("read capacity: "));
        assert_eq!(current_length(&FakeUnit::default(), 4096).unwrap(), 512, "READ CAPACITY is the truth when it answers");
    }

    #[test]
    fn the_netapp_x378_gets_its_own_long_descriptor_back() {
        let u = FakeUnit { netapp_x378: true, ..Default::default() };
        assert!(run_fake(&u).unwrap());
        assert_eq!(*u.log.borrow(), ["select10L 4096", "format immed=true fmtpinfo=2"]);
    }

    #[test]
    fn a_refused_4096_sends_no_format_unit() {
        // forge1 sdb/sdc/sdd (#130): 4096 refused at MODE SELECT. Before,
        // the run went on to 512 + PI1 by itself; now nothing is formatted
        // and the old length goes back.
        let u = FakeUnit { refuse_select: vec![4096], ..Default::default() };
        let line = refused(run_fake(&u));
        assert!(line.starts_with("4096 with PI type 1: mode select rejected"), "{line}");
        assert!(formats_sent(&u).is_empty(), "no FORMAT UNIT: {:?}", u.log.borrow());
        assert_eq!(u.log.borrow().last().unwrap(), "select10 512", "the old length goes back");
    }

    #[test]
    fn a_refused_mode_select_names_the_field_even_when_6_fails_at_the_transport() {
        // The long and short (10) are the drive's answer; the (6) dying at the
        // transport does not make it "failed".
        let u = FakeUnit { netapp_x378: true, refuse_select: vec![4096], ..Default::default() };
        let line = refused(run_fake(&u));
        assert!(line.contains("(10, long) illegal request"), "{line}");
        assert!(line.contains("(10) illegal request (key 0x5, asc/ascq 0x26/0x00, field pointer: parameter list byte 7)"), "{line}");
        assert!(line.contains("(6) scsi status 0x02, host 0x07"), "{line}");
        assert!(formats_sent(&u).is_empty());
        assert_eq!(u.log.borrow().last().unwrap(), "select10L 512");
    }

    #[test]
    fn a_refused_pi_format_is_not_run_without_pi() {
        let u = FakeUnit { refuse_fmtpinfo: vec![2], ..Default::default() };
        let line = refused(run_fake(&u));
        assert!(line.starts_with("4096 with PI type 1: format unit with PI type 1: refused"), "{line}");
        assert_eq!(formats_sent(&u), ["format immed=true fmtpinfo=2"], "no format without PI after it");
        assert_eq!(u.log.borrow().last().unwrap(), "select10 512");
    }

    #[test]
    fn a_failure_that_is_no_answer_stops_it() {
        let u = FakeUnit { transport_error_on_format: true, ..Default::default() };
        assert!(matches!(run_fake(&u), Err(Attempt::Failed(_))));
        assert_eq!(u.log.borrow().len(), 2, "no second format: {:?}", u.log.borrow());
    }

    #[test]
    fn a_refusal_offers_the_alternatives_as_a_choice() {
        let alts = [
            crate::pi::Choice { block_size: 4096, protection: Protection::None },
            crate::pi::Choice { block_size: 512, protection: Protection::Type1 },
        ];
        let e = refused_error("4096 with PI type 1: mode select rejected", &alts);
        assert_eq!(
            e,
            "the drive refused 4096 with PI type 1: mode select rejected; nothing was formatted. \
             It may take instead: 4096 with no PI, 512 with PI type 1 (format again choosing one)"
        );
        assert_eq!(refused_error("x", &[]), "the drive refused x; nothing was formatted");
    }

    /// Fixed-format REQUEST SENSE data.
    fn sense_data(key: u8, asc: u8, ascq: u8, progress: Option<u16>) -> Vec<u8> {
        let mut d = vec![0u8; 18];
        d[0] = 0x70;
        d[2] = key;
        d[7] = 10;
        d[12] = asc;
        d[13] = ascq;
        if let Some(p) = progress {
            d[15] = 0x80;
            d[16..18].copy_from_slice(&p.to_be_bytes());
        }
        d
    }

    fn no_tur() -> Result<(), ScsiError> {
        panic!("REQUEST SENSE answered; no TEST UNIT READY needed")
    }

    #[test]
    fn progress_comes_from_request_sense_and_tur_says_when_it_ended() {
        let rs = |d: Vec<u8>| move || -> Result<Vec<u8>, ScsiError> { Ok(d) };
        // 25%, GOOD: no failed command for the drive's `io errs`.
        assert_eq!(poll_format(rs(sense_data(2, 4, 4, Some(0x4000))), no_tur).unwrap(), Some(Some(0x4000)));
        // Idle sense data is not "done": TUR says.
        assert_eq!(poll_format(rs(sense_data(0, 0, 0, None)), || Ok(())).unwrap(), None);
        assert_eq!(poll_format(rs(sense_data(0, 0, 0, None)), || sense(2, 4, 4, Some(0x8000))).unwrap(), Some(Some(0x8000)));
        // No SKSV in REQUEST SENSE: TUR's may have one. Neither: still going.
        assert_eq!(poll_format(rs(sense_data(2, 4, 4, None)), || sense(2, 4, 4, Some(100))).unwrap(), Some(Some(100)));
        assert_eq!(poll_format(rs(sense_data(2, 4, 4, None)), || sense(2, 4, 4, None)).unwrap(), Some(None));
        // A drive that refuses REQUEST SENSE, or answers nothing.
        assert_eq!(poll_format(|| Err(ScsiError::Unsupported("x")), || sense(2, 4, 4, Some(100))).unwrap(), Some(Some(100)));
        assert_eq!(poll_format(rs(vec![0; 18]), || Ok(())).unwrap(), None);
        // A failed format, deferred error or not, reads as failed from either.
        assert!(poll_format(rs(sense_data(3, 0x31, 0, None)), no_tur).is_err());
        assert!(poll_format(rs(sense_data(0, 0, 0, None)), || sense(3, 0x31, 1, None)).is_err());
    }

    #[test]
    fn time_left_from_the_progress() {
        assert_eq!(eta_secs(3600, 0x8000), Some(3600), "half done in an hour");
        assert_eq!(eta_secs(3600, 0x4000), Some(10800), "a quarter");
        assert_eq!(eta_secs(10, 0), None);
        assert_eq!(eta_secs(10, 65), None, "under 0.1%: nothing to say");
        assert_eq!(eta_secs(72000, 65535), Some(1));
    }

    fn a_run() -> FormatHandle {
        FormatHandle {
            run: Mutex::new(FormatRun {
                drive: DriveId::derive(Some("naa.1"), "M", "S"),
                name: "sdz".into(),
                from_block_size: 512,
                to_block_size: 4096,
                state: FormatState::Running,
                phase: "formatting".into(),
                progress_pct: None,
                progress_at: None,
                eta_secs: None,
                refused: vec![],
                alternatives: vec![],
                block_size_after: None,
                started: SystemTime::now() - Duration::from_secs(7200),
                finished: None,
                error: None,
                immediate: true,
                protection: Protection::Type1,
                prot_type_after: None,
                resets: vec![],
            }),
        }
    }

    #[test]
    fn progress_reaches_the_record_and_a_poll_without_one_keeps_it() {
        let h = a_run();
        h.set_progress(Some(0x4000));
        h.set_progress(None);
        let run = h.run.lock().unwrap().clone();
        assert_eq!(run.progress_pct, Some(25));
        let eta = run.eta_secs.unwrap();
        assert!((21600..21700).contains(&eta), "{eta}");
        let mut rec = FormatRecord::default();
        copy_to_record(&run, &mut rec);
        assert_eq!((rec.progress_pct, rec.eta_secs, rec.phase.as_str()), (Some(25), Some(eta), "formatting"));
        assert!(rec.progress_at.is_some());
        // It survives inventory.json, and one written before the fields loads.
        let back: FormatRecord = serde_json::from_value(serde_json::to_value(&rec).unwrap()).unwrap();
        assert_eq!(back, rec);
        let old: FormatRecord = serde_json::from_value(serde_json::json!({
            "from_block_size": 520, "to_block_size": 4096, "state": "running", "started": null, "finished": null
        }))
        .unwrap();
        assert_eq!(old.progress_pct, None);
        h.set_finished_progress();
        assert_eq!(h.run.lock().unwrap().progress_pct, Some(100));
    }

    #[test]
    fn a_drive_is_found_by_wwid_when_names_moved() {
        let root = std::env::temp_dir().join(format!("stormdrive-wwid-{}", std::process::id()));
        for (n, w) in [("sdb", "naa.other"), ("sdc", "naa.mine"), ("sdf", "naa.mine")] {
            std::fs::create_dir_all(root.join(n).join("device")).unwrap();
            std::fs::write(root.join(n).join("device/wwid"), format!("{w}\n")).unwrap();
        }
        std::fs::create_dir_all(root.join("loop0")).unwrap();
        assert_eq!(names_by_wwid(&root, "naa.mine"), ["sdc", "sdf"]);
        assert!(names_by_wwid(&root, "naa.none").is_empty());
        assert!(names_by_wwid(&root.join("nothing"), "naa.mine").is_empty());
        std::fs::remove_dir_all(&root).unwrap();
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
