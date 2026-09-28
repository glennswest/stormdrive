//! The drive model: stable identity, kind, location, state, and health.
//!
//! Identity is the part stormblock got wrong (a fresh v4 UUID on every
//! open): a `DriveId` here is uuid5 of the WWID — or model+serial when the
//! device has no WWID — so it survives reboots, re-opens, and /dev path
//! changes.

use serde::{Deserialize, Serialize};
use std::time::SystemTime;
use uuid::Uuid;

/// Fixed namespace for uuid5 drive-id derivation. Never change this value:
/// every persisted DriveId depends on it.
pub const DRIVE_ID_NS: Uuid = Uuid::from_bytes([
    0x53, 0x74, 0x6f, 0x72, 0x6d, 0x44, 0x72, 0x69, 0x76, 0x65, 0x2e, 0x69, 0x64, 0x2e, 0x76,
    0x31,
]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DriveId(pub Uuid);

impl DriveId {
    /// Derive the stable id. WWID wins when present; model+serial is the
    /// fallback for devices that expose no WWID.
    pub fn derive(wwid: Option<&str>, model: &str, serial: &str) -> Self {
        let key = match wwid {
            Some(w) if !w.trim().is_empty() => w.trim().to_string(),
            _ => format!("{}:{}", model.trim(), serial.trim()),
        };
        DriveId(Uuid::new_v5(&DRIVE_ID_NS, key.as_bytes()))
    }
}

impl std::fmt::Display for DriveId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriveKind {
    NvmeSsd,
    SasSsd,
    SasHdd,
    SataSsd,
    SataHdd,
    Unknown,
}

impl DriveKind {
    /// Default stormblock slab tier for this kind of drive.
    pub fn default_tier(self) -> &'static str {
        match self {
            DriveKind::NvmeSsd => "hot",
            DriveKind::SasSsd | DriveKind::SataSsd => "warm",
            DriveKind::SasHdd | DriveKind::SataHdd => "cool",
            DriveKind::Unknown => "cool",
        }
    }

    pub fn is_ssd(self) -> bool {
        matches!(
            self,
            DriveKind::NvmeSsd | DriveKind::SasSsd | DriveKind::SataSsd
        )
    }
}

/// The HBA a drive hangs off. Multiple controllers per node is the normal
/// case on a shelf rig.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Controller {
    /// hostN (SCSI); the grouping key.
    pub scsi_host: Option<String>,
    pub pcie_addr: Option<String>,
    /// Kernel driver (mpt3sas, nvme, …).
    pub driver: Option<String>,
}

/// A SAS shelf (SES enclosure), e.g. a NetApp DS4246. Identity comes from
/// the SES processor's SCSI device; `serial` (VPD 0x80) is the canonical
/// shelf key — a dual-IOM shelf shows up as two enclosure devices with two
/// ids but one serial.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shelf {
    /// sysfs enclosure id (e.g. "1:0:8:0") — per-IOM, not canonical.
    pub id: Option<String>,
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub sas_address: Option<String>,
    /// SES enclosure logical identifier (page 0x01), lower-case hex — the
    /// shelf's own WWN-like id, identical through every IOM. mpt3sas
    /// prints it as "enclosure logical id" and exposes it as
    /// `enclosure_identifier` on the drive's sas_device.
    #[serde(default)]
    pub logical_id: Option<String>,
}

impl Shelf {
    /// The stable key for grouping: the enclosure logical id, else the
    /// serial, else the sysfs id. On NetApp shelves the SES device's VPD
    /// serial is the IOM's, so the logical id is the one that is truly
    /// per-shelf.
    pub fn key(&self) -> Option<String> {
        self.logical_id
            .clone()
            .or_else(|| self.serial.clone())
            .or_else(|| self.id.clone())
    }

    pub fn display(&self) -> String {
        let tail = self.key();
        match (&self.model, tail) {
            (Some(m), Some(k)) => format!("{m} {k}"),
            (Some(m), None) => m.clone(),
            (None, Some(k)) => k,
            _ => "shelf".into(),
        }
    }
}

/// Where the drive physically is: controller → shelf → bay. Every field
/// optional — populated with whatever the platform exposes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    #[serde(default)]
    pub controller: Option<Controller>,
    #[serde(default)]
    pub shelf: Option<Shelf>,
    pub bay: Option<u32>,
    /// The drive's own SAS address.
    pub sas_address: Option<String>,
    /// The phy its SAS end device is attached through: an expander phy
    /// (a shelf's slot wiring) or an HBA phy when direct-attached.
    #[serde(default)]
    pub sas_phy: Option<u32>,
    /// SAS address of the expander that phy belongs to; None when the
    /// drive hangs off the HBA directly.
    #[serde(default)]
    pub expander: Option<String>,
    /// NVMe drives: the namespace's controller BDF / physical slot.
    pub pcie_addr: Option<String>,
    pub pcie_slot: Option<String>,
}

