//! The drive worker (#5): prepare drives at fleet scale, by API — one
//! request for a drive, a shelf, a bay range, a model, or "every unusable
//! drive", each drive taken through a list of steps:
//!
//! - `format {block_size}`: SCSI FORMAT UNIT (format.rs; 520 → 512/4096) or
//!   NVMe Format NVM with the LBA format of that data size (erase.rs).
//! - `sanitize {method}`: NVMe Sanitize or SCSI SANITIZE — block, crypto or
//!   overwrite erase of the whole drive.
//! - `security_erase {enhanced?}`: ATA SECURITY ERASE UNIT for a SATA drive
//!   that has the Security feature set and not Sanitize (#36), under a
//!   one-time password kept in the job record until the erase is done.
//! - `test {kind}`: a drive test (drivetest.rs) — smoke, read_scan or
//!   destructive_sample — with its verdict in the job; a failed test stops
//!   the drive's later steps (#40).
//! - `partition {role}`: a GPT with one stormblock partition (gpt.rs).
//! - `enroll {tier, role}`: hand it to stormblock — open the partition (or
//!   the whole disk) with its labels and stable uuid, format a slab.
//!
//! **Safety.** A drive in the fleet is never touched: leave (drain) first.
//! A drive that is busy, reserved, missing or has a mounted partition is
//! refused. A drive that holds a stormblock slab or a filesystem is refused
//! a destructive step unless the request names it in `destroy` by its
//! stable id, WWN or serial — never by `/dev` name, which moves (the same
//! identity rule as stormblock's `data_slab_on`). Every guard runs when the
//! job is submitted and again before each step.
//!
//! **Scheduling.** Low-level steps run in parallel across drives, at most
//! `worker.max_per_hba` at once behind one HBA (NVMe: per controller).
//! `enroll` — the step that changes what stormblock holds — runs one at a
//! time per failure domain (shelf, else HBA) (`worker.enroll_per_domain`).
//!
//! **State.** Jobs persist in `<data_dir>/jobs.json`. After a restart a SCSI
//! format or sanitize still running on a drive, and an NVMe sanitize, are
//! re-attached and watched to the end; anything else that was running or
//! queued is `interrupted` — reported, never re-run blind — until
//! `POST …/resume`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::AppState;
use crate::drive::{Activity, Designation, Drive, DriveId, DriveKind, HealthStatus, Membership};
use crate::erase::SanitizeMethod;
use crate::events::Severity;

// ------------------------------------------------------------------ model

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    #[default]
    Data,
    System,
}

impl Role {
    pub fn word(self) -> &'static str {
        match self {
            Role::Data => "data",
            Role::System => "system",
        }
    }
    pub fn type_guid(self) -> [u8; 16] {
        match self {
            Role::Data => crate::gpt::TYPE_SLAB_DATA,
            Role::System => crate::gpt::TYPE_SLAB,
        }
    }
    pub fn partition_name(self) -> &'static str {
        match self {
            Role::Data => "stormblock-data",
            Role::System => "stormblock-system",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Step {
    Format {
        #[serde(alias = "blockSize")]
        block_size: u32,
        /// T10 protection information (#85): `none` (default) or `type1`.
        /// PI needs a SCSI drive whose probe says it offers it.
        #[serde(default)]
        protection: crate::pi::Protection,
    },
    Sanitize { method: SanitizeMethod },
    /// ATA SECURITY ERASE UNIT (#36). `enhanced`: None = when supported.
    SecurityErase {
        #[serde(default)]
        enhanced: Option<bool>,
    },
    /// A drive test (#40). Read-only unless `destructive_sample`.
    Test { kind: crate::drivetest::TestKind },
    Partition {
        #[serde(default)]
        role: Role,
    },
    Enroll {
        #[serde(default)]
        tier: Option<String>,
        #[serde(default)]
        role: Role,
    },
}

impl Step {
    fn rank(&self) -> u8 {
        match self {
            Step::Format { .. } | Step::Sanitize { .. } | Step::SecurityErase { .. } | Step::Test { .. } => 0,
            Step::Partition { .. } => 1,
            Step::Enroll { .. } => 2,
        }
    }
    pub fn low_level(&self) -> bool {
        self.rank() == 0
    }
    /// Destroys what is on the drive.
    pub fn destroys(&self) -> bool {
        match self {
            Step::Test { kind } => kind.is_destructive(),
            _ => self.rank() <= 1,
        }
    }
    /// Only reads the drive: a smoke test or a read scan.
    pub fn read_only(&self) -> bool {
        matches!(self, Step::Test { kind } if !kind.is_destructive())
    }
    pub fn name(&self) -> &'static str {
        match self {
            Step::Format { .. } => "format",
            Step::Sanitize { .. } => "sanitize",
            Step::SecurityErase { .. } => "security_erase",
            Step::Test { .. } => "test",
            Step::Partition { .. } => "partition",
            Step::Enroll { .. } => "enroll",
        }
    }
    pub fn describe(&self) -> String {
        match self {
            Step::Format { block_size, protection: crate::pi::Protection::None } => format!("format → {block_size}"),
            Step::Format { block_size, protection } => format!("format → {block_size} + {}", protection.word()),
            Step::Sanitize { method } => format!("sanitize ({})", serde_json::to_value(method).unwrap_or_default().as_str().unwrap_or("?")),
            Step::Test { kind } => format!("test ({})", serde_json::to_value(kind).unwrap_or_default().as_str().unwrap_or("?")),
            Step::SecurityErase { enhanced } => match enhanced {
                Some(true) => "security erase (enhanced)".into(),
                Some(false) => "security erase (normal)".into(),
                None => "security erase".into(),
            },
            Step::Partition { role } => format!("partition ({})", role.word()),
            Step::Enroll { tier, role } => format!("enroll ({} slab{})", role.word(), tier.as_ref().map(|t| format!(", tier {t}")).unwrap_or_default()),
        }
    }
}

/// Low-level before partition before enroll; each at most once; a partition
/// and the slab in it agree on the role.
pub fn validate_steps(steps: &[Step]) -> Result<(), String> {
    if steps.is_empty() {
        return Err("no steps".into());
    }
    let mut seen = BTreeSet::new();
    let mut rank = 0;
    for s in steps {
        if !seen.insert(s.name()) {
            return Err(format!("{} appears twice", s.name()));
        }
        if s.rank() < rank {
            return Err(format!("{} must come before the steps after it: low-level (format, sanitize), then partition, then enroll", s.name()));
        }
        rank = s.rank();
        if let Step::Format { block_size, .. } = s {
            if !crate::format::valid_target(*block_size) {
                return Err(format!("block_size {block_size}: use 512 or 4096"));
            }
        }
    }
    let part = steps.iter().find_map(|s| if let Step::Partition { role } = s { Some(*role) } else { None });
    let enroll = steps.iter().find_map(|s| if let Step::Enroll { role, .. } = s { Some(*role) } else { None });
    if let (Some(p), Some(e)) = (part, enroll) {
        if p != e {
            return Err(format!("partition role {} and enroll role {} differ", p.word(), e.word()));
        }
    }
    Ok(())
}

/// Which drives. Every given field must match (AND); at least one is needed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Select {
    /// Handles: id, WWN, /dev path or name, serial.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drives: Vec<String>,
    /// A shelf: logical id, serial, sysfs id or SES SCSI id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shelf: Option<String>,
    /// Bays on that shelf: `"0-11,14"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bays: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Drives the kernel cannot use (520/528-byte sectors).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unusable: bool,
}

impl Select {
    pub fn is_empty(&self) -> bool {
        self.drives.is_empty() && self.shelf.is_none() && self.model.is_none() && !self.unusable
    }
}

pub fn parse_bays(s: &str) -> Result<BTreeSet<u32>, String> {
    let mut out = BTreeSet::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let bad = || format!("bays {s:?}: use numbers and ranges like 0-11,14");
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (u32, u32) = (a.trim().parse().map_err(|_| bad())?, b.trim().parse().map_err(|_| bad())?);
                if a > b || b - a > 4096 {
                    return Err(bad());
                }
                out.extend(a..=b);
            }
            None => {
                out.insert(part.parse().map_err(|_| bad())?);
            }
        }
    }
    if out.is_empty() {
        return Err(format!("bays {s:?}: empty"));
    }
    Ok(out)
}

/// The filters other than `drives` (resolved by the caller).
pub fn matches(d: &Drive, sel: &Select, shelf_key: Option<&str>, bays: Option<&BTreeSet<u32>>) -> bool {
    if let Some(k) = shelf_key {
        if d.location.shelf.as_ref().and_then(|s| s.key()).as_deref() != Some(k) {
            return false;
        }
    }
    if let Some(b) = bays {
        if !d.location.bay.is_some_and(|x| b.contains(&x)) {
            return false;
        }
    }
    if let Some(m) = &sel.model {
        if !d.model.trim().eq_ignore_ascii_case(m.trim()) {
            return false;
        }
    }
    if sel.unusable && !d.needs_reformat() {
        return false;
    }
    true
}

/// `destroy` names this drive by stable id, WWN (any case) or serial. A
/// `/dev` name never counts: it can point at another drive tomorrow.
pub fn named_for_destroy(d: &Drive, destroy: &[String]) -> bool {
    destroy.iter().map(|x| x.trim()).any(|x| {
        !x.is_empty()
            && (x == d.id.0.to_string()
                || d.wwid.as_deref().is_some_and(|w| w.eq_ignore_ascii_case(x))
                || (!d.serial.is_empty() && x == d.serial))
    })
}

/// What the guard needs to know beyond the drive record.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// `contents::holds`: slabs or filesystems found on the drive.
    pub holds: Option<String>,
    pub mounted: bool,
    /// Other namespaces on the same NVMe controller.
    pub nvme_siblings: usize,
    pub stormblock: bool,
}

