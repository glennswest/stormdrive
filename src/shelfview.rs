//! What a shelf is made of, read from its SES report (#81): the shelf's
//! own identity (shelf ID, chassis serial, part number), its IOMs, power
//! supplies and SAS connectors, how many of the IOMs this node has a path
//! through, and *why* the enclosure reports anything but OK.
//!
//! Pure: it reads a [`ShelfReport`] (plus the node's HBA SAS addresses, to
//! name what a connector is cabled to) and decides nothing on its own.
//! The field names come from the NetApp DS224C/IOM12 descriptors
//! (docs/netapp-shelf.md); a shelf without them still gets the generic
//! parts (status, installed, paths, problems).

use crate::ses::{
    connector_type_name, normalize_sas, Element, ElementStatus, ShelfReport, ET_ENCLOSURE, ET_ESC_ELECTRONICS,
    ET_NETAPP_IOM_ETHERNET, ET_NETAPP_IOM_EXPANDER, ET_POWER_SUPPLY, ET_SAS_CONNECTOR,
};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    /// The shelf ID an operator sets on the shelf (NetApp `ID=`, the
    /// two-digit display on the front).
    pub shelf_id: Option<String>,
    /// The chassis serial (NetApp enclosure `SN=`).
    pub serial: Option<String>,
    pub part_number: Option<String>,
    pub ioms: Vec<Iom>,
    pub power_supplies: Vec<Psu>,
    pub connectors: Vec<Connector>,
    pub multipath: Multipath,
    /// Every element that is not OK (and not merely absent), and the page's
    /// own flags when no element explains them.
    pub problems: Vec<Problem>,
}

