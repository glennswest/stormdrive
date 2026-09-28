//! The drive worker's low-level steps beyond SCSI FORMAT UNIT (#5):
//! NVMe Format NVM (with the LBA format picked by data size), NVMe
//! Sanitize, and SCSI SANITIZE (which a SAT layer maps to ATA SANITIZE for
//! SATA drives behind a SAS HBA).
//!
//! Command building and response parsing are portable and unit-tested;
//! issuing them is Linux-only. Every function here destroys the drive's
//! data: the worker decides whether it may run (`worker::guard`).

use serde::{Deserialize, Serialize};

/// How a sanitize erases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SanitizeMethod {
    /// Every block erased (NVMe SANACT 2, SCSI 0x02).
    Block,
    /// The media encryption key replaced (NVMe SANACT 4, SCSI 0x03).
    Crypto,
    /// Every block overwritten once with zeros (NVMe SANACT 3, SCSI 0x01).
    Overwrite,
}

// ------------------------------------------------------------------ NVMe

/// One LBA format of a namespace (Identify Namespace, LBAF table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LbaFormat {
    pub index: u8,
    pub data_size: u32,
    pub metadata_size: u16,
    /// 0 best … 3 degraded (Relative Performance).
    pub relative_performance: u8,
}

/// Identify Namespace (CNS 0): the LBA formats and which one is in use.
pub fn parse_identify_ns(buf: &[u8]) -> Option<(Vec<LbaFormat>, u8)> {
    if buf.len() < 4096 {
        return None;
    }
    let nlbaf = buf[25] as usize + 1;
    let flbas = buf[26];
    let current = (flbas & 0x0F) | ((flbas >> 1) & 0x30);
    let mut out = vec![];
    for i in 0..nlbaf.min(64) {
        let d = u32::from_le_bytes(buf[128 + 4 * i..132 + 4 * i].try_into().unwrap());
        let lbads = (d >> 16) & 0xFF;
        if !(9..=16).contains(&lbads) {
            continue; // unused / reserved entry
        }
        out.push(LbaFormat {
            index: i as u8,
            data_size: 1 << lbads,
            metadata_size: (d & 0xFFFF) as u16,
            relative_performance: ((d >> 24) & 0x3) as u8,
        });
    }
    Some((out, current))
}

/// The LBA format to format to: `data_size` bytes, no metadata, best
/// relative performance.
pub fn pick_lba_format(formats: &[LbaFormat], data_size: u32) -> Option<LbaFormat> {
    formats
        .iter()
        .filter(|f| f.data_size == data_size && f.metadata_size == 0)
        .min_by_key(|f| (f.relative_performance, f.index))
        .copied()
}

/// What Identify Controller (CNS 1) says about erasing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct NvmeCaps {
    pub format_supported: bool,
    /// Format applies to every namespace on the controller (FNA bit 0).
    pub format_all_namespaces: bool,
    pub sanitize_crypto: bool,
    pub sanitize_block: bool,
    pub sanitize_overwrite: bool,
    /// Number of namespaces the controller supports (NN).
    pub namespaces: u32,
}

pub fn parse_identify_ctrl(buf: &[u8]) -> Option<NvmeCaps> {
    if buf.len() < 4096 {
        return None;
    }
    let oacs = u16::from_le_bytes([buf[256], buf[257]]);
    let sanicap = u32::from_le_bytes(buf[328..332].try_into().unwrap());
    let nn = u32::from_le_bytes(buf[516..520].try_into().unwrap());
    Some(NvmeCaps {
        format_supported: oacs & 0x2 != 0,
        format_all_namespaces: buf[524] & 0x1 != 0,
        sanitize_crypto: sanicap & 0x1 != 0,
        sanitize_block: sanicap & 0x2 != 0,
        sanitize_overwrite: sanicap & 0x4 != 0,
        namespaces: nn,
    })
}

/// Format NVM (0x80) CDW10: the LBA format, no metadata, no protection
/// information, no secure erase (the worker's `sanitize` step is the erase).
pub fn format_nvm_cdw10(lbaf: u8) -> u32 {
    (lbaf as u32 & 0x0F) | (((lbaf as u32 >> 4) & 0x3) << 12)
}