/// May `steps` run on `d`? The reason when not.
pub fn guard(d: &Drive, steps: &[Step], destroy_named: bool, cx: &Context) -> Result<(), String> {
    if d.activity == Activity::Missing {
        return Err("missing".into());
    }
    // Only reading (smoke tests, read scans, #40): allowed where the
    // single-drive test route allows it — fleet, reserved, mounted drives.
    let read_only = steps.iter().all(Step::read_only);
    if d.membership == Membership::Fleet && !read_only {
        return Err("in the fleet — leave (drain) first".into());
    }
    if d.activity != Activity::Idle {
        return Err(format!("busy: {}", serde_json::to_value(d.activity).unwrap_or_default().as_str().unwrap_or("?")));
    }
    if d.designation == Designation::Reserved && !read_only {
        return Err("designated reserved".into());
    }
    if cx.mounted && !read_only {
        return Err("has mounted partitions".into());
    }
    let destroys = steps.iter().any(Step::destroys);
    let held = cx.holds.clone().or_else(|| d.in_use_by.clone());
    if destroys && !destroy_named {
        if let Some(what) = held {
            return Err(format!("holds {what} — name it in \"destroy\" by id, WWN or serial to allow"));
        }
    }
    let formats_first = steps.iter().any(|s| matches!(s, Step::Format { .. }));
    for s in steps {
        match s {
            Step::SecurityErase { .. } if !matches!(d.kind, DriveKind::SataHdd | DriveKind::SataSsd) => {
                return Err("ATA security erase is for SATA drives; use sanitize".into());
            }
            Step::Format { block_size, protection } if *protection != crate::pi::Protection::None => {
                if d.kind == DriveKind::NvmeSsd {
                    return Err("PI formats are SCSI only here; NVMe end-to-end protection is not built".into());
                }
                let Some(sup) = &d.supports else {
                    return Err("not probed yet: what the drive offers (PI types, block lengths) is unknown".into());
                };
                if sup.offers(*block_size, *protection).is_none() {
                    return Err(crate::pi::plan(sup, Some((*block_size, *protection))).err().unwrap_or_default());
                }
            }
            Step::Sanitize { .. } if d.kind == DriveKind::NvmeSsd && cx.nvme_siblings > 0 => {
                return Err(format!(
                    "an NVMe sanitize erases every namespace on the controller; {} other namespace(s) share it",
                    cx.nvme_siblings
                ));
            }
            Step::Partition { .. } | Step::Enroll { .. } | Step::Test { .. } if !formats_first && (!d.usable || !crate::drive::USABLE_BLOCK_SIZES.contains(&d.block_size)) => {
                return Err(format!("{}-byte sectors: add a format step first", d.block_size));
            }
            Step::Enroll { .. } => {
                if !cx.stormblock {
                    return Err("stormblock integration is disabled".into());
                }
                if d.designation == Designation::Failed {
                    return Err("designated failed: it may be erased, not enrolled".into());
                }
                if d.health.status() >= HealthStatus::Failing {
                    return Err(format!("health is {:?}", d.health.status()));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The lane a low-level step waits in: the HBA a drive is behind, or an
/// NVMe drive's own controller.
pub fn hba_key(d: &Drive) -> String {
    if d.kind == DriveKind::NvmeSsd {
        if let Some((ctrl, _)) = crate::erase::nvme_names(&d.name) {
            return format!("nvme:{ctrl}");
        }
    }
    match &d.location.controller {
        Some(c) => format!("hba:{}", c.pcie_addr.clone().or_else(|| c.scsi_host.clone()).unwrap_or_default()),
        None => format!("drive:{}", d.id),
    }
}

/// The failure domain `enroll` is sequenced by: the shelf, else the HBA.
pub fn domain_key(d: &Drive) -> String {
    match d.location.shelf.as_ref().and_then(|s| s.key()) {
        Some(k) => format!("shelf:{k}"),
        None => hba_key(d),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DjState {
    Queued,
    Running,
    Done,
    Failed,
    Refused,
    Interrupted,
    Cancelled,
}

impl DjState {
    pub fn finished(self) -> bool {
        !matches!(self, DjState::Queued | DjState::Running | DjState::Interrupted)
    }
}

/// One drive's part of a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveJob {
    pub drive: DriveId,
    pub name: String,
    pub serial: String,
    #[serde(default)]
    pub wwn: Option<String>,
    pub kind: DriveKind,
    pub state: DjState,
    /// Index of the step running or next to run.
    pub step: usize,
    pub phase: String,
    pub progress_pct: Option<u8>,
    pub error: Option<String>,
    /// One line per finished step.
    #[serde(default)]
    pub done: Vec<String>,
    pub destroy_named: bool,
    pub started: Option<SystemTime>,
    pub finished: Option<SystemTime>,
    /// The one-time ATA user password a security erase set (#36), kept until
    /// the drive reads security-off again: a drive that loses power
    /// mid-erase stays locked with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ata_password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub created: SystemTime,
    pub select: Select,
    pub steps: Vec<Step>,
    #[serde(default)]
    pub destroy: Vec<String>,
    pub drives: Vec<DriveJob>,
    #[serde(default)]
    pub cancel: bool,
    /// Who asked (#45): re-checked with the apiserver before every step.
    /// `resume` replaces it with whoever resumes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester: Option<crate::kubeauth::Requester>,
    /// The DriveOperation object this job runs, when it came from one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// The DrivePolicy that submitted this job (#50), when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
}

impl Job {
    pub fn counts(&self) -> Value {
        let mut m: BTreeMap<String, usize> = BTreeMap::new();
        for d in &self.drives {
            *m.entry(serde_json::to_value(d.state).unwrap().as_str().unwrap().to_string()).or_default() += 1;
        }
        json!(m)
    }
    pub fn finished(&self) -> bool {
        self.drives.iter().all(|d| d.state.finished())
    }
    pub fn view(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        v["counts"] = self.counts();
        v["finished"] = json!(self.finished());
        v
    }
}

/// What a restart does with a drive job that was in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recover {
    /// The drive is still doing it by itself: watch it to the end.
    Reattach,
    /// Report and wait for `resume`.
    Interrupt(String),
    Leave,
}

pub fn recover_action(dj: &DriveJob, step: Option<&Step>) -> Recover {
    match dj.state {
        DjState::Running => match step {
            Some(Step::Format { .. }) if dj.kind != DriveKind::NvmeSsd => Recover::Reattach,
            Some(Step::Sanitize { .. }) => Recover::Reattach,
            // ERASE UNIT blocks until the drive is done and cannot be
            // watched; the drive finishes (or stays locked) on its own.
            Some(Step::SecurityErase { .. }) => Recover::Interrupt(match &dj.ata_password {
                Some(p) => format!(
                    "stormdrive restarted during an ATA security erase; the drive may still be erasing or locked with the one-time password {p}. \
                     Resume checks it with IDENTIFY and erases again with that password"
                ),
                None => "stormdrive restarted during an ATA security erase, before a password was set; resume to run it".into(),
            }),
            Some(s) => Recover::Interrupt(format!(
                "stormdrive restarted during {}; check the drive, then resume to run it again",
                s.name()
            )),
            None => Recover::Leave,
        },
        DjState::Queued => Recover::Interrupt("stormdrive restarted before this drive's next step; resume to run it".into()),
        _ => Recover::Leave,
    }
}

/// What a restart does with a drive that reads busy but no live run owns
/// (#39): a legacy format (`format::start`), a test or a firmware update
/// lives only in memory, so its `activity` outlives it in inventory.json.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Orphan {
    /// Not busy, or busy with something that recovers itself (a running
    /// worker step, a drain, a missing drive).
    Leave,
    /// A SCSI FORMAT UNIT we started: the drive keeps formatting by itself.
    ReattachFormat,
    /// Nothing is doing it any more: back to idle, with this reason.
    Idle(String),
}

/// `in_worker`: a worker job had this drive's step running when we stopped
/// (`recover` watches or interrupts it).
pub fn orphan_action(d: &Drive, in_worker: bool) -> Orphan {
    match d.activity {
        Activity::Formatting | Activity::Sanitizing if in_worker => Orphan::Leave,
        Activity::Formatting
            if d.kind != DriveKind::NvmeSsd && d.format.as_ref().is_some_and(|f| f.state == "running") =>
        {
            Orphan::ReattachFormat
        }
        Activity::Formatting => Orphan::Idle("a format was running when stormdrive stopped; check the drive's geometry before using it".into()),
        Activity::Sanitizing => Orphan::Idle("a sanitize was running when stormdrive stopped and no job owns it".into()),
        Activity::Testing => Orphan::Idle("a drive test was running when stormdrive stopped; it did not finish — run it again".into()),
        Activity::UpdatingFirmware => Orphan::Idle(
            "a firmware update was running when stormdrive stopped; it may be incomplete — check the version, then update again".into(),
        ),
        Activity::Idle | Activity::Draining | Activity::Missing => Orphan::Leave,
    }
}

/// Apply `Idle` to a drive: activity, and the persisted record marked
/// `interrupted`.
pub fn idle_orphan(d: &mut Drive, why: &str) {
    match d.activity {
        Activity::Formatting => {
            if let Some(f) = d.format.as_mut().filter(|f| f.state == "running") {
                f.state = "interrupted".into();
                f.finished = Some(SystemTime::now());
                f.error = Some(why.into());
            }
        }
        Activity::UpdatingFirmware => {
            if let Some(f) = d.firmware_update.as_mut().filter(|f| f.state == "running") {
                f.state = "interrupted".into();
                f.finished = Some(SystemTime::now());
                f.error = Some(why.into());
            }
        }
        _ => {}
    }
    d.activity = Activity::Idle;
}

/// Where a drive is on its way into the fleet: `unusable → formatting n% →
/// ready → enrolled` (plus `sanitizing`, `missing`, `busy`).
pub fn prep(d: &Drive, pct: Option<u8>) -> Value {
    let phase = if d.activity == Activity::Missing {
        "missing"
    } else if d.membership == Membership::Fleet {
        "enrolled"
    } else if d.activity == Activity::Formatting {
        "formatting"
    } else if d.activity == Activity::Sanitizing {
        "sanitizing"
    } else if d.needs_reformat() || !d.usable {
        "unusable"
    } else if d.activity != Activity::Idle {
        "busy"
    } else {
        "ready"
    };
    let pct = if matches!(phase, "formatting" | "sanitizing") { pct } else { None };
    json!({ "phase": phase, "pct": pct })
}

// ---------------------------------------------------------------- runtime

/// Jobs kept after they finish (the oldest finished ones go first).
const KEEP_JOBS: usize = 64;

pub struct Worker {
    jobs: Mutex<BTreeMap<String, Job>>,
    lanes: Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>,
    domains: Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>,
    path: Option<PathBuf>,
    save_lock: tokio::sync::Mutex<()>,
    seq: std::sync::atomic::AtomicU64,
}

impl Worker {
    /// Load `<data_dir>/jobs.json` (missing = none).
    pub fn load(data_dir: Option<&str>) -> Self {
        let path = data_dir.map(|d| PathBuf::from(d).join("jobs.json"));
        let jobs: BTreeMap<String, Job> = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).map_err(|e| tracing::error!("jobs.json unreadable, starting empty: {e}")).ok())
            .unwrap_or_default();
        Worker {
            jobs: Mutex::new(jobs),
            lanes: Default::default(),
            domains: Default::default(),
            path,
            save_lock: Default::default(),
            seq: Default::default(),
        }
    }

    pub fn list(&self) -> Vec<Value> {
        self.jobs.lock().unwrap().values().rev().map(Job::view).collect()
    }

    pub fn get(&self, id: &str) -> Option<Value> {
        self.jobs.lock().unwrap().get(id).map(Job::view)
    }

    pub fn job(&self, id: &str) -> Option<Job> {
        self.jobs.lock().unwrap().get(id).cloned()
    }

    /// The job a DriveOperation started, by the operation's name.
    pub fn by_operation(&self, op: &str) -> Option<Job> {
        self.jobs.lock().unwrap().values().rev().find(|j| j.operation.as_deref() == Some(op)).cloned()
    }

    /// Jobs started by DriveOperations that are not finished: (job, operation).
    pub fn operation_jobs(&self) -> Vec<(String, String)> {
        self.jobs.lock().unwrap().values().filter(|j| !j.finished()).filter_map(|j| j.operation.clone().map(|o| (j.id.clone(), o))).collect()
    }

    /// Jobs a DrivePolicy submitted that are not finished: (job, policy).
    pub fn policy_jobs(&self) -> Vec<(String, String)> {
        self.jobs.lock().unwrap().values().filter(|j| !j.finished()).filter_map(|j| j.policy.clone().map(|p| (j.id.clone(), p))).collect()
    }

    /// Drives some job has queued or running: not to be handed out again.
    pub fn active_drives(&self) -> std::collections::HashSet<DriveId> {
        let jobs = self.jobs.lock().unwrap();
        jobs.values()
            .flat_map(|j| j.drives.iter())
            .filter(|d| matches!(d.state, DjState::Queued | DjState::Running))
            .map(|d| d.drive)
            .collect()
    }

    /// The running step's progress for a drive, if a job has one.
    pub fn progress_of(&self, id: DriveId) -> Option<(String, Option<u8>)> {
        let jobs = self.jobs.lock().unwrap();
        jobs.values()
            .flat_map(|j| j.drives.iter())
            .find(|d| d.drive == id && d.state == DjState::Running)
            .map(|d| (d.phase.clone(), d.progress_pct))
    }

    fn with<R>(&self, job: &str, idx: usize, f: impl FnOnce(&mut DriveJob) -> R) -> Option<R> {
        let mut jobs = self.jobs.lock().unwrap();
        jobs.get_mut(job).and_then(|j| j.drives.get_mut(idx)).map(f)
    }

    fn progress(&self, job: &str, idx: usize, pct: Option<u8>, phase: &str) {
        self.with(job, idx, |d| {
            if pct.is_some() {
                d.progress_pct = pct;
            }
            d.phase = phase.to_string();
        });
    }

    async fn save(&self) {
        let Some(path) = &self.path else { return };
        let _g = self.save_lock.lock().await;
        let bytes = {
            let mut jobs = self.jobs.lock().unwrap();
            // Keep the newest; drop the oldest finished beyond the cap.
            while jobs.len() > KEEP_JOBS {
                let Some(old) = jobs.iter().find(|(_, j)| j.finished()).map(|(k, _)| k.clone()) else { break };
                jobs.remove(&old);
            }
            serde_json::to_vec(&*jobs).unwrap_or_default()
        };
        let path = path.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || crate::inventory::write_atomic(&path, &bytes)).await.unwrap_or_else(|e| Err(e.into())) {
            tracing::error!("jobs.json write failed: {e:#}");
        }
    }

    fn lane(&self, key: &str, permits: usize) -> Arc<tokio::sync::Semaphore> {
        self.lanes.lock().unwrap().entry(key.to_string()).or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(permits.max(1)))).clone()
    }

    fn domain(&self, key: &str, permits: usize) -> Arc<tokio::sync::Semaphore> {
        self.domains.lock().unwrap().entry(key.to_string()).or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(permits.max(1)))).clone()
    }

    fn new_id(&self) -> String {
        let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
        let n = self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("j{secs:x}{n:02x}")
    }
}

