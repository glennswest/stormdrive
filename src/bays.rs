//! Bay numbering per platform (#134): which physical bay a controller's slot
//! is, kept as data.
//!
//! A RAID controller numbers its drives its own way. forge1 (Dell PowerEdge
//! R6415, PERC H730 Mini) lists its four front drives as slots 0, 3, 4 and 7
//! (enclosure 32 slots 0 and 3, "Direct" ports 4 and 7); the front panel says
//! 0, 1, 2, 3. Owner: "for this model we need to fix the bay, should be
//! 0,1,2,3 mapped to the 0,3,4,7".
//!
//! The map is configuration (`[[bay_map]]` in stormdrive.toml, the
//! registry's config text), never code: one entry per system model and
//! controller, `slots` = the controller's slot of bay 0, 1, 2, …
//!
//! It applies to the shelves a RAID controller reads ([`crate::megaraid`]):
//! their bays become the physical bays and each drive keeps the controller's
//! slot beside its bay. An SES enclosure keeps its own numbering: its LEDs
//! and power are addressed by it. Where no entry matches, the bay is the
//! controller's slot, and says so (`bay_numbering: controller`).

use crate::ses::ShelfReport;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One platform's map, as configured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BayMap {
    /// A name for the map in the API and events; else "<system> / <controller>".
    #[serde(default)]
    pub name: Option<String>,
    /// The system model: DMI `product_name` ("PowerEdge R6415"), compared
    /// without case.
    pub system: String,
    /// The controller: its model as it reports it ("PERC H730 Mini"), its
    /// board name, PCI subsystem ("1028:1f49") or id, or its driver
    /// ("megaraid_sas"), compared without case.
    pub controller: String,
    /// The enclosure's model ("BP14G+"), when one controller drives more than
    /// one backplane kind on that system. Absent = any.
    #[serde(default)]
    pub enclosure: Option<String>,
    /// The controller's slot of bay 0, 1, 2, …: `[0, 3, 4, 7]`.
    pub slots: Vec<u32>,
}

impl BayMap {
    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| format!("{} / {}", self.system, self.controller))
    }

    /// The physical bay of a controller slot; None when the map does not
    /// name the slot.
    pub fn bay_of(&self, slot: u32) -> Option<u32> {
        self.slots.iter().position(|s| *s == slot).map(|b| b as u32)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.system.trim().is_empty() || self.controller.trim().is_empty() {
            return Err("bay_map: system and controller are required".into());
        }
        if self.slots.is_empty() {
            return Err(format!("bay_map {}: slots is empty", self.label()));
        }
        let mut seen = std::collections::BTreeSet::new();
        for s in &self.slots {
            if !seen.insert(s) {
                return Err(format!("bay_map {}: slot {s} names two bays", self.label()));
            }
        }
        Ok(())
    }
}

/// How a drive's or a shelf's bay is numbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BayNumbering {
    /// The physical bay, from a platform map.
    Platform,
    /// As the controller (or enclosure) numbers it: no map for this platform.
    Controller,
}

/// What a controller is known as: every name an entry's `controller` may use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ControllerNames(pub Vec<String>);

impl ControllerNames {
    /// The names of the controller at `pcie` / `host`: its MFI model, the
    /// HBA's board name, PCI subsystem and id, and the driver.
    pub fn of(pcie: Option<&str>, host: &str, driver: &str, hbas: &crate::hba::Hbas, controllers: &[crate::raidctl::ControllerReport]) -> Self {
        let mut n = vec![driver.to_string()];
        if let Some(c) = controllers.iter().find(|c| c.scsi_host == host || (pcie.is_some() && c.pcie_addr.as_deref() == pcie)) {
            n.extend(c.model.clone());
        }
        if let Some(h) = pcie.and_then(|p| hbas.get(p)) {
            n.extend(h.board_name.clone());
            n.extend(h.pci_subsystem.clone());
            n.extend(h.pci_id.clone());
            n.extend(h.driver.clone());
        }
        Self(n)
    }

    fn has(&self, want: &str) -> bool {
        self.0.iter().any(|n| n.trim().eq_ignore_ascii_case(want.trim()))
    }
}

