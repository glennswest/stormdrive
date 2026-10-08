//! Drive history in app-system-data (#64, stormcos#456): every drive's
//! health counters over time, in the node's kept volume, so they survive
//! every install.
//!
//! `<dir>/history/drives/<key>/<YYYY-MM>.jsonl`, one JSON [`Record`] a line.
//! `key` is the drive's WWN (`wwn-…`), else its model and serial
//! (`serial-…`): what the drive is, not where it sits. A record is written
//! for a drive's first sample, whenever a counter that matters changes
//! (errors, wear, cycles — not bytes or hours, which move every sample on a
//! busy drive), its verdict, firmware or bay changes, and otherwise every
//! `heartbeat_secs`.
//!
//! **Findings:** each record is compared with the drive's previous one,
//! read back from the file when stormdrive starts — so across a restart and
//! across an install. An error counter that grows, or a lifetime counter
//! that goes backwards (the drive's SMART was reset, or this is not the
//! drive it claims to be), is a finding: in the record and an event.
//!
//! The directory is never created: when the system-data volume is not
//! mounted there, history is off and says so (`GET /api/v1/history`).

use crate::drive::{Drive, HealthStatus};
use crate::smart::{AtaAttribute, Sample};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Who the drive is and where it was, as of the record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Identity {
    /// stormdrive's stable drive id.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wwn: Option<String>,
    pub serial: String,
    pub model: String,
    pub firmware: String,
    pub kind: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bay: Option<String>,
    pub capacity_bytes: u64,
}

/// One snapshot of a drive's health.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// RFC 3339 UTC.
    pub at: String,
    pub unix: u64,
    /// `/proc/sys/kernel/random/boot_id`: which boot took it.
    #[serde(default)]
    pub boot_id: String,
    #[serde(default)]
    pub node: String,
    #[serde(default)]
    pub kernel: String,
    #[serde(default)]
    pub stormdrive: String,
    /// `first`, `changed` or `heartbeat`.
    #[serde(default)]
    pub why: String,
    pub drive: Identity,
    /// The health verdict when it was taken.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<i32>,
    /// Every counter, by name (see [`spec`]).
    pub counters: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicted_failure: Option<String>,
    /// A SATA drive's whole SMART attribute table.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ata_attributes: Vec<AtaAttribute>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
}

/// A counter that moved the wrong way since the previous record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub counter: String,
    /// `grew` or `went_back`.
    pub change: String,
    pub from: u64,
    pub to: u64,
    /// When the previous record was taken, and whether in another boot
    /// (a restart or an install in between).
    pub since: String,
    pub other_boot: bool,
}

impl Finding {
    pub fn message(&self) -> String {
        let when = if self.other_boot { format!("since {} (an earlier boot)", self.since) } else { format!("since {}", self.since) };
        match self.change.as_str() {
            "grew" => format!("{} grew {} → {} {when}", self.counter, self.from, self.to),
            _ => format!("{} went backwards {} → {} {when}: the drive's counters were reset, or it is not the drive it was", self.counter, self.from, self.to),
        }
    }
}

/// How a counter is judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    /// Growth is a finding (an error counter).
    pub grow: bool,
    /// Going backwards is a finding (counted over the drive's life).
    pub back: bool,
    /// A change writes a record. Not for counters that move with every
    /// read or hour (bytes, commands, power-on hours).
    pub writes: bool,
}

/// The rule for each counter name.
pub fn spec(name: &str) -> Spec {
    const ERRORS: Spec = Spec { grow: true, back: true, writes: true };
    const LIFETIME: Spec = Spec { grow: false, back: true, writes: true };
    const ACTIVITY: Spec = Spec { grow: false, back: true, writes: false };
    const GAUGE: Spec = Spec { grow: false, back: false, writes: true };
    match name {
        "media_errors"
        | "smart.reallocated_sectors"
        | "smart.offline_uncorrectable"
        | "smart.reported_uncorrectable"
        | "smart.crc_errors"
        | "nvme.error_log_entries" => ERRORS,
        n if n.starts_with("sas.") && n.ends_with(".uncorrected") => ERRORS,
        // Pending sectors fall when the drive remaps them: only growth.
        "smart.pending_sectors" => Spec { grow: true, back: false, writes: true },
        // Since boot (sysfs ioerr_cnt), and not media errors (#58).
        "io_errors" | "available_spare_pct" | "critical_warning" => GAUGE,
        "power_on_hours"
        | "nvme.bytes_read"
        | "nvme.bytes_written"
        | "nvme.host_read_commands"
        | "nvme.host_write_commands"
        | "nvme.controller_busy_minutes" => ACTIVITY,
        n if n.starts_with("sas.") => ACTIVITY,
        _ => LIFETIME,
    }
}

