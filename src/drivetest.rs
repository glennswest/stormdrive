//! Drive testing: exercise a drive and record the verdict, independent of
//! fleet membership — with the hard rule that anything destructive requires
//! an out-of-fleet, unmounted drive.
//!
//! Kinds:
//! - `smoke` — sampled reads across the device (first/last MiB + spread).
//! - `read_scan` — full sequential read, progress + cancel; hours on HDDs.
//! - `destructive_sample` — write a pattern to sampled regions, sync, read
//!   back through O_DIRECT and verify. Destroys data in those regions.
//!
//! Read-back verification uses O_DIRECT on Linux so the page cache cannot
//! vouch for a write the media lost. On other platforms destructive tests
//! are refused outright.

use crate::api::AppState;
use crate::drive::{Activity, Drive, DriveId};
use crate::events::Severity;
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestKind {
    Smoke,
    ReadScan,
    DestructiveSample,
}

impl TestKind {
    pub fn is_destructive(self) -> bool {
        matches!(self, TestKind::DestructiveSample)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestState {
    Running,
    Passed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestRun {
    pub kind: TestKind,
    pub state: TestState,
    pub started: SystemTime,
    pub finished: Option<SystemTime>,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub errors: Vec<String>,
}

pub struct TestHandle {
    pub run: Mutex<TestRun>,
    pub cancel: AtomicBool,
}

impl TestHandle {
    fn new(kind: TestKind, bytes_total: u64) -> Self {
        Self {
            run: Mutex::new(TestRun {
                kind,
                state: TestState::Running,
                started: SystemTime::now(),
                finished: None,
                bytes_done: 0,
                bytes_total,
                errors: Vec::new(),
            }),
            cancel: AtomicBool::new(false),
        }
    }
}

const MIB: u64 = 1 << 20;
const SCAN_CHUNK: usize = 4 * MIB as usize;
const SAMPLE_REGIONS: u64 = 16;
const DESTRUCTIVE_REGIONS: u64 = 8;

/// Offsets for sampled tests: first MiB, last full MiB, and an even spread
/// between. Deduplicated and clamped so tiny devices still work.
pub fn sample_offsets(capacity: u64, regions: u64) -> Vec<u64> {
    if capacity < MIB {
        return vec![];
    }
    let last = (capacity - MIB) / MIB * MIB;
    let mut offs = vec![0u64, last];
    if regions > 2 && last > 0 {
        let step = last / (regions - 1);
        for i in 1..regions - 1 {
            offs.push(i * step / MIB * MIB);
        }
    }
    offs.sort_unstable();
    offs.dedup();
    offs
}

/// The write pattern for a region: a function of the byte offset, so a
/// block written to the wrong LBA fails verification.
pub fn fill_pattern(buf: &mut [u8], base_offset: u64) {
    for (i, chunk) in buf.chunks_mut(8).enumerate() {
        let v = (base_offset + (i as u64) * 8) ^ 0x5044_5256_4D52_5453; // "STRMVRDP"
        let bytes = v.to_le_bytes();
        for (j, b) in chunk.iter_mut().enumerate() {
            *b = bytes[j];
        }
    }
}

/// A Vec whose slice starting at `.1` is aligned for O_DIRECT. The heap
/// allocation does not move when the Vec is returned, so the offset stays
/// valid.
fn aligned_buf(len: usize, align: usize) -> (Vec<u8>, usize) {
    let v = vec![0u8; len + align];
    let off = (align - (v.as_ptr() as usize) % align) % align;
    (v, off)
}

fn open_direct_read(path: &str) -> std::io::Result<std::fs::File> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECT)
            .open(path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::fs::OpenOptions::new().read(true).open(path)
    }
}

fn run_blocking(drive: &Drive, kind: TestKind, handle: &TestHandle) {
    let result = match kind {
        TestKind::Smoke => run_sampled_read(drive, handle),
        TestKind::ReadScan => run_read_scan(drive, handle),
        TestKind::DestructiveSample => run_destructive(drive, handle),
    };
    let mut run = handle.run.lock().unwrap();
    run.finished = Some(SystemTime::now());
    run.state = if handle.cancel.load(Ordering::Relaxed) {
        TestState::Cancelled
    } else {
        match result {
            Ok(()) if run.errors.is_empty() => TestState::Passed,
            Ok(()) => TestState::Failed,
            Err(e) => {
                run.errors.push(e.to_string());
                TestState::Failed
            }
        }
    };
}

fn run_sampled_read(drive: &Drive, handle: &TestHandle) -> std::io::Result<()> {
    let mut f = open_direct_read(&drive.path)?;
    let (mut buf, off) = aligned_buf(MIB as usize, 4096);
    for region in sample_offsets(drive.capacity_bytes, SAMPLE_REGIONS) {
        if handle.cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        f.seek(SeekFrom::Start(region))?;
        if let Err(e) = f.read_exact(&mut buf[off..off + MIB as usize]) {
            handle
                .run
                .lock()
                .unwrap()
                .errors
                .push(format!("read at {region}: {e}"));
        }
        handle.run.lock().unwrap().bytes_done += MIB;
    }
    Ok(())
}

fn run_read_scan(drive: &Drive, handle: &TestHandle) -> std::io::Result<()> {
    let mut f = open_direct_read(&drive.path)?;
    let (mut buf, off) = aligned_buf(SCAN_CHUNK, 4096);
    let mut pos = 0u64;
    while pos < drive.capacity_bytes {
        if handle.cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let want = (SCAN_CHUNK as u64).min(drive.capacity_bytes - pos) as usize;
        // O_DIRECT needs block-multiple lengths; the device size is a
        // block multiple, so `want` is too.
        if let Err(e) = f.read_exact(&mut buf[off..off + want]) {
            handle
                .run
                .lock()
                .unwrap()
                .errors
                .push(format!("read at {pos}: {e}"));
            // Skip ahead rather than abort: map the damage, don't stop at it.
            pos += want as u64;
            f.seek(SeekFrom::Start(pos))?;
            let mut r = handle.run.lock().unwrap();
            r.bytes_done = pos;
            if r.errors.len() >= 64 {
                r.errors.push("too many errors, stopping scan".into());
                return Ok(());
            }
            continue;
        }
        pos += want as u64;
        handle.run.lock().unwrap().bytes_done = pos;
    }
    Ok(())
}

fn run_destructive(drive: &Drive, handle: &TestHandle) -> std::io::Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (drive, handle);
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "destructive tests require Linux (O_DIRECT verification)",
        ));
    }
    #[cfg(target_os = "linux")]
    {
        let mut w = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&drive.path)?;
        let mut r = open_direct_read(&drive.path)?;
        let (mut wbuf, woff) = aligned_buf(MIB as usize, 4096);
        let (mut rbuf, roff) = aligned_buf(MIB as usize, 4096);
        for region in sample_offsets(drive.capacity_bytes, DESTRUCTIVE_REGIONS) {
            if handle.cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            let wslice = &mut wbuf[woff..woff + MIB as usize];
            fill_pattern(wslice, region);
            w.seek(SeekFrom::Start(region))?;
            w.write_all(wslice)?;
            w.sync_data()?;
            r.seek(SeekFrom::Start(region))?;
            let rslice = &mut rbuf[roff..roff + MIB as usize];
            match r.read_exact(rslice) {
                Err(e) => handle
                    .run
                    .lock()
                    .unwrap()
                    .errors
                    .push(format!("verify read at {region}: {e}")),
                Ok(()) => {
                    if rslice != &wbuf[woff..woff + MIB as usize] {
                        handle
                            .run
                            .lock()
                            .unwrap()
                            .errors
                            .push(format!("verify mismatch at {region}"));
                    }
                }
            }
            handle.run.lock().unwrap().bytes_done += 2 * MIB;
        }
        Ok(())
    }
}

