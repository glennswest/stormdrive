//! Hardware assets in app-system-data (#64, stormcos#456): what the node
//! is made of, recorded each boot, with what changed since the boot before.
//!
//! `<dir>/assets/<YYYYMMDDTHHMMSSZ>-<boot_id>.json`, one file per boot. Each
//! holds every asset as an item under a stable key, so two boots compare
//! item by item:
//!
//! | key | from |
//! |---|---|
//! | `system`, `board`, `bios` | `/sys/class/dmi/id` |
//! | `cpu/<socket>` | `/proc/cpuinfo` (model, cores, threads, microcode) |
//! | `dimm/<locator>` | SMBIOS type 17 (`/sys/firmware/dmi/tables/DMI`) |
//! | `bmc` | SMBIOS type 38 (the BMC's interface; its detail is stormipmi's) |
//! | `nic/<mac>` | `/sys/class/net/*` with a device: driver, PCIe address |
//! | `hba/<pcie>` | the HBA scan (`hba.rs`): board, firmware, BIOS, NVDATA; and every other PCIe mass-storage controller (class 0x01, `/sys/bus/pci/devices`): PCI ids, class, kind, driver (null: none bound), an NVMe controller's model, serial, firmware, namespaces (#192) |
//! | `nvme/<serial>` | `/sys/class/nvme/*` (PCIe controllers only): model, firmware, PCIe address, PCI id, driver, controller, capacity, each namespace (#192) |
//! | `shelf/<key>` | the SES scan: vendor, model, serial, each IOM's revision |
//! | `bay/<bay key>`, `drive/<id>` | each present drive: model, serial, firmware, size |
//!
//! `changes` is the difference from the previous boot's file (added,
//! removed, field old → new), with an event. When something changes within
//! the boot (a shelf or drive hot-added), the file is rewritten and
//! `changed_in_boot` says what. Kernel `MemTotal` is kept, not compared (it
//! moves with the kernel's reservations).

use crate::history::{Boot, History};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One boot's hardware record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Assets {
    /// When this boot's record was first taken, and last rewritten.
    pub at: String,
    pub updated_at: String,
    pub boot_id: String,
    pub node: String,
    pub kernel: String,
    pub stormdrive: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_total_bytes: Option<u64>,
    pub items: BTreeMap<String, Value>,
    /// The previous boot's file, compared against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    #[serde(default)]
    pub changes: Vec<Change>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_in_boot: Vec<Change>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub item: String,
    /// `added`, `removed` or `changed`.
    pub change: String,
    /// For `changed`: `field: old → new`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
}

impl Change {
    pub fn describe(&self) -> String {
        match self.change.as_str() {
            "changed" => format!("{} {}", self.item, self.fields.join(", ")),
            c => format!("{} {c}", self.item),
        }
    }
}

fn show(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "—".into(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
    }
}

/// Item-by-item difference from `old` to `new`.
pub fn diff(old: &BTreeMap<String, Value>, new: &BTreeMap<String, Value>) -> Vec<Change> {
    let mut out = vec![];
    for (k, v) in new {
        match old.get(k) {
            None => out.push(Change { item: k.clone(), change: "added".into(), fields: vec![] }),
            Some(o) if o != v => {
                let fields = match (o, v) {
                    (Value::Object(a), Value::Object(b)) => {
                        let mut names: Vec<&String> = a.keys().chain(b.keys()).collect();
                        names.sort();
                        names.dedup();
                        names
                            .into_iter()
                            .filter(|f| a.get(*f) != b.get(*f))
                            .map(|f| format!("{f}: {} → {}", show(a.get(f)), show(b.get(f))))
                            .collect()
                    }
                    _ => vec![format!("{} → {}", show(Some(o)), show(Some(v)))],
                };
                out.push(Change { item: k.clone(), change: "changed".into(), fields });
            }
            _ => {}
        }
    }
    for k in old.keys().filter(|k| !new.contains_key(*k)) {
        out.push(Change { item: k.clone(), change: "removed".into(), fields: vec![] });
    }
    out
}

/// One SMBIOS structure: its type, formatted area and strings.
#[derive(Debug, Clone, PartialEq)]
pub struct Smbios {
    pub kind: u8,
    pub data: Vec<u8>,
    pub strings: Vec<String>,
}

impl Smbios {
    fn byte(&self, off: usize) -> Option<u8> {
        self.data.get(off).copied()
    }
    fn word(&self, off: usize) -> Option<u16> {
        Some(u16::from_le_bytes([self.byte(off)?, self.byte(off + 1)?]))
    }
    fn dword(&self, off: usize) -> Option<u32> {
        Some(u32::from_le_bytes([self.byte(off)?, self.byte(off + 1)?, self.byte(off + 2)?, self.byte(off + 3)?]))
    }
    /// The string a formatted byte points at (1-based; 0 = none).
    fn string(&self, off: usize) -> Option<String> {
        let i = self.byte(off)? as usize;
        let s = self.strings.get(i.checked_sub(1)?)?.trim().to_string();
        (!s.is_empty()).then_some(s)
    }
}