/// A job request (`POST /api/v1/worker/jobs`).
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    pub select: Select,
    pub steps: Vec<Step>,
    #[serde(default)]
    pub destroy: Vec<String>,
    #[serde(default, alias = "dryRun")]
    pub dry_run: bool,
}

/// What [`guard`] needs beyond the record: the drive's contents (a slab or a
/// filesystem), mounts, NVMe namespaces sharing the controller.
pub async fn context(state: &Arc<AppState>, d: &Drive) -> Context {
    let (path, name, usable, bs) = (d.path.clone(), d.name.clone(), d.usable, d.block_size);
    // A drive sd cannot read is read through SG at its own sector size
    // (#81): foreign data on a 520-byte drive must be named in `destroy`.
    let holds = tokio::task::spawn_blocking(move || crate::contents::holds_drive(&path, &name, usable, bs)).await.ok().flatten();
    let siblings = if d.kind == DriveKind::NvmeSsd {
        let ctrl = crate::erase::nvme_names(&d.name).map(|c| c.0);
        let inv = state.inventory.read().await;
        inv.drives
            .values()
            .filter(|o| o.id != d.id && o.activity != Activity::Missing)
            .filter(|o| ctrl.is_some() && crate::erase::nvme_names(&o.name).map(|c| c.0) == ctrl)
            .count()
    } else {
        0
    };
    Context { holds, mounted: crate::discovery::is_mounted(&d.name), nvme_siblings: siblings, stormblock: state.stormblock.enabled() }
}

/// Resolve the selection to drives. Unknown handles and shelves are errors.
pub async fn select(state: &Arc<AppState>, sel: &Select) -> Result<Vec<Drive>, String> {
    if sel.is_empty() {
        return Err("select at least one of: drives, shelf, model, unusable".into());
    }
    if sel.bays.is_some() && sel.shelf.is_none() {
        return Err("bays need a shelf".into());
    }
    let bays = sel.bays.as_deref().map(parse_bays).transpose()?;
    let shelf_key = match &sel.shelf {
        None => None,
        Some(h) => {
            let shelves = state.shelves.read().await;
            let norm = crate::ses::normalize_sas(h);
            let known = shelves.values().find(|r| {
                r.key == norm || r.key == *h || r.shelf.serial.as_deref() == Some(h) || r.shelf.id.as_deref() == Some(h) || r.esps.iter().any(|e| e.scsi_id == *h)
            });
            Some(known.map(|r| r.key.clone()).unwrap_or(norm))
        }
    };
    let inv = state.inventory.read().await;
    let base: Vec<&Drive> = if sel.drives.is_empty() {
        inv.drives.values().collect()
    } else {
        let mut v = vec![];
        for h in &sel.drives {
            let d = inv.resolve(h).ok_or_else(|| format!("drive {h:?}: not found"))?;
            if !v.iter().any(|x: &&Drive| x.id == d.id) {
                v.push(d);
            }
        }
        v
    };
    let mut out: Vec<Drive> = base.into_iter().filter(|d| matches(d, sel, shelf_key.as_deref(), bays.as_ref())).cloned().collect();
    if shelf_key.is_some() && out.is_empty() && sel.drives.is_empty() && sel.model.is_none() && !sel.unusable {
        return Err(format!("shelf {:?}: no drives (bays {})", sel.shelf.as_deref().unwrap_or(""), sel.bays.as_deref().unwrap_or("all")));
    }
    out.sort_by(|a, b| (a.location.bay, &a.name).cmp(&(b.location.bay, &b.name)));
    Ok(out)
}

/// Check, then (unless a dry run) create the job and start its drives.
/// `Err` only for a malformed request; refusals are reported per drive.
pub async fn submit(state: &Arc<AppState>, req: Request, requester: Option<crate::kubeauth::Requester>) -> Result<Value, String> {
    submit_for(state, req, requester, None).await
}

/// [`submit`], on behalf of a DriveOperation object (`operation` = its name).
pub async fn submit_for(
    state: &Arc<AppState>,
    req: Request,
    requester: Option<crate::kubeauth::Requester>,
    operation: Option<String>,
) -> Result<Value, String> {
    submit_tagged(state, req, requester, operation, None).await
}

