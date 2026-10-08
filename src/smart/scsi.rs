//! SAS/SATA health. From sysfs: `device/state`, `device/ioerr_cnt` (failed
//! commands, not media errors) and the hwmon temperature. From the drive
//! (#22), one or two commands a sample:
//!
//! - SAS (and anything not `ATA`): LOG SENSE — Informational Exceptions
//!   (0x2F: the drive's own failure prediction, ASC/ASCQ, and its
//!   temperature), Temperature (0x0D, only when 0x2F gave none) and, on an
//!   SSD, Solid State Media (0x11: Percentage Used Endurance Indicator).
//! - SATA (vendor `ATA`, also behind a SAS HBA's SAT): SMART READ DATA and
//!   READ THRESHOLDS through ATA PASS-THROUGH(16). Reallocated (5), pending
//!   (197) and offline-uncorrectable (198) sectors, temperature (194),
//!   power-on hours (9), SSD wear (177/231/233), and predicted failure: a
//!   pre-fail attribute at or below its threshold, the drive's own verdict
//!   without reading the status registers back.
//!
//! - SAS, also (#64): the error counter pages Write (0x02), Read (0x03) and
//!   Verify (0x05) — corrected, uncorrected, bytes processed. The drive's
//!   uncorrected errors are its media errors. A SATA drive's whole
//!   attribute table goes into the drive history.
//!
//! A page or command the drive does not support is simply not reported.
//! Parsers are portable and unit-tested; issuing the commands is Linux-only.

use super::{AtaAttribute, ErrorCounters, Sample, SmartCounters};

/// Parse an ioerr_cnt sysfs value ("0x12" or plain decimal).
pub fn parse_ioerr(s: &str) -> u64 {
    let t = s.trim();
    if let Some(hex) = t.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).unwrap_or(0)
    } else {
        t.parse().unwrap_or(0)
    }
}

/// LOG SENSE(10) of a page's cumulative values (PC = 01).
pub fn log_sense_cdb(page: u8, alloc: u16) -> [u8; 10] {
    let [hi, lo] = alloc.to_be_bytes();
    [0x4D, 0, 0x40 | (page & 0x3F), 0, 0, 0, 0, hi, lo, 0]
}

/// A log page's parameters: (parameter code, its data).
pub fn log_params(raw: &[u8], page: u8) -> Vec<(u16, &[u8])> {
    let mut out = vec![];
    if raw.len() < 4 || raw[0] & 0x3F != page {
        return out;
    }
    let end = (4 + u16::from_be_bytes([raw[2], raw[3]]) as usize).min(raw.len());
    let mut off = 4;
    while off + 4 <= end {
        let code = u16::from_be_bytes([raw[off], raw[off + 1]]);
        let len = raw[off + 3] as usize;
        let Some(data) = raw.get(off + 4..(off + 4 + len).min(end)) else { break };
        out.push((code, data));
        off += 4 + len;
    }
    out
}

/// Informational Exceptions (0x2F), parameter 0: the drive's failure
/// prediction (ASC/ASCQ, 0/0 = none) and its most recent temperature.
pub fn parse_ie(raw: &[u8]) -> Option<(Option<String>, Option<i32>)> {
    let (_, d) = log_params(raw, 0x2F).into_iter().find(|(c, _)| *c == 0)?;
    if d.len() < 2 {
        return None;
    }
    let (asc, ascq) = (d[0], d[1]);
    let predicted = (asc != 0).then(|| match asc {
        0x5D => format!("the drive predicts its own failure (SMART threshold exceeded, ASC 5Dh/{ascq:02X}h)"),
        0x0B => format!("the drive reports a warning (ASC 0Bh/{ascq:02X}h)"),
        _ => format!("the drive reports an informational exception (ASC {asc:02X}h/{ascq:02X}h)"),
    });
    let temp = d.get(2).filter(|t| **t != 0xFF && **t != 0).map(|t| i32::from(*t));
    Some((predicted, temp))
}

/// Temperature (0x0D), parameter 0: degrees C (0xFF = not available).
pub fn parse_temperature(raw: &[u8]) -> Option<i32> {
    let (_, d) = log_params(raw, 0x0D).into_iter().find(|(c, _)| *c == 0)?;
    d.get(1).filter(|t| **t != 0xFF).map(|t| i32::from(*t))
}

