//! Shelf (IOM) firmware through SES (#35): the Download Microcode Control
//! diagnostic page (0x0E, SEND DIAGNOSTIC), watched through the Download
//! Microcode Status page (0x0E, RECEIVE DIAGNOSTIC) — SES-3 §6.1.14/§6.1.15,
//! what `sg_ses_microcode` sends.
//!
//! One IOM at a time: on a dual-IOM shelf each IOM is its own SES device (an
//! ESP path), and every drive has a path through each. The first IOM is
//! updated, it restarts, and only when it answers again **and** the drives
//! have their second path back is the other one touched. A shelf where a
//! drive that serves data would lose its only path is refused unless the
//! request says `allow_path_loss`. Never automatic; the images come from the
//! same store as drive firmware (`<data_dir>/firmware`, #29 says where they
//! come from).
//!
//! Page building and parsing are portable and unit-tested; sending them is
//! Linux-only.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::api::AppState;
use crate::drive::Drive;
use crate::events::Severity;
use crate::ses::EspPath;

pub const PAGE_MICROCODE: u8 = 0x0E;
/// Download with offsets, save, and activate (the enclosure activates the
/// new microcode once the last byte arrives).
pub const MODE_OFFSETS_SAVE_ACTIVATE: u8 = 0x07;
/// A diagnostic page is at most 64 KiB (SEND DIAGNOSTIC's 16-bit length):
/// a chunk leaves room for the page's 24-byte header.
pub const MAX_CHUNK: usize = 65_508;

/// One chunk of the image as a Download Microcode Control page. The data is
/// padded to a multiple of 4 bytes; the page's own length says the padding.
pub fn control_page(subenclosure: u8, generation: u32, offset: u32, total: u32, chunk: &[u8]) -> Vec<u8> {
    let padded = (chunk.len() + 3) & !3;
    let mut p = vec![0u8; 24 + padded];
    p[0] = PAGE_MICROCODE;
    p[1] = subenclosure;
    let len = (p.len() - 4) as u16;
    p[2..4].copy_from_slice(&len.to_be_bytes());
    p[4..8].copy_from_slice(&generation.to_be_bytes());
    p[8] = MODE_OFFSETS_SAVE_ACTIVATE;
    // byte 11: buffer id 0
    p[12..16].copy_from_slice(&offset.to_be_bytes());
    p[16..20].copy_from_slice(&total.to_be_bytes());
    p[20..24].copy_from_slice(&(chunk.len() as u32).to_be_bytes());
    p[24..24 + chunk.len()].copy_from_slice(chunk);
    p
}

/// One subenclosure's descriptor in the Download Microcode Status page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SubStatus {
    pub subenclosure: u8,
    pub status: u8,
    pub additional: u8,
    /// The largest image it takes (0 = not said).
    pub max_size: u32,
    pub expected_buffer_id: u8,
    pub expected_offset: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadStatus {
    pub generation: u32,
    /// The primary subenclosure (the ESP's own) first.
    pub subs: Vec<SubStatus>,
}

pub fn parse_status(raw: &[u8]) -> Option<DownloadStatus> {
    if raw.len() < 8 || raw[0] != PAGE_MICROCODE {
        return None;
    }
    let n = raw[1] as usize + 1;
    let generation = u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]);
    let mut subs = vec![];
    for i in 0..n {
        let o = 8 + 16 * i;
        let Some(d) = raw.get(o..o + 16) else { break };
        subs.push(SubStatus {
            subenclosure: d[1],
            status: d[2],
            additional: d[3],
            max_size: u32::from_be_bytes([d[4], d[5], d[6], d[7]]),
            expected_buffer_id: d[11],
            expected_offset: u32::from_be_bytes([d[12], d[13], d[14], d[15]]),
        });
    }
    if subs.is_empty() {
        return None;
    }
    Some(DownloadStatus { generation, subs })
}