impl Location {
    /// Failure-domain labels for placement, in stormblock topology shape.
    pub fn labels(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(sh) = &self.shelf {
            if let Some(k) = sh.key() {
                out.push(("shelf".into(), k));
            }
        }
        if let Some(b) = self.bay {
            out.push(("bay".into(), b.to_string()));
        }
        if let Some(c) = &self.controller {
            if let Some(h) = &c.scsi_host {
                out.push(("hba".into(), h.clone()));
            }
        }
        if let Some(s) = &self.pcie_slot {
            out.push(("pcie_slot".into(), s.clone()));
        }
        out
    }

    /// Where a person would walk to, for events: "DS224C 5000… bay 4",
    /// "PCIe slot 3", or "unplaced".
    pub fn place(&self) -> String {
        let mut parts = Vec::new();
        if let Some(sh) = &self.shelf {
            parts.push(sh.display());
        }
        if let Some(b) = self.bay {
            parts.push(format!("bay {b}"));
        }
        if let Some(s) = &self.pcie_slot {
            parts.push(format!("PCIe slot {s}"));
        }
        if parts.is_empty() {
            "unplaced".into()
        } else {
            parts.join(" ")
        }
    }

    /// This rescan's location, merged with what we knew. On the same shelf
    /// (or with no shelf either time) a field the rescan could not read —
    /// a bay from an SES page that failed this pass, the shelf's model —
    /// keeps its known value instead of flapping to unknown; a value that
    /// is present and different wins, which is how a re-bay shows up. A
    /// different shelf is a different place: the rescan is taken whole.
    pub fn refreshed(&self, fresh: Location) -> Location {
        let old_key = self.shelf.as_ref().and_then(|s| s.key());
        let new_key = fresh.shelf.as_ref().and_then(|s| s.key());
        if old_key != new_key {
            return fresh;
        }
        let shelf = match (self.shelf.as_ref(), fresh.shelf) {
            (Some(o), Some(n)) => Some(Shelf {
                id: n.id.or_else(|| o.id.clone()),
                vendor: n.vendor.or_else(|| o.vendor.clone()),
                model: n.model.or_else(|| o.model.clone()),
                serial: n.serial.or_else(|| o.serial.clone()),
                sas_address: n.sas_address.or_else(|| o.sas_address.clone()),
                logical_id: n.logical_id.or_else(|| o.logical_id.clone()),
            }),
            (_, n) => n,
        };
        Location {
            controller: fresh.controller.or_else(|| self.controller.clone()),
            shelf,
            bay: fresh.bay.or(self.bay),
            sas_address: fresh.sas_address.or_else(|| self.sas_address.clone()),
            sas_phy: fresh.sas_phy.or(self.sas_phy),
            expander: fresh.expander.or_else(|| self.expander.clone()),
            pcie_addr: fresh.pcie_addr.or_else(|| self.pcie_addr.clone()),
            pcie_slot: fresh.pcie_slot.or_else(|| self.pcie_slot.clone()),
        }
    }

    /// The bay as a key, for "is this drive in the same bay as that one":
    /// shelf + bay, or the PCIe slot. None when the drive has no bay.
    pub fn bay_key(&self) -> Option<String> {
        match (self.bay, &self.pcie_slot) {
            (Some(b), _) => Some(format!("{}/bay/{b}", self.shelf.as_ref().and_then(|s| s.key()).unwrap_or_default())),
            (None, Some(s)) => Some(format!("slot/{s}")),
            _ => None,
        }
    }

    /// Did the drive physically move (or get placed for the first time)
    /// between `self` and `now`? Shelf, bay, HBA and PCIe slot — the
    /// failure-domain labels — are what count; detail filling in is not a
    /// move.
    pub fn moved_to(&self, now: &Location) -> bool {
        self.labels() != now.labels()
    }
}

/// Is the drive handed to stormblock? Orthogonal to designation and
/// activity: a spare or even an operator-failed drive can still be in the
/// fleet (until drained), and a reserved drive can sit out of it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Membership {
    /// Not registered with stormblock.
    #[default]
    Out,
    /// Registered with stormblock (in its drive list and/or carrying a slab).
    Fleet,
}

