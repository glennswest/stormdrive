//! The RAID controller itself, from the node (#132; owner: "hba is
//! mandatory").
//!
//! forge1's PERC H730 Mini stayed in RAID mode with three drives **Failed,
//! 0 KB** after their formats were cut by a power-off; neither the setup menu
//! nor the iDRAC would act on them. Dell's fix is from the OS: each Failed
//! drive set good, then the controller's personality to HBA (a reboot applies
//! it). stormdrive does both:
//!
//! - **Reads and drive state changes are MFI DCMDs** over the driver's char
//!   device (as [`crate::megaraid`]): `MR_DCMD_CTRL_GET_INFO` for model,
//!   serial, firmware, virtual disks and the JBOD switch; `MR_DCMD_PD_GET_INFO`
//!   for each disk's firmware state; `MFI_DCMD_PD_STATE_SET` to make a drive
//!   *unconfigured good* (FreeBSD mfiutil's `good`) or *JBOD* (`syspd`: Linux
//!   sees it with no personality change). Layouts: Linux
//!   `drivers/scsi/megaraid/megaraid_sas.h`, FreeBSD `sys/dev/mfi/mfireg.h`.
//! - **The personality is perccli's.** No public header names the DCMD that
//!   changes it (they carry only the `supportPersonalityChange` bits), so it
//!   is not guessed: stormdrive runs `perccli /cN set personality=HBA` when the
//!   binary is there (`[controllers] perccli`) and says it is not otherwise.
//!
//! A controller is in RAID mode by perccli's word, or — without perccli — when
//! it has virtual disks or a disk in a state only RAID mode has (unconfigured,
//! hot spare, failed, online, …: in HBA mode every disk is a plain device).
//! RAID mode is a finding on every node.
//!
//! Everything but the ioctl and the process is portable and tested against a
//! fake firmware and a fake perccli.

use crate::drive::{Drive, Membership};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// MR_DCMD_CTRL_GET_INFO: struct megasas_ctrl_info.
pub const DCMD_CTRL_INFO: u32 = 0x0101_0000;
/// MFI_DCMD_PD_STATE_SET: mailbox = device id, sequence number, new state.
pub const DCMD_PD_STATE_SET: u32 = 0x0203_0100;
/// sizeof(struct megasas_ctrl_info).
pub const CTRL_INFO_LEN: usize = 2048;

// struct megasas_ctrl_info offsets (packed).
const CI_PRODUCT_NAME: usize = 0x540; // char[80]
const CI_SERIAL: usize = 0x590; // char[32]
const CI_LD_PRESENT: usize = 0x5c0; // u16
const CI_PD_DISK_FAILED: usize = 0x5cc; // u16
const CI_PROPERTIES: usize = 0x600; // struct megasas_ctrl_prop, 64 bytes
const CI_PACKAGE_VERSION: usize = 0x640; // char[0x60]
/// OnOffProperties in megasas_ctrl_prop; enableJBOD is bit 13.
const PROP_ON_OFF: usize = 32;
const ENABLE_JBOD_BIT: u32 = 1 << 13;

// struct MR_PD_INFO offsets (packed).
const PD_SEQ: usize = 2;
const PD_MEDIA_ERR: usize = 168;
const PD_OTHER_ERR: usize = 172;
const PD_PRED_FAIL: usize = 176;
const PD_FW_STATE: usize = 184;
const PD_SAS_ADDR: usize = 200; // u64[2]
const PD_COERCED: usize = 248; // u64, 512-byte blocks
const PD_ENCL_DEVICE: usize = 256;
const PD_SLOT: usize = 259;

pub const STATE_UNCONFIGURED_GOOD: u16 = 0x00;
pub const STATE_UNCONFIGURED_BAD: u16 = 0x01;
pub const STATE_HOT_SPARE: u16 = 0x02;
pub const STATE_OFFLINE: u16 = 0x10;
pub const STATE_FAILED: u16 = 0x11;
pub const STATE_REBUILD: u16 = 0x14;
pub const STATE_ONLINE: u16 = 0x18;
pub const STATE_COPYBACK: u16 = 0x20;
/// "System": a JBOD / non-RAID disk, the one state the OS sees directly.
pub const STATE_JBOD: u16 = 0x40;

/// perccli's word for a firmware state (`/cN/eX/sY show`).
pub fn state_name(code: u16) -> &'static str {
    match code {
        STATE_UNCONFIGURED_GOOD => "unconfigured_good",
        STATE_UNCONFIGURED_BAD => "unconfigured_bad",
        STATE_HOT_SPARE => "hot_spare",
        STATE_OFFLINE => "offline",
        STATE_FAILED => "failed",
        STATE_REBUILD => "rebuild",
        STATE_ONLINE => "online",
        STATE_COPYBACK => "copyback",
        STATE_JBOD => "jbod",
        _ => "unknown",
    }
}

/// A state the controller only has in RAID mode: in HBA mode every disk is a
/// plain device (JBOD).
pub fn raid_only(code: u16) -> bool {
    code != STATE_JBOD
}

/// What an MFI status byte means (MFI_STAT_*, FreeBSD mfireg.h).
pub fn mfi_status_name(st: u8) -> &'static str {
    match st {
        0x00 => "ok",
        0x01 => "invalid command",
        0x02 => "invalid DCMD",
        0x03 => "invalid parameter",
        0x04 => "invalid sequence number",
        0x0b => "configuration resource conflict",
        0x0c => "device not found",
        0x23 => "not found",
        0x25 => "PD clear in progress",
        0x26 => "PD type wrong",
        0x32 => "wrong state",
        _ => "see MFI_STAT_*",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Raid,
    Hba,
    /// The older "JBOD personality" some MegaRAIDs have.
    Jbod,
    Unknown,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        let u = s.to_ascii_uppercase();
        if u.contains("HBA") {
            Some(Mode::Hba)
        } else if u.contains("JBOD") {
            Some(Mode::Jbod)
        } else if u.contains("RAID") {
            Some(Mode::Raid)
        } else {
            None
        }
    }
    pub fn perccli_word(self) -> &'static str {
        match self {
            Mode::Hba => "HBA",
            Mode::Jbod => "JBOD",
            Mode::Raid => "RAID",
            Mode::Unknown => "unknown",
        }
    }
}

/// The parts of struct megasas_ctrl_info stormdrive reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CtrlInfo {
    pub model: Option<String>,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    pub virtual_disks: u16,
    pub failed_disks: u16,
    pub jbod_enabled: bool,
}

