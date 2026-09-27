//! Health polling that holds up at 160+ drives per node (#15).
//!
//! The first cut sampled every drive in turn, each tick, on one task: a
//! single NVMe whose admin command hangs (the kernel's timeout is 60 s, and
//! a controller reset can take longer) stalled the round for every other
//! drive, and 160 drives were read in a burst once a minute.
//!
//! Now:
//!
//! - **Spread.** Each drive has its own phase in the interval, derived from
//!   its stable id, so 160 drives on a 60 s interval are ~2.7 samples a
//!   second, not 160 at once — and the phase survives restarts.
//! - **Bounded.** At most `max_concurrent` samples are in flight; no thread
//!   per drive.
//! - **Timed out, not stacked.** A sample that has not answered in
//!   `sample_timeout_secs` is reported as a timeout and the poller moves
//!   on. The blocking read cannot be cancelled, so the drive is marked
//!   *stuck* until it returns; while stuck it is not sampled again (no
//!   second thread piles onto a hung device) and each due time counts as
//!   another timeout, so the hysteresis still walks a hung drive to Failed.
//!   Blocking threads are bounded by `max_concurrent` + the stuck drives.
//! - **Costed.** `PollStats` (served at `GET /api/v1/monitor`) says what a
//!   cycle costs: samples, time per sample, busy time per interval, and
//!   which drives are stuck.

use crate::drive::{Drive, DriveId};
use crate::smart::Sample;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// Reads one drive's health. Blocking: ioctls and sysfs.
pub type Collector = Arc<dyn Fn(&Drive) -> Sample + Send + Sync>;

/// When each drive is next due. Pure bookkeeping, no I/O.
#[derive(Debug)]
pub struct Schedule {
    interval: Duration,
    next_due: HashMap<DriveId, Instant>,
}

/// The drive's offset into the interval: its id's hash, so the spread is
/// even and stable across restarts.
pub fn phase(id: &DriveId, interval: Duration) -> Duration {
    let ms = interval.as_millis().max(1);
    Duration::from_millis((id.0.as_u128() % ms) as u64)
}

impl Schedule {
    pub fn new(interval: Duration) -> Self {
        Schedule { interval: interval.max(Duration::from_secs(1)), next_due: HashMap::new() }
    }

    /// The drives due at `now`, out of those present. A drive seen for the
    /// first time is due at its phase; one that was due is next due one
    /// interval later (or one interval from now, when the poller fell
    /// behind — never a burst of catch-up samples).
    pub fn take_due(&mut self, now: Instant, present: &[DriveId]) -> Vec<DriveId> {
        let keep: HashSet<&DriveId> = present.iter().collect();
        self.next_due.retain(|id, _| keep.contains(id));
        let mut due = Vec::new();
        for id in present {
            let at = *self.next_due.entry(*id).or_insert_with(|| now + phase(id, self.interval));
            if at <= now {
                let mut next = at + self.interval;
                if next <= now {
                    next = now + self.interval;
                }
                self.next_due.insert(*id, next);
                due.push(*id);
            }
        }
        due
    }
}

/// What came of asking a drive.
#[derive(Debug)]
pub enum Outcome {
    Sampled(Sample),
    /// No answer within the timeout; the read is still running.
    TimedOut,
    /// An earlier read of this drive has still not returned; not asked
    /// again.
    StillStuck,
}

impl Outcome {
    /// The sample the threshold engine sees. A timeout is a device that is
    /// not answering: `kernel_ok = false`, damped by the hysteresis like
    /// any other worsening.
    pub fn sample(&self, timeout: Duration) -> Sample {
        match self {
            Outcome::Sampled(s) => s.clone(),
            Outcome::TimedOut => Sample {
                kernel_ok: false,
                messages: vec![format!("health read did not answer within {} s", timeout.as_secs())],
                ..Default::default()
            },
            Outcome::StillStuck => Sample {
                kernel_ok: false,
                messages: vec!["health read still hung from an earlier poll".into()],
                ..Default::default()
            },
        }
    }

    pub fn answered(&self) -> bool {
        matches!(self, Outcome::Sampled(_))
    }
}

/// What a polling cycle costs, and what is stuck.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PollStats {
    pub interval_secs: u64,
    pub max_concurrent: usize,
    pub sample_timeout_secs: u64,
    /// Drives being polled.
    pub drives: usize,
    pub in_flight: usize,
    /// Drives whose last read has not returned.
    pub stuck: Vec<String>,
    pub samples: u64,
    pub timeouts: u64,
    pub last_sample_ms: Option<u64>,
    /// Moving average (1/16 weight) of one sample's wall time.
    pub avg_sample_ms: Option<f64>,
    pub max_sample_ms: u64,
    /// avg × drives: wall time spent sampling per interval.
    pub busy_ms_per_interval: Option<f64>,
    /// busy / (interval × max_concurrent): how much of the poller's
    /// capacity one cycle uses. Past 100 % the drives are sampled less often
    /// than the interval says.
    pub load_pct: Option<f64>,
    /// The last discovery pass (sysfs walk, probes of new devices,
    /// location): its wall time and how many drives it saw.
    pub discovery_ms: Option<u64>,
    pub discovery_drives: usize,
    /// Devices whose READ CAPACITY / slab probe answer is cached.
    pub discovery_cached: usize,
}