/// An error counter page (0x02 write, 0x03 read, 0x05 verify): parameters
/// 0003h (total corrected), 0005h (bytes processed), 0006h (total
/// uncorrected), each a big-endian counter of its own length.
pub fn parse_error_counters(raw: &[u8], page: u8) -> Option<ErrorCounters> {
    let params = log_params(raw, page);
    let get = |code: u16| {
        params
            .iter()
            .find(|(c, _)| *c == code)
            .filter(|(_, d)| !d.is_empty() && d.len() <= 8)
            .map(|(_, d)| d.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
    };
    let c = ErrorCounters { corrected: get(3), uncorrected: get(6), bytes: get(5) };
    (c != ErrorCounters::default()).then_some(c)
}

/// The drive's own media errors from its error counter pages: uncorrected
/// read + write + verify errors.
pub fn sas_media_errors(c: &SmartCounters) -> u64 {
    [c.read_errors, c.write_errors, c.verify_errors].iter().flatten().filter_map(|e| e.uncorrected).sum()
}

/// Solid State Media (0x11), parameter 1: Percentage Used Endurance
/// Indicator.
pub fn parse_ssd_endurance(raw: &[u8]) -> Option<u8> {
    let (_, d) = log_params(raw, 0x11).into_iter().find(|(c, _)| *c == 1)?;
    d.get(3).copied()
}

/// ATA PASS-THROUGH(16) for a SMART command: PIO Data-In of one block,
/// FEATURES = the SMART subcommand, LBA mid/high = 4Fh/C2h (the SMART key).
pub fn ata_smart_cdb(subcommand: u8) -> [u8; 16] {
    let mut c = [0u8; 16];
    c[0] = 0x85;
    c[1] = 4 << 1; // PIO Data-In
    c[2] = 0x08 | 0x04 | 0x02; // T_DIR in, BYT_BLOK, T_LENGTH = sector count
    c[4] = subcommand;
    c[6] = 1;
    c[10] = 0x4F;
    c[12] = 0xC2;
    c[14] = 0xB0; // SMART
    c
}
pub const SMART_READ_DATA: u8 = 0xD0;
pub const SMART_READ_THRESHOLDS: u8 = 0xD1;

/// One SMART attribute: id, flags, normalized value, raw (48 bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attribute {
    pub id: u8,
    pub prefail: bool,
    pub value: u8,
    pub worst: u8,
    pub raw: u64,
}

/// SMART READ DATA: the 30 attribute slots from byte 2, 12 bytes each.
pub fn parse_smart_attributes(data: &[u8]) -> Vec<Attribute> {
    (0..30)
        .filter_map(|i| {
            let a = data.get(2 + i * 12..2 + i * 12 + 12)?;
            (a[0] != 0).then(|| Attribute {
                id: a[0],
                prefail: u16::from_le_bytes([a[1], a[2]]) & 1 != 0,
                value: a[3],
                worst: a[4],
                raw: a[5..11].iter().rev().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)),
            })
        })
        .collect()
}

/// SMART READ THRESHOLDS: id → threshold.
pub fn parse_smart_thresholds(data: &[u8]) -> Vec<(u8, u8)> {
    (0..30)
        .filter_map(|i| {
            let t = data.get(2 + i * 12..2 + i * 12 + 2)?;
            (t[0] != 0).then_some((t[0], t[1]))
        })
        .collect()
}

