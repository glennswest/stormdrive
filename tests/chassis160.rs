//! #31: a 160-bay NVMe chassis (ASG-4116S class), simulated. The owner
//! (2026-10-07): "160 bay, we need to simulate" — no real chassis is coming.
//! stormcos#328 emulates drives inside the engine, which stormdrive never
//! sees as NVMe bays; so the chassis here is a sysfs tree in a directory,
//! laid out the way the kernel lays out a real one, and stormdrive's own
//! discovery, topology and locate code run over it unchanged:
//!
//! - bays 1..=120: NVMe drives behind a PCIe switch (domain 0000);
//! - bays 121..=160: behind Intel VMD (domain 10000);
//! - bays 1..=8: native NVMe multipath — the gendisk is a subsystem head
//!   with no PCIe in its path, the per-controller path node is hidden;
//! - every bay a pciehp slot named by its number (`_SUN`, the chassis
//!   label); bays 1..=150 have an `attention` indicator, 151..=160 an NPEM
//!   `…:enclosure:locate` LED on their port instead.
//!
//! Checked: discovery sees exactly 160 drives; every one has its PCIe slot
//! and bay = the slot's number; locate lights that bay's LED and no other;
//! a pull is gone on the next pass, and a new drive pushed into the same
//! slot is found to replace the missing one.
//!
//! What a simulation cannot show is left for the NetApp shelf on C2NR0Q2
//! (#30): a real LED, a real hotplug interrupt, real NVMe admin commands.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use stormdrive::config::DiscoveryConfig;
use stormdrive::drive::{Drive, DriveId};

const BAYS: u32 = 160;
const SECTORS: u64 = 7_501_476_528; // 3.84 TB in 512-byte sectors

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

/// The endpoint (the drive) and the switch/VMD port above it, for a bay.
fn bdfs(bay: u32) -> (String, String, PathBuf) {
    if bay <= 120 {
        let i = bay - 1;
        let port = format!("0000:02:{:02x}.0", i % 32);
        let ep = format!("0000:{:02x}:00.0", 0x10 + i);
        let parent = PathBuf::from("devices/pci0000:00/0000:00:01.0/0000:01:00.0").join(&port);
        (ep, port, parent)
    } else {
        let k = bay - 121;
        let port = format!("10000:{:02x}:{:02x}.0", 1 + k / 32, k % 32);
        let ep = format!("10000:{:02x}:00.0", 0x10 + k);
        let parent = PathBuf::from("devices/pci0000:00/0000:00:0e.0/pci10000:00/10000:00:01.0").join(&port);
        (ep, port, parent)
    }
}

fn led_of(sys: &Path, bay: u32) -> PathBuf {
    if bay <= 150 {
        sys.join(format!("bus/pci/slots/{bay}/attention"))
    } else {
        sys.join(format!("class/leds/{}:enclosure:locate/brightness", bdfs(bay).1))
    }
}

/// Put a drive (controller instance `n`, serial `serial`) into `bay`.
fn insert(sys: &Path, bay: u32, n: u32, serial: &str) {
    let (ep, _port, parent) = bdfs(bay);
    let ctrl = sys.join(&parent).join(&ep).join("nvme").join(format!("nvme{n}"));
    write(&ctrl.join("model"), "Micron_7450_MTFDKCC3T8TFR\n");
    write(&ctrl.join("serial"), &format!("{serial}\n"));
    write(&ctrl.join("firmware_rev"), "E2MU200\n");
    let ns = |dir: &Path| {
        write(&dir.join("size"), &format!("{SECTORS}\n"));
        write(&dir.join("dev"), &format!("259:{n}\n"));
        write(&dir.join("queue/logical_block_size"), "4096\n");
        write(&dir.join("queue/physical_block_size"), "4096\n");
        write(&dir.join("wwid"), &format!("eui.00{n:014x}\n"));
    };
    let name = format!("nvme{n}n1");
    if bay <= 8 {
        // Native multipath: the gendisk is the subsystem's head.
        let path_node = ctrl.join(format!("nvme{n}c{n}n1"));
        ns(&path_node);
        write(&path_node.join("hidden"), "1\n");
        let subsys = sys.join(format!("devices/virtual/nvme-subsystem/nvme-subsys{n}"));
        write(&subsys.join("model"), "Micron_7450_MTFDKCC3T8TFR\n");
        write(&subsys.join("serial"), &format!("{serial}\n"));
        write(&subsys.join("firmware_rev"), "E2MU200\n");
        let head = subsys.join(&name);
        ns(&head);
        symlink(&subsys, head.join("device")).unwrap();
        std::fs::create_dir_all(head.join("multipath")).unwrap();
        symlink(&path_node, head.join("multipath").join(format!("nvme{n}c{n}n1"))).unwrap();
        symlink(&head, sys.join("block").join(&name)).unwrap();
        symlink(&path_node, sys.join("block").join(format!("nvme{n}c{n}n1"))).unwrap();
    } else {
        let dir = ctrl.join(&name);
        ns(&dir);
        symlink(&ctrl, dir.join("device")).unwrap();
        symlink(&dir, sys.join("block").join(&name)).unwrap();
    }
}

fn pull(sys: &Path, n: u32) {
    std::fs::remove_file(sys.join("block").join(format!("nvme{n}n1"))).unwrap();
}