/// [`submit`], tagged with the DriveOperation or DrivePolicy (#50) it is for.
pub async fn submit_tagged(
    state: &Arc<AppState>,
    req: Request,
    requester: Option<crate::kubeauth::Requester>,
    operation: Option<String>,
    policy: Option<String>,
) -> Result<Value, String> {
    validate_steps(&req.steps)?;
    let drives = select(state, &req.select).await?;
    if drives.is_empty() {
        return Err("the selection matches no drive".into());
    }
    let mut djs = vec![];
    for d in &drives {
        let named = named_for_destroy(d, &req.destroy);
        let cx = context(state, d).await;
        let verdict = guard(d, &req.steps, named, &cx);
        djs.push(DriveJob {
            drive: d.id,
            name: d.name.clone(),
            serial: d.serial.clone(),
            wwn: d.wwid.clone(),
            kind: d.kind,
            state: if verdict.is_ok() { DjState::Queued } else { DjState::Refused },
            step: 0,
            phase: if verdict.is_ok() { "queued".into() } else { "refused".into() },
            progress_pct: None,
            error: verdict.err(),
            done: vec![],
            destroy_named: named,
            started: None,
            finished: None,
            ata_password: None,
        });
    }
    let runnable = djs.iter().filter(|d| d.state == DjState::Queued).count();
    let plan = json!({
        "steps": req.steps.iter().map(Step::describe).collect::<Vec<_>>(),
        "runnable": runnable,
        "refused": djs.iter().filter(|d| d.state == DjState::Refused).map(|d| json!({ "drive": d.drive, "name": d.name, "reason": d.error })).collect::<Vec<_>>(),
    });
    if req.dry_run {
        let mut p = plan;
        p["dry_run"] = json!(true);
        // What is in each bay before anything is changed (#81): the dry run
        // is the operator's last look at vendor, model, firmware, sectors.
        let row = |d: &DriveJob| {
            let x = drives.iter().find(|x| x.id == d.drive);
            json!({
                "drive": d.drive,
                "name": d.name,
                "shelf": x.and_then(|x| x.location.shelf.as_ref()).and_then(|s| s.key()),
                "bay": x.and_then(|x| x.location.bay),
                "vendor": x.and_then(|x| x.vendor.clone()),
                "model": x.map(|x| x.model.clone()),
                "serial": d.serial,
                "firmware": x.map(|x| x.firmware.clone()),
                "kind": d.kind,
                "block_size": x.map(|x| x.block_size),
                "capacity_bytes": x.map(|x| x.capacity_bytes),
                "prot_type": x.and_then(|x| x.supports.as_ref()).map(|s| s.current_prot_type),
                // What the drive would get by default (#82), next to what
                // the job's own format step asks for.
                "planned_format": x.and_then(|x| x.supports.as_ref()).map(|s| match crate::pi::plan(s, None) {
                    Ok(p) => json!(p),
                    Err(e) => json!({ "error": e }),
                }),
            })
        };
        p["drives"] = json!(djs.iter().filter(|d| d.state == DjState::Queued).map(row).collect::<Vec<_>>());
        return Ok(p);
    }
    if runnable == 0 {
        return Err(format!("nothing to run: {}", plan["refused"]));
    }
    let id = state.worker.new_id();
    let who = requester.as_ref().map(|r| r.who.clone()).unwrap_or_else(|| "unknown".into());
    let job = Job {
        id: id.clone(),
        created: SystemTime::now(),
        select: req.select,
        steps: req.steps.clone(),
        destroy: req.destroy,
        drives: djs,
        cancel: false,
        requester,
        operation: operation.clone(),
        policy: policy.clone(),
    };
    let destructive = req.steps.iter().any(Step::destroys);
    state.events.write().await.push(
        None,
        if destructive { Severity::Warning } else { Severity::Info },
        "worker",
        format!(
            "job {id}{}: {} on {runnable} drive(s) for {who}{}",
            operation
                .as_deref()
                .map(|o| format!(" (DriveOperation {o})"))
                .or_else(|| policy.as_deref().map(|p| format!(" (DrivePolicy {p})")))
                .unwrap_or_default(),
            req.steps.iter().map(Step::describe).collect::<Vec<_>>().join(" → "),
            if destructive { " (their data is destroyed)" } else { "" }
        ),
    );
    let idxs: Vec<usize> = job.drives.iter().enumerate().filter(|(_, d)| d.state == DjState::Queued).map(|(i, _)| i).collect();
    state.worker.jobs.lock().unwrap().insert(id.clone(), job);
    state.worker.save().await;
    for i in idxs {
        tokio::spawn(run_drive(state.clone(), id.clone(), i));
    }
    Ok(state.worker.get(&id).unwrap_or_default())
}

pub async fn cancel(state: &Arc<AppState>, id: &str) -> Result<Value, String> {
    {
        let mut jobs = state.worker.jobs.lock().unwrap();
        let j = jobs.get_mut(id).ok_or_else(|| format!("job {id:?}: not found"))?;
        j.cancel = true;
        for d in j.drives.iter_mut().filter(|d| matches!(d.state, DjState::Queued | DjState::Interrupted)) {
            d.state = DjState::Cancelled;
            d.phase = "cancelled".into();
            d.finished = Some(SystemTime::now());
        }
    }
    state.worker.save().await;
    state.events.write().await.push(None, Severity::Info, "worker", format!("job {id}: cancelled (running steps finish; the rest do not start)"));
    Ok(state.worker.get(id).unwrap_or_default())
}

pub async fn resume(state: &Arc<AppState>, id: &str, requester: Option<crate::kubeauth::Requester>) -> Result<Value, String> {
    let idxs: Vec<usize> = {
        let mut jobs = state.worker.jobs.lock().unwrap();
        let j = jobs.get_mut(id).ok_or_else(|| format!("job {id:?}: not found"))?;
        if j.cancel {
            return Err(format!("job {id} was cancelled"));
        }
        if requester.is_some() {
            j.requester = requester;
        }
        let mut v = vec![];
        for (i, d) in j.drives.iter_mut().enumerate().filter(|(_, d)| d.state == DjState::Interrupted) {
            d.state = DjState::Queued;
            d.phase = "queued".into();
            d.error = None;
            v.push(i);
        }
        v
    };
    if idxs.is_empty() {
        return Err(format!("job {id}: nothing interrupted to resume"));
    }
    state.worker.save().await;
    for i in &idxs {
        tokio::spawn(run_drive(state.clone(), id.to_string(), *i));
    }
    state.events.write().await.push(None, Severity::Info, "worker", format!("job {id}: resumed {} drive(s)", idxs.len()));
    Ok(state.worker.get(id).unwrap_or_default())
}

async fn set_activity(state: &Arc<AppState>, id: DriveId, a: Activity) {
    if let Some(d) = state.inventory.write().await.drives.get_mut(&id) {
        d.activity = a;
    }
}

/// Take one drive through the job's steps, from where it stands.
async fn run_drive(state: Arc<AppState>, job: String, idx: usize) {
    let (steps, drive_id, destroy_named) = {
        let jobs = state.worker.jobs.lock().unwrap();
        let Some(j) = jobs.get(&job) else { return };
        let d = &j.drives[idx];
        (j.steps.clone(), d.drive, d.destroy_named)
    };
    let op = crate::kubeauth::Access { resource: "driveoperations", verb: "create", name: None };
    let cfg = state.config.worker.clone();
    loop {
        let (step_idx, cancelled, st) = {
            let jobs = state.worker.jobs.lock().unwrap();
            let j = &jobs[&job];
            (j.drives[idx].step, j.cancel, j.drives[idx].state)
        };
        if st != DjState::Queued && st != DjState::Running {
            return;
        }
        if step_idx >= steps.len() {
            state.worker.with(&job, idx, |d| {
                d.state = DjState::Done;
                d.phase = "done".into();
                d.progress_pct = None;
                d.finished = Some(SystemTime::now());
            });
            let name = state.worker.with(&job, idx, |d| d.name.clone()).unwrap_or_default();
            state.events.write().await.push(Some(drive_id), Severity::Info, "worker", format!("{name}: job {job} done ({})", steps.iter().map(Step::describe).collect::<Vec<_>>().join(" → ")));
            state.worker.save().await;
            return;
        }
        if cancelled {
            state.worker.with(&job, idx, |d| {
                d.state = DjState::Cancelled;
                d.phase = "cancelled".into();
                d.finished = Some(SystemTime::now());
            });
            state.worker.save().await;
            return;
        }
        let step = steps[step_idx].clone();
        let Some(drive) = state.inventory.read().await.drives.get(&drive_id).cloned() else {
            fail(&state, &job, idx, drive_id, "the drive is no longer in the inventory".into()).await;
            return;
        };
        // Wait for a lane: low-level steps per HBA, enroll per failure domain.
        let permit = if step.low_level() || matches!(step, Step::Partition { .. }) {
            state.worker.progress(&job, idx, None, &format!("waiting ({})", hba_key(&drive)));
            state.worker.lane(&hba_key(&drive), cfg.max_per_hba).acquire_owned().await.ok()
        } else {
            state.worker.progress(&job, idx, None, &format!("waiting ({})", domain_key(&drive)));
            state.worker.domain(&domain_key(&drive), cfg.enroll_per_domain).acquire_owned().await.ok()
        };
        // Everything is checked again right before it happens.
        let Some(drive) = state.inventory.read().await.drives.get(&drive_id).cloned() else {
            fail(&state, &job, idx, drive_id, "the drive is no longer in the inventory".into()).await;
            return;
        };
        let cx = context(&state, &drive).await;
        if let Err(why) = guard(&drive, &steps[step_idx..], destroy_named, &cx) {
            fail(&state, &job, idx, drive_id, format!("before {}: {why}", step.name())).await;
            return;
        }
        // …including whether the requester may still do it (#45): the
        // object or the request that started the job is not trusted alone.
        let requester = state.worker.jobs.lock().unwrap().get(&job).and_then(|j| j.requester.clone());
        let Some(requester) = requester else {
            fail(&state, &job, idx, drive_id, format!("before {}: the job names no requester to re-check", step.name())).await;
            return;
        };
        match state.gate.recheck(&requester, &op).await {
            Ok(()) => {}
            Err(crate::kubeauth::RecheckError::Denied(why)) => {
                fail(&state, &job, idx, drive_id, format!("before {}: refused — {why}", step.name())).await;
                return;
            }
            Err(crate::kubeauth::RecheckError::Unavailable(why)) => {
                let why = format!("before {}: could not re-check {} ({why}); resume when the apiserver answers", step.name(), requester.who);
                state.worker.with(&job, idx, |d| {
                    d.state = DjState::Interrupted;
                    d.phase = "interrupted".into();
                    d.error = Some(why.clone());
                });
                state.events.write().await.push(Some(drive_id), Severity::Warning, "worker", format!("{}: job {job} — {why}", drive.name));
                state.worker.save().await;
                return;
            }
        }
        state.worker.with(&job, idx, |d| {
            d.state = DjState::Running;
            d.phase = step.name().into();
            d.progress_pct = None;
            d.started.get_or_insert(SystemTime::now());
        });
        state.worker.save().await;
        let result = run_step(&state, &job, idx, &drive, &step, &steps).await;
        drop(permit);
        // The drive's kept history (#68): what was done, by whom, before
        // and after — the erase certificate for a sanitize or an erase.
        let after = state.inventory.read().await.drives.get(&drive_id).map(geometry).unwrap_or_default();
        crate::history::send(crate::history::Item::Action(
            drive_id,
            crate::history::Action {
                op: step.name().into(),
                params: serde_json::to_value(&step).unwrap_or_default(),
                result: if result.is_ok() { "done".into() } else { "failed".into() },
                error: result.as_ref().err().cloned(),
                requester: Some(requester.who.clone()),
                job: Some(job.clone()),
                before: geometry(&drive),
                after,
            },
        ));
        match result {
            Ok(note) => {
                state.worker.with(&job, idx, |d| {
                    d.done.push(format!("{}: {note}", step.describe()));
                    d.step += 1;
                    d.progress_pct = None;
                });
                state.worker.save().await;
            }
            Err(e) => {
                fail(&state, &job, idx, drive_id, format!("{}: {e}", step.name())).await;
                return;
            }
        }
    }
}

