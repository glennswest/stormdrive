//! Physical location: controller → shelf → bay, resolved from sysfs — and
//! the locate LED, which on Linux is a plain sysfs write on the enclosure
//! slot.
//!
//! Shelf identity comes from the SES processor's SCSI device (vendor,
//! model, serial from VPD page 0x80). A dual-IOM NetApp shelf shows up as
//! two enclosure devices with two sysfs ids but one serial — `Shelf::key`
//! prefers the serial for exactly that reason.

use crate::drive::{Controller, Location, Shelf};

/// Does this path component look like a PCI BDF ("0000:03:00.0")?
pub fn is_pci_bdf(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 12 {
        return false;
    }
    let hex = |r: std::ops::Range<usize>| b[r].iter().all(|c| c.is_ascii_hexdigit());
    hex(0..4)
        && b[4] == b':'
        && hex(5..7)
        && b[7] == b':'
        && hex(8..10)
        && b[10] == b'.'
        && b[11].is_ascii_hexdigit()
}

/// Parse SCSI VPD page 0x80 (unit serial number): 4-byte header
/// (peripheral, page code, length BE16) then ASCII serial.
pub fn parse_vpd80(raw: &[u8]) -> Option<String> {
    if raw.len() < 4 || raw[1] != 0x80 {
        return None;
    }
    let len = u16::from_be_bytes([raw[2], raw[3]]) as usize;
    let end = (4 + len).min(raw.len());
    let s = String::from_utf8_lossy(&raw[4..end]).trim().to_string();
    (!s.is_empty()).then_some(s)
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

    /// Find the enclosure component (slot) directory holding this block
    /// device, if any: /sys/class/enclosure/<enc>/<component>/device is a
    /// symlink to the SCSI device, whose block/<name> subdir names the disk.
    fn find_enclosure_slot(name: &str) -> Option<(String, PathBuf)> {
        for enc in std::fs::read_dir("/sys/class/enclosure").ok()?.flatten() {
            let enc_id = enc.file_name().to_string_lossy().to_string();
            let Ok(components) = std::fs::read_dir(enc.path()) else {
                continue;
            };
            for comp in components.flatten() {
                let comp_path = comp.path();
                if !comp_path.is_dir() {
                    continue;
                }
                if comp_path.join("device/block").join(name).exists() {
                    return Some((enc_id, comp_path));
                }
            }
        }
        None
    }

    /// Identity of the shelf behind an enclosure id: the SES processor's
    /// SCSI device at /sys/class/enclosure/<id>/device.
    fn shelf_identity(enc_id: &str) -> Shelf {
        let dev = PathBuf::from(format!("/sys/class/enclosure/{enc_id}/device"));
        let serial = std::fs::read(dev.join("vpd_pg80"))
            .ok()
            .and_then(|raw| parse_vpd80(&raw));
        Shelf {
            id: Some(enc_id.to_string()),
            vendor: read_trim(&dev.join("vendor")),
            model: read_trim(&dev.join("model")),
            serial,
            sas_address: read_trim(&dev.join("sas_address")),
        }
    }

    fn controller_of(real: &Path) -> Option<Controller> {
        let mut pcie_addr = None;
        let mut scsi_host = None;
        for comp in real.components() {
            let c = comp.as_os_str().to_string_lossy();
            if is_pci_bdf(&c) {
                pcie_addr = Some(c.to_string()); // last BDF wins: the endpoint
            }
            if c.starts_with("host") && c[4..].chars().all(|ch| ch.is_ascii_digit()) {
                scsi_host = Some(c.to_string());
            }
        }
        if pcie_addr.is_none() && scsi_host.is_none() {
            return None;
        }
        let driver = pcie_addr.as_ref().and_then(|bdf| {
            std::fs::read_link(format!("/sys/bus/pci/devices/{bdf}/driver"))
                .ok()
                .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
        });
        Some(Controller {
            scsi_host,
            pcie_addr,
            driver,
        })
    }

    pub fn locate(name: &str) -> Location {
        let mut loc = Location::default();
        let base = PathBuf::from(format!("/sys/block/{name}"));
        if let Ok(real) = std::fs::canonicalize(&base) {
            loc.controller = controller_of(&real);
        }
        loc.sas_address = read_trim(&base.join("device/sas_address"));
        if let Some((enc, slot_dir)) = find_enclosure_slot(name) {
            loc.bay = read_trim(&slot_dir.join("slot")).and_then(|s| s.parse().ok());
            loc.shelf = Some(shelf_identity(&enc));
        }
        if name.starts_with("nvme") {
            loc.pcie_addr = loc.controller.as_ref().and_then(|c| c.pcie_addr.clone());
            if let Some(bdf) = &loc.pcie_addr {
                loc.pcie_slot = pci_physical_slot(bdf);
            }
        }
        loc
    }

    /// /sys/bus/pci/slots/<label>/address holds "0000:03:00" (no function).
    fn pci_physical_slot(bdf: &str) -> Option<String> {
        let want = bdf.split('.').next()?;
        for slot in std::fs::read_dir("/sys/bus/pci/slots").ok()?.flatten() {
            if read_trim(&slot.path().join("address")).as_deref() == Some(want) {
                return Some(slot.file_name().to_string_lossy().to_string());
            }
        }
        None
    }

    pub fn set_locate(name: &str, on: bool) -> std::io::Result<()> {
        let Some((_, slot_dir)) = find_enclosure_slot(name) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{name}: no enclosure slot exposes this drive"),
            ));
        };
        std::fs::write(slot_dir.join("locate"), if on { "1" } else { "0" })
    }
}

/// Resolve the physical location of a drive. Best-effort: fields the
/// platform doesn't expose stay None.
pub fn locate(name: &str) -> Location {
    #[cfg(target_os = "linux")]
    {
        linux::locate(name)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = name;
        Location::default()
    }
}

/// Turn the enclosure locate LED for this drive on or off.
pub fn set_locate(name: &str, on: bool) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::set_locate(name, on)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (name, on);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "locate LEDs require Linux/SES",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bdf_detection() {
        assert!(is_pci_bdf("0000:03:00.0"));
        assert!(is_pci_bdf("0000:ff:1f.7"));
        assert!(!is_pci_bdf("0000:03:00"));
        assert!(!is_pci_bdf("host3"));
        assert!(!is_pci_bdf("0000-03-00.0"));
        assert!(!is_pci_bdf("00000:3:00.0"));
    }

    #[test]
    fn vpd80_parses_serial() {
        let mut raw = vec![0x0d, 0x80, 0x00, 0x08];
        raw.extend_from_slice(b"SN123   ");
        assert_eq!(parse_vpd80(&raw), Some("SN123".into()));
        assert_eq!(parse_vpd80(&[0x0d, 0x83, 0x00, 0x04, b'x']), None, "wrong page");
        assert_eq!(parse_vpd80(&[0x0d, 0x80]), None, "truncated header");
        assert_eq!(parse_vpd80(&[0x0d, 0x80, 0x00, 0x00]), None, "empty serial");
        let mut short = vec![0x0d, 0x80, 0x00, 0x20];
        short.extend_from_slice(b"AB");
        assert_eq!(parse_vpd80(&short), Some("AB".into()), "length clamped to buffer");
    }
}
