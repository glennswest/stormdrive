//! What a SCSI drive can be formatted to, and which format to give it
//! (#82, #85). Read-only: INQUIRY, a few VPD pages, MODE SENSE, READ
//! CAPACITY(16) — nothing that changes the drive.
//!
//! - Standard INQUIRY byte 5 bit 0, PROTECT: the drive does T10 protection
//!   information at all.
//! - VPD 0x86 Extended INQUIRY, SPT: which PI types (1, 2, 3).
//! - VPD 0xB4 Supported Block Lengths and Protection Types (SBC-4): each
//!   logical block length the drive can be formatted to, with the PI types
//!   at that length (T0PS..T3PS). Optional; many drives lack it.
//! - VPD 0xB1 Block Device Characteristics: rotation rate (1 = non-rotating).
//! - MODE SENSE(10) block descriptor: the block length it is set to.
//! - READ CAPACITY(16): the current block length, PROT_EN and P_TYPE.
//!
//! The plan (pure): the format asked for when the drive offers it, else
//! 4096+PI type 1 → 512+PI type 1 → 4096 → 512, the first it offers (#82's
//! rule). Without VPD 0xB4 the only lengths taken as offered are 512 (any
//! SCSI disk; a 520-byte drive is 512 + 8) and, with PI type 1 in SPT,
//! 512+PI1 — 4096 is never assumed, so a plan never asks for what the
//! drive did not say it can do.

use serde::{Deserialize, Serialize};

/// Protection information to format with. Type 2 and 3 are not offered:
/// nothing above the drive (the kernel's DIF, stormblock) wants them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protection {
    #[default]
    None,
    Type1,
}

impl Protection {
    /// READ CAPACITY(16)'s protection type as our `Capacity::prot_type`
    /// reads it: 0 = off, else P_TYPE + 1.
    pub fn prot_type(self) -> u8 {
        match self {
            Protection::None => 0,
            Protection::Type1 => 1,
        }
    }
    /// FORMAT UNIT FMTPINFO (CDB byte 1 bits 7:6), with PFU 000b in the
    /// parameter list: 00b = no PI, 10b = type 1.
    pub fn fmtpinfo(self) -> u8 {
        match self {
            Protection::None => 0,
            Protection::Type1 => 0b10,
        }
    }
    pub fn word(self) -> &'static str {
        match self {
            Protection::None => "no PI",
            Protection::Type1 => "PI type 1",
        }
    }
}

/// One length VPD 0xB4 lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockLength {
    pub length: u32,
    /// PI types at this length; 0 = without PI.
    pub pi_types: Vec<u8>,
}

/// What the drive says it is and can be (`Drive.supports`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Supports {
    /// INQUIRY PROTECT.
    pub protect: bool,
    /// PI types from VPD 0x86 SPT (empty without PROTECT or the page).
    pub pi_types: Vec<u8>,
    /// From VPD 0xB4; empty = the drive does not list them.
    pub block_lengths: Vec<BlockLength>,
    /// VPD 0xB1: rpm; None = not reported or non-rotating.
    pub rotation_rpm: Option<u16>,
    /// VPD 0xB1 rotation rate 1.
    pub non_rotating: Option<bool>,
    /// The VPD pages the drive lists (0x00).
    pub vpd_pages: Vec<u8>,
    /// Now: the block length and PI type (0 = off) READ CAPACITY reports.
    pub current_block_size: u32,
    pub current_prot_type: u8,
    /// The block length MODE SENSE's block descriptor holds.
    pub mode_block_length: Option<u32>,
}

/// The format a drive should get (`status.plannedFormat`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Planned {
    pub block_size: u32,
    pub protection: Protection,
    /// `vpd_b4` (the drive listed it) or `inferred` (no VPD 0xB4).
    pub basis: String,
    pub reason: String,
    /// Not the first choice (4096+PI1) and not asked for: the drive does
    /// not offer the default. An issue to show, never a quiet fallback
    /// (owner, #82).
    #[serde(default)]
    pub fallback: bool,
}

/// SPT (VPD 0x86 byte 4 bits 5:3) → PI types.
pub fn spt_types(spt: u8) -> Vec<u8> {
    match spt & 0x07 {
        0b000 => vec![1],
        0b001 => vec![1, 2],
        0b010 => vec![2],
        0b011 => vec![1, 3],
        0b100 => vec![3],
        0b101 => vec![2, 3],
        0b111 => vec![1, 2, 3],
        _ => vec![],
    }
}

/// Standard INQUIRY → PROTECT.
pub fn parse_protect(inquiry: &[u8]) -> bool {
    inquiry.len() > 5 && inquiry[5] & 0x01 != 0
}

