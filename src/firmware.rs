//! Firmware updates: an image from the node's store onto one drive or
//! many.
//!
//! SAS/SATA (through SAT): WRITE BUFFER mode 0x0E — download microcode
//! with offsets, save, defer activation — in chunks, then mode 0x0F to
//! activate. Drives that do not know 0x0E get mode 0x07 (download with
//! offsets and save; activates after the last chunk). Chunk size is the
//! configured value rounded up to the drive's READ BUFFER offset
//! boundary. After activation the drive is polled ready, the kernel is
//! asked to rescan, and INQUIRY is read back for the new revision.
//!
//! NVMe: Firmware Image Download (0x11) in chunks, then Firmware Commit
//! (0x10) with CA=3 (activate now); if the controller answers "activation
//! requires reset" (or refuses CA=3), it is committed with CA=1 and the
//! record says so — the new image runs after the next reset.
//!
//! Policy: out-of-fleet drives update in parallel; fleet drives one at a
//! time (a node-wide lock), never while Failing/Failed unless forced.
//! Never automatic.

use crate::api::AppState;
use crate::drive::{Activity, Drive, DriveId, DriveKind, FirmwareRecord, Membership};
use crate::events::Severity;
use crate::scsi::{self, Device, Error as ScsiError};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FwState {
    Queued,
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct FwRun {
    pub drive: DriveId,
    pub name: String,
    pub image: String,
    pub from_version: String,
    pub to_version: Option<String>,
    pub state: FwState,
    /// `queued`, `download`, `activate`, `ready`, `verify`, `done`, `failed`.
    pub phase: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub started: SystemTime,
    pub finished: Option<SystemTime>,
    pub error: Option<String>,
    pub reset_required: bool,
    /// The transfer mode that worked: `deferred` (0x0e/0x0f), `offsets`
    /// (0x07), `nvme`.
    pub mode: String,
}

pub struct FwHandle {
    pub run: Mutex<FwRun>,
}

impl FwHandle {
    fn phase(&self, p: &str) {
        self.run.lock().unwrap().phase = p.into();
    }
    fn progress(&self, done: u64) {
        self.run.lock().unwrap().bytes_done = done;
    }
}

/// An image in the store.
#[derive(Debug, Clone, Serialize)]
pub struct Image {
    pub name: String,
    pub size: u64,
    pub sha256: String,
    pub modified: Option<SystemTime>,
}

/// Image names are plain file names: no separators, no dot-leading.
pub fn valid_image_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 128
        && !n.starts_with('.')
        && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

pub fn image_dir(data_dir: Option<&str>) -> Option<PathBuf> {
    data_dir.map(|d| Path::new(d).join("firmware"))
}

/// Plain SHA-256 (no dependency): the store is small and this runs once
/// per upload/list.
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut a = h;
        for i in 0..64 {
            let s1 = a[4].rotate_right(6) ^ a[4].rotate_right(11) ^ a[4].rotate_right(25);
            let ch = (a[4] & a[5]) ^ (!a[4] & a[6]);
            let t1 = a[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a[0].rotate_right(2) ^ a[0].rotate_right(13) ^ a[0].rotate_right(22);
            let maj = (a[0] & a[1]) ^ (a[0] & a[2]) ^ (a[1] & a[2]);
            let t2 = s0.wrapping_add(maj);
            a = [t1.wrapping_add(t2), a[0], a[1], a[2], a[3].wrapping_add(t1), a[4], a[5], a[6]];
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(a[i]);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

pub fn list_images(dir: &Path) -> Vec<Image> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !valid_image_name(&name) {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let sha = std::fs::read(e.path()).map(|d| sha256_hex(&d)).unwrap_or_default();
        out.push(Image {
            name,
            size: meta.len(),
            sha256: sha,
            modified: meta.modified().ok(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Chunk length to use: the configured size, rounded up to the drive's
/// offset boundary when it has one, never 0.
pub fn chunk_len(configured: u32, boundary: u32) -> u32 {
    let c = configured.max(512);
    if boundary == 0 {
        return c;
    }
    c.div_ceil(boundary) * boundary
}

/// Interpret an NVMe Firmware Commit status: Ok(reset_required).
pub fn interpret_commit_status(st: u32) -> Result<bool, String> {
    match st {
        0 => Ok(false),
        // Firmware activation requires: conventional reset, NVM subsystem
        // reset, controller-level reset, max time violation → committed,
        // takes effect after reset.
        0x10b | 0x10c | 0x10d | 0x110 => Ok(true),
        0x106 => Err("invalid firmware slot".into()),
        0x107 => Err("invalid firmware image".into()),
        0x111 => Err("firmware activation prohibited".into()),
        0x112 => Err("overlapping range".into()),
        other => Err(format!("nvme status 0x{other:x}")),
    }
}

const T_CHUNK: u32 = 120_000;
const T_ACTIVATE: u32 = 10 * 60 * 1000;
const READY_WAIT: Duration = Duration::from_secs(5 * 60);

fn dev_path(drive: &Drive) -> String {
    scsi::sg_path_for_block(&drive.name).unwrap_or_else(|| drive.path.clone())
}

fn wait_ready(dev: &Device) -> Result<(), String> {
    let t0 = Instant::now();
    loop {
        match dev.test_unit_ready() {
            Ok(()) => return Ok(()),
            Err(ScsiError::Sense(s)) if s.is_unit_attention() || s.key == 0x2 => {}
            Err(e) if t0.elapsed() > Duration::from_secs(20) => return Err(format!("not ready after activation: {e}")),
            Err(_) => {}
        }
        if t0.elapsed() > READY_WAIT {
            return Err("drive did not come ready within 5 minutes of activation".into());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn run_scsi(drive: &Drive, image: &[u8], chunk_kib: u32, handle: &FwHandle) -> Result<(String, bool), String> {
    let dev = Device::open(&dev_path(drive)).map_err(|e| format!("open: {e}"))?;
    let _ = dev.test_unit_ready();
    let before = dev.inquiry().map(|i| i.revision).unwrap_or_else(|_| drive.firmware.clone());
    let boundary = dev.read_buffer_descriptor().map(|(b, _)| b).unwrap_or(0);
    let chunk = chunk_len(chunk_kib * 1024, boundary) as usize;

    handle.phase("download");
    let mut mode = 0x0e_u8;
    let mut off = 0usize;
    while off < image.len() {
        let end = (off + chunk).min(image.len());
        match dev.write_buffer(mode, off as u32, &image[off..end], T_CHUNK) {
            Ok(()) => {}
            Err(ScsiError::Sense(s)) if off == 0 && mode == 0x0e && s.is_illegal_request() => {
                tracing::info!(drive = %drive.name, "deferred download refused ({s}); using mode 0x07");
                mode = 0x07;
                handle.run.lock().unwrap().mode = "offsets".into();
                dev.write_buffer(mode, 0, &image[off..end], T_CHUNK)
                    .map_err(|e| format!("write buffer (mode 0x07) at 0: {e}"))?;
            }
            Err(e) => return Err(format!("write buffer (mode 0x{mode:02x}) at {off}: {e}")),
        }
        off = end;
        handle.progress(off as u64);
    }

    handle.phase("activate");
    if mode == 0x0e {
        dev.write_buffer(0x0f, 0, &[], T_ACTIVATE)
            .map_err(|e| format!("activate deferred microcode: {e}"))?;
    }
    handle.phase("ready");
    wait_ready(&dev)?;
    std::thread::sleep(Duration::from_secs(2));

    handle.phase("verify");
    let after = dev.inquiry().map(|i| i.revision).map_err(|e| format!("inquiry after activation: {e}"))?;
    for p in &drive.paths {
        rescan_block(p.trim_start_matches("/dev/"));
    }
    if after == before {
        tracing::warn!(drive = %drive.name, "firmware revision unchanged ({after}) after update");
    }
    Ok((after, false))
}

#[cfg(target_os = "linux")]
fn run_nvme(drive: &Drive, image: &[u8], chunk_kib: u32, handle: &FwHandle) -> Result<(String, bool), String> {
    use crate::smart::nvme::linux::admin;
    let chunk = chunk_len(chunk_kib * 1024, 4096) as usize;
    handle.run.lock().unwrap().mode = "nvme".into();
    handle.phase("download");
    let mut off = 0usize;
    while off < image.len() {
        let end = (off + chunk).min(image.len());
        let mut buf = image[off..end].to_vec();
        // Pad to a dword multiple; NUMD counts dwords - 1.
        while buf.len() % 4 != 0 {
            buf.push(0);
        }
        let numd = (buf.len() / 4 - 1) as u32;
        let ofst = (off / 4) as u32;
        let st = admin(&drive.path, 0x11, 0, numd, ofst, &mut buf, T_CHUNK)
            .map_err(|e| format!("firmware image download at {off}: {e}"))?;
        if st != 0 {
            return Err(format!("firmware image download at {off}: nvme status 0x{st:x}"));
        }
        off = end;
        handle.progress(off as u64);
    }
    handle.phase("activate");
    // CA=3: replace the image in the slot the controller picks and
    // activate without reset. CA=1 when that is refused.
    let reset_required;
    let st = admin(&drive.path, 0x10, 0, 3 << 3, 0, &mut [], T_ACTIVATE).map_err(|e| format!("firmware commit: {e}"))?;
    match interpret_commit_status(st) {
        Ok(r) => reset_required = r,
        Err(why) => {
            tracing::info!(drive = %drive.name, "commit CA=3 refused ({why}); committing for next reset (CA=1)");
            let st1 = admin(&drive.path, 0x10, 0, 1 << 3, 0, &mut [], T_ACTIVATE)
                .map_err(|e| format!("firmware commit (CA=1): {e}"))?;
            interpret_commit_status(st1).map_err(|e| format!("firmware commit (CA=1): {e}"))?;
            reset_required = true;
        }
    }
    handle.phase("verify");
    std::thread::sleep(Duration::from_secs(2));
    let after = std::fs::read_to_string(format!("/sys/block/{}/device/firmware_rev", drive.name))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| drive.firmware.clone());
    Ok((after, reset_required))
}

#[cfg(not(target_os = "linux"))]
fn run_nvme(_drive: &Drive, _image: &[u8], _chunk_kib: u32, _handle: &FwHandle) -> Result<(String, bool), String> {
    Err("NVMe firmware update requires Linux".into())
}

#[cfg(target_os = "linux")]
fn rescan_block(name: &str) {
    let _ = std::fs::write(format!("/sys/block/{name}/device/rescan"), "1");
}

#[cfg(not(target_os = "linux"))]
fn rescan_block(_name: &str) {}

fn run_blocking(drive: &Drive, image: &[u8], chunk_kib: u32, handle: &FwHandle) -> Result<(String, bool), String> {
    match drive.kind {
        DriveKind::NvmeSsd => run_nvme(drive, image, chunk_kib, handle),
        _ => run_scsi(drive, image, chunk_kib, handle),
    }
}

/// Start an update. Caller has validated the drive (`firmware_blocker`)
/// and read the image. Fleet drives wait for the node-wide lock so only
/// one of them is mid-update at a time.
pub async fn start(state: Arc<AppState>, drive: Drive, image_name: String, image: Arc<Vec<u8>>) -> Arc<FwHandle> {
    let handle = Arc::new(FwHandle {
        run: Mutex::new(FwRun {
            drive: drive.id,
            name: drive.name.clone(),
            image: image_name.clone(),
            from_version: drive.firmware.clone(),
            to_version: None,
            state: FwState::Queued,
            phase: "queued".into(),
            bytes_done: 0,
            bytes_total: image.len() as u64,
            started: SystemTime::now(),
            finished: None,
            error: None,
            reset_required: false,
            mode: "deferred".into(),
        }),
    });
    state.firmware.write().await.insert(drive.id, handle.clone());
    {
        let mut inv = state.inventory.write().await;
        if let Some(d) = inv.drives.get_mut(&drive.id) {
            d.activity = Activity::UpdatingFirmware;
            d.firmware_update = Some(FirmwareRecord {
                image: image_name.clone(),
                from_version: drive.firmware.clone(),
                to_version: None,
                state: "running".into(),
                started: Some(SystemTime::now()),
                finished: None,
                error: None,
                reset_required: false,
            });
        }
    }
    state.events.write().await.push(
        Some(drive.id),
        Severity::Info,
        "firmware",
        format!("{}: firmware update {} → image {image_name} queued", drive.name, drive.firmware),
    );
    state.persist().await;

    let h2 = handle.clone();
    let st2 = state.clone();
    let chunk_kib = state.config.firmware.chunk_kib;
    tokio::spawn(async move {
        // One fleet drive at a time; out-of-fleet drives do not queue.
        let _guard = if drive.membership == Membership::Fleet {
            Some(st2.fleet_firmware_lock.lock().await)
        } else {
            None
        };
        {
            let mut run = h2.run.lock().unwrap();
            run.state = FwState::Running;
            run.phase = "download".into();
            run.started = SystemTime::now();
        }
        let h3 = h2.clone();
        let d2 = drive.clone();
        let img = image.clone();
        let result = tokio::task::spawn_blocking(move || run_blocking(&d2, &img, chunk_kib, &h3))
            .await
            .unwrap_or_else(|e| Err(format!("firmware task panicked: {e}")));
        let (word, err, to, reset) = match &result {
            Ok((v, r)) => (FwState::Done, None, Some(v.clone()), *r),
            Err(e) => (FwState::Failed, Some(e.clone()), None, false),
        };
        {
            let mut run = h2.run.lock().unwrap();
            run.state = word;
            run.finished = Some(SystemTime::now());
            run.error = err.clone();
            run.to_version = to.clone();
            run.reset_required = reset;
            run.phase = if err.is_some() { "failed".into() } else { "done".into() };
        }
        {
            let mut inv = st2.inventory.write().await;
            if let Some(d) = inv.drives.get_mut(&drive.id) {
                if d.activity == Activity::UpdatingFirmware {
                    d.activity = Activity::Idle;
                }
                if let Some(v) = &to {
                    if !reset {
                        d.firmware = v.clone();
                    }
                }
                if let Some(f) = d.firmware_update.as_mut() {
                    f.state = if err.is_some() { "failed".into() } else { "done".into() };
                    f.finished = Some(SystemTime::now());
                    f.error = err.clone();
                    f.to_version = to.clone();
                    f.reset_required = reset;
                }
            }
        }
        st2.events.write().await.push(
            Some(drive.id),
            if err.is_some() { Severity::Error } else { Severity::Info },
            "firmware",
            match &result {
                Ok((v, true)) => format!("{}: image {image_name} committed; activates at next reset (running {v})", drive.name),
                Ok((v, false)) => format!("{}: firmware {} → {v} ({image_name})", drive.name, drive.firmware),
                Err(e) => format!("{}: firmware update FAILED: {e}", drive.name),
            },
        );
        st2.persist().await;
    });
    handle
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 56 bytes: exercises the padding boundary.
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn image_names() {
        assert!(valid_image_name("ST1200MM0098-N004.lod"));
        assert!(valid_image_name("fw_1.2.3+build.bin"));
        assert!(!valid_image_name(""));
        assert!(!valid_image_name("../etc/passwd"));
        assert!(!valid_image_name(".hidden"));
        assert!(!valid_image_name("a/b"));
        assert!(!valid_image_name("a b"));
    }

    #[test]
    fn chunk_rounds_to_boundary() {
        assert_eq!(chunk_len(32 * 1024, 0), 32 * 1024);
        assert_eq!(chunk_len(32 * 1024, 4096), 32 * 1024);
        assert_eq!(chunk_len(32 * 1024, 65536), 65536);
        assert_eq!(chunk_len(0, 0), 512);
        assert_eq!(chunk_len(1000, 512), 1024);
    }

    #[test]
    fn nvme_commit_status() {
        assert_eq!(interpret_commit_status(0), Ok(false));
        assert_eq!(interpret_commit_status(0x10b), Ok(true));
        assert_eq!(interpret_commit_status(0x10d), Ok(true));
        assert!(interpret_commit_status(0x107).is_err());
        assert!(interpret_commit_status(0x2).is_err());
    }

    #[test]
    fn image_store_lists_valid_files_with_hashes() {
        let dir = std::env::temp_dir().join(format!("sd-fw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("good.lod"), b"abc").unwrap();
        std::fs::write(dir.join(".partial"), b"x").unwrap();
        let imgs = list_images(&dir);
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].name, "good.lod");
        assert_eq!(imgs[0].size, 3);
        assert!(imgs[0].sha256.starts_with("ba7816bf"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