/// When the new microcode runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Activation {
    Now,
    AfterReset,
    AfterPowerCycle,
    /// Needs an activate (mode 0x0F), a hard reset or a power cycle.
    AfterActivate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// No download in progress (also: an IOM that restarted on new code).
    Idle,
    /// Waiting for more chunks.
    Awaiting,
    /// Writing flash, or a state this does not know but is not an error.
    Updating,
    Done(Activation),
    Failed(String),
}

pub fn phase(status: u8, additional: u8) -> Phase {
    match status {
        0x00 => Phase::Idle,
        0x01 => Phase::Awaiting,
        0x10 => Phase::Done(Activation::Now),
        0x11 => Phase::Done(Activation::AfterReset),
        0x12 => Phase::Done(Activation::AfterPowerCycle),
        0x13 => Phase::Done(Activation::AfterActivate),
        s if s >= 0x80 => Phase::Failed(format!(
            "the enclosure discarded the microcode (status 0x{s:02X}, additional 0x{additional:02X}{})",
            match s {
                0x80 => ": error",
                0x81 => ": vendor-specific error",
                0x82 => ": invalid image",
                _ => "",
            }
        )),
        _ => Phase::Updating,
    }
}

/// Bytes per chunk: the configured size, at most what a page holds, a
/// multiple of 4.
pub fn chunk_len(configured: usize) -> usize {
    (configured.clamp(4, MAX_CHUNK)) & !3
}

/// The drives on this shelf that serve data (in the fleet, or holding a
/// slab) and would lose their only path while one IOM restarts.
pub fn path_loss_blocker(shelf_key: &str, drives: &[Drive], esp_paths: usize) -> Option<String> {
    let at_risk: Vec<&str> = drives
        .iter()
        .filter(|d| d.location.shelf.as_ref().and_then(|s| s.key()).as_deref() == Some(shelf_key))
        .filter(|d| d.serves_data() && d.activity != crate::drive::Activity::Missing)
        .filter(|d| esp_paths < 2 || d.paths.len() < 2)
        .map(|d| d.name.as_str())
        .collect();
    if at_risk.is_empty() {
        return None;
    }
    Some(format!(
        "{} drive(s) serving data on this shelf have no second path, so an IOM restart takes them offline: {} (allow_path_loss to go ahead)",
        at_risk.len(),
        at_risk.join(", ")
    ))
}

// ------------------------------------------------------------------ runs

#[derive(Debug, Clone, Serialize)]
pub struct IomRun {
    pub scsi_id: String,
    pub serial: Option<String>,
    pub sas_address: Option<String>,
    pub from_revision: Option<String>,
    pub to_revision: Option<String>,
    /// `queued`, `download`, `updating`, `restarting`, `paths`, `done`,
    /// `failed`, `skipped`.
    pub phase: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub activation: Option<Activation>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShelfFwRun {
    pub shelf: String,
    pub image: String,
    /// `running`, `done`, `failed`.
    pub state: String,
    pub ioms: Vec<IomRun>,
    pub started: SystemTime,
    pub finished: Option<SystemTime>,
    pub error: Option<String>,
}

pub struct ShelfFwHandle {
    pub run: Mutex<ShelfFwRun>,
}

impl ShelfFwHandle {
    fn iom(&self, i: usize, f: impl FnOnce(&mut IomRun)) {
        if let Some(r) = self.run.lock().unwrap().ioms.get_mut(i) {
            f(r);
        }
    }
    pub fn view(&self) -> ShelfFwRun {
        self.run.lock().unwrap().clone()
    }
    pub fn running(&self) -> bool {
        self.run.lock().unwrap().state == "running"
    }
}

pub type ShelfRuns = HashMap<String, Arc<ShelfFwHandle>>;

const POLL: Duration = Duration::from_secs(5);
/// An IOM restarting on new code drops off the bus for a while.
const RESTART_WAIT: Duration = Duration::from_secs(15 * 60);
/// …and the drives' paths through it come back after it does.
const PATHS_WAIT: Duration = Duration::from_secs(10 * 60);

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::scsi::Device;