/// VPD 0x00 → page codes.
pub fn parse_vpd_list(raw: &[u8]) -> Vec<u8> {
    if raw.len() < 4 || raw[1] != 0x00 {
        return vec![];
    }
    let n = u16::from_be_bytes([raw[2], raw[3]]) as usize;
    raw[4..(4 + n).min(raw.len())].to_vec()
}

/// VPD 0x86 → SPT types.
pub fn parse_extended_inquiry(raw: &[u8]) -> Vec<u8> {
    if raw.len() < 5 || raw[1] != 0x86 {
        return vec![];
    }
    spt_types((raw[4] >> 3) & 0x07)
}

/// VPD 0xB4 → lengths with their PI types (byte 5: T3PS T2PS T1PS T0PS).
pub fn parse_block_lengths(raw: &[u8]) -> Vec<BlockLength> {
    if raw.len() < 4 || raw[1] != 0xB4 {
        return vec![];
    }
    let end = (4 + u16::from_be_bytes([raw[2], raw[3]]) as usize).min(raw.len());
    let mut out = vec![];
    let mut off = 4;
    while off + 8 <= end {
        let d = &raw[off..off + 8];
        let length = u32::from_be_bytes([d[0], d[1], d[2], d[3]]);
        let pi_types = (0..4u8).filter(|t| d[5] & (1 << t) != 0).collect();
        if length > 0 {
            out.push(BlockLength { length, pi_types });
        }
        off += 8;
    }
    out
}

/// VPD 0xB1 → (rpm, non-rotating).
pub fn parse_rotation(raw: &[u8]) -> (Option<u16>, Option<bool>) {
    if raw.len() < 6 || raw[1] != 0xB1 {
        return (None, None);
    }
    match u16::from_be_bytes([raw[4], raw[5]]) {
        0 => (None, None),
        1 => (None, Some(true)),
        r => (Some(r), Some(false)),
    }
}

/// The block length in a MODE SENSE(10) block descriptor (short: bytes
/// 5..8; long LBA: 12..16).
pub fn descriptor_block_length(bd: &[u8], long_lba: bool) -> Option<u32> {
    if long_lba {
        (bd.len() >= 16).then(|| u32::from_be_bytes([bd[12], bd[13], bd[14], bd[15]]))
    } else {
        (bd.len() >= 8).then(|| u32::from_be_bytes([0, bd[5], bd[6], bd[7]]))
    }
}

impl Supports {
    /// Does the drive offer `length` with `prot`? (Some(basis) when it
    /// does: `vpd_b4` or `inferred`.)
    pub fn offers(&self, length: u32, prot: Protection) -> Option<&'static str> {
        if !self.block_lengths.is_empty() {
            let e = self.block_lengths.iter().find(|b| b.length == length)?;
            let ok = match prot {
                // A listed length with no PI bits at all still formats
                // without PI; T0PS is not always set.
                Protection::None => e.pi_types.is_empty() || e.pi_types.contains(&0),
                Protection::Type1 => self.protect && e.pi_types.contains(&1),
            };
            return ok.then_some("vpd_b4");
        }
        match (length, prot) {
            (512, Protection::None) => Some("inferred"),
            (512, Protection::Type1) if self.protect && self.pi_types.contains(&1) => Some("inferred"),
            _ => None,
        }
    }

    pub fn protection_now(&self) -> Option<Protection> {
        match self.current_prot_type {
            0 => Some(Protection::None),
            1 => Some(Protection::Type1),
            _ => None,
        }
    }
}

/// The order a drive is offered formats in when nothing is asked (#82).
pub const PREFERENCE: [(u32, Protection); 4] =
    [(4096, Protection::Type1), (512, Protection::Type1), (4096, Protection::None), (512, Protection::None)];

/// The format for a drive: `want` when it offers it, else the preference.
pub fn plan(s: &Supports, want: Option<(u32, Protection)>) -> Result<Planned, String> {
    if let Some((len, prot)) = want {
        return match s.offers(len, prot) {
            Some(basis) => Ok(Planned { block_size: len, protection: prot, basis: basis.into(), reason: "asked for (spec.format)".into(), fallback: false }),
            None => Err(format!("the drive does not offer {len} with {}{}", prot.word(), unknown_hint(s))),
        };
    }
    for (i, (len, prot)) in PREFERENCE.into_iter().enumerate() {
        if let Some(basis) = s.offers(len, prot) {
            let mut reason = "first the drive offers in 4096+PI1, 512+PI1, 4096, 512".to_string();
            if basis == "inferred" {
                reason.push_str(" (no VPD 0xB4: 4096 not assumed)");
            }
            if i > 0 {
                reason = format!("4096 with PI type 1 not offered{}; {reason}", unknown_hint(s));
            }
            return Ok(Planned { block_size: len, protection: prot, basis: basis.into(), reason, fallback: i > 0 });
        }
    }
    Err(format!("the drive offers none of 4096/512 with or without PI{}", unknown_hint(s)))
}

