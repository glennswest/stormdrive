//! A backplane with no SES device behind a SAS HBA is a shelf too (#152).
//!
//! forge1's R730xd: four drives on the internal backplane behind an HBA330
//! (mpt3sas), no enclosure device in `lsscsi`. Every enclosure is a shelf
//! (internal, front, rear, external), SES or not. mpt3sas names the
//! enclosure in the SAS transport anyway — `sas_device/end_device-*` has
//! `enclosure_identifier` (here the HBA's own SAS address) and
//! `bay_identifier` — and [`crate::topology::locate`] already places each
//! drive there. What was missing was the shelf itself: with no SES report and
//! no RAID controller's, no Shelf object was written.
//!
//! [`scan`] makes one [`ShelfReport`] per such enclosure, read through the
//! HBA (`through.driver` = its driver, `through.bay_phys`): every bay with an
//! end device, each with the HBA (or expander) phy facing it and the rate it
//! negotiated. When the drives hang directly off the HBA and every bay is its
//! phy number (a direct-attached backplane), each HBA phy is a bay, and one
//! with no drive is empty. Behind an expander with no SES only the occupied
//! bays are known.
//!
//! An enclosure an SES or RAID report already covers is left to it.

use crate::drive::Shelf;
use crate::megaraid::Through;
use crate::ses::ShelfReport;
use crate::topology::Shelves;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One bay of a shelf read through its HBA.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BayPhy {
    /// The phy facing the bay (`phy_identifier`): the HBA's when
    /// direct-attached, the expander's behind one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phy: Option<u32>,
    /// Its sysfs name ("phy-1:2").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phy_name: Option<String>,
    /// The negotiated rate in Mbit/s; None when nothing is linked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_mbps: Option<u32>,
    /// The end device's SAS address; None = the bay is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sas_address: Option<String>,
}

fn read_trim(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// A phy directory's class attributes (`<dir>/sas_phy/<name>`, or the
/// directory itself): (phy_identifier, negotiated rate).
fn phy_attrs(dir: &Path) -> (Option<u32>, Option<u32>) {
    let Some(name) = dir.file_name().map(|n| n.to_string_lossy().to_string()) else {
        return (None, None);
    };
    let attrs = [dir.join("sas_phy").join(&name), dir.to_path_buf()]
        .into_iter()
        .find(|d| d.join("phy_identifier").exists() || d.join("negotiated_linkrate").exists());
    let Some(a) = attrs else { return (None, None) };
    (
        read_trim(&a.join("phy_identifier")).and_then(|s| s.parse().ok()),
        read_trim(&a.join("negotiated_linkrate")).and_then(|s| crate::link::parse_sysfs_rate(&s)),
    )
}

/// One end device the transport places in an enclosure.
struct EndDevice {
    enclosure: String,
    bay: u32,
    sas_address: Option<String>,
    host: String,
    host_dir: PathBuf,
    pcie: Option<String>,
    behind_expander: bool,
    phy: Option<(String, Option<u32>, Option<u32>)>,
}

fn end_device(class_dir: &Path) -> Option<EndDevice> {
    let enclosure = read_trim(&class_dir.join("enclosure_identifier"))
        .map(|s| crate::ses::normalize_sas(&s))
        .filter(|s| s.chars().any(|c| c != '0'))?;
    let bay = read_trim(&class_dir.join("bay_identifier"))?.parse().ok()?;
    let real = std::fs::canonicalize(class_dir).ok()?;
    let comps: Vec<String> = real.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect();
    let hi = comps.iter().position(|c| c.starts_with("host") && c.len() > 4 && c[4..].chars().all(|d| d.is_ascii_digit()))?;
    let host_dir: PathBuf = comps[..=hi].iter().collect();
    let pcie = comps[..hi].iter().rev().find(|c| crate::topology::is_pci_bdf(c)).cloned();
    let phy = crate::link::facing_phys(&real).into_iter().find_map(|d| {
        let name = d.file_name()?.to_string_lossy().to_string();
        let (id, rate) = phy_attrs(&d);
        Some((name, id, rate))
    });
    Some(EndDevice {
        enclosure,
        bay,
        sas_address: read_trim(&class_dir.join("sas_address")).map(|s| crate::ses::normalize_sas(&s)),
        host: comps[hi].clone(),
        host_dir,
        pcie,
        behind_expander: crate::topology::behind_expander(&real),
        phy,
    })
}

/// The HBA's own phys (`<host>/phy-H:N`), by phy_identifier.
fn host_phys(host_dir: &Path) -> BTreeMap<u32, (String, Option<u32>)> {
    std::fs::read_dir(host_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("phy-"))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let (id, rate) = phy_attrs(&e.path());
            Some((id?, (name, rate)))
        })
        .collect()
}