/// A drive's geometry and firmware, for a history action's before/after.
pub fn geometry(d: &Drive) -> Value {
    json!({
        "block_size": d.block_size,
        "prot_type": d.supports.as_ref().map(|s| s.current_prot_type),
        "capacity_bytes": d.capacity_bytes,
        "firmware": d.firmware,
        "usable": d.usable,
    })
}

async fn fail(state: &Arc<AppState>, job: &str, idx: usize, id: DriveId, why: String) {
    let name = state
        .worker
        .with(job, idx, |d| {
            d.state = DjState::Failed;
            d.phase = "failed".into();
            d.error = Some(why.clone());
            d.finished = Some(SystemTime::now());
            d.name.clone()
        })
        .unwrap_or_default();
    state.events.write().await.push(Some(id), Severity::Error, "worker", format!("{name}: job {job} FAILED — {why}"));
    state.worker.save().await;
}

async fn run_step(state: &Arc<AppState>, job: &str, idx: usize, d: &Drive, step: &Step, steps: &[Step]) -> Result<String, String> {
    let st = state.clone();
    let (j, i) = (job.to_string(), idx);
    let progress = move |pct: Option<u8>, phase: &str| st.worker.progress(&j, i, pct, phase);
    match step {
        Step::Format { block_size, .. } if d.kind == DriveKind::NvmeSsd => {
            set_activity(state, d.id, Activity::Formatting).await;
            let (path, name, bs) = (d.path.clone(), d.name.clone(), *block_size);
            let r = tokio::task::spawn_blocking(move || crate::erase::nvme_format(&path, &name, bs, &progress))
                .await
                .unwrap_or_else(|e| Err(format!("task: {e}")));
            let geometry = if r.is_ok() { read_geometry(&d.name) } else { None };
            if let Some(dr) = state.inventory.write().await.drives.get_mut(&d.id) {
                dr.activity = Activity::Idle;
                if let (Ok(size), Some((cap, lbs))) = (&r, geometry) {
                    dr.block_size = *size;
                    dr.physical_block_size = lbs.max(*size);
                    dr.capacity_bytes = cap;
                    dr.usable = true;
                }
            }
            state.persist().await;
            r.map(|s| format!("Format NVM to {s}-byte blocks"))
        }
        Step::Format { block_size, protection } => {
            // The SCSI path is the existing format job (records, events,
            // rescan and verify included); the worker waits on it.
            let handle = crate::format::start(state.clone(), d.clone(), *block_size, *protection).await;
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let run = handle.run.lock().unwrap().clone();
                state.worker.progress(job, idx, run.progress_pct, &format!("format: {}", run.phase));
                match run.state {
                    crate::format::FormatState::Running => continue,
                    crate::format::FormatState::Done => return Ok(format!("FORMAT UNIT to {block_size}-byte sectors, {}", protection.word())),
                    crate::format::FormatState::Failed => return Err(run.error.unwrap_or_else(|| "format failed".into())),
                }
            }
        }
        Step::Sanitize { method } => {
            set_activity(state, d.id, Activity::Sanitizing).await;
            state.events.write().await.push(Some(d.id), Severity::Warning, "worker", format!("{}: {} started (all data destroyed)", d.name, step.describe()));
            let (path, name, m, nvme) = (d.path.clone(), d.name.clone(), *method, d.kind == DriveKind::NvmeSsd);
            let r = tokio::task::spawn_blocking(move || {
                if nvme {
                    crate::erase::nvme_sanitize(&path, m, &progress)
                } else {
                    let sg = crate::scsi::sg_path_for_block(&name).unwrap_or(path);
                    crate::erase::scsi_sanitize_run(&sg, m, &progress)
                }
            })
            .await
            .unwrap_or_else(|e| Err(format!("task: {e}")));
            set_activity(state, d.id, Activity::Idle).await;
            state.persist().await;
            r.map(|_| format!("{method:?} sanitize complete").to_lowercase())
        }
        Step::Test { kind } => {
            let handle = crate::drivetest::start(state.clone(), d.clone(), *kind).await;
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let run = handle.run.lock().unwrap().clone();
                let pct = (run.bytes_total > 0).then(|| (run.bytes_done.saturating_mul(100) / run.bytes_total).min(100) as u8);
                state.worker.progress(job, idx, pct, &format!("test ({})", serde_json::to_value(kind).unwrap_or_default().as_str().unwrap_or("?")));
                if run.state == crate::drivetest::TestState::Running {
                    continue;
                }
                // The test's own task puts the drive back to idle after the
                // verdict; the next step's guard must not see it testing.
                for _ in 0..30 {
                    let testing = matches!(state.inventory.read().await.drives.get(&d.id), Some(x) if x.activity == Activity::Testing);
                    if !testing {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                let read = run.bytes_done >> 20;
                return match run.state {
                    crate::drivetest::TestState::Passed => Ok(format!("{} test passed ({read} MiB)", step.describe())),
                    crate::drivetest::TestState::Cancelled => Err(format!("{} cancelled", step.describe())),
                    _ => Err(format!(
                        "{} FAILED, {} error(s){} — the later steps do not run",
                        step.describe(),
                        run.errors.len(),
                        run.errors.first().map(|e| format!(": {e}")).unwrap_or_default()
                    )),
                };
            }
        }
        Step::SecurityErase { enhanced } => {
            let sg = crate::scsi::sg_path_for_block(&d.name).unwrap_or_else(|| d.path.clone());
            let ours = state.worker.with(job, idx, |x| x.ata_password.clone()).flatten();
            let (sg1, want) = (sg.clone(), *enhanced);
            let sec = tokio::task::spawn_blocking(move || crate::erase::ata_security(&sg1)).await.unwrap_or_else(|e| Err(format!("task: {e}")))?;
            let plan = crate::erase::plan_security_erase(&sec, ours.is_some(), want)?;
            // The password goes on record — jobs.json written, an event — before
            // the drive ever sees it.
            let password = match ours {
                Some(p) => p,
                None => {
                    let p = crate::erase::one_time_password(&random16()?);
                    state.worker.with(job, idx, |x| x.ata_password = Some(p.clone()));
                    state.worker.save().await;
                    state.events.write().await.push(
                        Some(d.id),
                        Severity::Warning,
                        "worker",
                        format!(
                            "{} ({}): ATA user password {p} set for a security erase (job {job}). If the drive loses power before the erase finishes it stays locked: hdparm --user-master u --security-unlock {p} /dev/{}, then --security-disable {p}",
                            d.name, d.serial, d.name
                        ),
                    );
                    p
                }
            };
            set_activity(state, d.id, Activity::Sanitizing).await;
            state.events.write().await.push(Some(d.id), Severity::Warning, "worker", format!("{}: {} started (all data destroyed)", d.name, step.describe()));
            let (pw, plan1) = (password.clone(), plan.clone());
            let r = tokio::task::spawn_blocking(move || crate::erase::ata_security_erase(&sg, &pw, &plan1, &progress))
                .await
                .unwrap_or_else(|e| Err(crate::erase::AtaEraseError { message: format!("task: {e}"), password_set: true }));
            let keep = r.as_ref().err().is_some_and(|e| e.password_set);
            if !keep {
                state.worker.with(job, idx, |x| x.ata_password = None);
            }
            state.worker.save().await;
            set_activity(state, d.id, Activity::Idle).await;
            state.persist().await;
            match r {
                Ok(()) => Ok(format!("ATA security erase{} complete; password removed", if plan.enhanced { " (enhanced)" } else { "" })),
                Err(e) if keep => Err(format!(
                    "{} — the drive may still be locked with the one-time password {password} (hdparm --user-master u --security-unlock {password} /dev/{}, then --security-disable {password})",
                    e.message, d.name
                )),
                Err(e) => Err(e.message),
            }
        }
        Step::Partition { role } => {
            set_activity(state, d.id, Activity::Formatting).await;
            progress(None, "partition");
            let (name, path, role) = (d.name.clone(), d.path.clone(), *role);
            let r = tokio::task::spawn_blocking(move || write_partition(&name, &path, role))
                .await
                .unwrap_or_else(|e| Err(format!("task: {e}")));
            set_activity(state, d.id, Activity::Idle).await;
            r.map(|(n, bytes)| format!("GPT, partition {n} ({} GiB, {})", bytes >> 30, role.partition_name()))
        }
        Step::Enroll { tier, role } => {
            let partitioned = steps.iter().any(|s| matches!(s, Step::Partition { .. }));
            let target = if partitioned { format!("/dev/{}", crate::gpt::partition_name(&d.name, 1)) } else { d.path.clone() };
            let tier = tier.clone().unwrap_or_else(|| state.stormblock.tier_for(d.kind));
            let labels = d.stormblock_labels();
            let listed = state.stormblock.list_drives().await.map_err(|e| format!("stormblock: {e:#}"))?;
            if listed.iter().any(|sd| sd.get("path").and_then(|v| v.as_str()) == Some(target.as_str())) {
                state.stormblock.set_labels(&target, &labels, Some(d.id.0)).await.map_err(|e| format!("labels: {e:#}"))?;
            } else {
                state.stormblock.add_drive(&target, &labels, Some(d.id.0)).await.map_err(|e| format!("open in stormblock: {e:#}"))?;
            }
            let has_slab = !state.stormblock.drive_slabs(&target).await.unwrap_or_default().is_empty();
            if !has_slab {
                state.stormblock.format_slab(&target, &tier, Some(role.word())).await.map_err(|e| format!("format slab: {e:#}"))?;
            }
            if let Some(dr) = state.inventory.write().await.drives.get_mut(&d.id) {
                dr.membership = Membership::Fleet;
                dr.fleet_partition = partitioned.then_some(1);
                dr.pushed_labels = labels.clone();
                dr.pushed_health = None;
                dr.pushed_overcommit = None;
                dr.drain = None;
            }
            state.events.write().await.push(Some(d.id), Severity::Info, "fleet", format!("{}: enrolled by the worker — {target}, {} slab, tier {tier}, labels {labels:?}", d.name, role.word()));
            state.persist().await;
            Ok(format!("{target} in stormblock, {} slab, tier {tier}", role.word()))
        }
    }
}

