//! SAS/SATA health, phase 1: sysfs (`device/state`, `device/ioerr_cnt`,
//! hwmon temperature via drivetemp). Phase 2 adds SG_IO log-sense pages
//! (Informational Exceptions 0x2F, Temperature 0x0D, SSD wear 0x11) and ATA
//! SMART passthrough — see the work plan.

use super::Sample;

/// Parse an ioerr_cnt sysfs value ("0x12" or plain decimal).
pub fn parse_ioerr(s: &str) -> u64 {
    let t = s.trim();
    if let Some(hex) = t.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).unwrap_or(0)
    } else {
        t.parse().unwrap_or(0)
    }
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

    pub fn collect(name: &str) -> Sample {
        let dev = PathBuf::from(format!("/sys/block/{name}/device"));
        let state = read_trim(&dev.join("state"));
        let kernel_ok = match state.as_deref() {
            None | Some("running") => true,
            Some(_) => false,
        };
        let media_errors = read_trim(&dev.join("ioerr_cnt"))
            .map(|s| parse_ioerr(&s))
            .unwrap_or(0);
        let mut messages = Vec::new();
        if let Some(s) = &state {
            if s != "running" {
                messages.push(format!("{name}: kernel device state is {s:?}"));
            }
        }
        Sample {
            temperature_c: find_hwmon_temp(&dev),
            media_errors,
            kernel_ok,
            messages,
            ..Default::default()
        }
    }
}

pub fn collect(name: &str) -> Sample {
    #[cfg(target_os = "linux")]
    {
        linux::collect(name)
    }
    #[cfg(not(target_os = "linux"))]
    {
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
}
