//! Shingled recording (SMR, #156): is the drive zoned, and how.
//!
//! - **host-managed**: zones must be written in order; the kernel says
//!   `queue/zoned = host-managed`. A stormblock slab writes at random, so no
//!   DrivePolicy enrolls one.
//! - **host-aware**: zoned commands offered, random writes still taken; the
//!   kernel says so (older kernels), or ATA IDENTIFY word 69 / VPD 0xB1 do.
//! - **drive-managed**: the drive hides its zones and reads `zoned: none` to
//!   the kernel. ATA IDENTIFY DEVICE word 69 bits 1:0 = 10b says it (ACS-4
//!   "ZONED CAPABILITIES"), as does VPD 0xB1 byte 8 bits 5:4 on SCSI. Many
//!   desktop drives (Seagate ST2000DM008, WD …EFAX) report nothing there, so
//!   a list of known DM-SMR models is the fallback.
//!
//! A drive-managed drive is a usable data drive (slow under random writes,
//! but stormblock's pacing and RAM absorb that, stormblock#672): it is shown,
//! and a DrivePolicy may select or leave it by `drives.smr`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Smr {
    #[default]
    None,
    HostManaged,
    HostAware,
    DriveManaged,
}

impl Smr {
    pub fn word(self) -> &'static str {
        match self {
            Smr::None => "none",
            Smr::HostManaged => "host-managed",
            Smr::HostAware => "host-aware",
            Smr::DriveManaged => "drive-managed",
        }
    }
}

/// What the drive is and what said so (`status.smr`, `status.smrSource`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmrInfo {
    pub smr: Smr,
    /// `queue/zoned`, `ATA IDENTIFY word 69`, `VPD 0xB1`, `known model`, or
    /// `nothing reports zoning`.
    pub source: String,
}

/// Drive-managed SMR models that do not say so in IDENTIFY or VPD 0xB1 (the
/// makers' own lists, 2020–2024). Matched as a substring of the model, as
/// sysfs has it (16 characters for SATA: `ST2000DM008-2UB1`).
pub const KNOWN_DM_SMR: &[&str] = &[
    // Seagate BarraCuda / BarraCuda Compute, Archive, laptop.
    "ST2000DM008", "ST3000DM007", "ST4000DM004", "ST6000DM003", "ST8000DM004",
    "ST8000AS0002", "ST8000AS0003", "ST5000LM000", "ST4000LM024", "ST2000LM015",
    "ST1000LM048", "ST500LM030",
    // WD Red (EFAX), Blue (EZAZ), Blue laptop (SPZX).
    "WD20EFAX", "WD30EFAX", "WD40EFAX", "WD60EFAX", "WD20EZAZ", "WD40EZAZ",
    "WD60EZAZ", "WD10SPZX", "WD20SPZX",
    // Toshiba P300 / DT02, L200 / MQ04.
    "DT02ABA400", "DT02ABA600", "HDWD240", "HDWD260", "MQ04ABF100", "MQ04ABD200",
];

/// ATA IDENTIFY DEVICE word 69 bits 1:0 (ZONED CAPABILITIES): 1 host-aware,
/// 2 device-managed, 0 not reported (3 reserved). `buf` is the 512-byte
/// block; None when short or word 69 is not valid (0xFFFF).
pub fn ata_zoned(buf: &[u8]) -> Option<u8> {
    if buf.len() < 512 {
        return None;
    }
    let w69 = u16::from_le_bytes([buf[138], buf[139]]);
    (w69 != 0xFFFF).then_some((w69 & 0x3) as u8)
}

/// VPD 0xB1 byte 8 bits 5:4 (ZONED, SBC-4): 1 host-aware, 2 device-managed.
pub fn b1_zoned(raw: &[u8]) -> Option<u8> {
    if raw.len() < 9 || raw[1] != 0xB1 {
        return None;
    }
    Some((raw[8] >> 4) & 0x3)
}