fn text(raw: &[u8], at: usize, len: usize) -> Option<String> {
    raw.get(at..at + len)
        .map(|b| {
            let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            String::from_utf8_lossy(&b[..end]).trim().to_string()
        })
        .filter(|s| !s.is_empty())
}

fn le16(raw: &[u8], at: usize) -> Option<u16> {
    raw.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
}

fn le32(raw: &[u8], at: usize) -> Option<u32> {
    raw.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn le64(raw: &[u8], at: usize) -> Option<u64> {
    raw.get(at..at + 8).map(|b| u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

pub fn parse_ctrl_info(raw: &[u8]) -> Option<CtrlInfo> {
    if raw.len() < CI_PACKAGE_VERSION + 0x60 {
        return None;
    }
    Some(CtrlInfo {
        model: text(raw, CI_PRODUCT_NAME, 80),
        serial: text(raw, CI_SERIAL, 32),
        firmware: text(raw, CI_PACKAGE_VERSION, 0x60),
        virtual_disks: le16(raw, CI_LD_PRESENT)?,
        failed_disks: le16(raw, CI_PD_DISK_FAILED)?,
        jbod_enabled: le32(raw, CI_PROPERTIES + PROP_ON_OFF)? & ENABLE_JBOD_BIT != 0,
    })
}

/// One disk as the controller sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdStatus {
    pub device_id: u16,
    /// The sequence number a state change must name (MR_PD_REF.seqNum).
    pub seq: u16,
    #[serde(default)]
    pub enclosure: Option<u16>,
    #[serde(default)]
    pub slot: Option<u8>,
    /// perccli's word: `failed`, `unconfigured_good`, `jbod`, `online`, …
    pub state: String,
    pub state_code: u16,
    #[serde(default)]
    pub vendor: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub sas_address: Option<String>,
    pub media_errors: u32,
    pub other_errors: u32,
    pub predictive_failures: u32,
    /// What the controller offers of it; 0 for forge1's Failed drives.
    pub size_bytes: u64,
    /// The inventory drive this is, when one matches (SAS address, else
    /// the controller and slot).
    #[serde(default)]
    pub drive: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

/// struct MR_PD_INFO → the disk's state. `pd` is its entry in the list.
pub fn parse_pd_status(pd: &crate::megaraid::Pd, raw: &[u8]) -> Option<PdStatus> {
    let code = le16(raw, PD_FW_STATE)?;
    let (vendor, model) = crate::megaraid::parse_pd_identity(raw);
    let sas = [PD_SAS_ADDR, PD_SAS_ADDR + 8].iter().filter_map(|&o| le64(raw, o)).find(|a| *a != 0).map(|a| format!("{a:016x}"));
    let encl = le16(raw, PD_ENCL_DEVICE).unwrap_or(pd.enclosure);
    let slot = raw.get(PD_SLOT).copied().unwrap_or(pd.slot);
    Some(PdStatus {
        device_id: pd.device_id,
        seq: le16(raw, PD_SEQ)?,
        enclosure: (encl != 0xffff).then_some(encl),
        slot: (slot != 0xff).then_some(slot),
        state: state_name(code).into(),
        state_code: code,
        vendor,
        model,
        sas_address: sas.or_else(|| pd.sas_address.clone()),
        media_errors: le32(raw, PD_MEDIA_ERR)?,
        other_errors: le32(raw, PD_OTHER_ERR)?,
        predictive_failures: le32(raw, PD_PRED_FAIL)?,
        size_bytes: le64(raw, PD_COERCED)?.saturating_mul(512),
        drive: None,
        name: None,
    })
}

/// MFI_DCMD_PD_STATE_SET's mailbox (mfiutil `drive_set_state`).
pub fn state_set_mbox(device_id: u16, seq: u16, state: u16) -> [u8; 6] {
    let mut m = [0u8; 6];
    m[0..2].copy_from_slice(&device_id.to_le_bytes());
    m[2..4].copy_from_slice(&seq.to_le_bytes());
    m[4..6].copy_from_slice(&state.to_le_bytes());
    m
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub code: String,
    /// `warning` | `info`
    pub severity: String,
    pub message: String,
}

/// One controller: identity, mode, its disks, findings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerReport {
    /// The PCIe function ("0000:03:00.0"), else the SCSI host.
    pub key: String,
    pub scsi_host: String,
    #[serde(default)]
    pub pcie_addr: Option<String>,
    pub driver: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub serial: Option<String>,
    #[serde(default)]
    pub firmware: Option<String>,
    #[serde(default)]
    pub virtual_disks: Option<u16>,
    #[serde(default)]
    pub jbod_enabled: Option<bool>,
    pub mode: Mode,
    /// How `mode` is known: "perccli", or the evidence it was inferred from.
    pub mode_basis: String,
    /// A personality asked for that a reboot has not applied yet.
    #[serde(default)]
    pub pending_mode: Option<Mode>,
    pub drives: Vec<PdStatus>,
    pub findings: Vec<Finding>,
    /// Why the controller could not be read.
    #[serde(default)]
    pub error: Option<String>,
    /// Where perccli is, or why there is none (the personality needs it).
    pub perccli: Result<String, String>,
}

/// The mode from what MFI says: RAID when the controller has virtual disks
/// or a disk in a RAID-only state; otherwise not known without perccli.
pub fn infer_mode(info: Option<&CtrlInfo>, drives: &[PdStatus]) -> (Mode, String) {
    if let Some(i) = info.filter(|i| i.virtual_disks > 0) {
        return (Mode::Raid, format!("inferred: {} virtual disk(s)", i.virtual_disks));
    }
    let mut odd: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for d in drives.iter().filter(|d| raid_only(d.state_code)) {
        *odd.entry(d.state.as_str()).or_default() += 1;
    }
    if !odd.is_empty() {
        let list: Vec<String> = odd.iter().map(|(s, n)| format!("{s} ×{n}")).collect();
        return (Mode::Raid, format!("inferred: disks in states only RAID mode has ({})", list.join(", ")));
    }
    (Mode::Unknown, "every disk is JBOD; RAID mode with JBOD and HBA mode read alike without perccli".into())
}

pub fn findings(r: &ControllerReport) -> Vec<Finding> {
    let mut out = Vec::new();
    let w = |code: &str, message: String| Finding { code: code.into(), severity: "warning".into(), message };
    if let Some(e) = &r.error {
        out.push(w("unreadable", format!("controller {} not read: {e}", r.key)));
    }
    if r.mode == Mode::Raid && r.pending_mode != Some(Mode::Hba) {
        out.push(w(
            "raid_mode",
            format!("controller {} ({}) is in RAID mode ({}); HBA mode is mandatory", r.key, r.model.as_deref().unwrap_or("?"), r.mode_basis),
        ));
    }
    if let Some(p) = r.pending_mode {
        out.push(Finding {
            code: "reboot_pending".into(),
            severity: "info".into(),
            message: format!("controller {}: personality {} asked for; a reboot applies it", r.key, p.perccli_word()),
        });
    }
    for d in r.drives.iter().filter(|d| matches!(d.state_code, STATE_FAILED | STATE_UNCONFIGURED_BAD | STATE_OFFLINE)) {
        out.push(w(
            "drive_failed",
            format!(
                "{} slot {}: the controller has it {} ({} bytes offered){}",
                r.key,
                d.slot.map_or("?".into(), |s| s.to_string()),
                d.state,
                d.size_bytes,
                d.name.as_ref().map(|n| format!(", was {n}")).unwrap_or_default()
            ),
        ));
    }
    out
}

/// The transport a controller is read and changed through.
pub trait Mfi {
    /// A read DCMD; `Err((status, message))`.
    fn read(&mut self, opcode: u32, mbox: &[u8], buf: &mut [u8]) -> Result<(), (Option<u8>, String)>;
    /// A DCMD that moves no data.
    fn exec(&mut self, opcode: u32, mbox: &[u8]) -> Result<(), (Option<u8>, String)>;
}

/// Every disk the controller lists, with its state.
pub fn read_drives(mfi: &mut dyn Mfi) -> Result<Vec<PdStatus>, String> {
    let mut list = vec![0u8; 8 + 24 * 512];
    mfi.read(crate::megaraid::DCMD_PD_LIST, &[], &mut list).map_err(|e| e.1)?;
    let pds = crate::megaraid::parse_pd_list(&list).ok_or("short drive list")?;
    let mut out = Vec::new();
    for p in pds.iter().filter(|p| p.scsi_type == 0x00) {
        let mut info = vec![0u8; 512];
        match mfi.read(crate::megaraid::DCMD_PD_INFO, &p.device_id.to_le_bytes(), &mut info) {
            Ok(()) => out.extend(parse_pd_status(p, &info)),
            Err((_, e)) => tracing::debug!(device = p.device_id, "drive info: {e}"),
        }
    }
    Ok(out)
}

/// Identity and disks of one controller; mode inferred (perccli's word, when
/// there is one, is put in by the caller).
pub fn read_controller(host: &str, pcie: Option<&str>, mfi: Result<&mut dyn Mfi, String>) -> ControllerReport {
    let mut r = ControllerReport {
        key: pcie.unwrap_or(host).to_string(),
        scsi_host: host.into(),
        pcie_addr: pcie.map(str::to_string),
        driver: "megaraid_sas".into(),
        model: None,
        serial: None,
        firmware: None,
        virtual_disks: None,
        jbod_enabled: None,
        mode: Mode::Unknown,
        mode_basis: String::new(),
        pending_mode: None,
        drives: Vec::new(),
        findings: Vec::new(),
        error: None,
        perccli: Err("not looked for".into()),
    };
    let mfi = match mfi {
        Ok(m) => m,
        Err(e) => {
            r.error = Some(e);
            r.mode_basis = "controller not read".into();
            return r;
        }
    };
    let mut buf = vec![0u8; CTRL_INFO_LEN];
    let info = match mfi.read(DCMD_CTRL_INFO, &[], &mut buf) {
        Ok(()) => parse_ctrl_info(&buf),
        Err((_, e)) => {
            r.error = Some(e);
            None
        }
    };
    if let Some(i) = &info {
        r.model = i.model.clone();
        r.serial = i.serial.clone();
        r.firmware = i.firmware.clone();
        r.virtual_disks = Some(i.virtual_disks);
        r.jbod_enabled = Some(i.jbod_enabled);
    }
    match read_drives(mfi) {
        Ok(d) => r.drives = d,
        Err(e) => r.error = Some(r.error.take().map_or(e.clone(), |o| format!("{o}; {e}"))),
    }
    (r.mode, r.mode_basis) = infer_mode(info.as_ref(), &r.drives);
    r
}

/// The inventory drive a controller's disk is: by SAS address, else on the
/// same SCSI host in the same bay when that slot is unique on the controller.
pub fn match_drive<'a>(r: &ControllerReport, pd: &PdStatus, drives: impl Iterator<Item = &'a Drive> + Clone) -> Option<&'a Drive> {
    let norm = |s: &str| s.trim_start_matches("0x").to_ascii_lowercase();
    if let Some(sas) = pd.sas_address.as_deref().map(norm) {
        if let Some(d) = drives.clone().find(|d| d.location.sas_address.as_deref().map(norm).as_deref() == Some(sas.as_str())) {
            return Some(d);
        }
    }
    let slot = u32::from(pd.slot?);
    if r.drives.iter().filter(|o| o.slot == pd.slot).count() != 1 {
        return None;
    }
    let mut on_host = drives.filter(|d| {
        d.location.bay == Some(slot) && d.location.controller.as_ref().and_then(|c| c.scsi_host.as_deref()) == Some(r.scsi_host.as_str())
    });
    let first = on_host.next()?;
    on_host.next().is_none().then_some(first)
}