fn unknown_hint(s: &Supports) -> &'static str {
    if s.block_lengths.is_empty() {
        " (it lists no block lengths, VPD 0xB4; only 512 is assumed)"
    } else {
        ""
    }
}

/// The drive's own rotation rate (VPD 0xB1) over the kernel's
/// `queue/rotational`: sd never reads 0xB1 from a drive it could not attach
/// (a 520-byte drive), and `rotational` then reads 0 — a 10K disk shown as
/// an SSD (#82). Unreported rotation keeps the kernel's answer.
pub fn kind_by_rotation(kind: crate::drive::DriveKind, s: Option<&Supports>) -> crate::drive::DriveKind {
    use crate::drive::DriveKind as K;
    let rotating = match s {
        Some(s) if s.rotation_rpm.is_some() => true,
        Some(s) if s.non_rotating == Some(true) => false,
        _ => return kind,
    };
    match (kind, rotating) {
        (K::SasSsd | K::SasHdd, true) => K::SasHdd,
        (K::SasSsd | K::SasHdd, false) => K::SasSsd,
        (K::SataSsd | K::SataHdd, true) => K::SataHdd,
        (K::SataSsd | K::SataHdd, false) => K::SataSsd,
        (k, _) => k,
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::scsi::{cdb, Device, Dir};

    /// Every read the plan needs. Fails only when the device cannot be
    /// opened or READ CAPACITY(16) fails (mid-format).
    pub fn probe(path: &str) -> Result<Supports, String> {
        let dev = Device::open(path).map_err(|e| format!("open {path}: {e}"))?;
        let _ = dev.test_unit_ready(); // clear a unit attention
        let mut inq = vec![0u8; 96];
        let n = dev.io(&cdb::inquiry(None, 96), Dir::FromDevice, &mut inq, crate::scsi::T_SHORT).map_err(|e| format!("inquiry: {e}"))?;
        inq.truncate(n);
        let cap = dev.read_capacity16().map_err(|e| format!("read capacity: {e}"))?;
        let vpd_pages = dev.vpd(0x00).map(|r| parse_vpd_list(&r)).unwrap_or_default();
        let has = |p: u8| vpd_pages.contains(&p);
        let protect = parse_protect(&inq);
        let pi_types = if protect && has(0x86) { dev.vpd(0x86).map(|r| parse_extended_inquiry(&r)).unwrap_or_default() } else { vec![] };
        let block_lengths = if has(0xB4) { dev.vpd(0xB4).map(|r| parse_block_lengths(&r)).unwrap_or_default() } else { vec![] };
        let (rotation_rpm, non_rotating) = if has(0xB1) { dev.vpd(0xB1).map(|r| parse_rotation(&r)).unwrap_or((None, None)) } else { (None, None) };
        let mode_block_length = dev
            .mode_sense10(0x01)
            .ok()
            .and_then(|h| h.block_descriptor.as_ref().and_then(|bd| descriptor_block_length(bd, h.long_lba)));
        Ok(Supports {
            protect,
            pi_types,
            block_lengths,
            rotation_rpm,
            non_rotating,
            vpd_pages,
            current_block_size: cap.block_len,
            current_prot_type: cap.prot_type,
            mode_block_length,
        })
    }
}

/// Probe a SCSI drive (its sg node or block node). Linux only.
pub fn probe(path: &str) -> Result<Supports, String> {
    #[cfg(target_os = "linux")]
    {
        linux::probe(path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Err("probing a drive requires Linux".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st1200(b4: Vec<BlockLength>) -> Supports {
        Supports { protect: true, pi_types: vec![1, 2], block_lengths: b4, current_block_size: 520, ..Default::default() }
    }

    #[test]
    fn spt_and_pages() {
        assert_eq!(spt_types(0b000), vec![1]);
        assert_eq!(spt_types(0b111), vec![1, 2, 3]);
        assert!(spt_types(0b110).is_empty());
        let mut inq = vec![0u8; 36];
        inq[5] = 0x01;
        assert!(parse_protect(&inq));
        let x86 = [0, 0x86, 0, 60, 0b0000_1000, 0];
        assert_eq!(parse_extended_inquiry(&x86), vec![1, 2], "SPT 001b");
        assert_eq!(parse_vpd_list(&[0, 0, 0, 3, 0x00, 0x80, 0xB4]), vec![0, 0x80, 0xB4]);
        assert_eq!(parse_rotation(&[0, 0xB1, 0, 60, 0x27, 0x10]), (Some(10000), Some(false)));
        assert_eq!(parse_rotation(&[0, 0xB1, 0, 60, 0, 1]), (None, Some(true)));
    }

    #[test]
    fn block_lengths_page() {
        let mut p = vec![0, 0xB4, 0, 24];
        p.extend_from_slice(&[0, 0, 2, 0, 0, 0b0011, 0, 0]); // 512: T0PS T1PS
        p.extend_from_slice(&[0, 0, 2, 8, 0, 0b0001, 0, 0]); // 520: T0PS
        p.extend_from_slice(&[0, 0, 16, 0, 0, 0b0011, 0, 0]); // 4096: T0 T1
        let b = parse_block_lengths(&p);
        assert_eq!(b.len(), 3);
        assert_eq!(b[0], BlockLength { length: 512, pi_types: vec![0, 1] });
        assert_eq!(b[2].length, 4096);
    }

    #[test]
    fn mode_descriptor_length() {
        assert_eq!(descriptor_block_length(&[0, 0, 0, 0, 0, 0, 2, 8], false), Some(520));
        let mut long = vec![0u8; 16];
        long[12..16].copy_from_slice(&4096u32.to_be_bytes());
        assert_eq!(descriptor_block_length(&long, true), Some(4096));
        assert_eq!(descriptor_block_length(&[0; 4], false), None);
    }

    #[test]
    fn plan_prefers_4096_pi1_then_512_pi1() {
        let all = st1200(vec![
            BlockLength { length: 512, pi_types: vec![0, 1] },
            BlockLength { length: 4096, pi_types: vec![0, 1] },
        ]);
        let p = plan(&all, None).unwrap();
        assert_eq!((p.block_size, p.protection, p.basis.as_str()), (4096, Protection::Type1, "vpd_b4"));
        assert!(!p.fallback);

        let no4k = st1200(vec![BlockLength { length: 512, pi_types: vec![0, 1] }, BlockLength { length: 520, pi_types: vec![0] }]);
        assert_eq!(plan(&no4k, None).unwrap().block_size, 512);
        assert!(plan(&no4k, None).unwrap().fallback, "not the default: flagged, not quiet");
        assert!(plan(&no4k, None).unwrap().reason.starts_with("4096 with PI type 1 not offered"));
        assert!(!plan(&no4k, Some((512, Protection::Type1))).unwrap().fallback, "asked for");
        assert_eq!(plan(&no4k, None).unwrap().protection, Protection::Type1);
        assert!(plan(&no4k, Some((4096, Protection::None))).is_err(), "not offered → refused, never guessed");
    }

    #[test]
    fn plan_without_vpd_b4_never_assumes_4096() {
        let p = plan(&st1200(vec![]), None).unwrap();
        assert_eq!((p.block_size, p.protection, p.basis.as_str()), (512, Protection::Type1, "inferred"));
        let no_pi = Supports { current_block_size: 520, ..Default::default() };
        let p = plan(&no_pi, None).unwrap();
        assert_eq!((p.block_size, p.protection), (512, Protection::None));
        let e = plan(&no_pi, Some((4096, Protection::Type1))).unwrap_err();
        assert!(e.contains("VPD 0xB4"), "{e}");
        assert!(plan(&no_pi, Some((512, Protection::Type1))).is_err(), "no PROTECT → no PI");
    }

    #[test]
    fn rotation_decides_the_kind() {
        use crate::drive::DriveKind as K;
        let hdd = Supports { rotation_rpm: Some(10000), non_rotating: Some(false), ..Default::default() };
        let ssd = Supports { non_rotating: Some(true), ..Default::default() };
        assert_eq!(kind_by_rotation(K::SasSsd, Some(&hdd)), K::SasHdd, "the ST1200MM0098 at 520");
        assert_eq!(kind_by_rotation(K::SasHdd, Some(&ssd)), K::SasSsd);
        assert_eq!(kind_by_rotation(K::SataSsd, Some(&hdd)), K::SataHdd);
        assert_eq!(kind_by_rotation(K::SasSsd, Some(&Supports::default())), K::SasSsd, "not reported: the kernel's");
        assert_eq!(kind_by_rotation(K::SasSsd, None), K::SasSsd);
        assert_eq!(kind_by_rotation(K::NvmeSsd, Some(&hdd)), K::NvmeSsd);
    }

    #[test]
    fn protection_bits() {
        assert_eq!(Protection::Type1.fmtpinfo(), 0b10);
        assert_eq!(Protection::Type1.prot_type(), 1);
        assert_eq!(Protection::None.prot_type(), 0);
        assert_eq!(serde_json::to_value(Protection::Type1).unwrap(), "type1");
    }
}