/// A sample's counters by name.
pub fn counters(s: &Sample) -> BTreeMap<String, u64> {
    let mut c = BTreeMap::new();
    let mut put = |k: &str, v: Option<u64>| {
        if let Some(v) = v {
            c.insert(k.to_string(), v);
        }
    };
    put("media_errors", Some(s.media_errors));
    put("io_errors", s.io_errors);
    put("power_on_hours", s.power_on_hours);
    put("wear_pct", s.wear_pct.map(u64::from));
    put("available_spare_pct", s.available_spare_pct.map(u64::from));
    put("critical_warning", (s.nvme.is_some() || s.critical_warning != 0).then_some(u64::from(s.critical_warning)));
    if let Some(n) = &s.nvme {
        put("nvme.available_spare_threshold_pct", Some(u64::from(n.available_spare_threshold_pct)));
        put("nvme.bytes_read", Some(n.bytes_read));
        put("nvme.bytes_written", Some(n.bytes_written));
        put("nvme.host_read_commands", Some(n.host_read_commands));
        put("nvme.host_write_commands", Some(n.host_write_commands));
        put("nvme.controller_busy_minutes", Some(n.controller_busy_minutes));
        put("nvme.power_cycles", Some(n.power_cycles));
        put("nvme.unsafe_shutdowns", Some(n.unsafe_shutdowns));
        put("nvme.error_log_entries", Some(n.error_log_entries));
        put("nvme.warning_temp_minutes", Some(n.warning_temp_minutes));
        put("nvme.critical_temp_minutes", Some(n.critical_temp_minutes));
    }
    if let Some(m) = &s.smart {
        put("smart.reallocated_sectors", m.reallocated_sectors);
        put("smart.pending_sectors", m.pending_sectors);
        put("smart.offline_uncorrectable", m.offline_uncorrectable);
        put("smart.reported_uncorrectable", m.reported_uncorrectable);
        put("smart.crc_errors", m.crc_errors);
        for (page, e) in [("write", m.write_errors), ("read", m.read_errors), ("verify", m.verify_errors)] {
            if let Some(e) = e {
                put(&format!("sas.{page}.corrected"), e.corrected);
                put(&format!("sas.{page}.uncorrected"), e.uncorrected);
                put(&format!("sas.{page}.bytes"), e.bytes);
            }
        }
    }
    // The rest of a SATA drive's table, by attribute: power cycles (12),
    // start/stop (4), load cycles (193), … over the drive's life.
    for a in &s.ata_attributes {
        if !matches!(a.id, 5 | 9 | 187 | 194 | 197 | 198 | 199) && LIFETIME_ATA.contains(&a.id) {
            put(&format!("ata.{}", a.id), Some(a.raw));
        }
    }
    c
}

/// ATA attributes whose raw value is a lifetime count (vendors agree on
/// these): start/stop (4), spin retries (10), power cycles (12), power-off
/// retracts (192), load cycles (193), command timeouts (188), reported
/// uncorrectable is 187 (named above).
const LIFETIME_ATA: &[u8] = &[4, 10, 12, 188, 192, 193];

/// Findings from `prev` to `cur`: an error counter that grew, a lifetime
/// counter that went backwards. A counter missing on either side is not
/// compared.
pub fn findings(prev: &Record, cur: &BTreeMap<String, u64>, cur_boot: &str) -> Vec<Finding> {
    let mut out = vec![];
    for (name, &to) in cur {
        let Some(&from) = prev.counters.get(name) else { continue };
        let s = spec(name);
        let change = if to > from && s.grow {
            "grew"
        } else if to < from && s.back {
            "went_back"
        } else {
            continue;
        };
        out.push(Finding {
            counter: name.clone(),
            change: change.into(),
            from,
            to,
            since: prev.at.clone(),
            other_boot: !prev.boot_id.is_empty() && prev.boot_id != cur_boot,
        });
    }
    out
}