/// The controller's view kept on a drive (shown in place of a bare
/// `missing`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveControllerState {
    pub controller: String,
    pub device_id: u16,
    #[serde(default)]
    pub enclosure: Option<u16>,
    #[serde(default)]
    pub slot: Option<u8>,
    pub state: String,
    pub state_code: u16,
    pub size_bytes: u64,
}

impl DriveControllerState {
    pub fn of(r: &ControllerReport, pd: &PdStatus) -> Self {
        Self {
            controller: r.key.clone(),
            device_id: pd.device_id,
            enclosure: pd.enclosure,
            slot: pd.slot,
            state: pd.state.clone(),
            state_code: pd.state_code,
            size_bytes: pd.size_bytes,
        }
    }
}

/// A state a person may ask a drive be put in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// Unconfigured good (`perccli … set good`).
    Good,
    /// JBOD / non-RAID (`perccli … set jbod`): Linux sees it.
    Jbod,
}

/// The state changes to send, in order, for `pd` to reach `want`; or why
/// not. `drive` is its inventory record, when it has one.
pub fn plan_state(pd: &PdStatus, want: Target, force: bool, drive: Option<&Drive>) -> Result<Vec<u16>, String> {
    if let Some(d) = drive {
        let u = crate::inuse::of(d);
        if u.use_ == crate::inuse::Use::System {
            return Err(format!("{}: {}; its controller state is not changed from here", d.name, u.reason.unwrap_or_else(|| "the system drive".into())));
        }
        if d.membership == Membership::Fleet {
            return Err(format!("{}: in the fleet; leave it first", d.name));
        }
        if u.in_use {
            return Err(format!("{}: {}", d.name, u.reason.unwrap_or_else(|| "in use".into())));
        }
    }
    let to_good = match pd.state_code {
        STATE_UNCONFIGURED_GOOD => vec![],
        STATE_FAILED if !force => return Err(format!("the controller has device {} failed: set good needs force (perccli's `set good force`)", pd.device_id)),
        STATE_FAILED | STATE_UNCONFIGURED_BAD => vec![STATE_UNCONFIGURED_GOOD],
        STATE_JBOD if want == Target::Jbod => return Ok(vec![]),
        STATE_JBOD => vec![STATE_UNCONFIGURED_GOOD],
        c => {
            return Err(format!(
                "device {} is {} (a member or spare of the controller's configuration): not changed from here",
                pd.device_id,
                state_name(c)
            ))
        }
    };
    Ok(match want {
        Target::Good => to_good,
        Target::Jbod => to_good.into_iter().chain([STATE_JBOD]).collect(),
    })
}