/// Start a test: flips the drive to Testing, runs on the blocking pool,
/// restores Idle and emits the verdict event when done. Returns the shared
/// handle the API polls.
pub async fn start(
    state: Arc<AppState>,
    drive: Drive,
    kind: TestKind,
) -> Arc<TestHandle> {
    let bytes_total = match kind {
        TestKind::Smoke => sample_offsets(drive.capacity_bytes, SAMPLE_REGIONS).len() as u64 * MIB,
        TestKind::ReadScan => drive.capacity_bytes,
        TestKind::DestructiveSample => {
            sample_offsets(drive.capacity_bytes, DESTRUCTIVE_REGIONS).len() as u64 * 2 * MIB
        }
    };
    let handle = Arc::new(TestHandle::new(kind, bytes_total));
    state.tests.write().await.insert(drive.id, handle.clone());
    set_activity(&state, drive.id, Activity::Testing).await;
    state.events.write().await.push(
        Some(drive.id),
        Severity::Info,
        "test",
        format!("{}: {kind:?} test started", drive.name),
    );

    let h2 = handle.clone();
    let st2 = state.clone();
    tokio::spawn(async move {
        let h3 = h2.clone();
        let d2 = drive.clone();
        let _ = tokio::task::spawn_blocking(move || run_blocking(&d2, kind, &h3)).await;
        set_activity(&st2, drive.id, Activity::Idle).await;
        let (verdict, errors) = {
            let run = h2.run.lock().unwrap();
            (run.state, run.errors.len())
        };
        let sev = match verdict {
            TestState::Passed | TestState::Cancelled => Severity::Info,
            _ => Severity::Error,
        };
        st2.events.write().await.push(
            Some(drive.id),
            sev,
            "test",
            format!("{}: {kind:?} test {verdict:?} ({errors} errors)", drive.name),
        );
        st2.persist().await;
    });
    handle
}

async fn set_activity(state: &Arc<AppState>, id: DriveId, activity: Activity) {
    let mut inv = state.inventory.write().await;
    if let Some(d) = inv.drives.get_mut(&id) {
        // Never resurrect a drive that went missing mid-test.
        if !(activity == Activity::Idle && d.activity == Activity::Missing) {
            d.activity = activity;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_offsets_cover_ends_and_spread() {
        let offs = sample_offsets(100 * MIB, 4);
        assert_eq!(offs.first(), Some(&0));
        assert_eq!(offs.last(), Some(&(99 * MIB)));
        assert!(offs.len() >= 3 && offs.len() <= 4);
        assert!(offs.windows(2).all(|w| w[0] < w[1]));
        assert!(offs.iter().all(|o| o % MIB == 0));
    }

    #[test]
    fn sample_offsets_tiny_devices() {
        assert!(sample_offsets(512, 8).is_empty());
        assert_eq!(sample_offsets(MIB, 8), vec![0]);
        let offs = sample_offsets(2 * MIB, 8);
        assert_eq!(offs, vec![0, MIB]);
    }

    #[test]
    fn pattern_depends_on_offset() {
        let mut a = vec![0u8; 64];
        let mut b = vec![0u8; 64];
        fill_pattern(&mut a, 0);
        fill_pattern(&mut b, MIB);
        assert_ne!(a, b);
        let mut a2 = vec![0u8; 64];
        fill_pattern(&mut a2, 0);
        assert_eq!(a, a2, "pattern is deterministic");
    }

    #[test]
    fn destructive_kind_flag() {
        assert!(TestKind::DestructiveSample.is_destructive());
        assert!(!TestKind::Smoke.is_destructive());
        assert!(!TestKind::ReadScan.is_destructive());
    }
}