/// Why a record is due now, or None. `heartbeat`: seconds.
pub fn due(prev: Option<&Record>, cur: &Record, heartbeat: u64) -> Option<&'static str> {
    let Some(p) = prev else { return Some("first") };
    let moved = cur.counters.iter().any(|(k, v)| spec(k).writes && p.counters.get(k) != Some(v))
        || p.counters.keys().any(|k| spec(k).writes && !cur.counters.contains_key(k))
        || p.status != cur.status
        || p.drive.firmware != cur.drive.firmware
        || p.drive.bay != cur.drive.bay
        || p.predicted_failure != cur.predicted_failure
        || !cur.findings.is_empty();
    if moved {
        Some("changed")
    } else if cur.unix.saturating_sub(p.unix) >= heartbeat {
        Some("heartbeat")
    } else {
        None
    }
}

/// The drive's directory name under `history/drives/`.
pub fn key(d: &Drive) -> String {
    match d.wwid.as_deref().map(str::trim).filter(|w| !w.is_empty()) {
        Some(w) => format!("wwn-{}", sanitize(w)),
        None => format!("serial-{}-{}", sanitize(&d.model), sanitize(&d.serial)),
    }
}

fn sanitize(s: &str) -> String {
    let mut out: String = s
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-') { c } else { '_' })
        .collect();
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    out.truncate(120);
    out
}

/// `YYYY-MM` of a unix time.
pub fn month(unix: u64) -> String {
    crate::controller::rfc3339(unix)[..7].to_string()
}

/// The month `keep` months before `m` (`YYYY-MM`): files older are pruned.
pub fn cutoff(m: &str, keep: u32) -> String {
    let y: i64 = m[..4].parse().unwrap_or(0);
    let mo: i64 = m[5..7].parse().unwrap_or(1);
    let total = y * 12 + (mo - 1) - i64::from(keep.saturating_sub(1));
    format!("{:04}-{:02}", total.div_euclid(12), total.rem_euclid(12) + 1)
}

/// Facts about this boot that every record and the assets carry.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Boot {
    pub boot_id: String,
    pub node: String,
    pub kernel: String,
    pub stormdrive: String,
}

impl Boot {
    pub fn read(node: &str) -> Self {
        let rd = |p: &str| std::fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default();
        Self {
            boot_id: rd("/proc/sys/kernel/random/boot_id"),
            node: node.to_string(),
            kernel: rd("/proc/sys/kernel/osrelease"),
            stormdrive: crate::VERSION.to_string(),
        }
    }
}

/// What `GET /api/v1/history` reports.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Status {
    pub dir: String,
    /// The system-data directory exists and history is written there.
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub boot: Boot,
    pub records_written: u64,
    pub findings: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// This boot's assets file (#64), once written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assets_file: Option<String>,
}

/// The drive history writer. Blocking (file I/O): call from
/// `spawn_blocking`.
pub struct History {
    pub root: PathBuf,
    heartbeat: u64,
    keep_months: u32,
    pub boot: Boot,
    /// The last record per drive key; loaded from the file on first use.
    last: Mutex<HashMap<String, Option<Record>>>,
    pub status: Mutex<Status>,
}

impl History {
    pub fn new(cfg: &crate::config::HistoryConfig, boot: Boot) -> Self {
        let status = Status { dir: cfg.dir.clone(), boot: boot.clone(), ..Default::default() };
        Self {
            root: PathBuf::from(&cfg.dir),
            heartbeat: cfg.heartbeat_secs.max(1),
            keep_months: cfg.keep_months.max(1),
            boot,
            last: Mutex::new(HashMap::new()),
            status: Mutex::new(status),
        }
    }

    /// Is the system-data directory there? Said once each time it changes.
    pub fn available(&self) -> bool {
        let ok = self.root.is_dir();
        let mut st = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if ok != st.active || (!ok && st.reason.is_none()) {
            if ok {
                tracing::info!(dir = %self.root.display(), "system-data: writing drive history and assets");
                st.reason = None;
            } else {
                let why = format!("{} does not exist: the system-data volume is not mounted for stormdrive", self.root.display());
                tracing::warn!("system-data: {why}; no drive history or assets are kept");
                st.reason = Some(why);
            }
            st.active = ok;
        }
        ok
    }

    pub fn drive_dir(&self, key: &str) -> PathBuf {
        self.root.join("history/drives").join(key)
    }