/// The first entry for this system, controller and enclosure.
pub fn find<'a>(maps: &'a [BayMap], system: Option<&str>, controller: &ControllerNames, enclosure: Option<&str>) -> Option<&'a BayMap> {
    let system = system?.trim();
    maps.iter().find(|m| {
        m.system.trim().eq_ignore_ascii_case(system)
            && controller.has(&m.controller)
            && m.enclosure.as_deref().map_or(true, |e| enclosure.is_some_and(|x| x.trim().eq_ignore_ascii_case(e.trim())))
    })
}

/// Renumber a controller-read shelf by `map`: its bays become physical bays,
/// each device's controller slot is kept, and a slot the map does not name is
/// on the shelf with no bay (never a bay another slot's number would collide
/// with). Without a map the controller's numbering stays and says so.
/// Returns the slots the map did not name.
pub fn apply(report: &mut ShelfReport, map: Option<&BayMap>) -> Vec<u32> {
    let Some(t) = report.through.as_mut() else { return Vec::new() };
    if t.bay_numbering.is_some() {
        return Vec::new(); // already applied
    }
    t.controller_slots = t.device_bays.clone();
    let Some(map) = map else {
        t.bay_numbering = Some(BayNumbering::Controller);
        return Vec::new();
    };
    t.bay_numbering = Some(BayNumbering::Platform);
    t.bay_map = Some(map.label());
    let mut unmapped = Vec::new();
    let mut bays = BTreeMap::new();
    for (dev, slot) in &t.controller_slots {
        match map.bay_of(*slot) {
            Some(b) => {
                bays.insert(*dev, b);
            }
            None => {
                unmapped.push(*slot);
                t.slot_not_matched.push(*dev);
            }
        }
    }
    t.device_bays = bays;
    report.slots = std::mem::take(&mut report.slots).into_iter().filter_map(|(slot, addrs)| map.bay_of(slot).map(|b| (b, addrs))).collect();
    unmapped
}

