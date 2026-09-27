//! Host bus adapters: what every drive hangs off, and what firmware it runs.
//!
//! Inventory only (#2, owner's decision 2026-09-24): stormdrive reports the
//! HBA's firmware, BIOS and NVDATA versions; it does not flash them, and
//! the node's own BIOS belongs to stormipmi.
//!
//! Source: `/sys/class/scsi_host/hostN`. Each is a symlink into
//! `/sys/devices/…/<pci bdf>/…/hostN`; hosts behind one PCIe function (an
//! AHCI controller has one host per port) are one HBA. The firmware
//! attribute name is driver-specific — mpt3sas `version_fw`, smartpqi and
//! aacraid `firmware_version`, hpsa `firmware_revision`, qla2xxx
//! `fw_version`, lpfc `fwrev` — and absent on AHCI and virtio.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// One HBA: a PCIe function with one or more SCSI hosts on it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hba {
    /// PCIe address (`0000:01:00.0`); the key.
    pub pcie_addr: String,
    /// SCSI hosts on this function, sorted by number (`host0`, `host1`, …).
    pub scsi_hosts: Vec<String>,
    /// Kernel driver bound to the PCIe function, else the host's proc_name.
    pub driver: Option<String>,
    /// PCI vendor:device and subsystem ids, lower-case hex without `0x`.
    pub pci_id: Option<String>,
    pub pci_subsystem: Option<String>,
    /// Board name as the firmware reports it (mpt3sas: "SAS9300-8i").
    pub board_name: Option<String>,
    pub board_assembly: Option<String>,
    /// Board tracer / serial number, when the driver exposes one.
    pub board_tracer: Option<String>,
    /// Controller firmware version.
    pub firmware: Option<String>,
    /// Option ROM (boot BIOS) version on the card — not the node's BIOS.
    pub bios: Option<String>,
    /// mpt3sas NVDATA (persistent configuration) version.
    pub nvdata: Option<String>,
    /// The HBA's own SAS address.
    pub sas_address: Option<String>,
}

impl Hba {
    /// What an operator calls the card.
    pub fn display(&self) -> String {
        match (&self.board_name, &self.driver) {
            (Some(b), _) => b.clone(),
            (None, Some(d)) => d.clone(),
            (None, None) => self.pcie_addr.clone(),
        }
    }
}

/// Every HBA on the node, by PCIe address.
pub type Hbas = BTreeMap<String, Hba>;

const FIRMWARE_ATTRS: &[&str] = &[
    "version_fw",
    "firmware_version",
    "firmware_revision",
    "fw_version",
    "fwrev",
];

/// The PCIe function a host's sysfs link lands under: the last BDF in the
/// path. None for a host that is not on PCIe at all, and for USB mass
/// storage, whose last BDF is the USB controller, not an HBA.
pub fn bdf_of_link(link: &str) -> Option<String> {
    let mut bdf = None;
    for comp in link.split('/') {
        if comp.starts_with("usb") {
            return None;
        }
        if crate::topology::is_pci_bdf(comp) {
            bdf = Some(comp.to_string());
        }
    }
    bdf
}

fn clean(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().trim_matches('\0').trim().to_string())
        .filter(|s| !s.is_empty() && s != "N/A")
}

fn hex_id(v: Option<String>) -> Option<String> {
    clean(v).map(|s| s.trim_start_matches("0x").to_lowercase())
}

