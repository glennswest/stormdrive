//! Physical location: controller → shelf → bay, resolved from sysfs — and
//! the locate LED, which on Linux is a plain sysfs write on the enclosure
//! slot.
//!
//! Shelf identity comes from the SES processor's SCSI device (vendor,
//! model, serial from VPD page 0x80). A dual-IOM NetApp shelf shows up as
//! two enclosure devices with two sysfs ids but one serial — `Shelf::key`
//! prefers the serial for exactly that reason.

use crate::drive::{Controller, Location, Shelf};
use crate::ses::ShelfReport;
use std::collections::BTreeMap;

pub type Shelves = BTreeMap<String, ShelfReport>;

/// Does this path component look like a PCI BDF ("0000:03:00.0")? The
/// domain is 4 hex digits, or more behind Intel VMD ("10000:01:00.0"),
/// which is how many NVMe backplanes are wired.
pub fn is_pci_bdf(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 12 || b.len() > 16 {
        return false;
    }
    let d = b.len() - 8; // domain digits
    let hex = |r: std::ops::Range<usize>| b[r].iter().all(|c| c.is_ascii_hexdigit());
    hex(0..d)
        && b[d] == b':'
        && hex(d + 1..d + 3)
        && b[d + 3] == b':'
        && hex(d + 4..d + 6)
        && b[d + 6] == b'.'
        && b[d + 7].is_ascii_hexdigit()
}

/// NVMe location at scale (#15): 160 E3.S/U.2 drives sit behind PCIe
/// switches (often inside a VMD domain), with no SES enclosure to name a
/// bay. What sysfs offers instead is the hotplug slot each drive is in
/// (`/sys/bus/pci/slots/<n>`, `n` being the platform's slot number, ACPI
/// `_SUN`, which is what the chassis label says) and, on newer kernels,
/// the slot's NPEM LEDs. `sys` is the sysfs root, so the walk is testable.
pub mod nvme {
    use super::is_pci_bdf;
    use std::path::{Path, PathBuf};

    fn read_trim(p: &Path) -> Option<String> {
        std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }

    /// The namespace's device path under `/sys/devices`. Under native NVMe
    /// multipath `/sys/block/nvme0n1` is a virtual subsystem head with no
    /// PCIe in its path; its `multipath/` directory links to the
    /// per-controller paths (`nvme0c0n1`), and the first of those (sorted,
    /// so stable) is followed to the controller.
    pub fn device_path(sys: &Path, name: &str) -> Option<PathBuf> {
        let real = std::fs::canonicalize(sys.join("block").join(name)).ok()?;
        if bdfs(&real).next().is_some() {
            return Some(real);
        }
        let mut paths: Vec<PathBuf> = std::fs::read_dir(sys.join("block").join(name).join("multipath"))
            .ok()?
            .flatten()
            .map(|e| e.path())
            .collect();
        paths.sort();
        paths.iter().find_map(|p| std::fs::canonicalize(p).ok().filter(|r| bdfs(r).next().is_some()))
    }

    /// Every PCI function on the path, root port first.
    pub fn bdfs(real: &Path) -> impl Iterator<Item = String> + '_ {
        real.components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .filter(|c| is_pci_bdf(c))
    }

    /// The hotplug slot on the drive's PCI chain: the one whose `address`
    /// (`domain:bus:dev`, no function) is a device on the chain, nearest
    /// the drive first.
    pub fn slot(sys: &Path, real: &Path) -> Option<String> {
        let slots: Vec<(String, String)> = std::fs::read_dir(sys.join("bus/pci/slots"))
            .ok()?
            .flatten()
            .filter_map(|e| Some((read_trim(&e.path().join("address"))?, e.file_name().to_string_lossy().to_string())))
            .collect();
        let chain: Vec<String> = bdfs(real).collect();
        chain.iter().rev().find_map(|bdf| {
            let want = bdf.rsplit_once('.').map(|(a, _)| a)?;
            slots.iter().find(|(addr, _)| addr == want).map(|(_, n)| n.clone())
        })
    }

    /// A slot number is the bay: `_SUN` is the number printed on the
    /// chassis. Only a plain number; a label like `NVMe-A3` stays a slot.
    pub fn bay_of_slot(slot: &str) -> Option<u32> {
        slot.parse().ok()
    }

    /// Where the drive's locate LED is: the slot's `attention` indicator
    /// (pciehp), else an NPEM `…:enclosure:locate` LED on a port of the
    /// chain (Linux 6.12+).
    pub fn locate_led(sys: &Path, real: &Path, slot: Option<&str>) -> Option<PathBuf> {
        if let Some(s) = slot {
            let att = sys.join("bus/pci/slots").join(s).join("attention");
            if att.exists() {
                return Some(att);
            }
        }
        let chain: Vec<String> = bdfs(real).collect();
        let mut leds: Vec<(String, PathBuf)> = std::fs::read_dir(sys.join("class/leds"))
            .ok()?
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                let bdf = n.strip_suffix(":enclosure:locate")?.to_string();
                Some((bdf, e.path().join("brightness")))
            })
            .collect();
        leds.sort();
        chain.iter().rev().find_map(|bdf| leds.iter().find(|(b, _)| b == bdf).map(|(_, p)| p.clone()))
    }
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