/// Sanitize (0x84) CDW10: the action, unrestricted exit on failure (AUSE),
/// one overwrite pass.
pub fn sanitize_cdw10(m: SanitizeMethod) -> u32 {
    let sanact = match m {
        SanitizeMethod::Block => 2,
        SanitizeMethod::Overwrite => 3,
        SanitizeMethod::Crypto => 4,
    };
    let owpass = if m == SanitizeMethod::Overwrite { 1 << 4 } else { 0 };
    sanact | (1 << 3) | owpass
}

pub fn nvme_supports(c: &NvmeCaps, m: SanitizeMethod) -> bool {
    match m {
        SanitizeMethod::Block => c.sanitize_block,
        SanitizeMethod::Crypto => c.sanitize_crypto,
        SanitizeMethod::Overwrite => c.sanitize_overwrite,
    }
}

/// Where a sanitize is, from the Sanitize Status log (LID 0x81).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SanitizeStatus {
    Never,
    Done,
    Running { pct: u8 },
    Failed,
}

pub fn parse_sanitize_log(buf: &[u8]) -> Option<SanitizeStatus> {
    if buf.len() < 4 {
        return None;
    }
    let sprog = u16::from_le_bytes([buf[0], buf[1]]) as u32;
    let sstat = u16::from_le_bytes([buf[2], buf[3]]);
    Some(match sstat & 0x7 {
        0 => SanitizeStatus::Never,
        1 | 4 => SanitizeStatus::Done,
        2 => SanitizeStatus::Running { pct: ((sprog * 100) / 65536) as u8 },
        _ => SanitizeStatus::Failed,
    })
}

/// `nvme3n1` → (`nvme3`, 1): the controller and namespace id.
pub fn nvme_names(name: &str) -> Option<(String, u32)> {
    let (ctrl, ns) = name.strip_prefix("nvme")?.rsplit_once('n')?;
    if ctrl.is_empty() || !ctrl.chars().all(|c| c.is_ascii_digit()) {
        return None; // not a namespace head (nvme0c1n1 is a path node)
    }
    Some((format!("nvme{ctrl}"), ns.parse().ok()?))
}

// ------------------------------------------------------------------ SCSI

/// SANITIZE (0x48) with IMMED and AUSE; a parameter list only for
/// overwrite.
pub fn scsi_sanitize(m: SanitizeMethod) -> ([u8; 10], Vec<u8>) {
    let sa = match m {
        SanitizeMethod::Overwrite => 0x01,
        SanitizeMethod::Block => 0x02,
        SanitizeMethod::Crypto => 0x03,
    };
    // One pass of a 4-byte zero pattern.
    let param = if m == SanitizeMethod::Overwrite { vec![0x01, 0x00, 0x00, 0x04, 0, 0, 0, 0] } else { vec![] };
    let len = param.len() as u16;
    let mut cdb = [0u8; 10];
    cdb[0] = 0x48;
    cdb[1] = 0x80 | 0x20 | sa;
    cdb[7..9].copy_from_slice(&len.to_be_bytes());
    (cdb, param)
}

/// A TEST UNIT READY answer during a SCSI sanitize: Ok(None) done,
/// Ok(Some(pct)) still going, Err failed.
pub fn interpret_sanitize_tur(r: &Result<(), crate::scsi::Error>) -> Result<Option<Option<u8>>, String> {
    use crate::scsi::Error;
    match r {
        Ok(()) => Ok(None),
        Err(Error::Sense(s)) if s.key == 0x2 && s.asc == 0x04 && s.ascq == 0x1B => Ok(Some(s.progress_pct())),
        Err(Error::Sense(s)) if s.key == 0x2 && s.asc == 0x04 && s.ascq <= 0x01 => Ok(Some(None)),
        Err(Error::Sense(s)) if s.is_unit_attention() => Ok(Some(None)),
        // 31/03: SANITIZE COMMAND FAILED.
        Err(Error::Sense(s)) if s.asc == 0x31 => Err(format!("sanitize failed: {s}")),
        Err(e) => Err(format!("{e}")),
    }
}