/// Build the HBA list from each host's link target and its attributes.
/// `host_attr(host, name)` reads `/sys/class/scsi_host/<host>/<name>`;
/// `pci_attr(bdf, name)` reads `/sys/bus/pci/devices/<bdf>/<name>`
/// (`driver` returns the bound driver's name).
pub fn assemble(
    hosts: &[(String, String)],
    host_attr: impl Fn(&str, &str) -> Option<String>,
    pci_attr: impl Fn(&str, &str) -> Option<String>,
) -> Hbas {
    let mut out = Hbas::new();
    let mut hosts: Vec<&(String, String)> = hosts.iter().collect();
    hosts.sort_by_key(|(h, _)| h.trim_start_matches("host").parse::<u32>().unwrap_or(u32::MAX));
    for (host, link) in hosts {
        let Some(bdf) = bdf_of_link(link) else {
            continue;
        };
        let hba = out.entry(bdf.clone()).or_insert_with(|| {
            let ids = |a: &str, b: &str| match (hex_id(pci_attr(&bdf, a)), hex_id(pci_attr(&bdf, b))) {
                (Some(v), Some(d)) => Some(format!("{v}:{d}")),
                _ => None,
            };
            Hba {
                pcie_addr: bdf.clone(),
                driver: clean(pci_attr(&bdf, "driver")),
                pci_id: ids("vendor", "device"),
                pci_subsystem: ids("subsystem_vendor", "subsystem_device"),
                ..Default::default()
            }
        });
        hba.scsi_hosts.push(host.clone());
        let a = |name: &str| clean(host_attr(host, name));
        if hba.driver.is_none() {
            hba.driver = a("proc_name");
        }
        // First host that reports a value wins; they share one card.
        let fill = |slot: &mut Option<String>, v: Option<String>| {
            if slot.is_none() {
                *slot = v;
            }
        };
        fill(&mut hba.firmware, FIRMWARE_ATTRS.iter().find_map(|&n| a(n)));
        fill(&mut hba.bios, a("version_bios"));
        fill(&mut hba.nvdata, a("version_nvdata_persistent"));
        fill(&mut hba.board_name, a("board_name"));
        fill(&mut hba.board_assembly, a("board_assembly"));
        fill(&mut hba.board_tracer, a("board_tracer"));
        fill(&mut hba.sas_address, a("host_sas_address"));
    }
    out
}

/// Scan sysfs. Empty where there is no sysfs.
pub fn scan() -> Hbas {
    scan_at(Path::new("/sys"))
}

pub fn scan_at(sys: &Path) -> Hbas {
    let class = sys.join("class/scsi_host");
    let mut hosts = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&class) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Ok(target) = std::fs::read_link(e.path()) {
                hosts.push((name, target.to_string_lossy().to_string()));
            }
        }
    }
    let read = |p: std::path::PathBuf| std::fs::read_to_string(p).ok();
    assemble(
        &hosts,
        |host, name| read(class.join(host).join(name)),
        |bdf, name| {
            let dev = sys.join("bus/pci/devices").join(bdf);
            if name == "driver" {
                std::fs::read_link(dev.join("driver"))
                    .ok()
                    .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
            } else {
                read(dev.join(name))
            }
        },
    )
}