/// What a SATA drive's SMART says, into a sample. `ssd`: read a wear
/// attribute.
pub fn apply_ata_smart(attrs: &[Attribute], thresholds: &[(u8, u8)], ssd: bool, s: &mut Sample) {
    let raw = |id: u8| attrs.iter().find(|a| a.id == id).map(|a| a.raw);
    let mut c = SmartCounters {
        source: "ata_smart".into(),
        reallocated_sectors: raw(5),
        pending_sectors: raw(197),
        offline_uncorrectable: raw(198),
        reported_uncorrectable: raw(187),
        crc_errors: raw(199),
        ..Default::default()
    };
    // The drive's own media-error count (#58); not the kernel's ioerr_cnt.
    s.media_errors = raw(187).unwrap_or(0);
    let failing: Vec<String> = attrs
        .iter()
        .filter(|a| a.prefail)
        .filter_map(|a| {
            let t = thresholds.iter().find(|(id, _)| *id == a.id)?.1;
            (t != 0 && a.value <= t).then(|| format!("attribute {} at {} ≤ threshold {t}", a.id, a.value))
        })
        .collect();
    if !failing.is_empty() {
        c.predicted_failure = Some(format!("the drive predicts its own failure: {}", failing.join(", ")));
    }
    if s.temperature_c.is_none() {
        // 194: the low byte of the raw value is degrees C.
        s.temperature_c = raw(194).map(|r| (r & 0xFF) as i32).filter(|t| *t > 0 && *t < 150);
    }
    s.power_on_hours = s.power_on_hours.or_else(|| raw(9).map(|r| r & 0xFFFF_FFFF));
    if ssd {
        // Normalized 100 → 1 as endurance is used (Samsung 177, 231 "life
        // left", Intel 233).
        s.wear_pct = s.wear_pct.or_else(|| {
            [233, 231, 177].iter().find_map(|id| attrs.iter().find(|a| a.id == *id)).map(|a| 100u8.saturating_sub(a.value.min(100)))
        });
    }
    s.smart = Some(c);
    s.ata_attributes = attrs
        .iter()
        .map(|a| AtaAttribute {
            id: a.id,
            prefail: a.prefail,
            value: a.value,
            worst: a.worst,
            threshold: thresholds.iter().find(|(id, _)| *id == a.id).map_or(0, |t| t.1),
            raw: a.raw,
        })
        .collect();
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::path::{Path, PathBuf};

    fn read_trim(p: &Path) -> Option<String> {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// drivetemp and SAS hwmon entries land under device/hwmon* — sometimes
    /// nested one level (device/hwmon/hwmonN).
    fn find_hwmon_temp(dev: &Path) -> Option<i32> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        for e in std::fs::read_dir(dev).ok()?.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with("hwmon") {
                candidates.push(e.path());
                if let Ok(inner) = std::fs::read_dir(e.path()) {
                    for i in inner.flatten() {
                        if i.file_name().to_string_lossy().starts_with("hwmon") {
                            candidates.push(i.path());
                        }
                    }
                }
            }
        }
        for c in candidates {
            if let Some(v) = read_trim(&c.join("temp1_input")) {
                if let Ok(milli) = v.parse::<i64>() {
                    return Some((milli / 1000) as i32);
                }
            }
        }
        None
    }

    /// LOG SENSE of one page; None when the drive does not have it.
    fn log_sense(dev: &crate::scsi::Device, page: u8) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; 1024];
        let n = dev.io(&log_sense_cdb(page, 1024), crate::scsi::Dir::FromDevice, &mut buf, 10_000).ok()?;
        buf.truncate(n);
        Some(buf)
    }

    fn ata_smart(dev: &crate::scsi::Device, sub: u8) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; 512];
        dev.io(&ata_smart_cdb(sub), crate::scsi::Dir::FromDevice, &mut buf, 10_000).ok()?;
        Some(buf)
    }

    /// What the drive itself says (#22). Nothing when it cannot be opened
    /// or answers nothing: the sysfs half stands.
    fn from_drive(name: &str, dev_dir: &Path, ssd: bool, s: &mut Sample) {
        let node = crate::scsi::sg_path_in(&dev_dir.join("scsi_generic").to_string_lossy()).unwrap_or_else(|| format!("/dev/{name}"));
        let Ok(dev) = crate::scsi::Device::open(&node) else { return };
        if read_trim(&dev_dir.join("vendor")).as_deref() == Some("ATA") {
            let Some(data) = ata_smart(&dev, SMART_READ_DATA) else { return };
            let thresholds = ata_smart(&dev, SMART_READ_THRESHOLDS).map(|t| parse_smart_thresholds(&t)).unwrap_or_default();
            apply_ata_smart(&parse_smart_attributes(&data), &thresholds, ssd, s);
            return;
        }
        let mut c = SmartCounters { source: "log_sense".into(), ..Default::default() };
        let mut any = false;
        if let Some((predicted, temp)) = log_sense(&dev, 0x2F).and_then(|r| parse_ie(&r)) {
            any = true;
            c.predicted_failure = predicted;
            s.temperature_c = s.temperature_c.or(temp);
        }
        if s.temperature_c.is_none() {
            s.temperature_c = log_sense(&dev, 0x0D).and_then(|r| parse_temperature(&r));
        }
        if ssd {
            if let Some(w) = log_sense(&dev, 0x11).and_then(|r| parse_ssd_endurance(&r)) {
                s.wear_pct = Some(w);
            }
        }
        // The error counter pages (#64): the drive's own media errors.
        c.write_errors = log_sense(&dev, 0x02).and_then(|r| parse_error_counters(&r, 0x02));
        c.read_errors = log_sense(&dev, 0x03).and_then(|r| parse_error_counters(&r, 0x03));
        c.verify_errors = log_sense(&dev, 0x05).and_then(|r| parse_error_counters(&r, 0x05));
        if c.write_errors.is_some() || c.read_errors.is_some() || c.verify_errors.is_some() {
            any = true;
            s.media_errors = sas_media_errors(&c);
        }
        if any {
            s.smart = Some(c);
        }
    }

    pub fn collect(name: &str, ssd: bool) -> Sample {
        let dev = PathBuf::from(format!("/sys/block/{name}/device"));
        let state = read_trim(&dev.join("state"));
        let kernel_ok = match state.as_deref() {
            None | Some("running") => true,
            Some(_) => false,
        };
        let io_errors = read_trim(&dev.join("ioerr_cnt")).map(|s| parse_ioerr(&s));
        let mut messages = Vec::new();
        if let Some(s) = &state {
            if s != "running" {
                messages.push(format!("{name}: kernel device state is {s:?}"));
            }
        }
        let mut s = Sample {
            temperature_c: find_hwmon_temp(&dev),
            io_errors,
            kernel_ok,
            messages,
            ..Default::default()
        };
        if kernel_ok {
            from_drive(name, &dev, ssd, &mut s);
        }
        s
    }
}