/// The disk's size and logical block size, from sysfs.
/// 16 bytes from the kernel's random source, for a one-time password.
fn random16() -> Result<[u8; 16], String> {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).map_err(|e| format!("/dev/urandom: {e}"))?;
    Ok(b)
}

fn read_geometry(name: &str) -> Option<(u64, u32)> {
    let base = format!("/sys/block/{name}");
    let sectors: u64 = std::fs::read_to_string(format!("{base}/size")).ok()?.trim().parse().ok()?;
    let lbs: u32 = std::fs::read_to_string(format!("{base}/queue/logical_block_size")).ok()?.trim().parse().ok()?;
    Some((sectors * 512, lbs))
}

/// Where `write_layout` writes: the disk, or (in tests) a counter.
pub trait LayoutTarget {
    fn put(&self, buf: &[u8], offset: u64) -> std::io::Result<()>;
    fn sync(&self) -> std::io::Result<()>;
}

#[cfg(unix)]
impl LayoutTarget for std::fs::File {
    fn put(&self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        std::os::unix::fs::FileExt::write_all_at(self, buf, offset)
    }
    fn sync(&self) -> std::io::Result<()> {
        self.sync_all()
    }
}

/// The most `write_layout` clears at each end of the disk.
pub const CLEAR_BYTES: u64 = 4 << 20;

/// Clear the first and last 4 MiB — what was there stops being there, not
/// just stops being described (old tables, superblocks, md/LVM labels) —
/// then write both copies of the table and sync. Metadata only (#72): about
/// 8 MiB whatever the disk's size, never a pass over its surface; what
/// stormblock never wrote it never reads.
pub fn write_layout(f: &impl LayoutTarget, capacity: u64, layout: &crate::gpt::Layout) -> Result<(), String> {
    let zero = vec![0u8; CLEAR_BYTES.min(capacity / 2) as usize];
    let tail = capacity.saturating_sub(zero.len() as u64);
    f.put(&zero, 0).map_err(|e| format!("clear head: {e}"))?;
    f.put(&zero, tail).map_err(|e| format!("clear tail: {e}"))?;
    f.put(&layout.primary, 0).map_err(|e| format!("write GPT: {e}"))?;
    f.put(&layout.backup, layout.backup_offset).map_err(|e| format!("write backup GPT: {e}"))?;
    f.sync().map_err(|e| format!("sync: {e}"))
}