/// DMI `product_name` under the sysfs root `sys`.
pub fn system_model(sys: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(sys.join("class/dmi/id/product_name")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::Shelf;
    use crate::megaraid::Through;

    fn r6415() -> BayMap {
        BayMap { name: None, system: "PowerEdge R6415".into(), controller: "PERC H730 Mini".into(), enclosure: None, slots: vec![0, 3, 4, 7] }
    }

    /// forge1's shelf as the PERC reads it (#122): device ids 0, 3, 4, 7 in
    /// slots 0, 3, 4, 7.
    fn forge1_shelf() -> ShelfReport {
        let device_bays: BTreeMap<u16, u32> = [(0, 0), (3, 3), (4, 4), (7, 7)].into_iter().collect();
        let slots = [(0, "a"), (3, "b"), (4, "c"), (7, "d")].into_iter().map(|(s, a)| (s, vec![a.to_string()])).collect();
        ShelfReport {
            key: "5e09406039f0cf00".into(),
            shelf: Shelf { model: Some("BP14G+".into()), ..Default::default() },
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
                driver: "megaraid_sas".into(),
                scsi_host: "host0".into(),
                pcie_addr: Some("0000:c1:00.0".into()),
                enclosure_device_id: Some(32),
                device_bays,
                no_enclosure_devices: vec![4, 7],
                slot_not_matched: Vec::new(),
                controller_slots: BTreeMap::new(),
                bay_numbering: None,
                bay_map: None,
                error: None,
            }),
            collected_at: std::time::SystemTime::UNIX_EPOCH,
            status_raw: Vec::new(),
        }
    }

    #[test]
    fn forge1s_slots_0_3_4_7_are_front_bays_0_1_2_3() {
        let names = ControllerNames(vec!["megaraid_sas".into(), "PERC H730 Mini".into()]);
        let maps = [r6415()];
        let m = find(&maps, Some("PowerEdge R6415"), &names, Some("BP14G+")).expect("the R6415 map");
        let mut r = forge1_shelf();
        assert!(apply(&mut r, Some(m)).is_empty());
        let t = r.through.as_ref().unwrap();
        assert_eq!(t.device_bays, [(0, 0), (3, 1), (4, 2), (7, 3)].into_iter().collect());
        assert_eq!(t.controller_slots, [(0, 0), (3, 3), (4, 4), (7, 7)].into_iter().collect(), "the controller's slot kept beside the bay");
        assert_eq!(t.bay_numbering, Some(BayNumbering::Platform));
        assert_eq!(t.bay_map.as_deref(), Some("PowerEdge R6415 / PERC H730 Mini"));
        assert_eq!(r.slots.keys().copied().collect::<Vec<_>>(), vec![0, 1, 2, 3]);
        assert_eq!(r.slots[&2], vec!["c".to_string()]);
        // Applying twice (a later pass over the same table) changes nothing.
        let again = r.clone();
        apply(&mut r, Some(m));
        assert_eq!(r, again);
    }

    #[test]
    fn an_unknown_platform_keeps_the_controllers_numbering_and_says_so() {
        let names = ControllerNames(vec!["megaraid_sas".into(), "PERC H730 Mini".into()]);
        let maps = [r6415()];
        assert!(find(&maps, Some("PowerEdge R640"), &names, None).is_none(), "another system");
        assert!(find(&maps, None, &names, None).is_none(), "no DMI");
        // After the HBA330 swap: another controller, so no map until one is set.
        assert!(find(&maps, Some("PowerEdge R6415"), &ControllerNames(vec!["mpt3sas".into(), "HBA330 Mini".into()]), None).is_none());
        let mut r = forge1_shelf();
        apply(&mut r, None);
        let t = r.through.as_ref().unwrap();
        assert_eq!(t.bay_numbering, Some(BayNumbering::Controller));
        assert_eq!(t.device_bays, t.controller_slots);
        assert_eq!(r.slots.keys().copied().collect::<Vec<_>>(), vec![0, 3, 4, 7]);
    }

    #[test]
    fn controller_and_enclosure_match_any_of_their_names_without_case() {
        let mut m = r6415();
        m.controller = "1028:1F49".into();
        m.enclosure = Some("bp14g+".into());
        let maps = [m];
        let names = ControllerNames(vec!["megaraid_sas".into(), "1028:1f49".into()]);
        assert!(find(&maps, Some("poweredge r6415"), &names, Some("BP14G+")).is_some());
        assert!(find(&maps, Some("PowerEdge R6415"), &names, Some("BP15G")).is_none(), "another backplane");
        assert!(find(&maps, Some("PowerEdge R6415"), &names, None).is_none(), "an enclosure asked for, none known");
    }

    #[test]
    fn a_slot_the_map_does_not_name_has_no_bay() {
        let mut r = forge1_shelf();
        r.through.as_mut().unwrap().device_bays.insert(9, 9);
        r.slots.insert(9, vec!["e".into()]);
        let unmapped = apply(&mut r, Some(&r6415()));
        assert_eq!(unmapped, vec![9]);
        let t = r.through.as_ref().unwrap();
        assert!(!t.device_bays.contains_key(&9) && t.slot_not_matched.contains(&9));
        assert_eq!(t.controller_slots[&9], 9);
        assert_eq!(r.slots.len(), 4);
    }

    #[test]
    fn validate_refuses_a_slot_in_two_bays_and_an_empty_map() {
        assert!(r6415().validate().is_ok());
        let mut m = r6415();
        m.slots = vec![0, 3, 3];
        assert!(m.validate().unwrap_err().contains("slot 3"));
        m.slots.clear();
        assert!(m.validate().is_err());
        m = r6415();
        m.system.clear();
        assert!(m.validate().is_err());
    }

    #[test]
    fn system_model_reads_dmi() {
        let d = std::env::temp_dir().join(format!("stormdrive-bays-{}", std::process::id()));
        std::fs::create_dir_all(d.join("class/dmi/id")).unwrap();
        std::fs::write(d.join("class/dmi/id/product_name"), "PowerEdge R6415\n").unwrap();
        assert_eq!(system_model(&d).as_deref(), Some("PowerEdge R6415"));
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(system_model(&d), None);
    }
}