pub fn collect(name: &str, ssd: bool) -> Sample {
    #[cfg(target_os = "linux")]
    {
        linux::collect(name, ssd)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = ssd;
        Sample {
            kernel_ok: true,
            messages: vec![format!("{name}: SCSI health unavailable on this platform")],
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioerr_parses_hex_and_decimal() {
        assert_eq!(parse_ioerr("0x0"), 0);
        assert_eq!(parse_ioerr("0x1f"), 31);
        assert_eq!(parse_ioerr("12"), 12);
        assert_eq!(parse_ioerr("garbage"), 0);
    }

    /// A log page from parameters: (code, data).
    fn page(code: u8, params: &[(u16, &[u8])]) -> Vec<u8> {
        let mut body = vec![];
        for (c, d) in params {
            body.extend_from_slice(&c.to_be_bytes());
            body.push(0x03);
            body.push(d.len() as u8);
            body.extend_from_slice(d);
        }
        let mut p = vec![code, 0];
        p.extend_from_slice(&(body.len() as u16).to_be_bytes());
        p.extend(body);
        p
    }

    #[test]
    fn log_sense_pages() {
        assert_eq!(log_sense_cdb(0x2F, 1024), [0x4D, 0, 0x6F, 0, 0, 0, 0, 0x04, 0x00, 0]);
        // IE: no exception, 38 °C.
        let ok = page(0x2F, &[(0, &[0, 0, 38, 60])]);
        assert_eq!(parse_ie(&ok), Some((None, Some(38))));
        // IE: failure predicted (5Dh/10h).
        let bad = page(0x2F, &[(0, &[0x5D, 0x10, 41])]);
        let (p, t) = parse_ie(&bad).unwrap();
        assert!(p.unwrap().contains("predicts its own failure"));
        assert_eq!(t, Some(41));
        assert_eq!(parse_ie(&page(0x0D, &[(0, &[0, 0, 38])])), None, "not page 0x2F");
        // Temperature: parameter 0 byte 1; 0xFF unknown.
        assert_eq!(parse_temperature(&page(0x0D, &[(0, &[0, 35]), (1, &[0, 65])])), Some(35));
        assert_eq!(parse_temperature(&page(0x0D, &[(0, &[0, 0xFF])])), None);
        // Solid State Media: parameter 1 byte 3.
        assert_eq!(parse_ssd_endurance(&page(0x11, &[(1, &[0, 0, 0, 7])])), Some(7));
        assert_eq!(parse_ssd_endurance(&page(0x11, &[(2, &[0, 0, 0, 7])])), None);
        // A truncated page is not read past its end.
        let mut short = page(0x2F, &[(0, &[0x5D, 0x10, 41])]);
        short.truncate(6);
        assert_eq!(parse_ie(&short), None);
    }

    #[test]
    fn sas_error_counter_pages() {
        // Read errors: 3 corrected (2-byte counter), 1 TB processed
        // (8-byte), 2 uncorrected (4-byte).
        let read = page(0x03, &[(0, &[0, 9]), (3, &[0, 3]), (5, &1_000_000_000_000u64.to_be_bytes()), (6, &[0, 0, 0, 2])]);
        let r = parse_error_counters(&read, 0x03).unwrap();
        assert_eq!((r.corrected, r.bytes, r.uncorrected), (Some(3), Some(1_000_000_000_000), Some(2)));
        assert_eq!(parse_error_counters(&read, 0x02), None, "not the write page");
        let write = page(0x02, &[(6, &[0, 1])]);
        let w = parse_error_counters(&write, 0x02).unwrap();
        assert_eq!((w.corrected, w.uncorrected), (None, Some(1)));
        assert_eq!(parse_error_counters(&page(0x05, &[(0, &[0, 1])]), 0x05), None, "no counter we read");
        let c = SmartCounters { read_errors: Some(r), write_errors: Some(w), ..Default::default() };
        assert_eq!(sas_media_errors(&c), 3);
    }

    /// A SMART READ DATA / THRESHOLDS pair: (id, flags, value, raw).
    fn smart(attrs: &[(u8, u16, u8, u64)], thresholds: &[(u8, u8)]) -> (Vec<u8>, Vec<u8>) {
        let mut d = vec![0u8; 512];
        let mut t = vec![0u8; 512];
        for (i, (id, flags, value, raw)) in attrs.iter().enumerate() {
            let o = 2 + i * 12;
            d[o] = *id;
            d[o + 1..o + 3].copy_from_slice(&flags.to_le_bytes());
            d[o + 3] = *value;
            d[o + 5..o + 11].copy_from_slice(&raw.to_le_bytes()[..6]);
        }
        for (i, (id, th)) in thresholds.iter().enumerate() {
            t[2 + i * 12] = *id;
            t[2 + i * 12 + 1] = *th;
        }
        (d, t)
    }

    #[test]
    fn ata_smart_counters_and_the_drives_own_verdict() {
        assert_eq!(&ata_smart_cdb(SMART_READ_DATA)[..], &[0x85, 0x08, 0x0E, 0, 0xD0, 0, 1, 0, 0, 0, 0x4F, 0, 0xC2, 0, 0xB0, 0]);
        // A healthy WD: 5 = 0, 197 = 0, 198 = 0, 194 raw 0x0000_2A00_1D → 29 °C, 9 = 18000 h.
        let (d, t) = smart(
            &[(5, 0x33, 200, 0), (9, 0x32, 75, 18000), (194, 0x22, 118, 0x2A_0000_001D), (197, 0x32, 200, 0), (198, 0x30, 200, 0)],
            &[(5, 140), (9, 0), (194, 0), (197, 0), (198, 0)],
        );
        let attrs = parse_smart_attributes(&d);
        assert_eq!(attrs.len(), 5);
        assert!(attrs[0].prefail && !attrs[1].prefail);
        let mut s = Sample::default();
        apply_ata_smart(&attrs, &parse_smart_thresholds(&t), false, &mut s);
        let c = s.smart.clone().unwrap();
        assert_eq!((c.reallocated_sectors, c.pending_sectors, c.offline_uncorrectable), (Some(0), Some(0), Some(0)));
        assert_eq!(c.predicted_failure, None);
        assert_eq!((s.temperature_c, s.power_on_hours, s.wear_pct), (Some(29), Some(18000), None));
        // The whole table, with thresholds, for the history (#64).
        assert_eq!(s.ata_attributes.len(), 5);
        assert_eq!(s.ata_attributes[0], AtaAttribute { id: 5, prefail: true, value: 200, worst: 0, threshold: 140, raw: 0 });

        // Reallocated sectors pre-fail at value 100 ≤ threshold 140: the
        // drive's own failure prediction; 8 pending.
        let (d, t) = smart(&[(5, 0x33, 100, 1200), (197, 0x32, 200, 8)], &[(5, 140), (197, 0)]);
        let mut s = Sample { temperature_c: Some(33), ..Default::default() };
        apply_ata_smart(&parse_smart_attributes(&d), &parse_smart_thresholds(&t), false, &mut s);
        let c = s.smart.unwrap();
        assert!(c.predicted_failure.unwrap().contains("attribute 5 at 100 ≤ threshold 140"));
        assert_eq!((c.reallocated_sectors, c.pending_sectors), (Some(1200), Some(8)));
        assert_eq!(s.temperature_c, Some(33), "hwmon's reading wins");

        // An SSD's wear: 233 normalized 97 → 3 % used.
        let (d, t) = smart(&[(233, 0x32, 97, 0)], &[]);
        let mut s = Sample::default();
        apply_ata_smart(&parse_smart_attributes(&d), &parse_smart_thresholds(&t), true, &mut s);
        assert_eq!(s.wear_pct, Some(3));
    }
}