/// Send the plan, then read the drive back: its state must be the last one.
pub fn apply_state(mfi: &mut dyn Mfi, pd: &PdStatus, steps: &[u16]) -> Result<PdStatus, String> {
    let mut seq = pd.seq;
    let entry = crate::megaraid::Pd { device_id: pd.device_id, enclosure: pd.enclosure.unwrap_or(0xffff), slot: pd.slot.unwrap_or(0xff), scsi_type: 0, sas_address: pd.sas_address.clone() };
    let reread = |mfi: &mut dyn Mfi| -> Result<PdStatus, String> {
        let mut info = vec![0u8; 512];
        mfi.read(crate::megaraid::DCMD_PD_INFO, &pd.device_id.to_le_bytes(), &mut info).map_err(|e| e.1)?;
        parse_pd_status(&entry, &info).ok_or_else(|| "short drive info".to_string())
    };
    for &st in steps {
        mfi.exec(DCMD_PD_STATE_SET, &state_set_mbox(pd.device_id, seq, st))
            .map_err(|(_, e)| format!("device {} → {}: {e}", pd.device_id, state_name(st)))?;
        // Each change moves the sequence number; the next one names the new.
        seq = reread(mfi)?.seq;
    }
    let now = reread(mfi)?;
    match steps.last() {
        Some(&want) if now.state_code != want => {
            Err(format!("device {}: asked {}, the controller reads {}", pd.device_id, state_name(want), now.state))
        }
        _ => Ok(now),
    }
}

// ---- perccli -------------------------------------------------------------

/// Where perccli is looked for when the config names none.
pub const PERCCLI_SEARCH: &[&str] =
    &["/usr/libexec/stormdrive/perccli64", "/opt/MegaRAID/perccli/perccli64", "/opt/dell/perccli/perccli64", "/usr/sbin/perccli64", "/usr/bin/perccli64", "/usr/sbin/perccli"];

/// The binary: the configured one, else the first of the search list.
pub fn find_perccli(configured: Option<&str>) -> Result<PathBuf, String> {
    if let Some(p) = configured {
        let p = PathBuf::from(p);
        return if p.is_file() { Ok(p) } else { Err(format!("perccli {}: not there", p.display())) };
    }
    PERCCLI_SEARCH.iter().map(PathBuf::from).find(|p| p.is_file()).ok_or_else(|| {
        format!("perccli is not in this image (looked in {}); the personality change needs it (#132)", PERCCLI_SEARCH.join(", "))
    })
}

/// Run perccli with `args` + `J`, bounded; its JSON.
pub fn run_perccli(bin: &Path, args: &[&str], limit: Duration) -> Result<serde_json::Value, String> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(bin)
        .args(args)
        .arg("J")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("perccli {}: no answer in {} s", args.join(" "), limit.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("perccli: {e}")),
        }
    }
    let out = child.wait_with_output().map_err(|e| format!("perccli: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(text.trim()).map_err(|e| {
        format!("perccli {}: not JSON ({e}): {}", args.join(" "), text.chars().take(200).collect::<String>())
    })
}

/// The `Controllers` entries, each `Command Status` checked: a failure is
/// its description (and detailed status) as perccli gives it.
pub fn perccli_ok(v: &serde_json::Value) -> Result<Vec<&serde_json::Value>, String> {
    let list = v.get("Controllers").and_then(|c| c.as_array()).ok_or("perccli: no Controllers in its answer")?;
    let mut out = Vec::new();
    for c in list {
        let st = &c["Command Status"];
        if st["Status"].as_str() != Some("Success") {
            let mut why = st["Description"].as_str().unwrap_or("failed").to_string();
            if let Some(d) = st.get("Detailed Status") {
                why.push_str(&format!(": {d}"));
            }
            return Err(format!("perccli: {why}"));
        }
        out.push(c);
    }
    Ok(out)
}

/// Strings anywhere under `v` whose key contains `needle` (case-blind).
fn find_strings<'a>(v: &'a serde_json::Value, needle: &str, out: &mut Vec<(&'a str, &'a str)>) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, x) in m {
                if let Some(s) = x.as_str() {
                    if k.to_ascii_lowercase().contains(needle) {
                        out.push((k, s));
                    }
                }
                find_strings(x, needle, out);
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| find_strings(x, needle, out)),
        _ => {}
    }
}

fn pci_triple(bdf: &str) -> Option<(u32, u32, u32)> {
    // "0000:03:00.0"
    let (rest, func) = bdf.rsplit_once('.')?;
    let mut p = rest.rsplit(':');
    let dev = u32::from_str_radix(p.next()?, 16).ok()?;
    let bus = u32::from_str_radix(p.next()?, 16).ok()?;
    Some((bus, dev, u32::from_str_radix(func, 16).ok()?))
}