/// What changed between two scans, as event messages: cards that appeared
/// or went away, and firmware/BIOS/NVDATA that changed under a card that
/// stayed (someone flashed it).
pub fn diff(old: &Hbas, new: &Hbas) -> Vec<String> {
    let mut out = Vec::new();
    for (bdf, n) in new {
        let Some(o) = old.get(bdf) else {
            out.push(format!(
                "HBA {} at {bdf}: firmware {}",
                n.display(),
                n.firmware.as_deref().unwrap_or("unknown")
            ));
            continue;
        };
        for (what, a, b) in [
            ("firmware", &o.firmware, &n.firmware),
            ("BIOS", &o.bios, &n.bios),
            ("NVDATA", &o.nvdata, &n.nvdata),
        ] {
            if a != b && b.is_some() {
                out.push(format!(
                    "HBA {} at {bdf}: {what} {} → {}",
                    n.display(),
                    a.as_deref().unwrap_or("unknown"),
                    b.as_deref().unwrap_or("unknown")
                ));
            }
        }
    }
    for (bdf, o) in old {
        if !new.contains_key(bdf) {
            out.push(format!("HBA {} at {bdf}: gone", o.display()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // The R230: an LSI SAS3008 on mpt3sas, as sysfs showed it.
    const R230: &str = "../../devices/pci0000:00/0000:00:01.0/0000:01:00.0/host0/scsi_host/host0";

    fn host_attrs(host: &str, name: &str) -> Option<String> {
        match (host, name) {
            ("host0", "proc_name") => Some("mpt3sas\n".into()),
            ("host0", "version_fw") => Some("16.00.10.00\n".into()),
            ("host0", "version_bios") => Some("08.37.00.00\n".into()),
            ("host0", "version_nvdata_persistent") => Some("0e.01.00.07\n".into()),
            ("host0", "board_name") => Some("SAS3008\n".into()),
            ("host0", "board_tracer") => Some("\n".into()),
            ("host0", "host_sas_address") => Some("0x500605b00dacc340\n".into()),
            (_, "proc_name") => Some("ahci\n".into()),
            _ => None,
        }
    }

    fn pci_attrs(bdf: &str, name: &str) -> Option<String> {
        match (bdf, name) {
            ("0000:01:00.0", "driver") => Some("mpt3sas".into()),
            ("0000:01:00.0", "vendor") => Some("0x1000\n".into()),
            ("0000:01:00.0", "device") => Some("0x0097\n".into()),
            ("0000:01:00.0", "subsystem_vendor") => Some("0x1028\n".into()),
            ("0000:01:00.0", "subsystem_device") => Some("0x1f45\n".into()),
            ("0000:00:17.0", "driver") => Some("ahci".into()),
            _ => None,
        }
    }

    #[test]
    fn mpt3sas_card_reports_its_versions() {
        let hbas = assemble(&[("host0".into(), R230.into())], host_attrs, pci_attrs);
        let h = &hbas["0000:01:00.0"];
        assert_eq!(h.scsi_hosts, vec!["host0"]);
        assert_eq!(h.driver.as_deref(), Some("mpt3sas"));
        assert_eq!(h.firmware.as_deref(), Some("16.00.10.00"));
        assert_eq!(h.bios.as_deref(), Some("08.37.00.00"));
        assert_eq!(h.nvdata.as_deref(), Some("0e.01.00.07"));
        assert_eq!(h.board_name.as_deref(), Some("SAS3008"));
        assert_eq!(h.board_tracer, None, "blank attribute is no value");
        assert_eq!(h.sas_address.as_deref(), Some("0x500605b00dacc340"));
        assert_eq!(h.pci_id.as_deref(), Some("1000:0097"));
        assert_eq!(h.pci_subsystem.as_deref(), Some("1028:1f45"));
        assert_eq!(h.display(), "SAS3008");
    }

    #[test]
    fn ahci_ports_are_one_card_without_firmware() {
        let port = |n: u32| {
            (
                format!("host{n}"),
                format!("../../devices/pci0000:00/0000:00:17.0/ata{}/host{n}/scsi_host/host{n}", n + 1),
            )
        };
        let hbas = assemble(&[port(10), port(2), port(1)], host_attrs, pci_attrs);
        assert_eq!(hbas.len(), 1);
        let h = &hbas["0000:00:17.0"];
        assert_eq!(h.scsi_hosts, vec!["host1", "host2", "host10"], "numeric order");
        assert_eq!(h.driver.as_deref(), Some("ahci"));
        assert_eq!(h.firmware, None);
        assert_eq!(h.display(), "ahci");
    }

    #[test]
    fn usb_and_virtual_hosts_are_not_hbas() {
        assert_eq!(
            bdf_of_link("../../devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/scsi_host/host6"),
            None
        );
        assert_eq!(bdf_of_link("../../devices/platform/host7/scsi_host/host7"), None);
        assert_eq!(bdf_of_link(R230).as_deref(), Some("0000:01:00.0"));
    }

    #[test]
    fn firmware_attribute_names_by_driver() {
        for attr in FIRMWARE_ATTRS {
            let hbas = assemble(
                &[("host0".into(), R230.into())],
                |_, n| (n == *attr).then(|| "7.1".to_string()),
                |_, _| None,
            );
            assert_eq!(hbas["0000:01:00.0"].firmware.as_deref(), Some("7.1"), "{attr}");
        }
    }

    #[test]
    fn diff_names_a_flash_and_a_new_card() {
        let old = assemble(&[("host0".into(), R230.into())], host_attrs, pci_attrs);
        assert!(diff(&old, &old).is_empty());
        let mut new = old.clone();
        new.get_mut("0000:01:00.0").unwrap().firmware = Some("16.00.12.00".into());
        assert_eq!(
            diff(&old, &new),
            vec!["HBA SAS3008 at 0000:01:00.0: firmware 16.00.10.00 → 16.00.12.00"]
        );
        let d = diff(&Hbas::new(), &old);
        assert_eq!(d, vec!["HBA SAS3008 at 0000:01:00.0: firmware 16.00.10.00"]);
        assert_eq!(diff(&old, &Hbas::new()), vec!["HBA SAS3008 at 0000:01:00.0: gone"]);
    }
}