/// Clear the old signatures, write a GPT with one partition, have the kernel
/// read it, and wait for the partition node. (partition number, bytes)
fn write_partition(name: &str, path: &str, role: Role) -> Result<(u32, u64), String> {
    let (capacity, lbs) = read_geometry(name).ok_or_else(|| format!("{name}: no size in sysfs"))?;
    let layout = crate::gpt::layout(
        capacity,
        lbs as u64,
        role.type_guid(),
        role.partition_name(),
        uuid::Uuid::new_v4().to_bytes_le(),
        uuid::Uuid::new_v4().to_bytes_le(),
    )?;
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::OpenOptions::new().read(true).write(true).open(path).map_err(|e| format!("open {path}: {e}"))?;
        write_layout(&f, capacity, &layout)?;
        // BLKRRPART = _IO(0x12, 95)
        const BLKRRPART: u64 = 0x125F;
        let r = unsafe { libc::ioctl(f.as_raw_fd(), BLKRRPART as _) };
        if r < 0 {
            return Err(format!("re-read partition table: {}", std::io::Error::last_os_error()));
        }
        drop(f);
        let part = crate::gpt::partition_name(name, 1);
        for _ in 0..50 {
            if std::path::Path::new(&format!("/sys/block/{name}/{part}")).exists() && std::path::Path::new(&format!("/dev/{part}")).exists() {
                return Ok((1, layout.partition_bytes()));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err(format!("the kernel did not show /dev/{part} within 10 s"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (path, layout);
        Err("partitioning is Linux-only".into())
    }
}

/// After a restart: watch what the drives are still doing, interrupt the
/// rest. Called once, when the daemon starts.
pub async fn recover(state: Arc<AppState>) {
    recover_orphans(&state).await;
    let mut reattach = vec![];
    let mut interrupted = vec![];
    {
        let mut jobs = state.worker.jobs.lock().unwrap();
        for j in jobs.values_mut() {
            let steps = j.steps.clone();
            for (i, d) in j.drives.iter_mut().enumerate() {
                match recover_action(d, steps.get(d.step)) {
                    Recover::Reattach => reattach.push((j.id.clone(), i, d.drive, steps[d.step].clone())),
                    Recover::Interrupt(why) => {
                        d.state = DjState::Interrupted;
                        d.phase = "interrupted".into();
                        d.error = Some(why);
                        interrupted.push(d.drive);
                    }
                    Recover::Leave => {}
                }
            }
        }
    }
    for id in &interrupted {
        if let Some(d) = state.inventory.write().await.drives.get_mut(id) {
            if matches!(d.activity, Activity::Formatting | Activity::Sanitizing) {
                d.activity = Activity::Idle;
            }
        }
    }
    if !interrupted.is_empty() || !reattach.is_empty() {
        state.events.write().await.push(
            None,
            Severity::Warning,
            "worker",
            format!("restarted: {} drive step(s) re-attached, {} interrupted (resume the job to continue)", reattach.len(), interrupted.len()),
        );
    }
    state.worker.save().await;
    for (job, idx, id, step) in reattach {
        tokio::spawn(watch(state.clone(), job, idx, id, step));
    }
}

/// Drives left busy by an operation that was not a worker job (#39).
async fn recover_orphans(state: &Arc<AppState>) {
    let owned: std::collections::HashSet<DriveId> = state
        .worker
        .jobs
        .lock()
        .unwrap()
        .values()
        .flat_map(|j| j.drives.iter().filter(|d| d.state == DjState::Running).map(|d| d.drive))
        .collect();
    let mut reattach = vec![];
    let mut idled = vec![];
    {
        let mut inv = state.inventory.write().await;
        for d in inv.drives.values_mut() {
            match orphan_action(d, owned.contains(&d.id)) {
                Orphan::Leave => {}
                Orphan::ReattachFormat => reattach.push(d.clone()),
                Orphan::Idle(why) => {
                    idled.push((d.id, format!("{}: {why}", d.name)));
                    idle_orphan(d, &why);
                }
            }
        }
    }
    if reattach.is_empty() && idled.is_empty() {
        return;
    }
    {
        let mut ev = state.events.write().await;
        for (id, msg) in idled {
            ev.push(Some(id), Severity::Warning, "restart", msg);
        }
    }
    for d in reattach {
        crate::format::reattach(state.clone(), d).await;
    }
    state.persist().await;
}

/// Follow a low-level step the drive kept running across our restart.
async fn watch(state: Arc<AppState>, job: String, idx: usize, id: DriveId, step: Step) {
    let Some(d) = state.inventory.read().await.drives.get(&id).cloned() else { return };
    let busy = if matches!(step, Step::Sanitize { .. }) { Activity::Sanitizing } else { Activity::Formatting };
    set_activity(&state, id, busy).await;
    let st = state.clone();
    let (j, i) = (job.clone(), idx);
    let progress = move |pct: Option<u8>, phase: &str| st.worker.progress(&j, i, pct, phase);
    let (name, path, nvme) = (d.name.clone(), d.path.clone(), d.kind == DriveKind::NvmeSsd);
    let is_sanitize = matches!(step, Step::Sanitize { .. });
    let r = tokio::task::spawn_blocking(move || {
        if nvme {
            crate::erase::nvme_sanitize_wait(&path, &progress)
        } else {
            let sg = crate::scsi::sg_path_for_block(&name).unwrap_or(path);
            crate::erase::scsi_wait(&sg, is_sanitize, &progress)
        }
    })
    .await
    .unwrap_or_else(|e| Err(format!("task: {e}")));
    set_activity(&state, id, Activity::Idle).await;
    if !is_sanitize {
        crate::format::rescan(&d.name);
    }
    match r {
        Ok(()) => {
            let more = state
                .worker
                .with(&job, idx, |dj| {
                    dj.done.push(format!("{}: finished while stormdrive was restarting (watched to the end)", step.describe()));
                    dj.step += 1;
                    dj.progress_pct = None;
                    dj.state = DjState::Interrupted;
                    dj.phase = "interrupted".into();
                    dj.error = Some("stormdrive restarted mid-job; the running step finished — resume to continue".into());
                    dj.step
                })
                .unwrap_or(0);
            let total = state.worker.jobs.lock().unwrap().get(&job).map(|j| j.steps.len()).unwrap_or(0);
            if more >= total {
                state.worker.with(&job, idx, |dj| {
                    dj.state = DjState::Done;
                    dj.phase = "done".into();
                    dj.error = None;
                    dj.finished = Some(SystemTime::now());
                });
            }
            state.worker.save().await;
        }
        Err(e) => fail(&state, &job, idx, id, format!("{} (after restart): {e}", step.name())).await,
    }
    state.persist().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::{Controller, Location, Shelf};

    fn drive(over: impl FnOnce(&mut Drive)) -> Drive {
        let mut d: Drive = serde_json::from_value(json!({
            "id": DriveId::derive(Some("naa.5000c500aaaa0001"), "M", "S1"), "path": "/dev/sdb", "name": "sdb", "paths": ["/dev/sdb"],
            "kind": "sas_hdd", "model": "ST1200MM0098", "serial": "S1", "firmware": "N003", "wwid": "naa.5000c500aaaa0001",
            "capacity_bytes": 1_200_243_695_616u64, "block_size": 512, "first_seen": {"secs_since_epoch": 0, "nanos_since_epoch": 0},
            "last_seen": {"secs_since_epoch": 0, "nanos_since_epoch": 0},
        }))
        .unwrap();
        d.location = Location {
            controller: Some(Controller { scsi_host: Some("host0".into()), pcie_addr: Some("0000:01:00.0".into()), driver: Some("mpt3sas".into()) }),
            shelf: Some(Shelf { logical_id: Some("5000a098aaaa0001".into()), model: Some("DS224C".into()), ..Default::default() }),
            bay: Some(4),
            ..Default::default()
        };
        over(&mut d);
        d
    }

    fn cx() -> Context {
        Context { stormblock: true, ..Default::default() }
    }

    const FMT: Step = Step::Format { block_size: 4096, protection: crate::pi::Protection::None };
    fn part() -> Step {
        Step::Partition { role: Role::Data }
    }
    fn enroll() -> Step {
        Step::Enroll { tier: None, role: Role::Data }
    }

    #[test]
    fn steps_come_in_order_once_each() {
        assert!(validate_steps(&[FMT, part(), enroll()]).is_ok());
        assert!(validate_steps(&[Step::Sanitize { method: SanitizeMethod::Crypto }, FMT]).is_ok());
        assert!(validate_steps(&[]).is_err());
        assert!(validate_steps(&[part(), FMT]).unwrap_err().contains("before"));
        assert!(validate_steps(&[FMT, FMT]).unwrap_err().contains("twice"));
        assert!(validate_steps(&[Step::Format { block_size: 520, protection: crate::pi::Protection::None }]).is_err());
        assert!(validate_steps(&[Step::Partition { role: Role::System }, enroll()]).unwrap_err().contains("differ"));
        // The JSON shape the API takes.
        let s: Vec<Step> = serde_json::from_value(json!([
            {"op": "format", "block_size": 4096}, {"op": "sanitize", "method": "crypto"},
            {"op": "partition"}, {"op": "enroll", "tier": "cool"}
        ]))
        .unwrap();
        assert_eq!(s[2], part());
        assert_eq!(s[3], Step::Enroll { tier: Some("cool".into()), role: Role::Data });
    }

    #[test]
    fn bays_and_selection() {
        assert_eq!(parse_bays("0-3, 7").unwrap().into_iter().collect::<Vec<_>>(), vec![0, 1, 2, 3, 7]);
        assert!(parse_bays("5-2").is_err());
        assert!(parse_bays("x").is_err());
        assert!(parse_bays("").is_err());
        let d = drive(|_| {});
        let sel = |f: fn(&mut Select)| {
            let mut s = Select::default();
            f(&mut s);
            s
        };
        assert!(matches(&d, &Select::default(), Some("5000a098aaaa0001"), Some(&parse_bays("0-11").unwrap())));
        assert!(!matches(&d, &Select::default(), Some("other"), None));
        assert!(!matches(&d, &Select::default(), None, Some(&parse_bays("5-9").unwrap())));
        assert!(matches(&d, &sel(|s| s.model = Some("st1200mm0098 ".into())), None, None));
        assert!(!matches(&d, &sel(|s| s.unusable = true), None, None), "a 512-byte drive is not unusable");
        let u = drive(|d| {
            d.block_size = 520;
            d.usable = false;
        });
        assert!(matches(&u, &sel(|s| s.unusable = true), None, None));
        assert!(Select::default().is_empty());
    }

    #[test]
    fn destroy_names_by_identity_never_by_dev_name() {
        let d = drive(|_| {});
        assert!(named_for_destroy(&d, &["NAA.5000C500AAAA0001".into()]));
        assert!(named_for_destroy(&d, &["S1".into()]));
        assert!(named_for_destroy(&d, &[d.id.0.to_string()]));
        assert!(!named_for_destroy(&d, &["sdb".into(), "/dev/sdb".into(), "".into()]));
    }

    #[test]
    fn the_guard() {
        let d = drive(|_| {});
        assert!(guard(&d, &[FMT, part(), enroll()], false, &cx()).is_ok());
        let fleet = drive(|d| d.membership = Membership::Fleet);
        assert!(guard(&fleet, &[FMT], true, &cx()).unwrap_err().contains("fleet"), "never a fleet drive, named or not");
        let busy = drive(|d| d.activity = Activity::Testing);
        assert!(guard(&busy, &[enroll()], false, &cx()).unwrap_err().contains("busy"));
        let reserved = drive(|d| d.designation = Designation::Reserved);
        assert!(guard(&reserved, &[FMT], false, &cx()).is_err());
        let mounted = Context { mounted: true, ..cx() };
        assert!(guard(&d, &[FMT], true, &mounted).unwrap_err().contains("mounted"), "a mounted drive even when named");
        // Data on the drive: refused unless named.
        let holds = Context { holds: Some("xfs (whole drive)".into()), ..cx() };
        assert!(guard(&d, &[FMT], false, &holds).unwrap_err().contains("destroy"));
        assert!(guard(&d, &[FMT], true, &holds).is_ok());
        assert!(guard(&d, &[enroll()], false, &holds).is_ok(), "enroll alone destroys nothing");
        let sys = drive(|d| d.in_use_by = Some("stormblock (slabs in partitions 2, 3)".into()));
        assert!(guard(&sys, &[part()], false, &cx()).unwrap_err().contains("stormblock"));
        // 520-byte drive: format first.
        let u = drive(|d| {
            d.block_size = 520;
            d.usable = false;
        });
        assert!(guard(&u, &[part()], false, &cx()).unwrap_err().contains("format step"));
        assert!(guard(&u, &[FMT, part()], false, &cx()).is_ok());
        // Enroll needs stormblock, a healthy drive, not failed.
        assert!(guard(&d, &[enroll()], false, &Context::default()).unwrap_err().contains("disabled"));
        let failed = drive(|d| d.designation = Designation::Failed);
        assert!(guard(&failed, &[Step::Sanitize { method: SanitizeMethod::Crypto }], false, &cx()).is_ok(), "erase a failed drive");
        assert!(guard(&failed, &[enroll()], false, &cx()).is_err());
        // NVMe sanitize is controller-wide.
        let nv = drive(|d| {
            d.kind = DriveKind::NvmeSsd;
            d.name = "nvme0n1".into();
        });
        let sib = Context { nvme_siblings: 1, ..cx() };
        assert!(guard(&nv, &[Step::Sanitize { method: SanitizeMethod::Block }], false, &sib).unwrap_err().contains("namespace"));
        assert!(guard(&nv, &[FMT], false, &sib).is_ok());
    }

    #[test]
    fn lanes_and_domains() {
        let d = drive(|_| {});
        assert_eq!(hba_key(&d), "hba:0000:01:00.0");
        assert_eq!(domain_key(&d), "shelf:5000a098aaaa0001");
        let direct = drive(|d| d.location.shelf = None);
        assert_eq!(domain_key(&direct), "hba:0000:01:00.0");
        let nv = drive(|d| {
            d.kind = DriveKind::NvmeSsd;
            d.name = "nvme3n1".into();
            d.location = Location::default();
        });
        assert_eq!(hba_key(&nv), "nvme:nvme3");
    }

    #[test]
    fn restart_reattaches_what_the_drive_keeps_doing() {
        let dj = |state, kind| DriveJob {
            drive: DriveId::derive(None, "m", "s"),
            name: "sdb".into(),
            serial: "s".into(),
            wwn: None,
            kind,
            state,
            step: 0,
            phase: String::new(),
            progress_pct: None,
            error: None,
            done: vec![],
            destroy_named: false,
            started: None,
            finished: None,
            ata_password: None,
        };
        assert_eq!(recover_action(&dj(DjState::Running, DriveKind::SasHdd), Some(&FMT)), Recover::Reattach);
        assert_eq!(recover_action(&dj(DjState::Running, DriveKind::NvmeSsd), Some(&Step::Sanitize { method: SanitizeMethod::Crypto })), Recover::Reattach);
        assert!(matches!(recover_action(&dj(DjState::Running, DriveKind::NvmeSsd), Some(&FMT)), Recover::Interrupt(_)), "an NVMe format cannot be watched");
        assert!(matches!(recover_action(&dj(DjState::Running, DriveKind::SasHdd), Some(&part())), Recover::Interrupt(_)));
        assert!(matches!(recover_action(&dj(DjState::Queued, DriveKind::SasHdd), Some(&FMT)), Recover::Interrupt(_)), "never started blind");
        assert_eq!(recover_action(&dj(DjState::Done, DriveKind::SasHdd), None), Recover::Leave);
        // #36: an ATA security erase cannot be watched; the restart says the
        // password the drive may be locked with.
        let se = Step::SecurityErase { enhanced: None };
        let mut locked = dj(DjState::Running, DriveKind::SataHdd);
        locked.ata_password = Some("sd-Xy7".into());
        let Recover::Interrupt(why) = recover_action(&locked, Some(&se)) else { panic!() };
        assert!(why.contains("sd-Xy7") && why.contains("IDENTIFY"), "{why}");
        let Recover::Interrupt(why) = recover_action(&dj(DjState::Running, DriveKind::SataHdd), Some(&se)) else { panic!() };
        assert!(why.contains("before a password was set"), "{why}");
    }

    #[test]
    fn a_test_step_reads_where_the_single_drive_route_reads() {
        use crate::drivetest::TestKind;
        let smoke = Step::Test { kind: TestKind::Smoke };
        let s: Step = serde_json::from_value(json!({ "op": "test", "kind": "read_scan" })).unwrap();
        assert_eq!(s, Step::Test { kind: TestKind::ReadScan });
        assert!(s.low_level() && !s.destroys() && s.read_only());
        assert_eq!(s.describe(), "test (read_scan)");
        let destr = Step::Test { kind: TestKind::DestructiveSample };
        assert!(destr.destroys() && !destr.read_only());
        // A qualify gate: format, test, then partition and enroll.
        assert!(validate_steps(&[FMT, smoke.clone(), part(), enroll()]).is_ok());
        assert!(validate_steps(&[part(), smoke.clone()]).is_err(), "a test after the partition is not a qualify gate");

        let base = Drive::test_fixture("sdc");
        let mut fleet = base.clone();
        fleet.membership = Membership::Fleet;
        assert_eq!(guard(&fleet, std::slice::from_ref(&smoke), false, &cx()), Ok(()), "reading a fleet drive is allowed");
        assert!(guard(&fleet, &[smoke.clone(), part()], false, &cx()).unwrap_err().contains("in the fleet"), "not when the job also writes");
        assert!(guard(&fleet, std::slice::from_ref(&destr), false, &cx()).unwrap_err().contains("in the fleet"));
        let mounted = Context { mounted: true, ..cx() };
        assert_eq!(guard(&base, std::slice::from_ref(&smoke), false, &mounted), Ok(()));
        assert!(guard(&base, std::slice::from_ref(&destr), false, &mounted).is_err());
        let mut reserved = base.clone();
        reserved.designation = Designation::Reserved;
        assert_eq!(guard(&reserved, std::slice::from_ref(&smoke), false, &cx()), Ok(()));
        // In use: reading is fine; the destructive sample needs `destroy`.
        let mut held = base.clone();
        held.in_use_by = Some("stormblock (partition 2)".into());
        assert_eq!(guard(&held, std::slice::from_ref(&smoke), false, &cx()), Ok(()));
        assert!(guard(&held, std::slice::from_ref(&destr), false, &cx()).unwrap_err().contains("destroy"));
        assert_eq!(guard(&held, std::slice::from_ref(&destr), true, &cx()), Ok(()));
        // Nothing to read on a 520-byte drive until it is formatted.
        let mut odd = base.clone();
        odd.block_size = 520;
        odd.usable = false;
        assert!(guard(&odd, std::slice::from_ref(&smoke), false, &cx()).unwrap_err().contains("format step first"));
        assert_eq!(guard(&odd, &[FMT, smoke], false, &cx()), Ok(()));
    }

    #[test]
    fn security_erase_is_a_sata_low_level_step() {
        let s: Step = serde_json::from_value(json!({ "op": "security_erase", "enhanced": true })).unwrap();
        assert_eq!(s, Step::SecurityErase { enhanced: Some(true) });
        let s: Step = serde_json::from_value(json!({ "op": "security_erase" })).unwrap();
        assert_eq!(s, Step::SecurityErase { enhanced: None });
        assert!(s.low_level() && s.destroys());
        assert_eq!(s.describe(), "security erase");
        assert!(validate_steps(&[s.clone(), part()]).is_ok());

        let mut sata = Drive::test_fixture("sdc");
        sata.kind = DriveKind::SataHdd;
        assert_eq!(guard(&sata, std::slice::from_ref(&s), false, &cx()), Ok(()));
        let mut sas = sata.clone();
        sas.kind = DriveKind::SasHdd;
        assert!(guard(&sas, std::slice::from_ref(&s), false, &cx()).unwrap_err().contains("SATA"));
        let mut nvme = sata;
        nvme.kind = DriveKind::NvmeSsd;
        assert!(guard(&nvme, &[s], false, &cx()).unwrap_err().contains("use sanitize"));
    }

    #[test]
    fn a_restart_leaves_no_drive_busy_without_an_owner() {
        use crate::drive::{FirmwareRecord, FormatRecord};
        let running_fmt = |d: &mut Drive| {
            d.activity = Activity::Formatting;
            d.format = Some(FormatRecord { from_block_size: 520, to_block_size: 4096, state: "running".into(), ..Default::default() });
        };
        // A legacy SCSI format: the drive is still formatting — watch it.
        assert_eq!(orphan_action(&drive(running_fmt), false), Orphan::ReattachFormat);
        // The same drive under a running worker step: the worker owns it.
        assert_eq!(orphan_action(&drive(running_fmt), true), Orphan::Leave);
        assert_eq!(orphan_action(&drive(|d| d.activity = Activity::Sanitizing), true), Orphan::Leave);
        // NVMe Format NVM is one blocking command: it cannot be watched.
        let nvme = drive(|d| {
            running_fmt(d);
            d.kind = DriveKind::NvmeSsd;
        });
        assert!(matches!(orphan_action(&nvme, false), Orphan::Idle(_)));
        // Formatting with no running record (e.g. a worker partition step
        // whose job is gone).
        assert!(matches!(orphan_action(&drive(|d| d.activity = Activity::Formatting), false), Orphan::Idle(_)));
        for a in [Activity::Testing, Activity::UpdatingFirmware, Activity::Sanitizing] {
            assert!(matches!(orphan_action(&drive(|d| d.activity = a), false), Orphan::Idle(_)), "{a:?}");
        }
        for a in [Activity::Idle, Activity::Draining, Activity::Missing] {
            assert_eq!(orphan_action(&drive(|d| d.activity = a), false), Orphan::Leave, "{a:?}");
        }

        let mut fw = drive(|d| {
            d.activity = Activity::UpdatingFirmware;
            d.firmware_update = Some(FirmwareRecord { image: "x.lod".into(), state: "running".into(), ..Default::default() });
        });
        idle_orphan(&mut fw, "why");
        assert_eq!(fw.activity, Activity::Idle);
        let r = fw.firmware_update.unwrap();
        assert_eq!((r.state.as_str(), r.error.as_deref()), ("interrupted", Some("why")));
        assert!(r.finished.is_some());

        let mut f = nvme.clone();
        idle_orphan(&mut f, "why");
        assert_eq!((f.activity, f.format.unwrap().state.as_str()), (Activity::Idle, "interrupted"));
        // A finished record is history: left as it was.
        let mut done = drive(|d| {
            d.activity = Activity::Testing;
            d.format = Some(FormatRecord { state: "done".into(), ..Default::default() });
        });
        idle_orphan(&mut done, "why");
        assert_eq!(done.format.unwrap().state, "done");
    }

    /// The table on a file, read back by our parser and — where the build
    /// box has util-linux — by sfdisk, which checks both headers and CRCs.
    #[cfg(unix)]
    #[test]
    fn a_written_table_is_a_table_to_sfdisk() {
        use std::io::Read;
        let path = std::env::temp_dir().join(format!("stormdrive-gpt-{}.img", std::process::id()));
        let cap = 64u64 << 20;
        let f = std::fs::OpenOptions::new().create(true).truncate(true).read(true).write(true).open(&path).unwrap();
        f.set_len(cap).unwrap();
        // An old xfs superblock that must be gone afterwards.
        std::os::unix::fs::FileExt::write_all_at(&f, b"XFSB", 1 << 20).unwrap();
        let l = crate::gpt::layout(cap, 512, crate::gpt::TYPE_SLAB_DATA, "stormblock-data", [7; 16], [9; 16]).unwrap();
        write_layout(&f, cap, &l).unwrap();
        let mut buf = vec![];
        std::fs::File::open(&path).unwrap().read_to_end(&mut buf).unwrap();
        let h = crate::contents::parse_gpt_header(&buf[512..1024]).unwrap();
        let parts = crate::contents::parse_gpt_entries(&buf[1024..1024 + 16384], &h);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].first_lba * 512, 1 << 20);
        assert_eq!(crate::contents::fs_signature(&buf[1 << 20..(1 << 20) + crate::contents::SIGNATURE_BYTES]), None, "old signature cleared");
        match std::process::Command::new("sfdisk").arg("--json").arg(&path).output() {
            Ok(out) if out.status.success() => {
                let v: Value = serde_json::from_slice(&out.stdout).unwrap();
                let t = &v["partitiontable"];
                assert_eq!(t["label"], "gpt");
                let p = &t["partitions"][0];
                assert_eq!(p["start"], 2048);
                assert_eq!(p["type"].as_str().unwrap().to_uppercase(), "7D3E5A91-6C24-4B8F-A05D-2E9147BC6F38");
                assert_eq!(p["name"], "stormblock-data");
                let stderr = String::from_utf8_lossy(&out.stderr);
                assert!(!stderr.to_lowercase().contains("corrupt"), "sfdisk: {stderr}");
                eprintln!("sfdisk read it: {}", String::from_utf8_lossy(&out.stdout));
                let verify = std::process::Command::new("sfdisk").arg("--verify").arg(&path).output().unwrap();
                let said = format!("{}{}", String::from_utf8_lossy(&verify.stdout), String::from_utf8_lossy(&verify.stderr));
                assert!(verify.status.success() && !said.to_lowercase().contains("error:"), "sfdisk --verify: {said}");
                eprintln!("sfdisk --verify: {}", said.trim());
            }
            other => eprintln!("sfdisk not usable here ({:?}); our own parser checked it", other.map(|o| o.status)),
        }
        let _ = std::fs::remove_file(&path);
    }

    /// #72: partitioning a 1 PiB disk writes metadata only — ~8 MiB at the
    /// two ends, counted — and takes no time to speak of.
    #[test]
    fn partitioning_a_pib_disk_writes_only_metadata() {
        struct Counter(std::sync::Mutex<Vec<(u64, u64)>>);
        impl LayoutTarget for Counter {
            fn put(&self, buf: &[u8], offset: u64) -> std::io::Result<()> {
                self.0.lock().unwrap().push((offset, buf.len() as u64));
                Ok(())
            }
            fn sync(&self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let cap = 1u64 << 50;
        let started = std::time::Instant::now();
        let l = crate::gpt::layout(cap, 4096, crate::gpt::TYPE_SLAB_DATA, "stormblock-data", [7; 16], [9; 16]).unwrap();
        let c = Counter(Default::default());
        write_layout(&c, cap, &l).unwrap();
        let writes = c.0.into_inner().unwrap();
        let bytes: u64 = writes.iter().map(|w| w.1).sum();
        assert_eq!(bytes, 2 * CLEAR_BYTES + l.primary.len() as u64 + l.backup.len() as u64);
        assert!(bytes < 9 << 20, "{bytes} bytes written on a 1 PiB disk");
        for (off, len) in &writes {
            assert!(off + len <= CLEAR_BYTES || *off >= cap - CLEAR_BYTES, "write at {off}+{len} is not at an end");
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "took {:?}", started.elapsed());
    }

    #[test]
    fn prep_phases() {
        let u = drive(|d| {
            d.block_size = 520;
            d.usable = false;
        });
        assert_eq!(prep(&u, None)["phase"], "unusable");
        let f = drive(|d| d.activity = Activity::Formatting);
        assert_eq!(prep(&f, Some(40)), json!({"phase": "formatting", "pct": 40}));
        assert_eq!(prep(&drive(|_| {}), Some(40)), json!({"phase": "ready", "pct": null}));
        assert_eq!(prep(&drive(|d| d.membership = Membership::Fleet), None)["phase"], "enrolled");
        assert_eq!(prep(&drive(|d| d.activity = Activity::Sanitizing), Some(7))["pct"], 7);
    }
}