/// perccli's controller number for ours: `/call show` names each one's PCI
/// address ("00:03:00:00"); a lone controller on both sides is the one.
pub fn perccli_index(all: &serde_json::Value, pcie: Option<&str>, ours: usize) -> Result<u32, String> {
    let ctrls = perccli_ok(all)?;
    let mut seen = Vec::new();
    for c in &ctrls {
        let idx = c["Command Status"]["Controller"].as_u64().map(|n| n as u32);
        let mut addr = Vec::new();
        find_strings(&c["Response Data"], "pci address", &mut addr);
        seen.push((idx, addr.first().map(|a| a.1.to_string())));
    }
    if let Some(want) = pcie.and_then(pci_triple) {
        for (idx, addr) in &seen {
            let parts: Vec<u32> = addr.iter().flat_map(|a| a.split([':', '.'])).filter_map(|x| u32::from_str_radix(x.trim(), 16).ok()).collect();
            if parts.windows(3).any(|w| (w[0], w[1], w[2]) == want) {
                if let Some(i) = idx {
                    return Ok(*i);
                }
            }
        }
    }
    match (seen.as_slice(), ours) {
        ([(Some(i), _)], 1) => Ok(*i),
        _ => Err(format!("perccli: no controller at {} among {:?}", pcie.unwrap_or("?"), seen)),
    }
}

/// The current and requested personality from `/cN show personality`.
pub fn perccli_personality(v: &serde_json::Value) -> Result<(Option<Mode>, Option<Mode>), String> {
    let ctrls = perccli_ok(v)?;
    let mut strs = Vec::new();
    for c in ctrls {
        find_strings(&c["Response Data"], "personality", &mut strs);
    }
    let pick = |pred: &dyn Fn(&str) -> bool| strs.iter().find(|(k, _)| pred(&k.to_ascii_lowercase())).and_then(|(_, s)| Mode::parse(s));
    let current = pick(&|k: &str| k.contains("current")).or_else(|| pick(&|k: &str| !k.contains("request") && !k.contains("pending") && !k.contains("support")));
    let pending = pick(&|k: &str| k.contains("request") || k.contains("pending"));
    Ok((current, pending.filter(|p| Some(*p) != current)))
}

/// Ask for a personality: refused while the controller has virtual disks
/// (perccli would refuse too; they are the owner's data).
pub fn personality_blocker(r: &ControllerReport, want: Mode) -> Option<String> {
    if want == Mode::Raid || want == Mode::Unknown {
        return Some(format!("mode {}: only HBA (or JBOD) is asked for from here", want.perccli_word()));
    }
    if let Some(n) = r.virtual_disks.filter(|n| *n > 0) {
        return Some(format!("controller {} has {n} virtual disk(s): they are not deleted from here", r.key));
    }
    if let Err(e) = &r.perccli {
        return Some(e.clone());
    }
    None
}

// ---- the node ------------------------------------------------------------

/// Every `megaraid_sas` controller on the node, read; perccli's mode put in
/// where it answers.
pub fn scan(perccli: Option<&str>) -> Vec<ControllerReport> {
    #[cfg(target_os = "linux")]
    {
        linux::scan(perccli)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = perccli;
        Vec::new()
    }
}

/// perccli's mode and pending mode into a report, when it answers.
pub fn with_perccli(r: &mut ControllerReport, configured: Option<&str>, ours: usize, all: &mut Option<Result<serde_json::Value, String>>) {
    let bin = match find_perccli(configured) {
        Ok(b) => b,
        Err(e) => {
            r.perccli = Err(e);
            r.findings = findings(r);
            return;
        }
    };
    r.perccli = Ok(bin.display().to_string());
    let limit = Duration::from_secs(60);
    let all = all.get_or_insert_with(|| run_perccli(&bin, &["/call", "show"], limit));
    let res = all.clone().and_then(|a| perccli_index(&a, r.pcie_addr.as_deref(), ours)).and_then(|i| {
        let v = run_perccli(&bin, &[&format!("/c{i}"), "show", "personality"], limit)?;
        perccli_personality(&v)
    });
    match res {
        Ok((cur, pending)) => {
            if let Some(m) = cur {
                r.mode = m;
                r.mode_basis = "perccli".into();
            }
            r.pending_mode = pending;
        }
        Err(e) => tracing::debug!(controller = %r.key, "perccli personality: {e}"),
    }
    r.findings = findings(r);
}

/// Set the personality through perccli; its answer's description.
pub fn set_personality(r: &ControllerReport, configured: Option<&str>, ours: usize, want: Mode) -> Result<String, String> {
    if let Some(why) = personality_blocker(r, want) {
        return Err(why);
    }
    let bin = find_perccli(configured)?;
    let limit = Duration::from_secs(300);
    let all = run_perccli(&bin, &["/call", "show"], Duration::from_secs(60))?;
    let i = perccli_index(&all, r.pcie_addr.as_deref(), ours)?;
    let v = run_perccli(&bin, &[&format!("/c{i}"), "set", &format!("personality={}", want.perccli_word())], limit)?;
    let ok = perccli_ok(&v)?;
    let mut said = Vec::new();
    for c in ok {
        find_strings(&c["Response Data"], "", &mut said);
    }
    let detail: Vec<String> = said.iter().map(|(k, s)| format!("{k}: {s}")).collect();
    Ok(if detail.is_empty() { format!("perccli /c{i}: personality {} set; a reboot applies it", want.perccli_word()) } else { detail.join("; ") })
}

/// Change one drive's state on the controller `key` (re-read first, so the
/// plan uses its present state and sequence number). The drive after.
pub fn set_drive_state(key: &str, device_id: u16, want: Target, force: bool, drive: Option<&Drive>) -> Result<(PdStatus, Vec<u16>), String> {
    #[cfg(target_os = "linux")]
    {
        linux::set_drive_state(key, device_id, want, force, drive)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (key, device_id, want, force, drive);
        Err("RAID controllers are read on Linux only".into())
    }
}