fn from_field(v: u8) -> Option<Smr> {
    match v {
        1 => Some(Smr::HostAware),
        2 => Some(Smr::DriveManaged),
        _ => None,
    }
}

/// Is `model` on the known drive-managed list?
pub fn known_model(model: &str) -> bool {
    let m = model.trim().to_ascii_uppercase();
    KNOWN_DM_SMR.iter().any(|k| m.contains(k))
}

/// The decision, strongest source first: the kernel's `queue/zoned`, then
/// IDENTIFY word 69, VPD 0xB1, the model list.
pub fn detect(queue_zoned: Option<&str>, supports: Option<&crate::pi::Supports>, model: &str) -> SmrInfo {
    let at = |smr, source: &str| SmrInfo { smr, source: source.into() };
    match queue_zoned.map(str::trim) {
        Some("host-managed") => return at(Smr::HostManaged, "queue/zoned"),
        Some("host-aware") => return at(Smr::HostAware, "queue/zoned"),
        _ => {}
    }
    if let Some(s) = supports.and_then(|s| s.ata_zoned).and_then(from_field) {
        return at(s, "ATA IDENTIFY word 69");
    }
    if let Some(s) = supports.and_then(|s| s.vpd_zoned).and_then(from_field) {
        return at(s, "VPD 0xB1");
    }
    if known_model(model) {
        return at(Smr::DriveManaged, "known model");
    }
    at(Smr::None, "nothing reports zoning")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pi::Supports;

    fn identify(w69: u16) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[138..140].copy_from_slice(&w69.to_le_bytes());
        b
    }

    #[test]
    fn word_69_and_vpd_b1_decode() {
        assert_eq!(ata_zoned(&identify(0x0002)), Some(2));
        assert_eq!(ata_zoned(&identify(0x4001)), Some(1), "other bits ignored");
        assert_eq!(ata_zoned(&identify(0xFFFF)), None);
        assert_eq!(ata_zoned(&[0; 100]), None);
        let mut b1 = vec![0u8; 64];
        b1[1] = 0xB1;
        b1[8] = 0x20;
        assert_eq!(b1_zoned(&b1), Some(2));
        b1[8] = 0x10;
        assert_eq!(b1_zoned(&b1), Some(1));
        b1[1] = 0xB0;
        assert_eq!(b1_zoned(&b1), None);
    }

    /// server8's sdb: zoned none to the kernel, nothing in word 69, on the list.
    #[test]
    fn server8s_seagate_is_drive_managed_by_its_model() {
        let s = Supports { ata_zoned: Some(0), vpd_zoned: Some(0), ..Default::default() };
        let i = detect(Some("none"), Some(&s), "ST2000DM008-2UB1");
        assert_eq!(i, SmrInfo { smr: Smr::DriveManaged, source: "known model".into() });
        assert_eq!(serde_json::to_value(i.smr).unwrap(), "drive-managed");
        assert!(known_model("WDC WD40EFAX-68JH4N0"));
        assert!(!known_model("Patriot P210 512GB"));
    }

    #[test]
    fn the_strongest_source_decides() {
        let dm = Supports { ata_zoned: Some(2), ..Default::default() };
        assert_eq!(detect(Some("host-managed"), Some(&dm), "x").smr, Smr::HostManaged);
        assert_eq!(detect(Some("none"), Some(&dm), "x"), SmrInfo { smr: Smr::DriveManaged, source: "ATA IDENTIFY word 69".into() });
        let ha = Supports { vpd_zoned: Some(1), ..Default::default() };
        assert_eq!(detect(None, Some(&ha), "x"), SmrInfo { smr: Smr::HostAware, source: "VPD 0xB1".into() });
        assert_eq!(detect(Some("none"), None, "Patriot P210 512GB"), SmrInfo { smr: Smr::None, source: "nothing reports zoning".into() });
    }
}