/// The structures of an SMBIOS table (`/sys/firmware/dmi/tables/DMI`).
pub fn parse_smbios(t: &[u8]) -> Vec<Smbios> {
    let mut out = vec![];
    let mut off = 0;
    while off + 4 <= t.len() {
        let (kind, len) = (t[off], t[off + 1] as usize);
        if len < 4 || off + len > t.len() {
            break;
        }
        let data = t[off..off + len].to_vec();
        // The string set ends with two NULs.
        let mut end = off + len;
        while end + 1 < t.len() && !(t[end] == 0 && t[end + 1] == 0) {
            end += 1;
        }
        let strings = t[off + len..end.min(t.len())]
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).to_string())
            .collect();
        out.push(Smbios { kind, data, strings });
        if kind == 127 {
            break;
        }
        off = end + 2;
    }
    out
}

fn memory_type(t: u8) -> String {
    match t {
        0x12 => "DDR".into(),
        0x13 => "DDR2".into(),
        0x18 => "DDR3".into(),
        0x1A => "DDR4".into(),
        0x1B => "LPDDR".into(),
        0x1C => "LPDDR2".into(),
        0x1D => "LPDDR3".into(),
        0x1E => "LPDDR4".into(),
        0x22 => "DDR5".into(),
        0x23 => "LPDDR5".into(),
        t => format!("type {t:#04x}"),
    }
}

/// Type 17 memory devices with a module in them: `dimm/<locator>` items.
/// Type 38: the BMC's interface, as `bmc`.
pub fn smbios_items(structs: &[Smbios], items: &mut BTreeMap<String, Value>) {
    for s in structs {
        match s.kind {
            17 => {
                let Some(size) = s.word(0x0C) else { continue };
                let mb: u64 = match size {
                    0 | 0xFFFF => continue, // empty slot / unknown
                    0x7FFF => u64::from(s.dword(0x1C).unwrap_or(0) & 0x7FFF_FFFF),
                    n if n & 0x8000 != 0 => u64::from(n & 0x7FFF) / 1024,
                    n => u64::from(n),
                };
                let locator = s.string(0x10).unwrap_or_else(|| format!("handle-{:04x}", s.word(0x02).unwrap_or(0)));
                items.insert(
                    format!("dimm/{locator}"),
                    json!({
                        "bank": s.string(0x11),
                        "size_mb": mb,
                        "type": s.byte(0x12).map(memory_type),
                        "speed_mts": s.word(0x15).filter(|v| *v != 0),
                        "configured_mts": s.word(0x20).filter(|v| *v != 0),
                        "manufacturer": s.string(0x17),
                        "serial": s.string(0x18),
                        "part": s.string(0x1A),
                    }),
                );
            }
            38 => {
                let interface = match s.byte(0x04) {
                    Some(1) => "KCS".to_string(),
                    Some(2) => "SMIC".into(),
                    Some(3) => "BT".into(),
                    Some(4) => "SSIF".into(),
                    Some(n) => format!("type {n}"),
                    None => continue,
                };
                let rev = s.byte(0x05).map(|r| format!("{}.{}", r >> 4, r & 0x0F));
                items.insert("bmc".into(), json!({ "interface": interface, "ipmi": rev }));
            }
            _ => {}
        }
    }
}

/// `/proc/cpuinfo`: one item per socket (`physical id`).
pub fn cpu_items(cpuinfo: &str, items: &mut BTreeMap<String, Value>) {
    let mut sockets: BTreeMap<String, (String, String, u64, String)> = BTreeMap::new();
    for block in cpuinfo.split("\n\n") {
        let field = |k: &str| {
            block.lines().find_map(|l| {
                let (a, b) = l.split_once(':')?;
                (a.trim() == k).then(|| b.trim().to_string())
            })
        };
        let Some(model) = field("model name").or_else(|| field("Processor")) else { continue };
        let socket = field("physical id").unwrap_or_else(|| "0".into());
        let e = sockets.entry(socket).or_insert((model, field("cpu cores").unwrap_or_default(), 0, field("microcode").unwrap_or_default()));
        e.2 += 1;
    }
    for (socket, (model, cores, threads, microcode)) in sockets {
        items.insert(
            format!("cpu/{socket}"),
            json!({ "model": model, "cores": cores.parse::<u64>().ok(), "threads": threads, "microcode": (!microcode.is_empty()).then_some(microcode) }),
        );
    }
}