/// The chassis, every bay filled; drive `nvme<bay>n1`, serial `SIM<bay>`.
fn chassis(root: &Path) -> PathBuf {
    let sys = root.join("sys");
    std::fs::create_dir_all(sys.join("block")).unwrap();
    std::fs::create_dir_all(root.join("dev")).unwrap();
    for bay in 1..=BAYS {
        let (ep, _, _) = bdfs(bay);
        let addr = ep.rsplit_once('.').unwrap().0;
        write(&sys.join(format!("bus/pci/slots/{bay}/address")), &format!("{addr}\n"));
        write(&led_of(&sys, bay), "0\n");
        insert(&sys, bay, bay, &format!("SIM{bay:05}"));
    }
    sys
}

fn bay_of_serial(s: &str) -> u32 {
    s.trim_start_matches("SIM").parse().unwrap()
}

fn lit(sys: &Path) -> Vec<u32> {
    (1..=BAYS).filter(|b| std::fs::read_to_string(led_of(sys, *b)).unwrap().trim() == "1").collect()
}

#[test]
fn a_simulated_160_bay_nvme_chassis() {
    let root = std::env::temp_dir().join(format!("stormdrive-chassis160-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let sys = chassis(&root);
    let dev = root.join("dev");
    let cfg = DiscoveryConfig::default();
    let shelves = Default::default();

    // Discovery: exactly 160, the hidden multipath path nodes not counted.
    let t0 = std::time::Instant::now();
    let seen = stormdrive::discovery::scan_in(&sys, &dev, "", &cfg);
    let scan_ms = t0.elapsed().as_millis();
    assert_eq!(seen.len(), BAYS as usize, "{:?}", seen.iter().map(|o| &o.name).collect::<Vec<_>>());
    assert!(seen.iter().all(|o| o.kind == stormdrive::drive::DriveKind::NvmeSsd));
    assert!(seen.iter().all(|o| o.capacity_bytes == SECTORS * 512 && o.block_size == 4096 && o.usable));
    let serials: std::collections::HashSet<&str> = seen.iter().map(|o| o.serial.as_str()).collect();
    assert_eq!(serials.len(), BAYS as usize, "every drive its own identity");
    println!("# discovery of 160 simulated NVMe bays: {scan_ms} ms");

    // Topology: every drive has its slot, and the bay is the slot's number.
    let mut by_bay = HashMap::new();
    for o in &seen {
        let bay = bay_of_serial(&o.serial);
        let loc = stormdrive::topology::locate_in(&sys, &o.name, &shelves);
        assert_eq!(loc.pcie_slot.as_deref(), Some(bay.to_string().as_str()), "{}: {loc:?}", o.name);
        assert_eq!(loc.bay, Some(bay), "{}", o.name);
        assert_eq!(loc.pcie_addr, Some(bdfs(bay).0), "{}: the endpoint, also under multipath and VMD", o.name);
        by_bay.insert(bay, (o.name.clone(), loc));
    }
    assert_eq!(by_bay.len(), BAYS as usize);

    // Locate: that bay's LED and no other — pciehp attention and NPEM both.
    for bay in [1, 8, 77, 120, 121, 150, 151, 160] {
        let (name, loc) = &by_bay[&bay];
        stormdrive::topology::set_locate_in(&sys, name, loc, &shelves, true).unwrap();
        assert_eq!(lit(&sys), vec![bay], "locate {name}");
        stormdrive::topology::set_locate_in(&sys, name, loc, &shelves, false).unwrap();
        assert!(lit(&sys).is_empty());
    }

    // Pull bay 17: gone on the next pass.
    pull(&sys, 17);
    let after_pull = stormdrive::discovery::scan_in(&sys, &dev, "", &cfg);
    assert_eq!(after_pull.len(), BAYS as usize - 1);
    assert!(!after_pull.iter().any(|o| o.serial == "SIM00017"));

    // Push a new drive into bay 17 (the kernel gives it a new instance): it
    // replaces the missing drive whose bay it took.
    insert(&sys, 17, 161, "NEW00017");
    let after_push = stormdrive::discovery::scan_in(&sys, &dev, "", &cfg);
    assert_eq!(after_push.len(), BAYS as usize);
    let new = after_push.iter().find(|o| o.serial == "NEW00017").expect("the new drive is seen");
    let new_loc = stormdrive::topology::locate_in(&sys, &new.name, &shelves);
    assert_eq!(new_loc.bay, Some(17));
    let old = &seen.iter().find(|o| o.serial == "SIM00017").unwrap();
    let old_id = DriveId::derive(old.wwid.as_deref(), &old.model, &old.serial);
    let old_drive: Drive = serde_json::from_value(serde_json::json!({
        "id": old_id, "path": old.path, "name": old.name, "paths": [old.path],
        "kind": "nvme_ssd", "model": old.model, "serial": old.serial, "firmware": old.firmware, "wwid": old.wwid,
        "capacity_bytes": old.capacity_bytes, "block_size": 4096, "activity": "missing",
        "location": by_bay[&17].1,
        "first_seen": {"secs_since_epoch": 0, "nanos_since_epoch": 0}, "last_seen": {"secs_since_epoch": 0, "nanos_since_epoch": 0},
    }))
    .unwrap();
    let drives: HashMap<DriveId, Drive> = [(old_id, old_drive)].into();
    assert_eq!(stormdrive::monitor::replaced_in_bay(&drives, &new_loc), Some(old_id), "the new drive replaces the pulled one");

    let _ = std::fs::remove_dir_all(&root);
}