/// Operator-set label. Applies both in fleet and out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Designation {
    #[default]
    None,
    /// Held back on purpose — not to be joined or consumed.
    Reserved,
    /// Standing by as a replacement.
    Spare,
    /// Operator declared it bad (health can also conclude this on its own).
    Failed,
}

/// May the slabs on this drive promise more than they hold (#13)? Off by
/// default: thin clones may not promise more than the drive's slab space.
/// On, `ratio` bounds it (2.0 = promise up to twice). stormdrive holds the
/// setting; stormblock enforces it when a claim binds (stormblock#152).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Overcommit {
    pub enabled: bool,
    pub ratio: f64,
}

/// The largest ratio accepted — a guard against a typo (20 for 2.0), not
/// a policy.
pub const MAX_OVERCOMMIT_RATIO: f64 = 16.0;

impl Default for Overcommit {
    fn default() -> Self {
        Overcommit { enabled: false, ratio: 1.0 }
    }
}

impl Overcommit {
    /// A checked setting. Off always stores ratio 1.0; on needs a ratio in
    /// 1.0..=16.0.
    pub fn new(enabled: bool, ratio: Option<f64>) -> Result<Self, String> {
        if !enabled {
            return Ok(Overcommit::default());
        }
        let ratio = ratio.ok_or("overcommit on needs a ratio (e.g. 2.0)")?;
        if !ratio.is_finite() || !(1.0..=MAX_OVERCOMMIT_RATIO).contains(&ratio) {
            return Err(format!("overcommit ratio {ratio} is outside 1.0..={MAX_OVERCOMMIT_RATIO}"));
        }
        Ok(Overcommit { enabled, ratio })
    }

    /// What one byte of slab space may promise: the ratio when on, else 1.
    pub fn factor(&self) -> f64 {
        if self.enabled { self.ratio } else { 1.0 }
    }

    /// For people: `off` or `2×`.
    pub fn word(&self) -> String {
        if self.enabled { format!("{}×", self.ratio) } else { "off".into() }
    }
}

/// What the drive is doing right now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    #[default]
    Idle,
    /// A drive test is running (see test.rs).
    Testing,
    /// Being evacuated ahead of removal (needs stormblock#70 to automate).
    Draining,
    /// A low-level format is running: SCSI FORMAT UNIT (format.rs) or NVMe
    /// Format NVM, or the drive worker is writing a partition table (#5).
    Formatting,
    /// A sanitize (block / crypto / overwrite erase) is running (#5).
    Sanitizing,
    /// A firmware image is being downloaded/activated (firmware.rs).
    UpdatingFirmware,
    /// Inventory remembers it; the node cannot see it.
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Unknown,
    Good,
    Warning,
    Failing,
    Failed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HealthReport {
    pub status: Option<HealthStatus>,
    pub temperature_c: Option<i32>,
    pub power_on_hours: Option<u64>,
    pub media_errors: u64,
    pub available_spare_pct: Option<u8>,
    /// NVMe percentage-used / SSD endurance-used. May exceed 100 per spec.
    pub wear_pct: Option<u8>,
    /// NVMe critical-warning bitfield; 0 for non-NVMe.
    pub critical_warning: u8,
    pub messages: Vec<String>,
    pub collected_at: Option<SystemTime>,
}