/// Runs samples: bounded, timed out, never two at once on one drive.
/// Cheap to clone; one per daemon.
#[derive(Clone)]
pub struct Sampler {
    collector: Collector,
    timeout: Duration,
    permits: Arc<Semaphore>,
    running: Arc<Mutex<HashMap<DriveId, String>>>,
    stats: Arc<Mutex<PollStats>>,
}

impl Sampler {
    pub fn new(collector: Collector, interval: Duration, max_concurrent: usize, timeout: Duration) -> Self {
        let max_concurrent = max_concurrent.max(1);
        let stats = PollStats {
            interval_secs: interval.as_secs(),
            max_concurrent,
            sample_timeout_secs: timeout.as_secs(),
            ..Default::default()
        };
        Sampler {
            collector,
            timeout,
            permits: Arc::new(Semaphore::new(max_concurrent)),
            running: Arc::default(),
            stats: Arc::new(Mutex::new(stats)),
        }
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn stats(&self) -> PollStats {
        let mut s = self.stats.lock().unwrap().clone();
        let running = self.running.lock().unwrap();
        s.in_flight = running.len();
        s
    }

    /// Record how many drives are being polled, for the cost figures.
    pub fn set_drives(&self, n: usize) {
        let mut s = self.stats.lock().unwrap();
        s.drives = n;
        refresh_cost(&mut s);
    }

    /// Ask one drive. Waits for a permit, then at most `timeout`.
    pub async fn sample(&self, drive: Drive) -> Outcome {
        let id = drive.id;
        {
            let mut running = self.running.lock().unwrap();
            if running.contains_key(&id) {
                return Outcome::StillStuck;
            }
            running.insert(id, drive.name.clone());
        }
        // The permit bounds reads in flight; a timed-out read gives its
        // permit back (the drive stays in `running` until it returns).
        let Ok(permit) = self.permits.clone().acquire_owned().await else {
            self.running.lock().unwrap().remove(&id);
            return Outcome::StillStuck;
        };
        let collector = self.collector.clone();
        let running = self.running.clone();
        let started = Instant::now();
        let task = tokio::task::spawn_blocking(move || {
            let s = collector(&drive);
            running.lock().unwrap().remove(&drive.id);
            s
        });
        let out = tokio::time::timeout(self.timeout, task).await;
        drop(permit);
        let ms = started.elapsed().as_millis() as u64;
        let mut st = self.stats.lock().unwrap();
        st.samples += 1;
        match out {
            Ok(Ok(sample)) => {
                st.last_sample_ms = Some(ms);
                st.max_sample_ms = st.max_sample_ms.max(ms);
                st.avg_sample_ms = Some(match st.avg_sample_ms {
                    Some(a) => a + (ms as f64 - a) / 16.0,
                    None => ms as f64,
                });
                refresh_cost(&mut st);
                Outcome::Sampled(sample)
            }
            Ok(Err(_)) => {
                // The collector panicked: nothing is running any more.
                self.running.lock().unwrap().remove(&id);
                Outcome::TimedOut
            }
            Err(_) => {
                st.timeouts += 1;
                Outcome::TimedOut
            }
        }
    }

    pub fn record_discovery(&self, ms: u64, drives: usize, cached: usize) {
        let mut s = self.stats.lock().unwrap();
        s.discovery_ms = Some(ms);
        s.discovery_drives = drives;
        s.discovery_cached = cached;
    }

    /// Names of drives whose read has not returned, for the stats.
    pub fn refresh_stuck(&self) {
        let mut names: Vec<String> = self.running.lock().unwrap().values().cloned().collect();
        names.sort();
        self.stats.lock().unwrap().stuck = names;
    }
}

fn refresh_cost(s: &mut PollStats) {
    if let Some(avg) = s.avg_sample_ms {
        let busy = avg * s.drives as f64;
        s.busy_ms_per_interval = Some(busy);
        let capacity = (s.interval_secs.max(1) * 1000) as f64 * s.max_concurrent as f64;
        s.load_pct = Some(100.0 * busy / capacity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::DriveId;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ids(n: usize) -> Vec<DriveId> {
        (0..n).map(|i| DriveId::derive(Some(format!("naa.{i:016x}").as_str()), "M", &i.to_string())).collect()
    }

    fn drive(id: DriveId, name: &str) -> Drive {
        serde_json::from_value(serde_json::json!({
            "id": id, "path": format!("/dev/{name}"), "name": name, "paths": [],
            "kind": "nvme_ssd", "model": "M", "serial": name, "firmware": "1", "wwid": null,
            "capacity_bytes": 1u64 << 40, "block_size": 4096,
            "first_seen": std::time::SystemTime::UNIX_EPOCH, "last_seen": std::time::SystemTime::UNIX_EPOCH,
        }))
        .unwrap()
    }

    /// 160 drives on a 60 s interval, stepped a second at a time over
    /// three intervals: every drive once per interval, never a burst.
    #[test]
    fn a_hundred_and_sixty_drives_are_spread_over_the_interval() {
        let interval = Duration::from_secs(60);
        let all = ids(160);
        let mut sch = Schedule::new(interval);
        let t0 = Instant::now();
        let mut per_drive: HashMap<DriveId, Vec<u64>> = HashMap::new();
        let mut worst_second = 0;
        for s in 0..=180 {
            let due = sch.take_due(t0 + Duration::from_secs(s), &all);
            worst_second = worst_second.max(due.len());
            for id in due {
                per_drive.entry(id).or_default().push(s);
            }
        }
        assert_eq!(per_drive.len(), 160);
        for (id, at) in &per_drive {
            // First due within the first interval (its phase, rounded up to
            // the next whole-second step), then exactly one interval apart.
            assert!(at[0] <= 60, "{id:?} first due at {}", at[0]);
            assert!(at.len() >= 3, "{id:?} due at {at:?}");
            assert!(at.windows(2).all(|w| w[1] - w[0] == 60), "{id:?} due at {at:?}");
        }
        // 160/60 ≈ 2.7 a second on average; hashing is not perfectly even.
        assert!(worst_second <= 10, "a burst of {worst_second} in one second");
    }

    #[test]
    fn phases_are_stable_and_gone_drives_are_forgotten() {
        let all = ids(3);
        let iv = Duration::from_secs(60);
        assert_eq!(phase(&all[0], iv), phase(&all[0], iv), "survives a restart");
        let mut sch = Schedule::new(iv);
        let t0 = Instant::now();
        sch.take_due(t0, &all);
        sch.take_due(t0, &all[..1]);
        assert_eq!(sch.next_due.len(), 1);
        // A poller that fell behind does not catch up in a burst.
        let late = t0 + Duration::from_secs(600);
        assert_eq!(sch.take_due(late, &all[..1]).len(), 1);
        assert!(sch.take_due(late + Duration::from_secs(1), &all[..1]).is_empty());
    }

    /// One hung drive among 160: it times out once, is then skipped (not
    /// re-read), and the other 159 are sampled with at most
    /// `max_concurrent` reads in flight.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn one_hung_drive_does_not_stall_the_other_159() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let (f, p, c) = (in_flight.clone(), peak.clone(), calls.clone());
        let collector: Collector = Arc::new(move |d: &Drive| {
            c.fetch_add(1, Ordering::SeqCst);
            let now = f.fetch_add(1, Ordering::SeqCst) + 1;
            p.fetch_max(now, Ordering::SeqCst);
            // The hung one answers long after the timeout (not forever,
            // so the test runtime can shut down).
            let t = if d.name == "nvme77n1" { 1_500 } else { 5 };
            std::thread::sleep(Duration::from_millis(t));
            f.fetch_sub(1, Ordering::SeqCst);
            Sample { kernel_ok: true, ..Default::default() }
        });
        let sampler = Sampler::new(collector, Duration::from_secs(60), 8, Duration::from_millis(200));
        let drives: Vec<Drive> = ids(160).into_iter().enumerate().map(|(i, id)| drive(id, &format!("nvme{i}n1"))).collect();
        sampler.set_drives(drives.len());

        let started = Instant::now();
        let outcomes = futures_util::future::join_all(drives.iter().cloned().map(|d| {
            let s = sampler.clone();
            async move { (d.name.clone(), s.sample(d).await) }
        }))
        .await;
        let took = started.elapsed();

        let timed_out: Vec<_> = outcomes.iter().filter(|(_, o)| !o.answered()).map(|(n, _)| n.as_str()).collect();
        assert_eq!(timed_out, ["nvme77n1"]);
        // 8 lanes; the hung read gave its lane back at the timeout but its
        // thread is still in the collector: 8 + 1 stuck.
        assert!(peak.load(Ordering::SeqCst) <= 9, "at most 8 + 1 stuck reads, saw {}", peak.load(Ordering::SeqCst));
        // 159 × 5 ms over 8 lanes ≈ 100 ms, plus the one 200 ms timeout.
        assert!(took < Duration::from_millis(1_200), "the round took {took:?}");

        // Still hung: skipped without a second read.
        let before = calls.load(Ordering::SeqCst);
        let again = sampler.sample(drives[77].clone()).await;
        assert!(matches!(again, Outcome::StillStuck));
        assert_eq!(calls.load(Ordering::SeqCst), before);
        sampler.refresh_stuck();
        let st = sampler.stats();
        assert_eq!(st.stuck, ["nvme77n1"]);
        assert_eq!((st.samples, st.timeouts), (160, 1));
        assert!(st.load_pct.is_some() && st.busy_ms_per_interval.is_some());

        // Once it has returned, it is read again (and, still slow, times
        // out again — but as a new read, not a skipped one).
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(!matches!(sampler.sample(drives[77].clone()).await, Outcome::StillStuck));
        assert_eq!(calls.load(Ordering::SeqCst), before + 1);
        let timeout_sample = Outcome::TimedOut.sample(Duration::from_secs(10));
        assert!(!timeout_sample.kernel_ok && timeout_sample.messages[0].contains("10 s"));
    }
}