    /// SEND DIAGNOSTIC for one chunk: the IOM may write flash before it
    /// answers.
    const T_CHUNK: u32 = 120_000;

    fn status(sg: &str) -> Result<DownloadStatus, String> {
        let dev = Device::open(sg).map_err(|e| format!("open {sg}: {e}"))?;
        let raw = dev.receive_diagnostic(PAGE_MICROCODE).map_err(|e| format!("download microcode status: {e}"))?;
        parse_status(&raw).ok_or_else(|| "download microcode status: short or not page 0x0E".into())
    }

    /// The SES device's revision now, found by SAS address (an IOM that
    /// restarted may come back under another H:C:T:L), else by SCSI id.
    pub fn revision_of(esp: &EspPath) -> Option<(String, Option<String>)> {
        let devices = crate::ses::enclosure_devices();
        let found = devices
            .iter()
            .find(|(_, dir)| {
                esp.sas_address.as_ref().is_some_and(|a| {
                    std::fs::read_to_string(dir.join("sas_address")).ok().map(|s| crate::ses::normalize_sas(&s)).as_deref() == Some(a.as_str())
                })
            })
            .or_else(|| devices.iter().find(|(id, _)| *id == esp.scsi_id))?;
        let rev = std::fs::read_to_string(found.1.join("rev")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let sg = crate::scsi::sg_path_in(&found.1.join("scsi_generic").to_string_lossy());
        Some((rev.unwrap_or_default(), sg))
    }

    /// Download the image into one IOM and wait until it runs it (or says
    /// when it will). `progress(bytes, phase)`.
    pub fn update_esp(esp: &EspPath, image: &[u8], chunk: usize, progress: &(dyn Fn(u64, &str) + Sync)) -> Result<Activation, String> {
        let sg = esp.sg_path.clone().ok_or("the ESP has no sg node")?;
        let st = status(&sg)?;
        let me = st.subs[0];
        match phase(me.status, me.additional) {
            Phase::Awaiting | Phase::Updating => return Err("a microcode download is already in progress on this IOM".into()),
            _ => {}
        }
        if me.max_size > 0 && image.len() as u64 > u64::from(me.max_size) {
            return Err(format!("the image is {} bytes; this IOM takes at most {}", image.len(), me.max_size));
        }
        let total = image.len() as u32;
        let chunk = chunk_len(chunk);
        let dev = Device::open(&sg).map_err(|e| format!("open {sg}: {e}"))?;
        let mut off = 0usize;
        progress(0, "download");
        while off < image.len() {
            let end = (off + chunk).min(image.len());
            let page = control_page(me.subenclosure, st.generation, off as u32, total, &image[off..end]);
            dev.send_diagnostic_timeout(&page, T_CHUNK).map_err(|e| format!("SEND DIAGNOSTIC at offset {off}: {e}"))?;
            off = end;
            progress(off as u64, "download");
            // Stop at the first chunk the enclosure refuses.
            if off < image.len() {
                if let Ok(s) = status(&sg) {
                    if let Phase::Failed(why) = phase(s.subs[0].status, s.subs[0].additional) {
                        return Err(why);
                    }
                }
            }
        }
        drop(dev);
        // The last chunk starts the update; the IOM may restart and vanish.
        progress(off as u64, "updating");
        let start = std::time::Instant::now();
        let mut lost = false;
        loop {
            std::thread::sleep(POLL);
            let now_sg = revision_of(esp).and_then(|(_, sg)| sg).unwrap_or_else(|| sg.clone());
            match status(&now_sg) {
                Ok(s) => match phase(s.subs[0].status, s.subs[0].additional) {
                    Phase::Done(a) => return Ok(a),
                    // Back on the new code: the status reset with it.
                    Phase::Idle => return Ok(Activation::Now),
                    Phase::Failed(why) => return Err(why),
                    Phase::Awaiting => return Err(format!("the IOM still waits for data at offset {} after the whole image", s.subs[0].expected_offset)),
                    Phase::Updating => progress(off as u64, "updating"),
                },
                Err(_) => {
                    if !lost {
                        lost = true;
                        progress(off as u64, "restarting");
                    }
                }
            }
            if start.elapsed() > RESTART_WAIT {
                return Err(format!("the IOM did not report a finished update within {} min", RESTART_WAIT.as_secs() / 60));
            }
        }
    }
}

#[cfg(target_os = "linux")]
use linux::{revision_of, update_esp};

#[cfg(not(target_os = "linux"))]
fn revision_of(_: &EspPath) -> Option<(String, Option<String>)> {
    None
}
#[cfg(not(target_os = "linux"))]
fn update_esp(_: &EspPath, _: &[u8], _: usize, _: &(dyn Fn(u64, &str) + Sync)) -> Result<Activation, String> {
    Err("SES requires Linux".into())
}

/// Each drive on the shelf and how many paths it has now.
async fn shelf_paths(state: &AppState, key: &str) -> Vec<(String, usize)> {
    let inv = state.inventory.read().await;
    inv.drives
        .values()
        .filter(|d| d.location.shelf.as_ref().and_then(|s| s.key()).as_deref() == Some(key))
        .filter(|d| d.activity != crate::drive::Activity::Missing)
        .map(|d| (d.name.clone(), d.paths.len()))
        .collect()
}

/// Start updating every IOM of the shelf, one at a time. `Err` when it
/// cannot start: unknown shelf, a run already going, no ESP, or a drive
/// that would lose its only path.
pub async fn start(state: Arc<AppState>, report: crate::ses::ShelfReport, image_name: String, image: Arc<Vec<u8>>, allow_path_loss: bool) -> Result<Arc<ShelfFwHandle>, String> {
    let key = report.key.clone();
    if state.shelf_firmware.read().await.get(&key).is_some_and(|h| h.running()) {
        return Err(format!("shelf {key}: a firmware update is already running"));
    }
    let esps: Vec<EspPath> = {
        let mut e: Vec<EspPath> = report.esps.iter().filter(|e| e.sg_path.is_some()).cloned().collect();
        e.sort_by(|a, b| a.scsi_id.cmp(&b.scsi_id));
        e
    };
    if esps.is_empty() {
        return Err(format!("shelf {key}: no SES path with an sg node"));
    }
    if !allow_path_loss {
        let drives: Vec<Drive> = state.inventory.read().await.drives.values().cloned().collect();
        if let Some(why) = path_loss_blocker(&key, &drives, esps.len()) {
            return Err(format!("shelf {key}: {why}"));
        }
    }
    let handle = Arc::new(ShelfFwHandle {
        run: Mutex::new(ShelfFwRun {
            shelf: key.clone(),
            image: image_name.clone(),
            state: "running".into(),
            ioms: esps
                .iter()
                .map(|e| IomRun {
                    scsi_id: e.scsi_id.clone(),
                    serial: e.serial.clone(),
                    sas_address: e.sas_address.clone(),
                    from_revision: e.revision.clone(),
                    to_revision: None,
                    phase: "queued".into(),
                    bytes_done: 0,
                    bytes_total: image.len() as u64,
                    activation: None,
                    error: None,
                })
                .collect(),
            started: SystemTime::now(),
            finished: None,
            error: None,
        }),
    });
    state.shelf_firmware.write().await.insert(key.clone(), handle.clone());
    state.events.write().await.push(
        None,
        Severity::Warning,
        "shelf",
        format!("shelf {}: IOM firmware update with {image_name} started, {} IOM(s), one at a time", report.shelf.display(), esps.len()),
    );
    let chunk = (state.config.firmware.chunk_kib as usize) * 1024;
    let h = handle.clone();
    tokio::spawn(async move {
        let mut error = None;
        for (i, esp) in esps.iter().enumerate() {
            let before = shelf_paths(&state, &key).await;
            let (h2, e2, img) = (h.clone(), esp.clone(), image.clone());
            let r = tokio::task::spawn_blocking(move || {
                let progress = |bytes: u64, phase: &str| h2.iom(i, |r| {
                    r.bytes_done = bytes;
                    r.phase = phase.into();
                });
                update_esp(&e2, &img, chunk, &progress)
            })
            .await
            .unwrap_or_else(|e| Err(format!("task: {e}")));
            let rev = revision_of(esp).map(|(r, _)| r).filter(|r| !r.is_empty());
            match r {
                Ok(a) => {
                    h.iom(i, |r| {
                        r.activation = Some(a);
                        r.to_revision = rev.clone();
                        r.phase = "paths".into();
                    });
                    // The drives' paths through this IOM must be back before
                    // the next IOM goes down.
                    let waited = std::time::Instant::now();
                    loop {
                        let now = shelf_paths(&state, &key).await;
                        let short: Vec<&str> = before
                            .iter()
                            .filter(|(n, p)| !matches!(now.iter().find(|(m, _)| m == n), Some((_, q)) if q >= p))
                            .map(|(n, _)| n.as_str())
                            .collect();
                        if short.is_empty() {
                            break;
                        }
                        if waited.elapsed() > PATHS_WAIT {
                            error = Some(format!("after IOM {}: paths did not come back for {}; the other IOM(s) were not touched", esp.scsi_id, short.join(", ")));
                            break;
                        }
                        tokio::time::sleep(POLL).await;
                    }
                    let msg = format!(
                        "shelf {key}: IOM {} ({}) updated {} → {}{}",
                        esp.scsi_id,
                        esp.serial.clone().unwrap_or_default(),
                        esp.revision.clone().unwrap_or_else(|| "?".into()),
                        rev.clone().unwrap_or_else(|| "?".into()),
                        match a {
                            Activation::Now => String::new(),
                            other => format!(" (runs {other:?})"),
                        }
                    );
                    state.events.write().await.push(None, Severity::Info, "shelf", msg);
                    h.iom(i, |r| r.phase = if error.is_some() { "failed".into() } else { "done".into() });
                    if error.is_some() {
                        break;
                    }
                }
                Err(e) => {
                    h.iom(i, |r| {
                        r.phase = "failed".into();
                        r.error = Some(e.clone());
                    });
                    error = Some(format!("IOM {}: {e}; the other IOM(s) were not touched", esp.scsi_id));
                    break;
                }
            }
        }
        {
            let mut run = h.run.lock().unwrap();
            for r in run.ioms.iter_mut().filter(|r| r.phase == "queued") {
                r.phase = "skipped".into();
            }
            run.state = if error.is_some() { "failed".into() } else { "done".into() };
            run.error = error.clone();
            run.finished = Some(SystemTime::now());
        }
        state.events.write().await.push(
            None,
            if error.is_some() { Severity::Error } else { Severity::Info },
            "shelf",
            match &error {
                Some(e) => format!("shelf {key}: IOM firmware update FAILED — {e}"),
                None => format!("shelf {key}: IOM firmware update done"),
            },
        );
    });
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::{Location, Membership, Shelf};

    #[test]
    fn a_chunk_is_a_download_microcode_control_page() {
        let p = control_page(0, 0x0000_0102, 4096, 10_000, &[0xAA; 5]);
        assert_eq!(p.len(), 24 + 8, "5 bytes padded to 8");
        assert_eq!(p[0], 0x0E);
        assert_eq!(u16::from_be_bytes([p[2], p[3]]) as usize, p.len() - 4);
        assert_eq!(&p[4..8], &[0, 0, 1, 2], "the expected generation");
        assert_eq!(p[8], 0x07, "offsets, save, activate");
        assert_eq!(u32::from_be_bytes([p[12], p[13], p[14], p[15]]), 4096);
        assert_eq!(u32::from_be_bytes([p[16], p[17], p[18], p[19]]), 10_000);
        assert_eq!(u32::from_be_bytes([p[20], p[21], p[22], p[23]]), 5, "the data length, not the padded one");
        assert_eq!(&p[24..29], &[0xAA; 5]);
        assert_eq!(&p[29..], &[0, 0, 0]);
        assert_eq!(control_page(2, 0, 0, 4, &[1, 2, 3, 4])[1], 2, "subenclosure");
    }

    #[test]
    fn the_status_page_per_subenclosure() {
        let mut raw = vec![0u8; 8 + 32];
        raw[0] = 0x0E;
        raw[1] = 1; // one secondary
        raw[3] = 36;
        raw[4..8].copy_from_slice(&7u32.to_be_bytes());
        // primary: id 0, awaiting, max 2 MiB, expecting offset 65536
        raw[9] = 0;
        raw[10] = 0x01;
        raw[12..16].copy_from_slice(&(2u32 << 20).to_be_bytes());
        raw[20..24].copy_from_slice(&65536u32.to_be_bytes());
        // secondary: id 1, done (after reset)
        raw[25] = 1;
        raw[26] = 0x11;
        let s = parse_status(&raw).unwrap();
        assert_eq!(s.generation, 7);
        assert_eq!(s.subs.len(), 2);
        assert_eq!((s.subs[0].status, s.subs[0].max_size, s.subs[0].expected_offset), (0x01, 2 << 20, 65536));
        assert_eq!((s.subs[1].subenclosure, s.subs[1].status), (1, 0x11));
        assert!(parse_status(&raw[..6]).is_none());
        assert!(parse_status(&[0x02, 0, 0, 0, 0, 0, 0, 0]).is_none(), "not page 0x0E");
    }

    #[test]
    fn status_codes() {
        assert_eq!(phase(0x00, 0), Phase::Idle);
        assert_eq!(phase(0x01, 0), Phase::Awaiting);
        assert_eq!(phase(0x02, 0), Phase::Updating);
        assert_eq!(phase(0x10, 0), Phase::Done(Activation::Now));
        assert_eq!(phase(0x11, 0), Phase::Done(Activation::AfterReset));
        assert_eq!(phase(0x12, 0), Phase::Done(Activation::AfterPowerCycle));
        assert_eq!(phase(0x13, 0), Phase::Done(Activation::AfterActivate));
        assert!(matches!(phase(0x82, 0x05), Phase::Failed(w) if w.contains("invalid image") && w.contains("0x05")));
        assert_eq!(phase(0x70, 0), Phase::Updating, "unknown, not an error: keep watching (bounded)");
        assert_eq!(chunk_len(32 * 1024), 32 * 1024);
        assert_eq!(chunk_len(1 << 20), MAX_CHUNK & !3);
        assert_eq!(chunk_len(4097), 4096);
        assert_eq!(chunk_len(0), 4);
    }

    fn on_shelf(name: &str, paths: usize, fleet: bool) -> Drive {
        let mut d = Drive::test_fixture(name);
        d.location = Location { shelf: Some(Shelf { logical_id: Some("5000a098aaaa0001".into()), ..Default::default() }), ..Default::default() };
        d.paths = (0..paths).map(|i| format!("/dev/{name}{i}")).collect();
        if fleet {
            d.membership = Membership::Fleet;
        }
        d
    }

    #[test]
    fn an_iom_restart_must_not_take_a_drive_serving_data_offline() {
        let key = "5000a098aaaa0001";
        let dual = [on_shelf("sda", 2, true), on_shelf("sdb", 2, true), on_shelf("sdc", 1, false)];
        assert_eq!(path_loss_blocker(key, &dual, 2), None, "every data drive keeps a path; sdc serves nothing");
        let e = path_loss_blocker(key, &dual, 1).unwrap();
        assert!(e.contains("sda, sdb") && e.contains("allow_path_loss"), "{e}");
        let mut one_path = on_shelf("sdd", 1, false);
        one_path.in_use_by = Some("stormblock (partition 1)".into());
        let e = path_loss_blocker(key, &[one_path], 2).unwrap();
        assert!(e.starts_with("1 drive(s)") && e.contains("sdd"), "{e}");
        assert_eq!(path_loss_blocker("another-shelf", &dual, 1), None);
    }
}