/// Progress callback: (percent when known, phase).
pub type Progress<'a> = &'a (dyn Fn(Option<u8>, &str) + Sync);

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::smart::nvme::linux::admin;
    use std::time::{Duration, Instant};

    const T_ADMIN: u32 = 30_000;
    /// Format NVM blocks until the drive is done; on a large SSD with a
    /// secure-erase-free format that is seconds to minutes.
    const T_FORMAT: u32 = 4 * 60 * 60 * 1000;
    const POLL: Duration = Duration::from_secs(5);
    const MAX_WAIT: Duration = Duration::from_secs(48 * 60 * 60);

    fn identify(path: &str, cns: u32, nsid: u32) -> Result<Vec<u8>, String> {
        let mut buf = vec![0u8; 4096];
        let st = admin(path, 0x06, nsid, cns, 0, &mut buf, T_ADMIN).map_err(|e| format!("identify: {e}"))?;
        if st != 0 {
            return Err(format!("identify cns {cns}: status 0x{st:x}"));
        }
        Ok(buf)
    }

    pub fn nvme_caps(path: &str) -> Result<NvmeCaps, String> {
        parse_identify_ctrl(&identify(path, 1, 0)?).ok_or_else(|| "identify controller: short".into())
    }

    pub fn nvme_format(path: &str, name: &str, data_size: u32, progress: Progress) -> Result<u32, String> {
        let (_, nsid) = nvme_names(name).ok_or_else(|| format!("{name}: not an NVMe namespace name"))?;
        let caps = nvme_caps(path)?;
        if !caps.format_supported {
            return Err("controller does not support Format NVM".into());
        }
        let (formats, current) = parse_identify_ns(&identify(path, 0, nsid)?).ok_or("identify namespace: short")?;
        let f = pick_lba_format(&formats, data_size).ok_or_else(|| {
            format!(
                "no {data_size}-byte LBA format without metadata (has: {})",
                formats.iter().map(|f| format!("{}+{}", f.data_size, f.metadata_size)).collect::<Vec<_>>().join(", ")
            )
        })?;
        progress(None, &format!("format nvm lbaf {} (was {current})", f.index));
        let target = if caps.format_all_namespaces { 0xFFFF_FFFF } else { nsid };
        let st = admin(path, 0x80, target, format_nvm_cdw10(f.index), 0, &mut [], T_FORMAT).map_err(|e| format!("format nvm: {e}"))?;
        if st != 0 {
            return Err(format!("format nvm: status 0x{st:x}"));
        }
        Ok(f.data_size)
    }

    pub fn nvme_sanitize(path: &str, m: SanitizeMethod, progress: Progress) -> Result<(), String> {
        let caps = nvme_caps(path)?;
        if !nvme_supports(&caps, m) {
            return Err(format!("controller does not support {m:?} sanitize"));
        }
        let st = admin(path, 0x84, 0, sanitize_cdw10(m), 0, &mut [], T_ADMIN).map_err(|e| format!("sanitize: {e}"))?;
        if st != 0 {
            return Err(format!("sanitize: status 0x{st:x}"));
        }
        let start = Instant::now();
        loop {
            std::thread::sleep(POLL);
            let mut log = [0u8; 512];
            let cdw10 = 0x81 | ((512 / 4 - 1) << 16);
            match admin(path, 0x02, 0xFFFF_FFFF, cdw10, 0, &mut log, T_ADMIN) {
                Ok(0) => match parse_sanitize_log(&log) {
                    Some(SanitizeStatus::Done) => return Ok(()),
                    Some(SanitizeStatus::Failed) => return Err("sanitize failed (log 0x81)".into()),
                    Some(SanitizeStatus::Running { pct }) => progress(Some(pct), "sanitizing"),
                    _ => progress(None, "sanitizing"),
                },
                // Commands are refused while a sanitize runs on some drives.
                _ => progress(None, "sanitizing"),
            }
            if start.elapsed() > MAX_WAIT {
                return Err("sanitize did not finish in 48 h".into());
            }
        }
    }

    pub fn scsi_sanitize_run(sg: &str, m: SanitizeMethod, progress: Progress) -> Result<(), String> {
        use crate::scsi::{Device, Dir, Error};
        let dev = Device::open(sg).map_err(|e| format!("open: {e}"))?;
        let _ = dev.test_unit_ready();
        let (cdb, mut param) = scsi_sanitize(m);
        let dir = if param.is_empty() { Dir::None } else { Dir::ToDevice };
        match dev.io(&cdb, dir, &mut param, crate::scsi::T_SHORT) {
            Ok(_) => {}
            Err(Error::Sense(s)) if s.is_illegal_request() => {
                return Err(format!("the drive (or its SAT layer) does not support {m:?} SANITIZE: {s}"))
            }
            Err(e) => return Err(format!("sanitize: {e}")),
        }
        progress(None, "sanitizing");
        let start = Instant::now();
        loop {
            std::thread::sleep(POLL);
            match interpret_sanitize_tur(&dev.test_unit_ready()) {
                Ok(None) => return Ok(()),
                Ok(Some(pct)) => progress(pct, "sanitizing"),
                Err(e) => return Err(e),
            }
            if start.elapsed() > MAX_WAIT {
                return Err("sanitize did not finish in 48 h".into());
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::{nvme_caps, nvme_format, nvme_sanitize, scsi_sanitize_run};

#[cfg(not(target_os = "linux"))]
pub fn nvme_caps(_: &str) -> Result<NvmeCaps, String> {
    Err("NVMe admin commands are Linux-only".into())
}
#[cfg(not(target_os = "linux"))]
pub fn nvme_format(_: &str, _: &str, _: u32, _: Progress) -> Result<u32, String> {
    Err("NVMe admin commands are Linux-only".into())
}
#[cfg(not(target_os = "linux"))]
pub fn nvme_sanitize(_: &str, _: SanitizeMethod, _: Progress) -> Result<(), String> {
    Err("NVMe admin commands are Linux-only".into())
}
#[cfg(not(target_os = "linux"))]
pub fn scsi_sanitize_run(_: &str, _: SanitizeMethod, _: Progress) -> Result<(), String> {
    Err("SG_IO is Linux-only".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns_page(nlbaf: u8, flbas: u8, lbafs: &[(u16, u8, u8)]) -> Vec<u8> {
        let mut b = vec![0u8; 4096];
        b[25] = nlbaf;
        b[26] = flbas;
        for (i, &(ms, lbads, rp)) in lbafs.iter().enumerate() {
            let d = ms as u32 | (lbads as u32) << 16 | (rp as u32) << 24;
            b[128 + 4 * i..132 + 4 * i].copy_from_slice(&d.to_le_bytes());
        }
        b
    }

    #[test]
    fn lba_formats_and_the_pick() {
        // 512, 512+8, 4096, 4096+8, 4096 at worse performance.
        let page = ns_page(4, 0, &[(0, 9, 2), (8, 9, 2), (0, 12, 0), (8, 12, 0), (0, 12, 1)]);
        let (f, cur) = parse_identify_ns(&page).unwrap();
        assert_eq!(cur, 0);
        assert_eq!(f.len(), 5);
        assert_eq!(pick_lba_format(&f, 4096).unwrap().index, 2, "4K, no metadata, best performance");
        assert_eq!(pick_lba_format(&f, 512).unwrap().index, 0);
        assert!(pick_lba_format(&f, 520).is_none());
        // FLBAS upper bits (index 17 = 0x11: bits 3:0 = 1, bits 6:5 = 1).
        let page = ns_page(0, 0x21, &[(0, 9, 0)]);
        assert_eq!(parse_identify_ns(&page).unwrap().1, 0x11);
        assert!(parse_identify_ns(&[0u8; 100]).is_none());
    }

    #[test]
    fn controller_caps() {
        let mut b = vec![0u8; 4096];
        b[256] = 0x2; // OACS: Format NVM
        b[328] = 0x5; // SANICAP: crypto + overwrite
        b[516..520].copy_from_slice(&32u32.to_le_bytes());
        b[524] = 0x1;
        let c = parse_identify_ctrl(&b).unwrap();
        assert!(c.format_supported && c.format_all_namespaces);
        assert!(c.sanitize_crypto && !c.sanitize_block && c.sanitize_overwrite);
        assert_eq!(c.namespaces, 32);
        assert!(nvme_supports(&c, SanitizeMethod::Crypto) && !nvme_supports(&c, SanitizeMethod::Block));
    }

    #[test]
    fn nvme_command_words() {
        assert_eq!(format_nvm_cdw10(2), 2);
        assert_eq!(format_nvm_cdw10(0x11), 0x1 | (1 << 12), "upper LBAF bits go to 13:12");
        assert_eq!(sanitize_cdw10(SanitizeMethod::Block), 2 | 8);
        assert_eq!(sanitize_cdw10(SanitizeMethod::Crypto), 4 | 8);
        assert_eq!(sanitize_cdw10(SanitizeMethod::Overwrite), 3 | 8 | 16);
    }

    #[test]
    fn sanitize_log() {
        let log = |sprog: u16, sstat: u16| {
            let mut b = [0u8; 512];
            b[0..2].copy_from_slice(&sprog.to_le_bytes());
            b[2..4].copy_from_slice(&sstat.to_le_bytes());
            b
        };
        assert_eq!(parse_sanitize_log(&log(0, 0)), Some(SanitizeStatus::Never));
        assert_eq!(parse_sanitize_log(&log(32768, 2)), Some(SanitizeStatus::Running { pct: 50 }));
        assert_eq!(parse_sanitize_log(&log(0, 1)), Some(SanitizeStatus::Done));
        assert_eq!(parse_sanitize_log(&log(0, 4)), Some(SanitizeStatus::Done));
        assert_eq!(parse_sanitize_log(&log(0, 3)), Some(SanitizeStatus::Failed));
    }

    #[test]
    fn nvme_name_split() {
        assert_eq!(nvme_names("nvme0n1"), Some(("nvme0".into(), 1)));
        assert_eq!(nvme_names("nvme12n3"), Some(("nvme12".into(), 3)));
        assert_eq!(nvme_names("sda"), None);
        assert_eq!(nvme_names("nvmen1"), None);
        assert_eq!(nvme_names("nvme0c1n1"), None, "a multipath path node, not a namespace head");
    }

    #[test]
    fn scsi_sanitize_cdbs() {
        let (cdb, p) = scsi_sanitize(SanitizeMethod::Crypto);
        assert_eq!((cdb[0], cdb[1], cdb[7], cdb[8]), (0x48, 0xA3, 0, 0));
        assert!(p.is_empty());
        let (cdb, p) = scsi_sanitize(SanitizeMethod::Overwrite);
        assert_eq!(cdb[1], 0xA1);
        assert_eq!(u16::from_be_bytes([cdb[7], cdb[8]]), 8);
        assert_eq!(p, vec![1, 0, 0, 4, 0, 0, 0, 0]);
        assert_eq!(scsi_sanitize(SanitizeMethod::Block).0[1], 0xA2);
    }

    #[test]
    fn sanitize_progress_from_tur() {
        use crate::scsi::{Error, Sense};
        let s = |key, asc, ascq, progress| Err(Error::Sense(Sense { key, asc, ascq, progress }));
        assert_eq!(interpret_sanitize_tur(&Ok(())), Ok(None));
        assert_eq!(interpret_sanitize_tur(&s(2, 4, 0x1B, Some(16384))), Ok(Some(Some(25))));
        assert_eq!(interpret_sanitize_tur(&s(6, 0x29, 0, None)), Ok(Some(None)));
        assert!(interpret_sanitize_tur(&s(3, 0x31, 3, None)).is_err());
    }
}
