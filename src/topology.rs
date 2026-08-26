//! Physical location: PCIe address, SAS address, HBA, SES enclosure/bay —
//! and the locate LED, which on Linux is a plain sysfs write on the
//! enclosure slot.

use crate::drive::Location;

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

    pub fn locate(name: &str) -> Location {
        let mut loc = Location::default();
        let base = PathBuf::from(format!("/sys/block/{name}"));
        // The resolved sysfs path carries the PCI chain and (for SCSI) the
        // hostN component.
        if let Ok(real) = std::fs::canonicalize(&base) {
            for comp in real.components() {
                let c = comp.as_os_str().to_string_lossy();
                if is_pci_bdf(&c) {
                    loc.pcie_addr = Some(c.to_string()); // last BDF wins: the closest bridge/endpoint
                }
                if c.starts_with("host") && c[4..].chars().all(|ch| ch.is_ascii_digit()) {
                    loc.scsi_host = Some(c.to_string());
                }
            }
        }
        loc.sas_address = read_trim(&base.join("device/sas_address"));
        if let Some((enc, slot_dir)) = find_enclosure_slot(name) {
            loc.enclosure = Some(enc);
            loc.bay = read_trim(&slot_dir.join("slot")).and_then(|s| s.parse().ok());
        }
        if let Some(bdf) = &loc.pcie_addr {
            loc.pcie_slot = pci_physical_slot(bdf);
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
}