/// Plan and send against one controller's transport (portable half).
pub fn change_state(mfi: &mut dyn Mfi, device_id: u16, want: Target, force: bool, drive: Option<&Drive>) -> Result<(PdStatus, Vec<u16>), String> {
    let pd = read_drives(mfi)?.into_iter().find(|p| p.device_id == device_id).ok_or_else(|| format!("no device {device_id} on the controller"))?;
    let steps = plan_state(&pd, want, force, drive)?;
    if steps.is_empty() {
        return Ok((pd, steps));
    }
    let after = apply_state(mfi, &pd, &steps)?;
    Ok((after, steps))
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::fs::File;

    struct Ioctl<'a> {
        file: &'a File,
        host_no: u16,
    }

    impl Mfi for Ioctl<'_> {
        fn read(&mut self, opcode: u32, mbox: &[u8], buf: &mut [u8]) -> Result<(), (Option<u8>, String)> {
            crate::megaraid::linux::dcmd_raw(self.file, self.host_no, opcode, mbox, Some(buf))
        }
        fn exec(&mut self, opcode: u32, mbox: &[u8]) -> Result<(), (Option<u8>, String)> {
            crate::megaraid::linux::dcmd_raw(self.file, self.host_no, opcode, mbox, None)
        }
    }

    fn host_no(host: &str) -> Result<u16, String> {
        host.trim_start_matches("host").parse::<u16>().map_err(|e| format!("{host}: {e}"))
    }

    pub fn scan(perccli: Option<&str>) -> Vec<ControllerReport> {
        let hosts = crate::megaraid::linux::hosts(Path::new("/sys"));
        if hosts.is_empty() {
            return Vec::new();
        }
        let file = crate::megaraid::linux::open_node();
        let mut all = None;
        let n = hosts.len();
        hosts
            .iter()
            .map(|(host, pcie)| {
                let mut r = match (&file, &host_no(host)) {
                    (Ok(f), Ok(no)) => {
                        let mut io = Ioctl { file: f, host_no: *no };
                        read_controller(host, pcie.as_deref(), Ok(&mut io))
                    }
                    (Err(e), _) | (_, Err(e)) => read_controller(host, pcie.as_deref(), Err(e.clone())),
                };
                with_perccli(&mut r, perccli, n, &mut all);
                r
            })
            .collect()
    }

    pub fn set_drive_state(key: &str, device_id: u16, want: Target, force: bool, drive: Option<&Drive>) -> Result<(PdStatus, Vec<u16>), String> {
        let hosts = crate::megaraid::linux::hosts(Path::new("/sys"));
        let (host, _) = hosts.iter().find(|(h, p)| p.as_deref().unwrap_or(h) == key || h == key).ok_or_else(|| format!("no controller {key}"))?;
        let file = crate::megaraid::linux::open_node()?;
        let mut io = Ioctl { file: &file, host_no: host_no(host)? };
        change_state(&mut io, device_id, want, force, drive)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A controller's firmware: a drive list, each drive's info, and state
    /// changes that move a drive and bump its sequence number.
    pub(crate) struct Fake {
        pub info: Vec<u8>,
        pub pds: BTreeMap<u16, (u16, u8, u16, u16, u64)>, // id → (encl, slot, state, seq, sas)
        pub sent: Vec<(u16, u16, u16)>,
        /// state transitions the firmware refuses: (from, to) → MFI status
        pub refuse: Vec<(u16, u16, u8)>,
    }

    pub(crate) fn ctrl_info(model: &str, fw: &str, vds: u16, jbod: bool) -> Vec<u8> {
        let mut b = vec![0u8; CTRL_INFO_LEN];
        b[CI_PRODUCT_NAME..CI_PRODUCT_NAME + model.len()].copy_from_slice(model.as_bytes());
        b[CI_SERIAL..CI_SERIAL + 6].copy_from_slice(b"5BX0QX");
        b[CI_PACKAGE_VERSION..CI_PACKAGE_VERSION + fw.len()].copy_from_slice(fw.as_bytes());
        b[CI_LD_PRESENT..CI_LD_PRESENT + 2].copy_from_slice(&vds.to_le_bytes());
        if jbod {
            b[CI_PROPERTIES + PROP_ON_OFF..CI_PROPERTIES + PROP_ON_OFF + 4].copy_from_slice(&ENABLE_JBOD_BIT.to_le_bytes());
        }
        b
    }

    fn pd_info(id: u16, (encl, slot, state, seq, sas): (u16, u8, u16, u16, u64)) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[0..2].copy_from_slice(&id.to_le_bytes());
        b[PD_SEQ..PD_SEQ + 2].copy_from_slice(&seq.to_le_bytes());
        b[4 + 8..4 + 16].copy_from_slice(b"HGST    ");
        b[4 + 16..4 + 32].copy_from_slice(b"H7210A520SUN010T");
        b[PD_FW_STATE..PD_FW_STATE + 2].copy_from_slice(&state.to_le_bytes());
        b[PD_SAS_ADDR..PD_SAS_ADDR + 8].copy_from_slice(&sas.to_le_bytes());
        let blocks: u64 = if state == STATE_FAILED { 0 } else { 19_532_873_728 };
        b[PD_COERCED..PD_COERCED + 8].copy_from_slice(&blocks.to_le_bytes());
        b[PD_ENCL_DEVICE..PD_ENCL_DEVICE + 2].copy_from_slice(&encl.to_le_bytes());
        b[PD_SLOT] = slot;
        b
    }

    impl Mfi for Fake {
        fn read(&mut self, opcode: u32, mbox: &[u8], buf: &mut [u8]) -> Result<(), (Option<u8>, String)> {
            let data = match opcode {
                DCMD_CTRL_INFO => self.info.clone(),
                crate::megaraid::DCMD_PD_LIST => {
                    let mut l = Vec::new();
                    l.extend_from_slice(&0u32.to_le_bytes());
                    l.extend_from_slice(&(self.pds.len() as u32 + 1).to_le_bytes());
                    let mut entry = |id: u16, encl: u16, slot: u8, ty: u8, sas: u64| {
                        let mut e = vec![0u8; 24];
                        e[0..2].copy_from_slice(&id.to_le_bytes());
                        e[2..4].copy_from_slice(&encl.to_le_bytes());
                        e[5] = slot;
                        e[6] = ty;
                        e[8..16].copy_from_slice(&sas.to_le_bytes());
                        l.extend(e);
                    };
                    entry(32, 0xffff, 0xff, 0x0d, 0x5e09_4060_39f0_cf00);
                    for (id, (encl, slot, _, _, sas)) in &self.pds {
                        entry(*id, *encl, *slot, 0, *sas);
                    }
                    l
                }
                crate::megaraid::DCMD_PD_INFO => {
                    let id = u16::from_le_bytes([mbox[0], mbox[1]]);
                    match self.pds.get(&id) {
                        Some(p) => pd_info(id, *p),
                        None => return Err((Some(0x0c), "device not found".into())),
                    }
                }
                _ => return Err((Some(0x02), "invalid DCMD".into())),
            };
            let n = data.len().min(buf.len());
            buf[..n].copy_from_slice(&data[..n]);
            Ok(())
        }
        fn exec(&mut self, opcode: u32, mbox: &[u8]) -> Result<(), (Option<u8>, String)> {
            assert_eq!(opcode, DCMD_PD_STATE_SET);
            let id = u16::from_le_bytes([mbox[0], mbox[1]]);
            let seq = u16::from_le_bytes([mbox[2], mbox[3]]);
            let st = u16::from_le_bytes([mbox[4], mbox[5]]);
            self.sent.push((id, seq, st));
            let p = self.pds.get_mut(&id).ok_or((Some(0x0c), "device not found".to_string()))?;
            if p.3 != seq {
                return Err((Some(0x04), "invalid sequence number".into()));
            }
            if let Some(&(_, _, code)) = self.refuse.iter().find(|(f, t, _)| *f == p.2 && *t == st) {
                return Err((Some(code), format!("controller status 0x{code:02x} ({})", mfi_status_name(code))));
            }
            p.2 = st;
            p.3 += 1;
            Ok(())
        }
    }

    /// forge1: sda JBOD in slot 0 (the system drive), sdb–sdd Failed in
    /// slots 3, 4, 7; the BP14G+ is enclosure 32.
    pub(crate) fn forge1() -> Fake {
        let mut pds = BTreeMap::new();
        pds.insert(0, (32, 0, STATE_JBOD, 7, 0x5000_cca2_0000_0001));
        pds.insert(3, (32, 3, STATE_FAILED, 11, 0x5000_cca2_0000_0003));
        pds.insert(4, (0xffff, 4, STATE_FAILED, 11, 0x5000_cca2_0000_0004));
        pds.insert(7, (0xffff, 7, STATE_FAILED, 11, 0x5000_cca2_0000_0007));
        Fake { info: ctrl_info("PERC H730 Mini", "25.5.4.0006", 0, true), pds, sent: Vec::new(), refuse: Vec::new() }
    }

    #[test]
    fn forge1s_perc_reads_raid_mode_with_three_failed_drives() {
        let mut f = forge1();
        let r = read_controller("host0", Some("0000:03:00.0"), Ok(&mut f));
        assert_eq!(r.model.as_deref(), Some("PERC H730 Mini"));
        assert_eq!(r.firmware.as_deref(), Some("25.5.4.0006"));
        assert_eq!(r.serial.as_deref(), Some("5BX0QX"));
        assert_eq!(r.virtual_disks, Some(0));
        assert_eq!(r.jbod_enabled, Some(true));
        assert_eq!(r.drives.len(), 4, "the enclosure is not a drive");
        let failed: Vec<_> = r.drives.iter().filter(|d| d.state == "failed").map(|d| d.slot).collect();
        assert_eq!(failed, vec![Some(3), Some(4), Some(7)]);
        assert!(r.drives.iter().filter(|d| d.state == "failed").all(|d| d.size_bytes == 0), "0 KB");
        assert_eq!(r.drives[0].size_bytes, 19_532_873_728 * 512);
        assert_eq!(r.drives[0].sas_address.as_deref(), Some("5000cca200000001"));
        assert_eq!(r.drives[1].enclosure, Some(32));
        assert_eq!(r.drives[2].enclosure, None);
        assert_eq!(r.mode, Mode::Raid);
        assert!(r.mode_basis.contains("failed ×3"), "{}", r.mode_basis);
        let mut r = r;
        r.findings = findings(&r);
        let codes: Vec<&str> = r.findings.iter().map(|f| f.code.as_str()).collect();
        assert_eq!(codes, vec!["raid_mode", "drive_failed", "drive_failed", "drive_failed"]);
        assert!(r.findings[0].message.contains("HBA mode is mandatory"));
    }

    #[test]
    fn mode_inference() {
        let jbod = |slot| PdStatus {
            device_id: slot,
            seq: 0,
            enclosure: None,
            slot: Some(slot as u8),
            state: "jbod".into(),
            state_code: STATE_JBOD,
            vendor: None,
            model: None,
            sas_address: None,
            media_errors: 0,
            other_errors: 0,
            predictive_failures: 0,
            size_bytes: 1,
            drive: None,
            name: None,
        };
        let all_jbod = vec![jbod(0), jbod(1)];
        assert_eq!(infer_mode(None, &all_jbod).0, Mode::Unknown);
        let vds = CtrlInfo { virtual_disks: 2, ..Default::default() };
        assert_eq!(infer_mode(Some(&vds), &all_jbod).0, Mode::Raid);
        let mut online = all_jbod.clone();
        online[1].state_code = STATE_ONLINE;
        online[1].state = "online".into();
        assert_eq!(infer_mode(None, &online).0, Mode::Raid);
    }

    #[test]
    fn set_good_with_force_then_jbod_and_read_back() {
        let mut f = forge1();
        let (after, steps) = change_state(&mut f, 3, Target::Good, true, None).unwrap();
        assert_eq!(steps, vec![STATE_UNCONFIGURED_GOOD]);
        assert_eq!(after.state, "unconfigured_good");
        assert_eq!(f.sent, vec![(3, 11, STATE_UNCONFIGURED_GOOD)], "the drive's own sequence number");
        // JBOD from Failed: good first, then JBOD, each with the new sequence.
        let (after, steps) = change_state(&mut f, 4, Target::Jbod, true, None).unwrap();
        assert_eq!(steps, vec![STATE_UNCONFIGURED_GOOD, STATE_JBOD]);
        assert_eq!(after.state, "jbod");
        assert_eq!(&f.sent[1..], &[(4, 11, STATE_UNCONFIGURED_GOOD), (4, 12, STATE_JBOD)]);
        // Already there: nothing sent.
        let n = f.sent.len();
        assert!(change_state(&mut f, 4, Target::Jbod, false, None).unwrap().1.is_empty());
        assert_eq!(f.sent.len(), n);
    }

    #[test]
    fn refusals() {
        let mut f = forge1();
        let e = change_state(&mut f, 7, Target::Good, false, None).unwrap_err();
        assert!(e.contains("needs force"), "{e}");
        assert!(f.sent.is_empty());
        f.pds.get_mut(&7).unwrap().2 = STATE_ONLINE;
        let e = change_state(&mut f, 7, Target::Good, true, None).unwrap_err();
        assert!(e.contains("online"), "{e}");
        assert!(change_state(&mut f, 9, Target::Good, true, None).unwrap_err().contains("no device 9"));
        // The firmware refuses: its status is named, nothing claimed.
        f.pds.get_mut(&7).unwrap().2 = STATE_FAILED;
        f.refuse.push((STATE_FAILED, STATE_UNCONFIGURED_GOOD, 0x32));
        let e = change_state(&mut f, 7, Target::Good, true, None).unwrap_err();
        assert!(e.contains("wrong state"), "{e}");
    }

    #[test]
    fn the_system_drive_and_a_fleet_drive_are_refused() {
        let pd = PdStatus {
            device_id: 0,
            seq: 1,
            enclosure: Some(32),
            slot: Some(0),
            state: "jbod".into(),
            state_code: STATE_JBOD,
            vendor: None,
            model: None,
            sas_address: None,
            media_errors: 0,
            other_errors: 0,
            predictive_failures: 0,
            size_bytes: 1,
            drive: None,
            name: None,
        };
        let f = crate::inuse::tests::forge1();
        let e = plan_state(&pd, Target::Good, true, Some(&f[0])).unwrap_err();
        assert!(e.contains("system"), "{e}");
        let mut d = f[1].clone();
        d.membership = Membership::Fleet;
        assert!(plan_state(&pd, Target::Good, true, Some(&d)).unwrap_err().contains("fleet"));
        let d = f[2].clone();
        assert_eq!(plan_state(&pd, Target::Good, false, Some(&d)).unwrap(), vec![STATE_UNCONFIGURED_GOOD]);
    }

    #[test]
    fn a_failed_drive_is_matched_to_its_missing_record() {
        let mut f = forge1();
        let r = read_controller("host0", Some("0000:03:00.0"), Ok(&mut f));
        let mut by_sas = crate::drive::Drive::test_fixture("sdb");
        by_sas.location.sas_address = Some("0x5000CCA200000003".into());
        let mut by_bay = crate::drive::Drive::test_fixture("sdc");
        by_bay.location.bay = Some(4);
        by_bay.location.controller = Some(crate::drive::Controller { scsi_host: Some("host0".into()), pcie_addr: None, driver: None });
        let drives = [by_sas.clone(), by_bay.clone()];
        assert_eq!(match_drive(&r, &r.drives[1], drives.iter()).map(|d| d.name.as_str()), Some("sdb"));
        assert_eq!(match_drive(&r, &r.drives[2], drives.iter()).map(|d| d.name.as_str()), Some("sdc"));
        assert!(match_drive(&r, &r.drives[3], drives.iter()).is_none());
    }

    #[test]
    fn frames() {
        let f = crate::megaraid::dcmd_frame_with(DCMD_PD_STATE_SET, &state_set_mbox(4, 11, STATE_UNCONFIGURED_GOOD), None);
        assert_eq!(f[0], 0x05);
        assert_eq!(f[7], 0, "no SGE");
        assert_eq!(u16::from_le_bytes([f[0x10], f[0x11]]), 0, "MFI_FRAME_DIR_NONE");
        assert_eq!(u32::from_le_bytes([f[0x18], f[0x19], f[0x1a], f[0x1b]]), 0x0203_0100);
        assert_eq!(&f[0x1c..0x22], &[4, 0, 11, 0, 0, 0]);
        assert_eq!(state_set_mbox(4, 11, STATE_JBOD), [4, 0, 11, 0, 0x40, 0]);
        assert!(parse_ctrl_info(&[0; 100]).is_none());
    }

    fn controllers_json(addr: &str) -> serde_json::Value {
        serde_json::json!({"Controllers":[{"Command Status":{"CLI Version":"007.1910","Controller":0,"Status":"Success","Description":"None"},
            "Response Data":{"Basics":{"Controller":0,"Model":"PERC H730 Mini","PCI Address":addr}}}]})
    }

    #[test]
    fn perccli_answers() {
        assert_eq!(perccli_index(&controllers_json("00:03:00:00"), Some("0000:03:00.0"), 1), Ok(0));
        assert_eq!(perccli_index(&controllers_json("00:81:00:00"), Some("0000:03:00.0"), 1), Ok(0), "the only one on both sides");
        assert!(perccli_index(&controllers_json("00:81:00:00"), Some("0000:03:00.0"), 2).is_err());
        let p = serde_json::json!({"Controllers":[{"Command Status":{"Controller":0,"Status":"Success","Description":"None"},
            "Response Data":{"Personality Info":[{"Current Personality":"RAID-Mode","Requested Personality":"HBA-Mode","Default Personality":"RAID-Mode"}]}}]});
        assert_eq!(perccli_personality(&p), Ok((Some(Mode::Raid), Some(Mode::Hba))));
        let fail = serde_json::json!({"Controllers":[{"Command Status":{"Controller":0,"Status":"Failure","Description":"Un-supported command"}}]});
        assert_eq!(perccli_personality(&fail), Err("perccli: Un-supported command".into()));
    }

    #[cfg(unix)]
    #[test]
    fn a_fake_perccli_sets_the_personality() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("perccli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("perccli64");
        let log = dir.join("calls");
        let script = format!(
            r#"#!/bin/sh
echo "$@" >> {log}
case "$*" in
  "/call show J") echo '{all}' ;;
  "/c0 show personality J") echo '{{"Controllers":[{{"Command Status":{{"Controller":0,"Status":"Success","Description":"None"}},"Response Data":{{"Current Personality":"RAID-Mode"}}}}]}}' ;;
  "/c0 set personality=HBA J") echo '{{"Controllers":[{{"Command Status":{{"Controller":0,"Status":"Success","Description":"None"}},"Response Data":{{"Personality":"HBA","Reboot Required":"Yes"}}}}]}}' ;;
  *) echo '{{"Controllers":[{{"Command Status":{{"Status":"Failure","Description":"bad args"}}}}]}}' ;;