fn driver_of(sys: &Path, pcie: Option<&str>) -> Option<String> {
    let link = std::fs::read_link(sys.join("bus/pci/devices").join(pcie?).join("driver")).ok()?;
    Some(link.file_name()?.to_string_lossy().to_string())
}

/// Every enclosure the SAS transport names that `have` (the SES and RAID
/// reports) does not cover, as a shelf read through its HBA.
pub fn scan(have: &Shelves) -> Shelves {
    scan_in(Path::new("/sys"), have)
}

/// [`scan`] in a sysfs tree at `sys`.
pub fn scan_in(sys: &Path, have: &Shelves) -> Shelves {
    let covered = |enc: &str| have.contains_key(enc) || have.values().any(|r| r.shelf.logical_id.as_deref() == Some(enc));
    let mut by_enc: BTreeMap<String, Vec<EndDevice>> = BTreeMap::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(sys.join("class/sas_device")).into_iter().flatten().flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("end_device-")) {
            continue;
        }
        if let Some(d) = end_device(&p).filter(|d| !covered(&d.enclosure)) {
            by_enc.entry(d.enclosure.clone()).or_default().push(d);
        }
    }
    by_enc.into_iter().map(|(enc, devs)| (enc.clone(), report(sys, enc, &devs))).collect()
}