impl HealthReport {
    pub fn status(&self) -> HealthStatus {
        self.status.unwrap_or(HealthStatus::Unknown)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Drive {
    pub id: DriveId,
    /// Primary /dev node. May change across boots; `id` does not.
    pub path: String,
    /// Kernel name of the primary path (sda, nvme0n1).
    pub name: String,
    /// Every /dev node this physical drive answers on. Dual-IOM shelves
    /// present two; `path` is always the first (sorted). Includes `path`.
    #[serde(default)]
    pub paths: Vec<String>,
    pub kind: DriveKind,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub wwid: Option<String>,
    pub capacity_bytes: u64,
    /// Logical block length as the drive reports it (READ CAPACITY), not
    /// as the kernel exposes it — 520 on a NetApp-formatted drive.
    pub block_size: u32,
    #[serde(default)]
    pub physical_block_size: u32,
    /// The kernel accepted the sector size and exposes the capacity. False
    /// for 520/528-byte drives: the block node exists with 0 blocks and no
    /// I/O is possible until a reformat.
    #[serde(default = "default_true")]
    pub usable: bool,
    /// Whose data is on the drive, read off its sectors: a stormblock slab
    /// (the node's own system disk on stormcos). Join, format and the
    /// destructive test refuse such a drive whatever its membership says.
    #[serde(default)]
    pub in_use_by: Option<String>,
    /// The last sector-size reformat of this drive, persisted.
    #[serde(default)]
    pub format: Option<FormatRecord>,
    /// The last firmware update of this drive, persisted.
    #[serde(default)]
    pub firmware_update: Option<FirmwareRecord>,
    #[serde(default)]
    pub location: Location,
    #[serde(default)]
    pub membership: Membership,
    #[serde(default)]
    pub designation: Designation,
    /// Operator-set, like designation (#13).
    #[serde(default)]
    pub overcommit: Overcommit,
    #[serde(default)]
    pub activity: Activity,
    #[serde(default)]
    pub health: HealthReport,
    pub first_seen: SystemTime,
    pub last_seen: SystemTime,
    /// The labels stormblock currently holds for this drive, so a change of
    /// location is pushed once rather than every tick.
    #[serde(default)]
    pub pushed_labels: Vec<(String, String)>,
    /// The health state last reported to stormblock, so a report is sent
    /// on change rather than every poll.
    #[serde(default)]
    pub pushed_health: Option<String>,
    /// The overcommit setting stormblock last accepted, so it is pushed
    /// on change rather than every tick. Not persisted: a restart pushes
    /// it once more, in case the engine forgot it.
    #[serde(skip)]
    pub pushed_overcommit: Option<Overcommit>,
    /// A drain in progress or finished, as stormblock last reported it.
    #[serde(default)]
    pub drain: Option<DrainRecord>,
    /// The missing drive this one took the bay of (#15): the replace half
    /// of the failure workflow, found by bay, not by anyone typing it in.
    #[serde(default)]
    pub replaces: Option<DriveId>,
    /// Capacity, the stormblock slabs on the drive and how much is left
    /// (#12). None until stormblock's slab listing has answered once.
    #[serde(default)]
    pub usage: Option<crate::usage::Usage>,
    /// The drive worker enrolled this drive through a partition (#5): the
    /// partition number stormblock holds, instead of the whole disk. Kept
    /// as a number so a renamed disk (`sdb` → `sdc`) still resolves.
    #[serde(default)]
    pub fleet_partition: Option<u32>,
}

impl Drive {
    /// The path stormblock knows this drive by: its enrolled partition, or
    /// the whole disk.
    pub fn stormblock_path(&self) -> String {
        match self.fleet_partition {
            Some(n) => format!("/dev/{}", crate::gpt::partition_name(&self.name, n)),
            None => self.path.clone(),
        }
    }
}

fn default_true() -> bool {
    true
}

/// Sector sizes Linux and stormblock can use.
pub const USABLE_BLOCK_SIZES: [u32; 2] = [512, 4096];

/// The last FORMAT UNIT we ran on a drive.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatRecord {
    pub from_block_size: u32,
    pub to_block_size: u32,
    /// `running`, `done`, `failed`.
    pub state: String,
    pub started: Option<SystemTime>,
    pub finished: Option<SystemTime>,
    #[serde(default)]
    pub error: Option<String>,
}

/// The last firmware update we ran on a drive.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareRecord {
    pub image: String,
    pub from_version: String,
    pub to_version: Option<String>,
    /// `running`, `done`, `failed`.
    pub state: String,
    pub started: Option<SystemTime>,
    pub finished: Option<SystemTime>,
    #[serde(default)]
    pub error: Option<String>,
    /// The image is committed but takes effect at the next reset/power
    /// cycle (NVMe CA=1, or a drive that reports so).
    #[serde(default)]
    pub reset_required: bool,
}

/// What we know about a drain of this drive.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainRecord {
    /// `running`, `empty`, `stuck`, `cancelled`.
    pub state: String,
    pub moved: u64,
    pub failed: u64,
    pub remaining: u64,
    #[serde(default)]
    pub errors: Vec<String>,
    /// Why it was started: `health`, `operator`, `leave`.
    #[serde(default)]
    pub reason: String,
    /// Whether the drive should leave the fleet once the drain is empty.
    #[serde(default)]
    pub then_leave: bool,
}

impl Drive {
    /// The labels stormblock should hold for this drive right now.
    pub fn stormblock_labels(&self) -> Vec<(String, String)> {
        self.location.labels()
    }