/// One I/O module (an ESC electronics element).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Iom {
    pub index: u32,
    pub status: ElementStatus,
    pub installed: bool,
    /// This IOM is the one answering the ESP we read (REPORT bit).
    pub answering: bool,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    pub part_number: Option<String>,
    /// The IOM's expander SAS address (NetApp vendor element `SA=`).
    pub expander_sas_address: Option<String>,
    /// The IOM's Ethernet MAC (NetApp vendor element `OM=`).
    pub mac: Option<String>,
    /// This node reaches the shelf through this IOM (one of the shelf's
    /// ESPs sits on its expander). None when the IOM's address is unknown.
    pub path: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Psu {
    pub index: u32,
    pub status: ElementStatus,
    pub installed: bool,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    pub part_number: Option<String>,
    /// Rated output (NetApp `PW=`).
    pub watts: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Connector {
    pub index: u32,
    pub status: ElementStatus,
    pub installed: bool,
    pub kind: Option<String>,
    /// The SAS address at the other end of the cable (NetApp `AA=`).
    pub attached_sas_address: Option<String>,
    /// What that address is on this node ("host0 0000:01:00.0"), if ours.
    pub attached_to: Option<String>,
    pub part_number: Option<String>,
    pub cable_vendor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Multipath {
    pub ioms_installed: usize,
    /// IOMs this node has a path through (known only where the IOM's
    /// expander address is reported).
    pub ioms_with_path: usize,
    /// ESPs (SES devices) this node sees for the shelf.
    pub paths: usize,
    pub cabled_connectors: usize,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Problem {
    /// "device slot 5 (bay 5)", "power supply 1", "enclosure".
    pub element: String,
    pub status: ElementStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sas_address: Option<String>,
}

fn attr(e: &Element, k: &str) -> Option<String> {
    e.attributes.get(k).cloned()
}

fn installed(e: &Element) -> bool {
    !matches!(e.status, ElementStatus::NotInstalled | ElementStatus::Unsupported)
}

fn individuals(rep: &ShelfReport, t: u8) -> impl Iterator<Item = &Element> {
    rep.elements.iter().filter(move |e| e.element_type == t && !e.overall)
}

fn sas_u64(s: &str) -> Option<u64> {
    u64::from_str_radix(&normalize_sas(s), 16).ok()
}

/// An ESP sits on an expander when its address is the expander's or the
/// one below it (an IOM12's SES target is the expander address − 1:
/// expander 500a09800853bf4d, ESP 500a09800853bf4c).
fn esp_on(esp: &str, expander: &str) -> bool {
    match (sas_u64(esp), sas_u64(expander)) {
        (Some(a), Some(x)) => a == x || a.checked_add(1) == Some(x),
        _ => false,
    }
}

fn label(e: &Element) -> String {
    match e.bay {
        Some(b) => format!("{} {} (bay {b})", e.type_name, e.index),
        None => format!("{} {}", e.type_name, e.index),
    }
}

/// `hba_sas`: this node's HBA SAS addresses (normalized) → a name.
pub fn summarize(rep: &ShelfReport, hba_sas: &BTreeMap<String, String>) -> Summary {
    let enclosure = individuals(rep, ET_ENCLOSURE).next();
    let expanders: Vec<&Element> = individuals(rep, ET_NETAPP_IOM_EXPANDER).collect();
    let ethernet: Vec<&Element> = individuals(rep, ET_NETAPP_IOM_ETHERNET).collect();
    let esp_addrs: Vec<&str> = rep.esps.iter().filter_map(|e| e.sas_address.as_deref()).collect();

    let ioms: Vec<Iom> = individuals(rep, ET_ESC_ELECTRONICS)
        .map(|e| {
            let i = e.index as usize;
            let expander_sas_address = expanders.get(i).and_then(|x| attr(x, "SA")).map(|s| normalize_sas(&s));
            let path = expander_sas_address.as_ref().map(|x| esp_addrs.iter().any(|a| esp_on(a, x)));
            Iom {
                index: e.index,
                status: e.status,
                installed: installed(e),
                answering: e.raw[2] & 0x01 != 0,
                serial: attr(e, "SN"),
                firmware: attr(e, "FW"),
                part_number: attr(e, "PN"),
                expander_sas_address,
                mac: ethernet.get(i).and_then(|x| attr(x, "OM")),
                path,
            }
        })
        .collect();

    let power_supplies = individuals(rep, ET_POWER_SUPPLY)
        .map(|e| Psu {
            index: e.index,
            status: e.status,
            installed: installed(e),
            serial: attr(e, "SN"),
            firmware: attr(e, "FW"),
            part_number: attr(e, "PN"),
            watts: attr(e, "PW").and_then(|w| w.parse().ok()),
            flags: e.flags.clone(),
        })
        .collect();

    let connectors: Vec<Connector> = individuals(rep, ET_SAS_CONNECTOR)
        .map(|e| {
            let attached = attr(e, "AA").map(|a| normalize_sas(&a));
            Connector {
                index: e.index,
                status: e.status,
                installed: installed(e),
                kind: if installed(e) { connector_type_name(e.raw[1]).map(Into::into) } else { None },
                attached_to: attached.as_ref().and_then(|a| hba_sas.get(a).cloned()),
                attached_sas_address: attached,
                part_number: attr(e, "PN"),
                cable_vendor: attr(e, "VN"),
            }
        })
        .collect();

    let ioms_installed = ioms.iter().filter(|i| i.installed).count();
    let ioms_with_path = ioms.iter().filter(|i| i.installed && i.path == Some(true)).count();
    let cabled_connectors = connectors.iter().filter(|c| c.attached_sas_address.is_some()).count();
    let paths = rep.esps.len();
    let note = match (ioms_installed, paths) {
        (0, _) => "no IOM reported".to_string(),
        (1, _) => "one IOM installed: single path by hardware".to_string(),
        (n, p) if p >= 2 => format!("{n} IOMs installed, {p} paths seen: multipath"),
        (n, _) => {
            let missing: Vec<String> = ioms
                .iter()
                .filter(|i| i.installed && i.path == Some(false))
                .map(|i| match &i.expander_sas_address {
                    Some(a) => format!("IOM {} ({a})", i.index),
                    None => format!("IOM {}", i.index),
                })
                .collect();
            if missing.is_empty() {
                format!("{n} IOMs installed, one path seen")
            } else {
                format!(
                    "{n} IOMs installed, one path seen: no path through {} — cable it to an HBA port for a second path",
                    missing.join(", ")
                )
            }
        }
    };

    let mut problems: Vec<Problem> = rep
        .elements
        .iter()
        .filter(|e| !e.overall && (e.status.is_bad() || e.fault || e.predicted_failure))
        .map(|e| Problem {
            element: label(e),
            status: e.status,
            flags: e.flags.clone(),
            sas_address: e.sas_address.clone(),
        })
        .collect();
    let page_flag = if rep.unrecoverable {
        Some(ElementStatus::Unrecoverable)
    } else if rep.critical {
        Some(ElementStatus::Critical)
    } else if rep.noncritical {
        Some(ElementStatus::Noncritical)
    } else {
        None
    };
    if let Some(st) = page_flag {
        if !problems.iter().any(|p| p.status == st) {
            problems.push(Problem {
                element: "enclosure (status page flag; no element says why)".into(),
                status: st,
                flags: Vec::new(),
                sas_address: None,
            });
        }
    }

    Summary {
        shelf_id: enclosure.and_then(|e| attr(e, "ID")),
        serial: enclosure.and_then(|e| attr(e, "SN")).or_else(|| rep.shelf.serial.clone()),
        part_number: enclosure.and_then(|e| attr(e, "PN")),
        ioms,
        power_supplies,
        connectors,
        multipath: Multipath {
            ioms_installed,
            ioms_with_path,
            paths,
            cabled_connectors,
            note,
        },
        problems,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ses::{assemble, EspPath, RawPages, ET_DEVICE_SLOT};

    /// The DS224C on the Dell (C2NR0Q2, 2026-10-10), cut down: 3 slots,
    /// 2 PSUs, 2 ESCs, 1 enclosure, 2 connectors, the two NetApp vendor
    /// types — status bytes and descriptors as `/api/v1/shelves` showed.
    fn ds224c() -> ShelfReport {
        let types: [(u8, u8); 7] = [
            (ET_DEVICE_SLOT, 3),
            (ET_POWER_SUPPLY, 2),
            (ET_ESC_ELECTRONICS, 2),
            (ET_ENCLOSURE, 1),
            (ET_SAS_CONNECTOR, 2),
            (ET_NETAPP_IOM_EXPANDER, 2),
            (ET_NETAPP_IOM_ETHERNET, 2),
        ];
        let mut cfg = vec![crate::ses::PAGE_CONFIG, 0, 0, 0, 0, 0, 0, 5];
        cfg.extend_from_slice(&[0x11, 0, types.len() as u8, 36]);
        cfg.extend_from_slice(&[0x50, 0x0a, 0x09, 0x80, 0x0e, 0x35, 0x91, 0x35]);
        cfg.extend_from_slice(b"NETAPP  DS22412IOM12A   0401");
        for (t, n) in types {
            cfg.extend_from_slice(&[t, n, 0, 0]);
        }
        let l = (cfg.len() - 4) as u16;
        cfg[2..4].copy_from_slice(&l.to_be_bytes());

        let elements: Vec<([u8; 4], &str)> = vec![
            ([0, 0, 0, 0], ""),
            ([1, 0, 0, 0], ""),
            ([3, 5, 0, 0], ""),
            ([5, 17, 0, 0], ""),
            ([0, 0, 0, 0], ""),
            ([1, 0, 0, 32], "TP=7D;SN=PSQ094201003468;FW=0111;PN=114-00148+F0;PW=913 ;IV=110 ;"),
            ([5, 0, 0, 32], "TP=;  SN=;               FW=;    PN=;            PW=;    IV=;"),
            ([0, 0, 0, 0], ""),
            ([1, 0, 1, 128], "TP=BA;SN=032026007488   ;FW=0401;CV=24;PN=111-02850+C5;AI=A;NA=A;IF=;"),
            ([1, 0, 0, 128], "TP=BA;SN=032026007862   ;FW=0401;CV=24;PN=111-02850+C5;AI=A;NA=A;IF=;"),
            ([0, 0, 0, 0], ""),
            ([1, 0, 2, 0], "ID=02 ;MPN=110-00541+A0;WWN=500a09800e359135;PN=116-00652+C0;SN=SHFGB2037000386;"),
            ([0, 0, 0, 0], ""),
            ([1, 5, 255, 0], "SN=2581210064      ;VN=OIKWAN          ;CT=0f01a000;PN=AQ-1-MS441031000;AA=500605B00DACC340;UA=1;AP=00;"),
            ([5, 0, 255, 0], "SN=;                VN=;                CT=;        PN=;                AA=;                UA=; AP=;"),
            ([0, 0, 0, 0], ""),
            ([1, 136, 0, 224], "FI=00;FM=10;SA=500A09800853BF4D;FPI=IOM12A  ;"),
            ([1, 128, 0, 224], "FI=00;FM=10;SA=500A09800853BFA5;FPI=IOM12A  ;"),
            ([0, 0, 0, 0], ""),
            ([1, 24, 0, 0], "OM=D0:39:EA:44:F1:4F;"),
            ([1, 24, 0, 0], "OM=D0:39:EA:44:F1:65;"),
        ];
        let mut st = vec![crate::ses::PAGE_STATUS, 0x04, 0, 0, 0, 0, 0, 5];
        let mut desc = vec![crate::ses::PAGE_DESCRIPTORS, 0, 0, 0, 0, 0, 0, 5];
        for (b, d) in &elements {
            st.extend_from_slice(b);
            desc.extend_from_slice(&[0, 0]);
            desc.extend_from_slice(&(d.len() as u16).to_be_bytes());
            desc.extend_from_slice(d.as_bytes());
        }
        let l = (st.len() - 4) as u16;
        st[2..4].copy_from_slice(&l.to_be_bytes());
        let l = (desc.len() - 4) as u16;
        desc[2..4].copy_from_slice(&l.to_be_bytes());
        let mut help = vec![crate::ses::PAGE_HELP, 0, 0, 0];
        help.extend_from_slice(b"PSU 2 missing\0\0");
        let l = (help.len() - 4) as u16;
        help[2..4].copy_from_slice(&l.to_be_bytes());

        let esp = EspPath {
            scsi_id: "0:0:18:0".into(),
            sg_path: Some("/dev/sg18".into()),
            sas_address: Some("500a09800853bf4c".into()),
            serial: None,
            revision: Some("0401".into()),
        };
        let pages = RawPages { config: &cfg, status: &st, descriptors: Some(&desc), help: Some(&help), ..Default::default() };
        assemble(vec![esp], None, pages).unwrap()
    }

    fn hbas() -> BTreeMap<String, String> {
        BTreeMap::from([("500605b00dacc340".to_string(), "host0 0000:01:00.0".to_string())])
    }

    #[test]
    fn descriptors_become_attributes_and_vendor_types_are_named() {
        let rep = ds224c();
        let psu = rep.elements.iter().find(|e| e.element_type == ET_POWER_SUPPLY && !e.overall).unwrap();
        assert_eq!(psu.attributes.get("PW").map(String::as_str), Some("913"), "value trimmed");
        let absent = rep.elements.iter().filter(|e| e.element_type == ET_POWER_SUPPLY && !e.overall).nth(1).unwrap();
        assert!(absent.attributes.is_empty(), "empty values dropped");
        let x = rep.elements.iter().find(|e| e.element_type == ET_NETAPP_IOM_EXPANDER).unwrap();
        assert_eq!(x.type_name, "iom expander");
        assert_eq!(rep.shelf.serial.as_deref(), Some("SHFGB2037000386"), "chassis serial from the enclosure element");
        assert_eq!(rep.help_text.as_deref(), Some("PSU 2 missing"));
        assert!(crate::ses::parse_attributes("Bay 0").is_empty(), "a plain name is not a list");
    }

    #[test]
    fn ds224c_summary_names_ioms_psus_connectors_and_the_missing_path() {
        let s = summarize(&ds224c(), &hbas());
        assert_eq!(s.shelf_id.as_deref(), Some("02"));
        assert_eq!(s.serial.as_deref(), Some("SHFGB2037000386"));
        assert_eq!(s.part_number.as_deref(), Some("116-00652+C0"));

        assert_eq!(s.ioms.len(), 2);
        let (a, b) = (&s.ioms[0], &s.ioms[1]);
        assert!(a.installed && a.answering && a.path == Some(true));
        assert_eq!(a.serial.as_deref(), Some("032026007488"));
        assert_eq!(a.firmware.as_deref(), Some("0401"));
        assert_eq!(a.expander_sas_address.as_deref(), Some("500a09800853bf4d"));
        assert_eq!(a.mac.as_deref(), Some("D0:39:EA:44:F1:4F"));
        assert!(b.installed && !b.answering && b.path == Some(false));

        assert_eq!(s.power_supplies.len(), 2);
        assert_eq!(s.power_supplies[0].watts, Some(913));
        assert_eq!(s.power_supplies[0].serial.as_deref(), Some("PSQ094201003468"));
        assert!(!s.power_supplies[1].installed);

        let c = &s.connectors[0];
        assert_eq!(c.kind.as_deref(), Some("Mini SAS HD 4x receptacle (SFF-8644)"));
        assert_eq!(c.attached_sas_address.as_deref(), Some("500605b00dacc340"));
        assert_eq!(c.attached_to.as_deref(), Some("host0 0000:01:00.0"));
        assert!(s.connectors[1].attached_sas_address.is_none() && s.connectors[1].kind.is_none());

        assert_eq!(s.multipath.ioms_installed, 2);
        assert_eq!(s.multipath.ioms_with_path, 1);
        assert_eq!(s.multipath.paths, 1);
        assert_eq!(s.multipath.cabled_connectors, 1);
        assert!(s.multipath.note.contains("no path through IOM 1 (500a09800853bfa5)"), "{}", s.multipath.note);
    }

    #[test]
    fn problems_are_the_bad_individual_elements_not_the_overall_ones() {
        let s = summarize(&ds224c(), &hbas());
        // The Dell's DS224C: bay 5's slot is noncritical, and the enclosure
        // element (status ok) has FAILURE INDICATION set — the shelf's
        // fault LED is lit. Overall elements (all zero) are not problems.
        assert_eq!(s.problems.len(), 2, "{:?}", s.problems);
        assert_eq!(s.problems[0].element, "device slot 1 (bay 5)");
        assert_eq!(s.problems[0].status, ElementStatus::Noncritical);
        assert_eq!(s.problems[1].element, "enclosure 0");
        assert_eq!(s.problems[1].flags, vec!["failure indicated".to_string()]);

        // The page says critical and no element does → the page's flag.
        let mut rep = ds224c();
        rep.critical = true;
        let s = summarize(&rep, &hbas());
        assert!(s.problems.iter().any(|p| p.status == ElementStatus::Critical && p.element.starts_with("enclosure")));
    }

    #[test]
    fn second_path_makes_it_multipath() {
        let mut rep = ds224c();
        let mut b = rep.esps[0].clone();
        b.scsi_id = "1:0:18:0".into();
        b.sas_address = Some("500a09800853bfa4".into());
        rep.esps.push(b);
        let s = summarize(&rep, &hbas());
        assert_eq!(s.multipath.ioms_with_path, 2);
        assert!(s.ioms.iter().all(|i| i.path == Some(true)));
        assert!(s.multipath.note.ends_with("multipath"));
    }

    #[test]
    fn esp_address_is_the_expanders_or_one_below() {
        assert!(esp_on("500a09800853bf4c", "500A09800853BF4D"));
        assert!(esp_on("0x500a09800853bf4d", "500a09800853bf4d"));
        assert!(!esp_on("500a09800853bf4c", "500a09800853bfa5"));
    }
}