/// `MemTotal` in bytes.
pub fn mem_total(meminfo: &str) -> Option<u64> {
    let l = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = l.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

fn rd(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn link_name(p: &Path) -> Option<String> {
    std::fs::read_link(p).ok()?.file_name().map(|n| n.to_string_lossy().to_string())
}

/// The host's own assets under `root` (`/` on a node; a tree in tests):
/// DMI, CPUs, DIMMs, BMC, NICs, NVMe controllers, PCIe storage controllers
/// (`hba/<pcie>`: merge them under the HBA scan's). Returns the items and
/// the kernel's MemTotal.
pub fn host_items(root: &Path) -> (BTreeMap<String, Value>, Option<u64>) {
    let mut items = BTreeMap::new();
    let dmi = root.join("sys/class/dmi/id");
    let d = |f: &str| rd(&dmi.join(f));
    if dmi.is_dir() {
        items.insert(
            "system".into(),
            json!({ "vendor": d("sys_vendor"), "product": d("product_name"), "version": d("product_version"), "serial": d("product_serial"), "uuid": d("product_uuid"), "chassis_serial": d("chassis_serial") }),
        );
        items.insert("board".into(), json!({ "vendor": d("board_vendor"), "name": d("board_name"), "version": d("board_version"), "serial": d("board_serial") }));
        items.insert("bios".into(), json!({ "vendor": d("bios_vendor"), "version": d("bios_version"), "date": d("bios_date") }));
    }
    if let Some(c) = rd(&root.join("proc/cpuinfo")) {
        cpu_items(&c, &mut items);
    }
    if let Ok(t) = std::fs::read(root.join("sys/firmware/dmi/tables/DMI")) {
        smbios_items(&parse_smbios(&t), &mut items);
    }
    if let Ok(rdir) = std::fs::read_dir(root.join("sys/class/net")) {
        for e in rdir.flatten() {
            let p = e.path();
            let dev = p.join("device");
            // Physical NICs only: a virtual one has no device.
            if !dev.exists() {
                continue;
            }
            let Some(mac) = rd(&p.join("address")) else { continue };
            let pci_id = match (rd(&dev.join("vendor")), rd(&dev.join("device"))) {
                (Some(v), Some(d)) => Some(format!("{}:{}", v.trim_start_matches("0x"), d.trim_start_matches("0x"))),
                _ => None,
            };
            items.insert(
                format!("nic/{mac}"),
                json!({ "name": e.file_name().to_string_lossy(), "driver": link_name(&dev.join("driver")), "pcie": link_name(&dev), "pci_id": pci_id }),
            );
        }
    }
    let nvme = nvme_controllers(root);
    for c in &nvme {
        let Some(serial) = &c.serial else { continue };
        items.insert(
            format!("nvme/{serial}"),
            json!({ "model": c.model, "firmware": c.firmware, "pcie": c.pcie, "pci_id": c.pci_id, "driver": c.driver, "controller": c.name,
                    "capacity_bytes": c.namespaces.iter().filter_map(|n| n["capacity_bytes"].as_u64()).sum::<u64>(), "namespaces": c.namespaces }),
        );
    }
    storage_controllers(root, &nvme, &mut items);
    let total = rd(&root.join("proc/meminfo")).and_then(|m| mem_total(&m));
    (items, total)
}

/// One PCIe NVMe controller from `/sys/class/nvme/<name>`.
#[derive(Debug, Default)]
struct NvmeCtl {
    name: String,
    serial: Option<String>,
    model: Option<String>,
    firmware: Option<String>,
    pcie: Option<String>,
    pci_id: Option<String>,
    driver: Option<String>,
    namespaces: Vec<Value>,
}

fn pci_ids(dev: &Path, a: &str, b: &str) -> Option<String> {
    match (rd(&dev.join(a)), rd(&dev.join(b))) {
        (Some(v), Some(d)) => Some(format!("{}:{}", v.trim_start_matches("0x").to_lowercase(), d.trim_start_matches("0x").to_lowercase())),
        _ => None,
    }
}

/// Is `n` a namespace of controller `ctl`: `nvme0n1`, or under native
/// multipath its path node `nvme0c0n1` (#192)?
fn namespace_of(ctl: &str, n: &str) -> bool {
    let Some(rest) = n.strip_prefix(ctl) else { return false };
    let rest = match rest.strip_prefix('c') {
        Some(r) => r.trim_start_matches(|c: char| c.is_ascii_digit()),
        None => rest,
    };
    rest.strip_prefix('n').is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// Every PCIe NVMe controller the kernel has bound (NVMe-oF controllers are
/// someone's export, #49, not hardware), with its namespaces.
fn nvme_controllers(root: &Path) -> Vec<NvmeCtl> {
    let mut out = vec![];
    let Ok(rdir) = std::fs::read_dir(root.join("sys/class/nvme")) else { return out };
    let mut names: Vec<_> = rdir.flatten().map(|e| e.path()).collect();
    names.sort();
    for p in names {
        if rd(&p.join("transport")).is_some_and(|t| t != "pcie") {
            continue;
        }
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let dev = p.join("device");
        let mut namespaces: Vec<Value> = std::fs::read_dir(&p)
            .map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| namespace_of(&name, n)).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .map(|n| {
                let ns = p.join(&n);
                let block = rd(&ns.join("queue/logical_block_size")).and_then(|v| v.parse::<u64>().ok());
                // `size` is in 512-byte sectors whatever the LBA size.
                let cap = rd(&ns.join("size")).and_then(|v| v.parse::<u64>().ok()).map(|s| s * 512);
                json!({ "name": n, "nsid": rd(&ns.join("nsid")).and_then(|v| v.parse::<u32>().ok()), "capacity_bytes": cap, "block_size": block })
            })
            .collect();
        namespaces.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        out.push(NvmeCtl {
            serial: rd(&p.join("serial")),
            model: rd(&p.join("model")),
            firmware: rd(&p.join("firmware_rev")),
            pcie: rd(&p.join("address")).or_else(|| link_name(&dev)),
            pci_id: pci_ids(&dev, "vendor", "device"),
            driver: link_name(&dev.join("driver")),
            namespaces,
            name,
        });
    }
    out
}

/// A PCI class code's storage kind (class 0x01, by subclass).
fn storage_kind(class: u32) -> &'static str {
    match (class >> 8) & 0xff {
        0x00 => "scsi",
        0x01 => "ide",
        0x04 => "raid",
        0x06 => "sata",
        0x07 => "sas",
        0x08 => "nvme",
        0x80 => "other",
        _ => "storage",
    }
}

/// Every PCIe mass-storage controller (class 0x01) under
/// `/sys/bus/pci/devices` as `hba/<pcie>` (#192): its PCI ids, class and
/// bound driver (null: none bound, so a controller the kernel has no driver
/// for is still on the record), and for an NVMe controller its model,
/// serial, firmware and namespaces. An `hba/` item the HBA scan already
/// wrote keeps its own fields (the scan's are richer): the caller merges
/// with `or_insert`.
fn storage_controllers(root: &Path, nvme: &[NvmeCtl], items: &mut BTreeMap<String, Value>) {
    let Ok(rdir) = std::fs::read_dir(root.join("sys/bus/pci/devices")) else { return };
    for e in rdir.flatten() {
        let bdf = e.file_name().to_string_lossy().to_string();
        let dev = e.path();
        let Some(class) = rd(&dev.join("class")).and_then(|c| u32::from_str_radix(c.trim_start_matches("0x"), 16).ok()) else { continue };
        if class >> 16 != 0x01 {
            continue;
        }
        let mut v = json!({
            "driver": link_name(&dev.join("driver")),
            "pci_id": pci_ids(&dev, "vendor", "device"),
            "pci_subsystem": pci_ids(&dev, "subsystem_vendor", "subsystem_device"),
            "class": format!("0x{class:06x}"),
            "kind": storage_kind(class),
        });
        if let Some(c) = nvme.iter().find(|c| c.pcie.as_deref() == Some(bdf.as_str())) {
            v["board"] = json!(c.model);
            v["model"] = json!(c.model);
            v["serial"] = json!(c.serial);
            v["firmware"] = json!(c.firmware);
            v["controller"] = json!(c.name);
            v["namespaces"] = json!(c.namespaces.len());
        }
        items.insert(format!("hba/{bdf}"), v);
    }
}

/// What stormdrive's own scans know: HBAs, shelves, drives by bay.
pub fn scanned_items(drives: &[crate::drive::Drive], hbas: &crate::hba::Hbas, shelves: &crate::topology::Shelves, items: &mut BTreeMap<String, Value>) {
    for (addr, h) in hbas {
        items.insert(
            format!("hba/{addr}"),
            json!({ "driver": h.driver, "pci_id": h.pci_id, "board": h.board_name, "assembly": h.board_assembly, "tracer": h.board_tracer, "firmware": h.firmware, "bios": h.bios, "nvdata": h.nvdata, "sas_address": h.sas_address }),
        );
    }
    for (key, s) in shelves {
        let ioms: Vec<Value> = s.esps.iter().map(|e| json!({ "serial": e.serial, "sas_address": e.sas_address, "revision": e.revision })).collect();
        items.insert(
            format!("shelf/{key}"),
            json!({ "vendor": s.shelf.vendor, "model": s.shelf.model, "serial": s.shelf.serial, "logical_id": s.shelf.logical_id, "position": s.shelf.position, "ioms": ioms, "bays": s.slots.len() }),
        );
    }
    // A shelf only its drives name (no SES device answers for it): its
    // position's history is kept all the same (#65).
    for d in drives.iter().filter(|d| d.activity != crate::drive::Activity::Missing) {
        let Some(sh) = &d.location.shelf else { continue };
        let Some(key) = sh.key() else { continue };
        if shelves.contains_key(&key) {
            continue;
        }
        items.entry(format!("shelf/{key}")).or_insert_with(|| {
            json!({ "vendor": sh.vendor, "model": sh.model, "serial": sh.serial, "logical_id": sh.logical_id, "position": sh.position })
        });
    }
    for d in drives.iter().filter(|d| d.activity != crate::drive::Activity::Missing) {
        let item = match d.location.bay_key() {
            Some(b) => format!("bay/{b}"),
            None => format!("drive/{}", d.wwid.clone().filter(|w| !w.trim().is_empty()).unwrap_or_else(|| format!("{}_{}", d.model, d.serial))),
        };
        items.insert(
            item,
            json!({ "wwn": d.wwid, "serial": d.serial, "model": d.model, "firmware": d.firmware, "capacity_bytes": d.capacity_bytes, "block_size": d.block_size }),
        );
    }
}

/// Boot files kept: the newest this many.
const KEEP_BOOTS: usize = 1000;

fn compact(rfc: &str) -> String {
    rfc.chars().filter(|c| *c != '-' && *c != ':').collect()
}

fn boot_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|r| r.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect())
        .unwrap_or_default();
    v.sort();
    v
}