/// SAS wiring from a drive's canonical sysfs path
/// (`…/host0/port-0:0/expander-0:0/port-0:0:5/end_device-0:0:5/…`): the
/// phy its end device is attached through — the lowest one of a wide port
/// — and the SAS address of the expander that phy is on, when it is on
/// one. Behind a shelf's expander the phy is the shelf's own slot wiring;
/// direct-attached it is the HBA phy.
pub fn sas_attachment(real: &std::path::Path) -> (Option<u32>, Option<String>) {
    use std::path::PathBuf;
    let read = |p: PathBuf| {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let comps: Vec<String> = real
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    let Some(ed) = comps.iter().position(|c| c.starts_with("end_device-")) else {
        return (None, None);
    };
    if ed < 1 || !comps[ed - 1].starts_with("port-") {
        return (None, None);
    }
    let prefix = |n: usize| comps[..n].iter().collect::<PathBuf>();
    let phy = std::fs::read_dir(prefix(ed))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("phy-"))
        .filter_map(|e| read(e.path().join("phy_identifier"))?.parse::<u32>().ok())
        .min();
    let expander = (ed >= 2 && comps[ed - 2].starts_with("expander-"))
        .then(|| read(prefix(ed - 1).join("sas_device").join(&comps[ed - 2]).join("sas_address")))
        .flatten();
    (phy, expander)
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
    /// SCSI device at /sys/class/enclosure/<id>/device, enriched with the
    /// logical id from the SES scan when that ESP is in it.
    fn shelf_identity(enc_id: &str, shelves: &Shelves) -> Shelf {
        let dev = PathBuf::from(format!("/sys/class/enclosure/{enc_id}/device"));
        let serial = std::fs::read(dev.join("vpd_pg80"))
            .ok()
            .and_then(|raw| parse_vpd80(&raw));
        let logical_id = shelves
            .values()
            .find(|r| r.esps.iter().any(|e| e.scsi_id == enc_id))
            .and_then(|r| r.shelf.logical_id.clone());
        Shelf {
            id: Some(enc_id.to_string()),
            vendor: read_trim(&dev.join("vendor")),
            model: read_trim(&dev.join("model")),
            serial,
            sas_address: read_trim(&dev.join("sas_address")),
            logical_id,
        }
    }

    /// The SAS transport's view, which mpt3sas fills in with or without
    /// the ses module: /sys/class/sas_device/end_device-H:P:N/
    /// {enclosure_identifier, bay_identifier}. Returns (logical id hex,
    /// bay).
    fn sas_device_enclosure(real: &Path) -> Option<(String, Option<u32>)> {
        let end_dev = real
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .find(|c| c.starts_with("end_device-"))?;
        let base = PathBuf::from(format!("/sys/class/sas_device/{end_dev}"));
        let enc = read_trim(&base.join("enclosure_identifier"))
            .map(|s| crate::ses::normalize_sas(&s))
            .filter(|s| !s.is_empty() && s.chars().any(|c| c != '0'))?;
        let bay = read_trim(&base.join("bay_identifier")).and_then(|s| s.parse().ok());
        Some((enc, bay))
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

    pub fn locate(name: &str, shelves: &Shelves) -> Location {
        let mut loc = Location::default();
        let base = PathBuf::from(format!("/sys/block/{name}"));
        let real = std::fs::canonicalize(&base).ok();
        if let Some(r) = &real {
            loc.controller = controller_of(r);
            (loc.sas_phy, loc.expander) = sas_attachment(r);
        }
        loc.sas_address = read_trim(&base.join("device/sas_address"));
        if let Some((enc, slot_dir)) = find_enclosure_slot(name) {
            loc.bay = read_trim(&slot_dir.join("slot")).and_then(|s| s.parse().ok());
            loc.shelf = Some(shelf_identity(&enc, shelves));
        } else if let Some((enc_id, bay)) = real.as_deref().and_then(sas_device_enclosure) {
            let rep = shelves.get(&enc_id);
            loc.shelf = Some(match rep {
                Some(r) => r.shelf.clone(),
                None => Shelf {
                    logical_id: Some(enc_id.clone()),
                    ..Default::default()
                },
            });
            loc.bay = bay.or_else(|| {
                rep.zip(loc.sas_address.as_deref())
                    .and_then(|(r, sas)| r.bay_of(sas))
            });
        }
        if name.starts_with("nvme") {
            let sys = Path::new("/sys");
            if let Some(dev) = super::nvme::device_path(sys, name) {
                // A multipath head's own path has no PCIe; its controller's does.
                if loc.controller.as_ref().map_or(true, |c| c.pcie_addr.is_none()) {
                    loc.controller = controller_of(&dev);
                }
                loc.pcie_slot = super::nvme::slot(sys, &dev);
                if loc.bay.is_none() {
                    loc.bay = loc.pcie_slot.as_deref().and_then(super::nvme::bay_of_slot);
                }
            }
            loc.pcie_addr = loc.controller.as_ref().and_then(|c| c.pcie_addr.clone());
        }
        loc
    }

    pub fn set_locate(name: &str, loc: &Location, shelves: &Shelves, on: bool) -> std::io::Result<()> {
        if let Some((_, slot_dir)) = find_enclosure_slot(name) {
            return std::fs::write(slot_dir.join("locate"), if on { "1" } else { "0" });
        }
        // NVMe: the slot's attention indicator or an NPEM LED.
        if name.starts_with("nvme") {
            let sys = Path::new("/sys");
            if let Some(led) = super::nvme::device_path(sys, name)
                .and_then(|dev| super::nvme::locate_led(sys, &dev, loc.pcie_slot.as_deref()))
            {
                return std::fs::write(led, if on { "1" } else { "0" });
            }
        }
        // No ses module: talk SES ourselves through the shelf's ESP.
        let key = loc.shelf.as_ref().and_then(|s| s.key());
        let rep = key.as_ref().and_then(|k| shelves.get(k));
        match (rep, loc.bay) {
            (Some(r), Some(b)) => crate::ses::set_ident(r, Some(b), on),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{name}: no enclosure slot (sysfs or SES) exposes this drive"),
            )),
        }
    }
}