esac
"#,
            log = log.display(),
            all = controllers_json("00:03:00:00")
        );
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut f = forge1();
        let mut r = read_controller("host0", Some("0000:03:00.0"), Ok(&mut f));
        let cfg = bin.to_str();
        with_perccli(&mut r, cfg, 1, &mut None);
        assert_eq!((r.mode, r.mode_basis.as_str()), (Mode::Raid, "perccli"));
        let said = set_personality(&r, cfg, 1, Mode::Hba).unwrap();
        assert!(said.contains("Reboot Required: Yes"), "{said}");
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(calls.contains("/c0 set personality=HBA J"), "{calls}");
        // Refused: RAID asked for, virtual disks present, no perccli.
        assert!(set_personality(&r, cfg, 1, Mode::Raid).is_err());
        let mut vds = r.clone();
        vds.virtual_disks = Some(1);
        assert!(set_personality(&vds, cfg, 1, Mode::Hba).unwrap_err().contains("virtual disk"));
        let missing = dir.join("nope");
        let mut r2 = r.clone();
        with_perccli(&mut r2, missing.to_str(), 1, &mut None);
        assert!(r2.perccli.is_err());
        assert!(set_personality(&r2, missing.to_str(), 1, Mode::Hba).unwrap_err().contains("not there"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