fn load(p: &Path) -> Option<Assets> {
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

/// What recording did: the boot's record, and the events to raise
/// (severity warning when something was removed).
pub struct Recorded {
    pub assets: Assets,
    pub file: String,
    pub events: Vec<(bool, String)>,
}

/// Write this boot's record under `<root>/assets/` when it is new or its
/// items changed; an unchanged one is returned as it is, with no events.
/// None when the system-data directory is absent.
pub fn record(h: &History, boot: &Boot, items: BTreeMap<String, Value>, mem: Option<u64>, unix: u64) -> anyhow::Result<Option<Recorded>> {
    if !h.available() {
        return Ok(None);
    }
    let dir = h.root.join("assets");
    std::fs::create_dir_all(&dir)?;
    let files = boot_files(&dir);
    let suffix = format!("-{}.json", if boot.boot_id.is_empty() { "unknown" } else { &boot.boot_id });
    let this = files.iter().find(|p| p.to_string_lossy().ends_with(&suffix)).cloned();
    let prev_file = files.iter().rev().find(|p| Some(*p) != this.as_ref()).cloned();
    let prev = prev_file.as_deref().and_then(load);
    let now = crate::controller::rfc3339(unix);
    let existing = this.as_deref().and_then(load);
    if let Some(e) = existing.as_ref().filter(|e| e.items == items) {
        let file = this.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        h.status.lock().unwrap_or_else(|e| e.into_inner()).assets_file = Some(file.clone());
        return Ok(Some(Recorded { assets: e.clone(), file, events: vec![] }));
    }
    let changes = prev.as_ref().map(|p| diff(&p.items, &items)).unwrap_or_default();
    let mut events = vec![];
    let in_boot = match &existing {
        Some(e) => {
            let c = diff(&e.items, &items);
            if !c.is_empty() {
                events.push((
                    c.iter().any(|c| c.change == "removed"),
                    format!("hardware changed during this boot: {}", c.iter().map(Change::describe).collect::<Vec<_>>().join("; ")),
                ));
            }
            let mut all = e.changed_in_boot.clone();
            all.extend(c);
            all
        }
        None => {
            match (&prev, changes.is_empty()) {
                (None, _) => events.push((false, format!("hardware assets recorded: {} items (the first record in system-data)", items.len()))),
                (Some(p), true) => events.push((false, format!("hardware unchanged since the previous boot ({}): {} items", p.at, items.len()))),
                (Some(p), false) => events.push((
                    changes.iter().any(|c| c.change == "removed"),
                    format!(
                        "hardware since the previous boot ({}): {} change(s): {}",
                        p.at,
                        changes.len(),
                        changes.iter().map(Change::describe).collect::<Vec<_>>().join("; ")
                    ),
                )),
            }
            vec![]
        }
    };
    let assets = Assets {
        at: existing.as_ref().map_or_else(|| now.clone(), |e| e.at.clone()),
        updated_at: now.clone(),
        boot_id: boot.boot_id.clone(),
        node: boot.node.clone(),
        kernel: boot.kernel.clone(),
        stormdrive: boot.stormdrive.clone(),
        memory_total_bytes: mem,
        items,
        previous: prev_file.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()),
        changes,
        changed_in_boot: in_boot,
    };
    let path = this.unwrap_or_else(|| dir.join(format!("{}{suffix}", compact(&now))));
    crate::inventory::write_atomic(&path, &serde_json::to_vec_pretty(&assets)?)?;
    let files = boot_files(&dir);
    if files.len() > KEEP_BOOTS {
        for p in &files[..files.len() - KEEP_BOOTS] {
            let _ = std::fs::remove_file(p);
        }
    }
    let file = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    h.status.lock().unwrap_or_else(|e| e.into_inner()).assets_file = Some(file.clone());
    Ok(Some(Recorded { assets, file, events }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Dell's sda: a shelf only the drive names, no SES report. Its
    /// position is on the asset record all the same, so its history is kept.
    #[test]
    fn a_shelf_only_a_drive_names_is_an_asset_with_its_position() {
        use crate::drive::{Drive, DriveId, Location, Position, Shelf};
        let mut d: Drive = serde_json::from_value(json!({
            "id": DriveId::derive(Some("w"), "M", "S"),
            "path": "/dev/sda", "name": "sda", "kind": "sata_hdd", "model": "M",
            "serial": "S", "firmware": "F", "wwid": "w", "capacity_bytes": 1u64 << 40,
            "block_size": 512,
            "first_seen": std::time::SystemTime::UNIX_EPOCH, "last_seen": std::time::SystemTime::UNIX_EPOCH,
        }))
        .unwrap();
        d.location = Location {
            shelf: Some(Shelf { logical_id: Some("5a".into()), position: Some(Position::Internal), ..Default::default() }),
            bay: Some(4),
            position: Some(Position::Internal),
            ..Default::default()
        };
        let mut items = BTreeMap::new();
        scanned_items(&[d], &Default::default(), &Default::default(), &mut items);
        assert_eq!(items["shelf/5a"]["position"], "internal");
    }

    /// An SMBIOS structure: formatted bytes after the 4-byte header, then
    /// strings.
    fn st(kind: u8, body: &[u8], strings: &[&str]) -> Vec<u8> {
        let mut v = vec![kind, (4 + body.len()) as u8, 0x10, 0x00];
        v.extend_from_slice(body);
        for s in strings {
            v.extend_from_slice(s.as_bytes());
            v.push(0);
        }
        if strings.is_empty() {
            v.push(0);
        }
        v.push(0);
        v
    }

    /// A type 17 body (offsets from 0x04): size, locator 1, bank 2, type,
    /// speed, manufacturer 3, serial 4, part 5, extended size, configured.
    fn dimm(size: u16, ext: u32, mtype: u8) -> Vec<u8> {
        let mut b = vec![0u8; 0x22 - 4];
        let at = |o: usize| o - 4;
        b[at(0x0C)..at(0x0C) + 2].copy_from_slice(&size.to_le_bytes());
        b[at(0x10)] = 1;
        b[at(0x11)] = 2;
        b[at(0x12)] = mtype;
        b[at(0x15)..at(0x15) + 2].copy_from_slice(&2666u16.to_le_bytes());
        b[at(0x17)] = 3;
        b[at(0x18)] = 4;
        b[at(0x1A)] = 5;
        b[at(0x1C)..at(0x1C) + 4].copy_from_slice(&ext.to_le_bytes());
        b[at(0x20)..at(0x20) + 2].copy_from_slice(&2400u16.to_le_bytes());
        b
    }

    fn table() -> Vec<u8> {
        let mut t = vec![];
        t.extend(st(0, &[1, 2, 0, 0], &["Dell Inc.", "2.17.0"]));
        t.extend(st(17, &dimm(16384, 0, 0x1A), &["DIMM A1", "BANK 0", "Hynix", "1234ABCD", "HMA82GR7AFR8N-VK"]));
        t.extend(st(17, &dimm(0, 0, 0x02), &["DIMM A2", "BANK 1"])); // empty slot
        t.extend(st(17, &dimm(0x7FFF, 65536, 0x22), &["DIMM B1", "BANK 2", "Samsung", "S1", "M321R8GA0BB0"]));
        t.extend(st(38, &[1, 0x20, 0x20, 0xFF, 0xA2, 0x0C, 0, 0, 0, 0, 0, 0], &[]));
        t.extend(st(127, &[], &[]));
        t
    }

    #[test]
    fn smbios_dimms_and_bmc() {
        let s = parse_smbios(&table());
        assert_eq!(s.iter().map(|s| s.kind).collect::<Vec<_>>(), vec![0, 17, 17, 17, 38, 127]);
        assert_eq!(s[1].strings, vec!["DIMM A1", "BANK 0", "Hynix", "1234ABCD", "HMA82GR7AFR8N-VK"]);
        let mut items = BTreeMap::new();
        smbios_items(&s, &mut items);
        let a1 = &items["dimm/DIMM A1"];
        assert_eq!((a1["size_mb"].as_u64(), a1["type"].as_str(), a1["speed_mts"].as_u64(), a1["configured_mts"].as_u64()), (Some(16384), Some("DDR4"), Some(2666), Some(2400)));
        assert_eq!((a1["serial"].as_str(), a1["part"].as_str(), a1["bank"].as_str()), (Some("1234ABCD"), Some("HMA82GR7AFR8N-VK"), Some("BANK 0")));
        assert!(!items.contains_key("dimm/DIMM A2"), "an empty slot is not an asset");
        assert_eq!((items["dimm/DIMM B1"]["size_mb"].as_u64(), items["dimm/DIMM B1"]["type"].as_str()), (Some(65536), Some("DDR5")));
        assert_eq!(items["bmc"], json!({ "interface": "KCS", "ipmi": "2.0" }));
        // A truncated table stops; it does not panic.
        let t = table();
        assert_eq!(parse_smbios(&t[..30]).len(), 1);
    }

    #[test]
    fn cpus_by_socket() {
        let info = "processor\t: 0\nmodel name\t: Intel(R) Xeon(R) E-2124\nphysical id\t: 0\ncpu cores\t: 4\nmicrocode\t: 0xf6\n\n\
                    processor\t: 1\nmodel name\t: Intel(R) Xeon(R) E-2124\nphysical id\t: 0\ncpu cores\t: 4\nmicrocode\t: 0xf6\n\n";
        let mut items = BTreeMap::new();
        cpu_items(info, &mut items);
        assert_eq!(items["cpu/0"], json!({ "model": "Intel(R) Xeon(R) E-2124", "cores": 4, "threads": 2, "microcode": "0xf6" }));
        assert_eq!(mem_total("MemTotal:       16303428 kB\nMemFree: 1 kB\n"), Some(16_303_428 * 1024));
    }

    #[test]
    fn diff_items() {
        let old: BTreeMap<String, Value> = [
            ("dimm/A1".to_string(), json!({ "size_mb": 16384 })),
            ("dimm/A2".to_string(), json!({ "size_mb": 16384 })),
            ("bay/x/bay/4".to_string(), json!({ "serial": "S1", "firmware": "N003" })),
        ]
        .into();
        let new: BTreeMap<String, Value> = [
            ("dimm/A1".to_string(), json!({ "size_mb": 16384 })),
            ("bay/x/bay/4".to_string(), json!({ "serial": "S1", "firmware": "N004" })),
            ("nic/aa".to_string(), json!({ "driver": "ixgbe" })),
        ]
        .into();
        let c = diff(&old, &new);
        assert_eq!(c.len(), 3);
        assert_eq!(c[0], Change { item: "bay/x/bay/4".into(), change: "changed".into(), fields: vec!["firmware: N003 → N004".into()] });
        assert_eq!((c[1].item.as_str(), c[1].change.as_str()), ("nic/aa", "added"));
        assert_eq!((c[2].item.as_str(), c[2].change.as_str()), ("dimm/A2", "removed"));
        assert!(diff(&new, &new).is_empty());
    }

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    #[test]
    fn host_tree() {
        let root = std::env::temp_dir().join(format!("sd-assets-host-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write(&root.join("sys/class/dmi/id/sys_vendor"), "Dell Inc.\n");
        write(&root.join("sys/class/dmi/id/product_name"), "PowerEdge R230\n");
        write(&root.join("sys/class/dmi/id/bios_version"), "2.17.0\n");
        write(&root.join("proc/meminfo"), "MemTotal: 1024 kB\n");
        std::fs::create_dir_all(root.join("sys/firmware/dmi/tables")).unwrap();
        std::fs::write(root.join("sys/firmware/dmi/tables/DMI"), table()).unwrap();
        // eno1 is physical (a device, a driver); lo is not.
        write(&root.join("sys/class/net/eno1/address"), "00:1a:2b:3c:4d:5e\n");
        write(&root.join("devices/pci0000:00/0000:02:00.0/vendor"), "0x8086\n");
        write(&root.join("devices/pci0000:00/0000:02:00.0/device"), "0x1533\n");
        write(&root.join("drivers/igb/x"), "");
        std::os::unix::fs::symlink(root.join("devices/pci0000:00/0000:02:00.0"), root.join("sys/class/net/eno1/device")).unwrap();
        std::os::unix::fs::symlink(root.join("drivers/igb"), root.join("devices/pci0000:00/0000:02:00.0/driver")).unwrap();
        write(&root.join("sys/class/net/lo/address"), "00:00:00:00:00:00\n");
        // A PCIe NVMe controller, and an NVMe-oF one (#49).
        write(&root.join("sys/class/nvme/nvme0/serial"), "PHLJ123\n");
        write(&root.join("sys/class/nvme/nvme0/model"), "INTEL SSDPE2KX020T8\n");
        write(&root.join("sys/class/nvme/nvme0/firmware_rev"), "VDV10131\n");
        write(&root.join("sys/class/nvme/nvme0/transport"), "pcie\n");
        write(&root.join("sys/class/nvme/nvme1/serial"), "fabric\n");
        write(&root.join("sys/class/nvme/nvme1/transport"), "tcp\n");
        // #192: its PCI function (bound to nvme), a namespace, an M.2 NVMe
        // controller no driver took, the NIC (class 02, not storage).
        write(&root.join("sys/class/nvme/nvme0/address"), "0000:03:00.0\n");
        let f = root.join("sys/bus/pci/devices/0000:03:00.0");
        write(&f.join("class"), "0x010802\n");
        write(&f.join("vendor"), "0x8086\n");
        write(&f.join("device"), "0x0a54\n");
        write(&f.join("subsystem_vendor"), "0x8086\n");
        write(&f.join("subsystem_device"), "0x4802\n");
        write(&root.join("drivers/nvme/x"), "");
        std::os::unix::fs::symlink(root.join("drivers/nvme"), f.join("driver")).unwrap();
        std::os::unix::fs::symlink(&f, root.join("sys/class/nvme/nvme0/device")).unwrap();
        write(&root.join("sys/class/nvme/nvme0/nvme0n1/size"), "3907029168\n");
        write(&root.join("sys/class/nvme/nvme0/nvme0n1/nsid"), "1\n");
        write(&root.join("sys/class/nvme/nvme0/nvme0n1/queue/logical_block_size"), "512\n");
        write(&root.join("sys/class/nvme/nvme0/power/control"), "auto\n");
        let u = root.join("sys/bus/pci/devices/0000:04:00.0");
        write(&u.join("class"), "0x010802\n");
        write(&u.join("vendor"), "0x144d\n");
        write(&u.join("device"), "0xa808\n");
        write(&root.join("sys/bus/pci/devices/0000:02:00.0/class"), "0x020000\n");
        let (items, total) = host_items(&root);
        assert_eq!(total, Some(1024 * 1024));
        assert_eq!(items["system"]["product"], "PowerEdge R230");
        assert_eq!(items["bios"]["version"], "2.17.0");
        assert!(items.contains_key("dimm/DIMM A1") && items.contains_key("bmc"));
        assert_eq!(items["nic/00:1a:2b:3c:4d:5e"], json!({ "name": "eno1", "driver": "igb", "pcie": "0000:02:00.0", "pci_id": "8086:1533" }));
        assert!(!items.keys().any(|k| k == "nic/00:00:00:00:00:00"));
        assert_eq!(items["nvme/PHLJ123"]["firmware"], "VDV10131");
        assert!(!items.contains_key("nvme/fabric"));
        let n = &items["nvme/PHLJ123"];
        assert_eq!((n["pci_id"].as_str(), n["driver"].as_str(), n["controller"].as_str(), n["pcie"].as_str()), (Some("8086:0a54"), Some("nvme"), Some("nvme0"), Some("0000:03:00.0")));
        assert_eq!(n["capacity_bytes"], 3907029168u64 * 512);
        assert_eq!(n["namespaces"], json!([{ "name": "nvme0n1", "nsid": 1, "capacity_bytes": 3907029168u64 * 512, "block_size": 512 }]));
        let c = &items["hba/0000:03:00.0"];
        assert_eq!((c["kind"].as_str(), c["class"].as_str(), c["driver"].as_str(), c["pci_id"].as_str()), (Some("nvme"), Some("0x010802"), Some("nvme"), Some("8086:0a54")));
        assert_eq!((c["model"].as_str(), c["serial"].as_str(), c["firmware"].as_str(), c["namespaces"].as_u64()), (Some("INTEL SSDPE2KX020T8"), Some("PHLJ123"), Some("VDV10131"), Some(1)));
        // No driver bound: on the record all the same, driver null.
        let c = &items["hba/0000:04:00.0"];
        assert_eq!((c["kind"].as_str(), c["pci_id"].as_str()), (Some("nvme"), Some("144d:a808")));
        assert!(c["driver"].is_null() && c["serial"].is_null());
        assert!(!items.contains_key("hba/0000:02:00.0"), "a NIC is not a storage controller");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn namespaces_of_a_controller() {
        for n in ["nvme0n1", "nvme0n12", "nvme0c0n1", "nvme0c3n2"] {
            assert!(namespace_of("nvme0", n), "{n}");
        }
        for n in ["nvme1n1", "nvme10n1", "nvme0", "nvme0n", "nvme0c0n", "nvme0n1p1", "power", "device"] {
            assert!(!namespace_of("nvme0", n), "{n}");
        }
        assert_eq!((storage_kind(0x010802), storage_kind(0x010601), storage_kind(0x010700), storage_kind(0x010400)), ("nvme", "sata", "sas", "raid"));
    }

    #[test]
    fn a_record_per_boot_with_changes() {
        let root = std::env::temp_dir().join(format!("sd-assets-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let cfg = crate::config::HistoryConfig { dir: root.display().to_string(), ..Default::default() };
        let boot = |id: &str| Boot { boot_id: id.into(), node: "n1".into(), kernel: "6.x".into(), stormdrive: "0".into() };
        let h = History::new(&cfg, boot("a"));
        let items = |fw: &str, dimms: usize| -> BTreeMap<String, Value> {
            let mut m: BTreeMap<String, Value> = (0..dimms).map(|i| (format!("dimm/A{i}"), json!({ "size_mb": 16384 }))).collect();
            m.insert("hba/0000:01:00.0".into(), json!({ "firmware": fw }));
            m
        };
        let oct = 1_791_417_600;

        let r = record(&h, &boot("a"), items("16.00.01.00", 2), Some(1), oct).unwrap().unwrap();
        assert!(r.events[0].1.contains("first record"));
        assert_eq!(r.file, "20261008T000000Z-a.json");
        let same = record(&h, &boot("a"), items("16.00.01.00", 2), Some(1), oct + 30).unwrap().unwrap();
        assert!(same.events.is_empty() && same.assets.updated_at == "2026-10-08T00:00:00Z", "unchanged: not rewritten");

        // The same boot, a hot-added DIMM is impossible, but an HBA flash
        // is not: rewritten in place, changed_in_boot.
        let r = record(&h, &boot("a"), items("16.00.12.00", 2), Some(1), oct + 60).unwrap().unwrap();
        assert_eq!(r.file, "20261008T000000Z-a.json");
        assert_eq!(r.assets.changed_in_boot[0].fields, vec!["firmware: 16.00.01.00 → 16.00.12.00"]);
        assert_eq!(r.assets.at, "2026-10-08T00:00:00Z");

        // The next boot (an install in between): a DIMM gone → warning.
        let r = record(&h, &boot("b"), items("16.00.12.00", 1), Some(1), oct + 3600).unwrap().unwrap();
        assert_eq!(r.assets.previous.as_deref(), Some("20261008T000000Z-a.json"));
        assert_eq!(r.assets.changes, vec![Change { item: "dimm/A1".into(), change: "removed".into(), fields: vec![] }]);
        assert!(r.events[0].0 && r.events[0].1.contains("1 change(s): dimm/A1 removed"));
        assert_eq!(boot_files(&root.join("assets")).len(), 2);
        assert_eq!(h.status.lock().unwrap().assets_file.as_deref(), Some("20261008T010000Z-b.json"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