    /// The drive's last record (from the file the first time).
    pub fn last(&self, key: &str) -> Option<Record> {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        last.entry(key.to_string()).or_insert_with(|| read_last(&self.drive_dir(key))).clone()
    }

    /// The media-error count the drive had in its last record, as the
    /// growth baseline for its first sample in this run (across an install).
    pub fn baseline_media_errors(&self, d: &Drive) -> Option<u64> {
        if !self.available() {
            return None;
        }
        self.last(&key(d))?.counters.get("media_errors").copied()
    }

    /// One sample: the record, its findings, written when due. Returns the
    /// findings (for events).
    pub fn observe(&self, d: &Drive, s: &Sample, status: HealthStatus, unix: u64) -> Vec<Finding> {
        if !self.available() {
            return vec![];
        }
        let k = key(d);
        let prev = self.last(&k);
        let mut rec = Record {
            at: crate::controller::rfc3339(unix),
            unix,
            boot_id: self.boot.boot_id.clone(),
            node: self.boot.node.clone(),
            kernel: self.boot.kernel.clone(),
            stormdrive: self.boot.stormdrive.clone(),
            why: String::new(),
            drive: Identity {
                id: d.id.0.to_string(),
                wwn: d.wwid.clone(),
                serial: d.serial.clone(),
                model: d.model.clone(),
                firmware: d.firmware.clone(),
                kind: serde_json::to_value(d.kind).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default(),
                path: d.path.clone(),
                bay: d.location.bay_key(),
                capacity_bytes: d.capacity_bytes,
            },
            status: serde_json::to_value(status).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default(),
            temperature_c: s.temperature_c,
            counters: counters(s),
            predicted_failure: s.smart.as_ref().and_then(|m| m.predicted_failure.clone()),
            ata_attributes: s.ata_attributes.clone(),
            findings: vec![],
        };
        if let Some(p) = &prev {
            rec.findings = findings(p, &rec.counters, &self.boot.boot_id);
        }
        let Some(why) = due(prev.as_ref(), &rec, self.heartbeat) else { return vec![] };
        rec.why = why.into();
        let res = self.append(&k, &rec);
        let mut st = self.status.lock().unwrap_or_else(|e| e.into_inner());
        match res {
            Ok(()) => {
                st.records_written += 1;
                st.findings += rec.findings.len() as u64;
                st.last_error = None;
                drop(st);
                let f = rec.findings.clone();
                self.last.lock().unwrap_or_else(|e| e.into_inner()).insert(k, Some(rec));
                f
            }
            Err(e) => {
                tracing::warn!(drive = %k, "system-data: drive history not written: {e:#}");
                st.last_error = Some(format!("{k}: {e:#}"));
                // Not written, so not the baseline either; findings are
                // still reported.
                rec.findings
            }
        }
    }

    fn append(&self, key: &str, rec: &Record) -> anyhow::Result<()> {
        let dir = self.drive_dir(key);
        std::fs::create_dir_all(&dir)?;
        let m = month(rec.unix);
        let file = dir.join(format!("{m}.jsonl"));
        let new_month = !file.exists();
        let mut line = serde_json::to_vec(rec)?;
        line.push(b'\n');
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&file)?;
        f.write_all(&line)?;
        f.sync_data()?;
        if new_month {
            prune(&dir, &cutoff(&m, self.keep_months));
        }
        Ok(())
    }

    /// The drive's records, oldest first, at most `limit` (the newest).
    pub fn read(&self, d: &Drive, limit: usize) -> Vec<Record> {
        read_records(&self.drive_dir(&key(d)), limit)
    }
}