/// Resolve the physical location of a drive. Best-effort: fields the
/// platform doesn't expose stay None. `shelves` is the latest SES scan,
/// used to name the shelf when only its logical id is known.
pub fn locate(name: &str, shelves: &Shelves) -> Location {
    #[cfg(target_os = "linux")]
    {
        linux::locate(name, shelves)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (name, shelves);
        Location::default()
    }
}

/// Turn the enclosure locate LED for this drive on or off: sysfs slot
/// when the ses module is bound, SES control page otherwise.
pub fn set_locate(name: &str, loc: &Location, shelves: &Shelves, on: bool) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::set_locate(name, loc, shelves, on)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (name, loc, shelves, on);
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
    fn vmd_domains_are_bdfs_too() {
        assert!(is_pci_bdf("10000:01:00.0"));
        assert!(is_pci_bdf("10001:e1:1f.7"));
        assert!(!is_pci_bdf("10000:1:00.0"));
        assert!(!is_pci_bdf("pci10000:00"));
    }

    /// A 160-bay NVMe chassis in miniature: two drives behind a PCIe
    /// switch inside a VMD domain, one of them under native multipath;
    /// slots numbered as the chassis labels them; one slot with a pciehp
    /// attention indicator, the other with an NPEM LED.
    #[cfg(unix)]
    #[test]
    fn nvme_behind_a_switch_gets_its_slot_bay_and_led() {
        use std::os::unix::fs::symlink;
        let sys = std::env::temp_dir().join(format!("stormdrive-nvme-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sys);
        let mk = |p: std::path::PathBuf| {
            std::fs::create_dir_all(&p).unwrap();
            p
        };
        let w = |p: std::path::PathBuf, v: &str| std::fs::write(p, v).unwrap();
        let sw = sys.join("devices/pci0000:00/0000:00:01.0/pci10000:00/10000:00:02.0/10000:01:00.0");
        // Drive in bay 17, plain: /sys/block/nvme5n1 → …/nvme/nvme5/nvme5n1.
        let d17 = mk(sw.join("10000:02:00.0/10000:03:00.0/nvme/nvme5/nvme5n1"));
        // Drive in bay 142, multipath: the head is virtual.
        let c142 = mk(sw.join("10000:02:01.0/10000:04:00.0/nvme/nvme9/nvme9c9n1"));
        let head = mk(sys.join("devices/virtual/nvme-subsystem/nvme-subsys9/nvme9n1"));
        mk(head.join("multipath"));
        symlink(&c142, head.join("multipath/nvme9c9n1")).unwrap();
        mk(sys.join("block"));
        symlink(&d17, sys.join("block/nvme5n1")).unwrap();
        symlink(&head, sys.join("block/nvme9n1")).unwrap();
        let s17 = mk(sys.join("bus/pci/slots/17"));
        w(s17.join("address"), "10000:03:00\n");
        w(s17.join("attention"), "0");
        w(mk(sys.join("bus/pci/slots/142")).join("address"), "10000:04:00\n");
        w(mk(sys.join("bus/pci/slots/unused")).join("address"), "0000:7f:00\n");
        mk(sys.join("class/leds/10000:02:01.0:enclosure:locate"));

        let dev17 = nvme::device_path(&sys, "nvme5n1").unwrap();
        assert_eq!(nvme::slot(&sys, &dev17).as_deref(), Some("17"));
        assert_eq!(nvme::bay_of_slot("17"), Some(17));
        assert_eq!(nvme::locate_led(&sys, &dev17, Some("17")), Some(sys.join("bus/pci/slots/17/attention")));

        let dev142 = nvme::device_path(&sys, "nvme9n1").expect("multipath head followed to its controller");
        assert!(dev142.ends_with("10000:04:00.0/nvme/nvme9/nvme9c9n1"));
        assert_eq!(nvme::slot(&sys, &dev142).as_deref(), Some("142"));
        assert_eq!(
            nvme::locate_led(&sys, &dev142, Some("142")),
            Some(sys.join("class/leds/10000:02:01.0:enclosure:locate/brightness")),
            "no attention file: the NPEM LED on the switch port above it"
        );
        assert_eq!(nvme::bay_of_slot("NVMe-A3"), None);
        let _ = std::fs::remove_dir_all(&sys);
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

    #[test]
    fn sas_phy_and_expander_from_the_device_path() {
        let root = std::env::temp_dir().join(format!("stormdrive-sas-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mk = |p: &std::path::Path, f: &str, v: &str| {
            std::fs::create_dir_all(p).unwrap();
            std::fs::write(p.join(f), v).unwrap();
        };

        // Behind a shelf expander: the expander phy and its SAS address.
        let host = root.join("devices/pci0000:00/0000:01:00.0/host0");
        let exp = host.join("port-0:0/expander-0:0");
        let port = exp.join("port-0:0:5");
        mk(&exp.join("sas_device/expander-0:0"), "sas_address", "0x500a09800abc0001\n");
        mk(&port.join("phy-0:0:5"), "phy_identifier", "5\n");
        let sdc = port.join("end_device-0:0:5/target0:0:5/0:0:5:0/block/sdc");
        std::fs::create_dir_all(&sdc).unwrap();
        assert_eq!(sas_attachment(&sdc), (Some(5), Some("0x500a09800abc0001".into())));

        // Direct to the HBA over a wide port: the lowest phy, no expander.
        let port = host.join("port-0:4");
        mk(&port.join("phy-0:6"), "phy_identifier", "6");
        mk(&port.join("phy-0:4"), "phy_identifier", "4");
        let sda = port.join("end_device-0:4/target0:0:0/0:0:0:0/block/sda");
        std::fs::create_dir_all(&sda).unwrap();
        assert_eq!(sas_attachment(&sda), (Some(4), None));

        // Not SAS at all.
        assert_eq!(sas_attachment(std::path::Path::new("/sys/devices/pci0000:00/0000:00:1d.0/nvme/nvme0/nvme0n1")), (None, None));
        let _ = std::fs::remove_dir_all(&root);
    }
}