fn report(sys: &Path, enc: String, devs: &[EndDevice]) -> ShelfReport {
    let first = &devs[0];
    let behind = devs.iter().any(|d| d.behind_expander);
    let mut bay_phys: BTreeMap<u32, BayPhy> = BTreeMap::new();
    let mut slots: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for d in devs {
        if let Some(a) = &d.sas_address {
            slots.entry(d.bay).or_default().push(a.clone());
        }
        bay_phys.insert(
            d.bay,
            BayPhy {
                phy: d.phy.as_ref().and_then(|p| p.1),
                phy_name: d.phy.as_ref().map(|p| p.0.clone()),
                rate_mbps: d.phy.as_ref().and_then(|p| p.2),
                sas_address: d.sas_address.clone(),
            },
        );
    }
    // A direct-attached backplane: every bay is its HBA phy, so the HBA's
    // other phys are the empty bays.
    let one_host = devs.iter().all(|d| d.host_dir == first.host_dir);
    if !behind && one_host && bay_phys.iter().all(|(b, p)| p.phy == Some(*b)) {
        for (id, (name, rate)) in host_phys(&first.host_dir) {
            bay_phys.entry(id).or_insert(BayPhy { phy: Some(id), phy_name: Some(name), rate_mbps: rate, sas_address: None });
        }
    }
    let shelf = Shelf {
        logical_id: Some(enc.clone()),
        position: Some(crate::topology::position_no_enclosure(behind)),
        ..Default::default()
    };
    ShelfReport {
        key: enc,
        shelf,
        esps: Vec::new(),
        generation: 0,
        critical: false,
        noncritical: false,
        unrecoverable: false,
        info: false,
        elements: Vec::new(),
        slots,
        help_text: None,
        through: Some(Through {
            driver: driver_of(sys, first.pcie.as_deref()).unwrap_or_else(|| "sas".into()),
            scsi_host: first.host.clone(),
            pcie_addr: first.pcie.clone(),
            enclosure_device_id: None,
            device_bays: BTreeMap::new(),
            no_enclosure_devices: Vec::new(),
            slot_not_matched: Vec::new(),
            controller_slots: BTreeMap::new(),
            bay_numbering: Some(crate::bays::BayNumbering::Controller),
            bay_map: None,
            error: None,
            bay_phys,
        }),
        collected_at: SystemTime::now(),
        status_raw: Vec::new(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::drive::Position;

    struct Tree(PathBuf);
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn w(p: &Path, v: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, format!("{v}\n")).unwrap();
    }

    /// forge1: an HBA330 at 0000:c1:00.0 (host1, 8 phys), four drives
    /// direct-attached in bays 0..3 on phys 0..3, enclosure = the HBA's SAS
    /// address, no SES device.
    fn forge1(tag: &str) -> Tree {
        let root = std::env::temp_dir().join(format!("stormdrive-hbaenc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let pci = root.join("devices/pci0000:c0/0000:c0:01.1/0000:c1:00.0");
        let host = pci.join("host1");
        let drv = root.join("bus/pci/drivers/mpt3sas");
        std::fs::create_dir_all(&drv).unwrap();
        std::fs::create_dir_all(root.join("bus/pci/devices/0000:c1:00.0")).unwrap();
        std::os::unix::fs::symlink(&drv, root.join("bus/pci/devices/0000:c1:00.0/driver")).unwrap();
        for n in 0..8u32 {
            let attrs = host.join(format!("phy-1:{n}/sas_phy/phy-1:{n}"));
            w(&attrs.join("phy_identifier"), &n.to_string());
            w(&attrs.join("negotiated_linkrate"), if n < 4 { "12.0 Gbit" } else { "Unknown" });
        }
        std::fs::create_dir_all(root.join("class/sas_device")).unwrap();
        for n in 0..4u32 {
            let port = host.join(format!("port-1:{n}"));
            std::fs::create_dir_all(&port).unwrap();
            std::os::unix::fs::symlink(host.join(format!("phy-1:{n}")), port.join(format!("phy-1:{n}"))).unwrap();
            let cls = port.join(format!("end_device-1:{n}/sas_device/end_device-1:{n}"));
            w(&cls.join("enclosure_identifier"), "0x52cea7f04e8aa500");
            w(&cls.join("bay_identifier"), &n.to_string());
            w(&cls.join("sas_address"), &format!("0x5000cca26801b64{n}"));
            std::os::unix::fs::symlink(&cls, root.join(format!("class/sas_device/end_device-1:{n}"))).unwrap();
        }
        Tree(root)
    }

    #[test]
    fn a_backplane_with_no_ses_is_an_internal_shelf_with_every_phy_a_bay() {
        let t = forge1("direct");
        let shelves = scan_in(&t.0, &Shelves::new());
        assert_eq!(shelves.len(), 1);
        let r = &shelves["52cea7f04e8aa500"];
        assert_eq!(r.shelf.key().as_deref(), Some("52cea7f04e8aa500"));
        assert_eq!(r.shelf.position, Some(Position::Internal));
        let th = r.through.as_ref().unwrap();
        assert_eq!(th.driver, "mpt3sas");
        assert!(!th.is_raid());
        assert_eq!(th.via(), "its HBA (mpt3sas)");
        assert_eq!(th.scsi_host, "host1");
        assert_eq!(th.pcie_addr.as_deref(), Some("0000:c1:00.0"));
        assert_eq!(th.bay_phys.len(), 8, "{:?}", th.bay_phys);
        let b2 = &th.bay_phys[&2];
        assert_eq!((b2.phy, b2.rate_mbps, b2.sas_address.as_deref(), b2.phy_name.as_deref()), (Some(2), Some(12000), Some("5000cca26801b642"), Some("phy-1:2")));
        let b6 = &th.bay_phys[&6];
        assert_eq!((b6.phy, b6.rate_mbps, b6.sas_address.as_deref()), (Some(6), None, None));
        assert_eq!(r.bay_of("0x5000cca26801b643"), Some(3));
        assert_eq!(r.slots.len(), 4);
    }

    #[test]
    fn an_enclosure_ses_already_reports_is_left_to_it() {
        let t = forge1("covered");
        let mut have = Shelves::new();
        let mut ses = scan_in(&t.0, &Shelves::new()).remove("52cea7f04e8aa500").unwrap();
        ses.key = "some-ses-key".into();
        ses.through = None;
        have.insert(ses.key.clone(), ses);
        assert!(scan_in(&t.0, &have).is_empty());
    }

    #[test]
    fn bays_that_are_not_the_phy_numbers_list_only_the_drives() {
        let t = forge1("renumbered");
        // Bay 0 on phy 0 … but bay 3 says 9: not a phy-per-bay backplane.
        w(&t.0.join("class/sas_device/end_device-1:3/bay_identifier"), "9");
        let r = &scan_in(&t.0, &Shelves::new())["52cea7f04e8aa500"];
        let bays: Vec<u32> = r.through.as_ref().unwrap().bay_phys.keys().copied().collect();
        assert_eq!(bays, vec![0, 1, 2, 9]);
    }
}