    /// The health word stormblock understands for our current status.
    /// Warning is `healthy` to the engine — a warm drive is still a good
    /// place for data; only Failing and Failed change placement.
    pub fn stormblock_health(&self) -> &'static str {
        match self.health.status() {
            HealthStatus::Failed => "failed",
            HealthStatus::Failing => "failing",
            _ => "healthy",
        }
    }

    /// Why this drive cannot join the fleet right now, if it can't.
    pub fn fleet_join_blocker(&self) -> Option<String> {
        if self.membership == Membership::Fleet {
            return Some("already in the fleet".into());
        }
        if self.designation == Designation::Failed {
            return Some("designated failed".into());
        }
        if self.designation == Designation::Reserved {
            return Some("designated reserved".into());
        }
        if self.activity != Activity::Idle {
            return Some(format!("activity is {:?}", self.activity));
        }
        if self.health.status() >= HealthStatus::Failing {
            return Some(format!("health is {:?}", self.health.status()));
        }
        if let Some(who) = &self.in_use_by {
            return Some(format!("in use: holds data for {who}"));
        }
        if !self.usable {
            return Some(format!(
                "kernel cannot use {}-byte sectors — reformat to 4096 first",
                self.block_size
            ));
        }
        None
    }

    /// Destructive tests are only allowed on drives that are out of the
    /// fleet and present.
    pub fn destructive_test_blocker(&self) -> Option<String> {
        if self.membership == Membership::Fleet {
            return Some("in the fleet — destructive tests need an out-of-fleet drive".into());
        }
        if let Some(who) = &self.in_use_by {
            return Some(format!("in use: holds data for {who}"));
        }
        if self.activity != Activity::Idle {
            return Some(format!("activity is {:?}", self.activity));
        }
        if !self.usable {
            return Some(format!(
                "kernel cannot use {}-byte sectors — reformat first",
                self.block_size
            ));
        }
        None
    }

    /// The kernel refuses this sector size; a reformat to 512/4096 is the
    /// only way in.
    pub fn needs_reformat(&self) -> bool {
        !USABLE_BLOCK_SIZES.contains(&self.block_size) || !self.usable
    }

    /// Does a reset of this drive interrupt live data — a fleet drive, or
    /// one stormblock serves from anyway (the system disk)? Such drives
    /// take firmware one at a time.
    pub fn serves_data(&self) -> bool {
        self.membership == Membership::Fleet || self.in_use_by.is_some()
    }

    /// Why a firmware update cannot start now. In-fleet drives are allowed
    /// (deferred activation, the drive resets once) but the caller
    /// serialises them; a Failing/Failed drive is not worth the risk
    /// unless the operator forces it.
    pub fn firmware_blocker(&self, force: bool) -> Option<String> {
        if self.activity == Activity::UpdatingFirmware {
            return Some("a firmware update is already running".into());
        }
        if self.activity != Activity::Idle {
            return Some(format!("activity is {:?}", self.activity));
        }
        if !force && self.health.status() >= HealthStatus::Failing {
            return Some(format!("health is {:?} (pass force to override)", self.health.status()));
        }
        None
    }

    /// Why a sector-size reformat cannot start now. Reformatting is the
    /// most destructive thing this daemon does: out of the fleet, idle,
    /// present, and not the operator's reserved drive.
    pub fn format_blocker(&self) -> Option<String> {
        if self.membership == Membership::Fleet {
            return Some("in the fleet — leave (drain) first".into());
        }
        if let Some(who) = &self.in_use_by {
            return Some(format!("in use: holds data for {who}"));
        }
        if self.activity == Activity::Formatting {
            return Some("a format is already running".into());
        }
        if self.activity != Activity::Idle {
            return Some(format!("activity is {:?}", self.activity));
        }
        if self.designation == Designation::Reserved {
            return Some("designated reserved".into());
        }
        if self.kind == DriveKind::NvmeSsd {
            return Some("NVMe: use namespace format (not implemented)".into());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_id_is_stable_and_prefers_wwid() {
        let a = DriveId::derive(Some("naa.5000c500a1b2c3d4"), "X", "Y");
        let b = DriveId::derive(Some("naa.5000c500a1b2c3d4"), "OTHER", "OTHER");
        assert_eq!(a, b, "wwid alone determines the id");

        let c = DriveId::derive(None, "Micron_7450", "S1234");
        let d = DriveId::derive(None, "Micron_7450", "S1234");
        let e = DriveId::derive(None, "Micron_7450", "S9999");
        assert_eq!(c, d);
        assert_ne!(c, e);
    }

    #[test]
    fn drive_id_ignores_blank_wwid_and_whitespace() {
        let a = DriveId::derive(Some("  "), "M", "S");
        let b = DriveId::derive(None, " M ", " S ");
        assert_eq!(a, b);
    }

    #[test]
    fn default_tiers() {
        assert_eq!(DriveKind::NvmeSsd.default_tier(), "hot");
        assert_eq!(DriveKind::SasSsd.default_tier(), "warm");
        assert_eq!(DriveKind::SataHdd.default_tier(), "cool");
    }

    #[test]
    fn health_status_orders_by_severity() {
        assert!(HealthStatus::Failed > HealthStatus::Failing);
        assert!(HealthStatus::Failing > HealthStatus::Warning);
        assert!(HealthStatus::Warning > HealthStatus::Good);
    }

    fn base_drive() -> Drive {
        Drive {
            id: DriveId::derive(None, "M", "S"),
            path: "/dev/sdx".into(),
            name: "sdx".into(),
            paths: vec!["/dev/sdx".into()],
            kind: DriveKind::SataSsd,
            model: "M".into(),
            serial: "S".into(),
            firmware: "1".into(),
            wwid: None,
            capacity_bytes: 1 << 30,
            block_size: 512,
            physical_block_size: 512,
            usable: true,
            in_use_by: None,
            format: None,
            firmware_update: None,
            location: Location::default(),
            membership: Membership::Out,
            designation: Designation::None,
            overcommit: Default::default(),
            activity: Activity::Idle,
            health: HealthReport::default(),
            first_seen: SystemTime::now(),
            last_seen: SystemTime::now(),
            pushed_labels: Vec::new(),
            pushed_health: None,
            pushed_overcommit: None,
            replaces: None,
            drain: None,
            usage: None,
            fleet_partition: None,
        }
    }

    #[test]
    fn bay_keys_name_a_bay_on_a_shelf_or_a_pcie_slot() {
        let shelf = Shelf { logical_id: Some("5000abc".into()), ..Default::default() };
        let l = Location { shelf: Some(shelf), bay: Some(7), ..Default::default() };
        assert_eq!(l.bay_key().as_deref(), Some("5000abc/bay/7"));
        let l = Location { pcie_slot: Some("142".into()), ..Default::default() };
        assert_eq!(l.bay_key().as_deref(), Some("slot/142"));
        assert_eq!(Location::default().bay_key(), None);
    }

    #[test]
    fn overcommit_is_off_by_default_and_checked_when_on() {
        assert_eq!(base_drive().overcommit, Overcommit { enabled: false, ratio: 1.0 });
        let old: Drive = serde_json::from_value({
            let mut v = serde_json::to_value(base_drive()).unwrap();
            v.as_object_mut().unwrap().remove("overcommit");
            v
        })
        .unwrap();
        assert!(!old.overcommit.enabled, "an inventory from before #13 loads as off");

        assert_eq!(Overcommit::new(false, Some(3.0)).unwrap(), Overcommit::default(), "off drops the ratio");
        assert_eq!(Overcommit::new(true, Some(2.0)).unwrap().factor(), 2.0);
        assert_eq!(Overcommit::new(true, Some(1.0)).unwrap().word(), "1×");
        assert!(Overcommit::new(true, None).is_err());
        for bad in [0.5, 16.5, f64::NAN, f64::INFINITY] {
            assert!(Overcommit::new(true, Some(bad)).is_err(), "{bad}");
        }
        assert_eq!(Overcommit::default().factor(), 1.0);
        assert_eq!(Overcommit::default().word(), "off");
    }

    #[test]
    fn unusable_sector_size_blocks_join_and_destructive_but_not_format() {
        let mut d = base_drive();
        d.block_size = 520;
        d.usable = false;
        assert!(d.needs_reformat());
        assert!(d.fleet_join_blocker().unwrap().contains("520"));
        assert!(d.destructive_test_blocker().is_some());
        assert!(d.format_blocker().is_none(), "the whole point: a 520 drive can be formatted");
        d.block_size = 4096;
        d.usable = true;
        assert!(!d.needs_reformat());
        assert!(d.format_blocker().is_none(), "a 512/4096 drive may still be reformatted");
    }

    #[test]
    fn firmware_blockers() {
        let mut d = base_drive();
        assert!(d.firmware_blocker(false).is_none());
        d.membership = Membership::Fleet;
        assert!(d.firmware_blocker(false).is_none(), "fleet drives may be updated (serialised by the engine)");
        d.health.status = Some(HealthStatus::Failing);
        assert!(d.firmware_blocker(false).is_some());
        assert!(d.firmware_blocker(true).is_none(), "force overrides the health gate");
        let mut d = base_drive();
        d.activity = Activity::Formatting;
        assert!(d.firmware_blocker(true).is_some());
        d.activity = Activity::UpdatingFirmware;
        assert!(d.firmware_blocker(true).unwrap().contains("already"));
    }

    #[test]
    fn format_blockers() {
        let mut d = base_drive();
        d.membership = Membership::Fleet;
        assert!(d.format_blocker().is_some());
        let mut d = base_drive();
        d.activity = Activity::Formatting;
        assert!(d.format_blocker().unwrap().contains("already"));
        let mut d = base_drive();
        d.activity = Activity::Missing;
        assert!(d.format_blocker().is_some());
        let mut d = base_drive();
        d.designation = Designation::Reserved;
        assert!(d.format_blocker().is_some());
        d.designation = Designation::Failed;
        assert!(d.format_blocker().is_none(), "an operator-failed drive may be reformatted (out of fleet)");
        let mut d = base_drive();
        d.kind = DriveKind::NvmeSsd;
        assert!(d.format_blocker().is_some());
    }

    /// An inventory written before the sector-format fields existed loads
    /// with usable=true, so a known-good 512 drive is not suddenly
    /// flagged.
    #[test]
    fn old_inventory_defaults_usable() {
        let d = base_drive();
        let mut v = serde_json::to_value(&d).unwrap();
        let o = v.as_object_mut().unwrap();
        o.remove("physical_block_size");
        o.remove("usable");
        o.remove("format");
        let back: Drive = serde_json::from_value(v).unwrap();
        assert!(back.usable);
        assert_eq!(back.physical_block_size, 0);
        assert!(!back.needs_reformat());
    }

    #[test]
    fn fleet_join_blockers() {
        assert!(base_drive().fleet_join_blocker().is_none());

        let mut d = base_drive();
        d.membership = Membership::Fleet;
        assert!(d.fleet_join_blocker().is_some());

        let mut d = base_drive();
        d.designation = Designation::Failed;
        assert!(d.fleet_join_blocker().is_some());
        d.designation = Designation::Reserved;
        assert!(d.fleet_join_blocker().is_some());
        d.designation = Designation::Spare;
        assert!(d.fleet_join_blocker().is_none(), "a spare may be pressed into service");

        let mut d = base_drive();
        d.activity = Activity::Testing;
        assert!(d.fleet_join_blocker().is_some());

        let mut d = base_drive();
        d.health.status = Some(HealthStatus::Failing);
        assert!(d.fleet_join_blocker().is_some());
        d.health.status = Some(HealthStatus::Warning);
        assert!(d.fleet_join_blocker().is_none());
    }

    #[test]
    fn destructive_test_blockers() {
        assert!(base_drive().destructive_test_blocker().is_none());
        let mut d = base_drive();
        d.membership = Membership::Fleet;
        assert!(d.destructive_test_blocker().is_some());
        let mut d = base_drive();
        d.activity = Activity::Missing;
        assert!(d.destructive_test_blocker().is_some());
    }

    /// The R230 (#2): the system disk is out of the fleet and has nothing
    /// in /proc/mounts, yet stormblock serves the root filesystem from it.
    #[test]
    fn a_drive_holding_stormblock_slabs_refuses_destruction() {
        let mut d = base_drive();
        d.in_use_by = Some("stormblock (slabs in partitions 2 'data', 3 'system')".into());
        assert_eq!(d.membership, Membership::Out);
        for why in [d.fleet_join_blocker(), d.destructive_test_blocker(), d.format_blocker()] {
            assert!(why.unwrap().contains("stormblock"));
        }
        assert!(d.firmware_blocker(false).is_none(), "firmware is what this node needs");
        assert!(d.serves_data(), "…one data-serving drive at a time");
        assert!(!base_drive().serves_data());
    }

    #[test]
    fn location_labels() {
        let loc = Location {
            shelf: Some(Shelf {
                id: Some("1:0:8:0".into()),
                serial: Some("SHFSN1".into()),
                model: Some("DS4246".into()),
                ..Default::default()
            }),
            bay: Some(7),
            controller: Some(Controller {
                scsi_host: Some("host7".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let labels = loc.labels();
        assert!(labels.contains(&("shelf".into(), "SHFSN1".into())), "serial beats sysfs id");
        assert!(labels.contains(&("bay".into(), "7".into())));
        assert!(labels.contains(&("hba".into(), "host7".into())));
    }

    #[test]
    fn shelf_key_prefers_logical_id_then_serial_and_display_reads_well() {
        let sh = Shelf {
            id: Some("1:0:8:0".into()),
            serial: Some("SN9".into()),
            model: Some("DS4246".into()),
            ..Default::default()
        };
        assert_eq!(sh.key(), Some("SN9".into()));
        assert_eq!(sh.display(), "DS4246 SN9");
        let with_id = Shelf {
            logical_id: Some("500a09800e359135".into()),
            ..sh.clone()
        };
        assert_eq!(with_id.key(), Some("500a09800e359135".into()), "logical id beats the IOM serial");
        assert_eq!(with_id.display(), "DS4246 500a09800e359135");
        let bare = Shelf {
            id: Some("1:0:8:0".into()),
            ..Default::default()
        };
        assert_eq!(bare.key(), Some("1:0:8:0".into()));
        assert_eq!(bare.display(), "1:0:8:0");
    }

    /// The engine hears Failing/Failed; a warm or worn-but-working drive is
    /// still `healthy` to placement.
    #[test]
    fn stormblock_health_word_only_changes_placement_for_failing_and_failed() {
        let mut d = base_drive();
        for (st, want) in [
            (HealthStatus::Unknown, "healthy"),
            (HealthStatus::Good, "healthy"),
            (HealthStatus::Warning, "healthy"),
            (HealthStatus::Failing, "failing"),
            (HealthStatus::Failed, "failed"),
        ] {
            d.health.status = Some(st);
            assert_eq!(d.stormblock_health(), want, "{st:?}");
        }
    }

    /// Labels sent to stormblock are the resolved location, in the engine's
    /// rung vocabulary.
    #[test]
    fn stormblock_labels_are_the_location_chain() {
        let mut d = base_drive();
        d.location.bay = Some(7);
        d.location.controller = Some(Controller { scsi_host: Some("host3".into()), ..Default::default() });
        let labels = d.stormblock_labels();
        assert!(labels.contains(&("bay".to_string(), "7".to_string())));
        assert!(labels.contains(&("hba".to_string(), "host3".to_string())));
        assert!(d.pushed_labels.is_empty(), "nothing pushed until the loop runs");
    }

    /// A drain record round-trips through the inventory file.
    #[test]
    fn drain_record_persists() {
        let mut d = base_drive();
        d.drain = Some(DrainRecord { state: "running".into(), moved: 3, failed: 0, remaining: 9, errors: vec![], reason: "health".into(), then_leave: true });
        let json = serde_json::to_string(&d).unwrap();
        let back: Drive = serde_json::from_str(&json).unwrap();
        assert_eq!(back.drain, d.drain);
        // An inventory written before these fields existed still loads.
        let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
        v.as_object_mut().unwrap().remove("drain");
        v.as_object_mut().unwrap().remove("pushed_labels");
        v.as_object_mut().unwrap().remove("pushed_health");
        let old: Drive = serde_json::from_value(v).unwrap();
        assert!(old.drain.is_none() && old.pushed_labels.is_empty());
    }

    fn shelf_bay(key: &str, model: Option<&str>, bay: Option<u32>) -> Location {
        Location {
            shelf: Some(Shelf {
                logical_id: Some(key.into()),
                model: model.map(Into::into),
                ..Default::default()
            }),
            bay,
            ..Default::default()
        }
    }

    #[test]
    fn a_rescan_that_loses_detail_keeps_the_known_place() {
        let known = shelf_bay("5000a098", Some("DS224C"), Some(4));
        // SES failed this pass: same shelf, no model, no bay.
        let now = known.refreshed(shelf_bay("5000a098", None, None));
        assert_eq!(now, known);
        assert!(!known.moved_to(&now));
    }

    #[test]
    fn a_rebay_and_a_new_shelf_are_moves() {
        let known = shelf_bay("5000a098", Some("DS224C"), Some(4));
        let rebay = known.refreshed(shelf_bay("5000a098", None, Some(9)));
        assert_eq!(rebay.bay, Some(9));
        assert_eq!(rebay.shelf.as_ref().unwrap().model.as_deref(), Some("DS224C"));
        assert!(known.moved_to(&rebay));

        let other = known.refreshed(shelf_bay("5000b111", None, None));
        assert_eq!(other, shelf_bay("5000b111", None, None), "a new shelf is taken whole");
        assert!(known.moved_to(&other));

        let placed = Location::default().refreshed(known.clone());
        assert!(Location::default().moved_to(&placed));
        assert_eq!(placed.place(), "DS224C 5000a098 bay 4");
        assert_eq!(Location::default().place(), "unplaced");
    }
}