/// Month files of a drive, oldest first.
fn months(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|r| r.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// Remove month files before `cutoff` (`YYYY-MM`).
fn prune(dir: &Path, cutoff: &str) {
    for p in months(dir) {
        let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if stem.as_str() < cutoff {
            if let Err(e) = std::fs::remove_file(&p) {
                tracing::warn!(file = %p.display(), "system-data: old drive history not removed: {e}");
            }
        }
    }
}

/// The newest whole record (a torn last line is skipped).
fn read_last(dir: &Path) -> Option<Record> {
    for p in months(dir).iter().rev() {
        let text = std::fs::read_to_string(p).ok()?;
        if let Some(r) = text.lines().rev().find_map(|l| serde_json::from_str::<Record>(l).ok()) {
            return Some(r);
        }
    }
    None
}

fn read_records(dir: &Path, limit: usize) -> Vec<Record> {
    let mut out: Vec<Record> = vec![];
    for p in months(dir).iter().rev() {
        let Ok(text) = std::fs::read_to_string(p) else { continue };
        for l in text.lines().rev() {
            if out.len() >= limit {
                break;
            }
            if let Ok(r) = serde_json::from_str::<Record>(l) {
                out.push(r);
            }
        }
        if out.len() >= limit {
            break;
        }
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::DriveKind;
    use crate::smart::{ErrorCounters, NvmeCounters, SmartCounters};

    fn drive() -> Drive {
        let mut d = Drive::test_fixture("sda");
        d.kind = DriveKind::SasHdd;
        d.wwid = Some("naa.5000c500a1b2c3d4".into());
        d.model = "ST1200MM0098".into();
        d.serial = "S40ABC".into();
        d.firmware = "N003".into();
        d
    }

    fn sas(read_unc: u64, poh: u64) -> Sample {
        Sample {
            power_on_hours: Some(poh),
            media_errors: read_unc,
            io_errors: Some(2),
            kernel_ok: true,
            smart: Some(SmartCounters {
                source: "log_sense".into(),
                read_errors: Some(ErrorCounters { corrected: Some(10), uncorrected: Some(read_unc), bytes: Some(poh * 1000) }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sd-hist-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn history(root: &Path, boot: &str) -> History {
        let cfg = crate::config::HistoryConfig { dir: root.display().to_string(), heartbeat_secs: 3600, keep_months: 2 };
        History::new(&cfg, Boot { boot_id: boot.into(), node: "n1".into(), kernel: "6.x".into(), stormdrive: "0".into() })
    }

    #[test]
    fn keys_and_months() {
        assert_eq!(key(&drive()), "wwn-naa.5000c500a1b2c3d4");
        let mut d = drive();
        d.wwid = None;
        d.model = "WDC WD20EFAX-68F".into();
        assert_eq!(key(&d), "serial-WDC_WD20EFAX-68F-S40ABC");
        assert_eq!(month(1_791_417_600), "2026-10"); // 2026-10-08
        assert_eq!(cutoff("2026-10", 24), "2024-11");
        assert_eq!(cutoff("2026-01", 1), "2026-01");
        assert_eq!(cutoff("2026-01", 2), "2025-12");
    }

    #[test]
    fn counters_by_name_and_their_rules() {
        let mut s = sas(1, 100);
        s.nvme = Some(NvmeCounters { error_log_entries: 4, bytes_written: 9, ..Default::default() });
        s.ata_attributes = vec![AtaAttribute { id: 12, raw: 77, ..Default::default() }, AtaAttribute { id: 1, raw: 123, ..Default::default() }];
        let c = counters(&s);
        assert_eq!(c["media_errors"], 1);
        assert_eq!(c["sas.read.uncorrected"], 1);
        assert_eq!(c["sas.read.corrected"], 10);
        assert_eq!(c["nvme.error_log_entries"], 4);
        assert_eq!(c["ata.12"], 77, "power cycles");
        assert!(!c.contains_key("ata.1"), "raw read error rate is vendor-scaled, not a count");
        assert!(spec("sas.read.uncorrected").grow && spec("smart.pending_sectors").grow && !spec("smart.pending_sectors").back);
        assert!(!spec("io_errors").grow && !spec("io_errors").back, "per boot, and not media errors (#58)");
        assert!(spec("power_on_hours").back && !spec("power_on_hours").writes);
    }

    #[test]
    fn findings_grew_and_went_back_not_io_errors() {
        let prev = Record {
            at: "2026-10-01T00:00:00Z".into(),
            boot_id: "boot-a".into(),
            counters: counters(&sas(1, 5000)),
            ..Default::default()
        };
        let mut s = sas(3, 4000);
        s.io_errors = Some(0); // a new boot: ioerr_cnt starts again
        let f = findings(&prev, &counters(&s), "boot-b");
        let by: BTreeMap<_, _> = f.iter().map(|f| (f.counter.as_str(), (f.change.as_str(), f.from, f.to))).collect();
        assert_eq!(by["media_errors"], ("grew", 1, 3));
        assert_eq!(by["sas.read.uncorrected"], ("grew", 1, 3));
        assert_eq!(by["power_on_hours"], ("went_back", 5000, 4000));
        assert_eq!(by["sas.read.bytes"], ("went_back", 5_000_000, 4_000_000));
        assert!(!by.contains_key("io_errors"));
        assert!(f.iter().all(|f| f.other_boot && f.since == "2026-10-01T00:00:00Z"));
        assert!(f[0].message().contains("an earlier boot"));
        // Unchanged, or moving the normal way: nothing.
        assert!(findings(&prev, &counters(&sas(1, 5001)), "boot-a").is_empty());
    }

    #[test]
    fn due_on_change_and_heartbeat_not_on_activity() {
        let base = Record { unix: 1000, status: "healthy".into(), counters: counters(&sas(0, 10)), ..Default::default() };
        assert_eq!(due(None, &base, 3600), Some("first"));
        // Hours and bytes moved: not on their own.
        let busy = Record { unix: 1060, counters: counters(&sas(0, 11)), ..base.clone() };
        assert_eq!(due(Some(&base), &busy, 3600), None);
        assert_eq!(due(Some(&base), &Record { unix: 4600, ..busy.clone() }, 3600), Some("heartbeat"));
        let err = Record { unix: 1060, counters: counters(&sas(1, 10)), ..base.clone() };
        assert_eq!(due(Some(&base), &err, 3600), Some("changed"));
        let worse = Record { unix: 1060, status: "warning".into(), ..base.clone() };
        assert_eq!(due(Some(&base), &worse, 3600), Some("changed"));
    }

    #[test]
    fn written_read_back_across_an_install_and_pruned() {
        let root = tmp("drives");
        let d = drive();
        let oct = 1_791_417_600; // 2026-10-08
        let h = history(&root, "boot-a");
        assert!(h.observe(&d, &sas(1, 100), HealthStatus::Good, oct).is_empty());
        assert!(h.observe(&d, &sas(1, 100), HealthStatus::Good, oct + 60).is_empty(), "nothing moved");
        let dir = root.join("history/drives/wwn-naa.5000c500a1b2c3d4");
        assert_eq!(std::fs::read_to_string(dir.join("2026-10.jsonl")).unwrap().lines().count(), 1);

        // "Reinstalled": a new History (empty cache), another boot. The
        // baseline comes from the file; the growth is a finding.
        let h2 = history(&root, "boot-b");
        assert_eq!(h2.baseline_media_errors(&d), Some(1));
        let f = h2.observe(&d, &sas(4, 101), HealthStatus::Good, oct + 120);
        assert!(f.iter().any(|f| f.counter == "media_errors" && f.from == 1 && f.to == 4 && f.other_boot));
        let recs = h2.read(&d, 10);
        assert_eq!(recs.len(), 2);
        assert_eq!((recs[0].why.as_str(), recs[1].why.as_str()), ("first", "changed"));
        assert_eq!(recs[1].findings.len(), 2, "media_errors and sas.read.uncorrected");
        assert_eq!(recs[1].drive.serial, "S40ABC");
        assert_eq!(h2.status.lock().unwrap().records_written, 1);

        // A torn last line does not lose the baseline.
        std::fs::OpenOptions::new().append(true).open(dir.join("2026-10.jsonl")).unwrap().write_all(b"{\"at\":").unwrap();
        assert_eq!(history(&root, "boot-c").last(&key(&d)).unwrap().counters["media_errors"], 4);

        // keep_months = 2: a record in 2026-12 removes 2026-10.
        std::fs::write(dir.join("2026-11.jsonl"), b"").unwrap();
        h2.observe(&d, &sas(4, 102), HealthStatus::Warning, 1_796_688_000); // 2026-12-08
        assert!(!dir.join("2026-10.jsonl").exists());
        assert!(dir.join("2026-11.jsonl").exists() && dir.join("2026-12.jsonl").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn off_without_the_volume() {
        let root = std::env::temp_dir().join(format!("sd-hist-{}-absent", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let h = history(&root, "b");
        assert!(h.observe(&drive(), &sas(1, 1), HealthStatus::Good, 1).is_empty());
        assert!(!root.exists(), "never created");
        let st = h.status.lock().unwrap().clone();
        assert!(!st.active && st.reason.unwrap().contains("not mounted"));
    }
}
