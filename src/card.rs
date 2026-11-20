//! A SAS card as storcli and sas3flash see it, from its own interface
//! (#166): the [`CardReport`] each discovery pass reads through [`crate::mpt`],
//! the `HostBusAdapter` object made from it, and the flash a `CardOperation`
//! runs.
//!
//! **The report.** `MPT3IOCINFO`, IOC Facts, Manufacturing Page 0 (board),
//! IO Unit Page 0 (NVDATA), BIOS Page 3, SAS IO Unit Pages 0 and 1 (each
//! phy: negotiated rate, attached device, the max/min the card is set to,
//! disabled), every SAS Device Page 0 and Enclosure Page 0. The firmware
//! regions (firmware, BIOS/UEFI option ROM, backup, manufacturing) are read
//! back with FW Upload once per daemon run and after every flash: each
//! with its size, sha256 and version.
//!
//! **The flash** ([`flash`]): an image from the node's firmware store whose
//! sha256 is the one the operation names, checked against the card (an
//! MPI firmware header for this vendor and product; an option ROM for this
//! vendor), written with FW Download, read back with FW Upload and compared
//! byte for byte, then — only when every byte matches and the card's drives
//! have no I/O in flight — the driver's host reset, after which the card is
//! asked its versions again. A read-back that differs is never reset: the
//! card keeps running what it ran.

use crate::mpt::{self, Device, Enclosure, Facts, IocInfo, Man0, Phy, Port, Region};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// One region read back from the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub region: Region,
    /// Within the BIOS region: `legacy_bios` / `uefi` (an option ROM's
    /// images, each on its own line); None for the region as a whole.
    pub part: Option<String>,
    pub size: u64,
    pub sha256: String,
    pub version: Option<String>,
    pub product_id: Option<u16>,
    pub name: Option<String>,
}

/// What a card's firmware regions hold, from one read.
pub fn images_of(region: Region, b: &[u8]) -> Vec<Image> {
    let whole = |version: Option<String>, product_id: Option<u16>, name: Option<String>| Image {
        region,
        part: None,
        size: b.len() as u64,
        sha256: crate::firmware::sha256_hex(b),
        version,
        product_id,
        name,
    };
    match region {
        Region::Firmware | Region::FirmwareBackup => {
            let h = mpt::parse_fw_header(b);
            vec![whole(h.as_ref().map(|h| mpt::version(h.fw_version)), h.as_ref().map(|h| h.product_id), h.and_then(|h| h.name))]
        }
        Region::Bios => {
            let mut v = vec![whole(None, None, None)];
            for r in mpt::parse_rom(b) {
                let part = &b[r.offset..r.offset + r.len];
                v.push(Image { region, part: Some(r.kind().to_string()), size: r.len as u64, sha256: crate::firmware::sha256_hex(part), version: None, product_id: None, name: None });
            }
            v
        }
        Region::Manufacturing => vec![whole(None, None, None)],
    }
}

/// A flash that ran on this card (kept on the report, so the object shows
/// the last one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashRecord {
    pub operation: String,
    pub region: Region,
    pub image: String,
    pub sha256: String,
    pub before: Option<String>,
    pub after: Option<String>,
    pub started: u64,
    pub finished: u64,
    pub result: String,
}

/// One card's view.
#[derive(Debug, Clone, Default)]
pub struct CardReport {
    pub pci: String,
    pub host: String,
    pub ioc: u32,
    pub ctl: String,
    pub info: IocInfo,
    pub facts: Option<Facts>,
    pub man0: Option<Man0>,
    /// IO Unit Page 0: NVDATA (default, persistent).
    pub nvdata: Option<(u32, u32)>,
    pub bios_version: Option<u32>,
    pub phys: Vec<Phy>,
    pub devices: Vec<Device>,
    pub enclosures: Vec<Enclosure>,
    pub images: Vec<Image>,
    /// When the regions were read back (unix seconds); None: not yet.
    pub images_read: Option<u64>,
    pub last_flash: Option<FlashRecord>,
    /// What could not be read, each in a line.
    pub errors: Vec<String>,
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Every flash region read back.
pub fn read_images(p: &dyn Port, ioc: u32, mpi25: bool) -> (Vec<Image>, Vec<String>) {
    let mut images = Vec::new();
    let mut errors = Vec::new();
    for r in Region::ALL {
        match mpt::upload(p, ioc, mpi25, r, &|_, _| {}) {
            Ok(Some(b)) => images.extend(images_of(r, &b)),
            Ok(None) => {}
            Err(e) => errors.push(e),
        }
    }
    (images, errors)
}

/// The card's report: pages always, the regions when `with_images`.
pub fn read(p: &dyn Port, ioc: u32, pci: &str, host: &str, with_images: bool) -> Result<CardReport, String> {
    let info = p.info(ioc).map_err(|e| format!("{}: ioc {ioc}: IOC info: {e}", p.name()))?;
    let mut r = CardReport { pci: pci.to_string(), host: host.to_string(), ioc, ctl: p.name(), info, ..Default::default() };
    let mut note = |e: String| r.errors.push(e);
    let facts = mpt::ioc_facts(p, ioc).map_err(&mut note).ok();
    let std_page = |ty: u8, n: u8| mpt::page(p, ioc, ty, 0, n, 0);
    let man0 = std_page(mpt::PAGETYPE_MANUFACTURING, 0).map_err(&mut note).ok().flatten().map(|b| mpt::parse_man0(&b));
    let nvdata = std_page(mpt::PAGETYPE_IO_UNIT, 0).map_err(&mut note).ok().flatten().map(|b| mpt::parse_io_unit0(&b));
    let bios = std_page(mpt::PAGETYPE_BIOS, 3).map_err(&mut note).ok().flatten().map(|b| mpt::parse_bios3(&b));
    let mut phys = match mpt::ext_page(p, ioc, mpt::EXTPAGE_SAS_IO_UNIT, 0, 0) {
        Ok(Some(b)) => mpt::parse_sas_io_unit0(&b),
        Ok(None) => Vec::new(),
        Err(e) => return Err(format!("{}: ioc {ioc}: {e}", p.name())),
    };
    if let Some(b) = mpt::ext_page(p, ioc, mpt::EXTPAGE_SAS_IO_UNIT, 1, 0).map_err(&mut note).ok().flatten() {
        for (ph, (max, min, disabled)) in phys.iter_mut().zip(mpt::parse_sas_io_unit1(&b)) {
            ph.max_rate = Some(max);
            ph.min_rate = Some(min);
            ph.disabled = disabled;
        }
    }
    let devices = mpt::walk(p, ioc, mpt::EXTPAGE_SAS_DEVICE, 0x18).map_err(&mut note).unwrap_or_default().iter().map(Vec::as_slice).map(mpt::parse_sas_device0).collect();
    let enclosures = mpt::walk(p, ioc, mpt::EXTPAGE_ENCLOSURE, 0x16).map_err(&mut note).unwrap_or_default().iter().map(Vec::as_slice).map(mpt::parse_enclosure0).collect();
    r.facts = facts;
    r.man0 = man0;
    r.nvdata = nvdata;
    r.bios_version = bios;
    r.phys = phys;
    r.devices = devices;
    r.enclosures = enclosures;
    if with_images {
        let (images, errs) = read_images(p, ioc, r.info.mpi25());
        r.images = images;
        r.errors.extend(errs);
        r.images_read = Some(now_secs());
    }
    Ok(r)
}

/// Every card, by PCI address.
pub type Cards = BTreeMap<String, CardReport>;

/// The mpt2sas/mpt3sas SCSI hosts under `sys`: (ioc number, PCI function,
/// hostN). The driver sets each host's `unique_id` to its IOC number.
pub fn mpt_hosts(sys: &Path) -> Vec<(u32, String, String)> {
    let mut v = Vec::new();
    let Ok(rd) = std::fs::read_dir(sys.join("class/scsi_host")) else { return v };
    for e in rd.flatten() {
        let p = e.path();
        let proc_name = std::fs::read_to_string(p.join("proc_name")).unwrap_or_default();
        if !matches!(proc_name.trim(), "mpt3sas" | "mpt2sas") {
            continue;
        }
        let Some(id) = std::fs::read_to_string(p.join("unique_id")).ok().and_then(|s| s.trim().parse().ok()) else { continue };
        let Some(pci) = std::fs::read_link(&p).ok().and_then(|l| crate::hba::bdf_of_link(&l.to_string_lossy())) else { continue };
        v.push((id, pci, e.file_name().to_string_lossy().into_owned()));
    }
    v.sort();
    v
}

/// Read every card on the node. A card already read keeps its regions and
/// last flash unless `images_for` names it (read them again).
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub fn scan(prev: &Cards, images_for: &[String]) -> (Cards, Vec<String>) {
    let mut out = Cards::new();
    let mut errs = Vec::new();
    let hosts = mpt_hosts(Path::new("/sys"));
    if hosts.is_empty() {
        return (out, errs);
    }
    let ctls = match mpt::Ctl::open_all(Path::new("/dev")) {
        Ok(c) => c,
        Err(e) => {
            errs.push(format!("the mpt control device cannot be opened: {e}"));
            return (out, errs);
        }
    };
    for (ioc, pci, host) in hosts {
        let old = prev.get(&pci);
        let images = !matches!(old, Some(o) if o.images_read.is_some()) || images_for.contains(&pci);
        let mut last = None;
        for c in &ctls {
            match read(c, ioc, &pci, &host, images) {
                Ok(mut r) => {
                    if let (false, Some(o)) = (images, old) {
                        r.images = o.images.clone();
                        r.images_read = o.images_read;
                    }
                    r.last_flash = old.and_then(|o| o.last_flash.clone());
                    out.insert(pci.clone(), r);
                    last = None;
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        if let Some(e) = last {
            errs.push(format!("{pci} {host}: {e}"));
        }
    }
    (out, errs)
}

/// The control device that answers for `ioc` (SAS3 cards on mpt3ctl, SAS2
/// on mpt2ctl).
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub fn port_for(ioc: u32) -> Result<mpt::Ctl, String> {
    let ctls = mpt::Ctl::open_all(Path::new("/dev")).map_err(|e| format!("the mpt control device: {e}"))?;
    let mut last = String::new();
    for c in ctls {
        match c.info(ioc) {
            Ok(_) => return Ok(c),
            Err(e) => last = format!("{}: {e}", c.name()),
        }
    }
    Err(format!("no mpt control device answers for ioc {ioc} ({last})"))
}

// ---- the object ----

fn hex(a: u64) -> String {
    format!("{a:016x}")
}

/// A SAS address or logical id as sysfs or a page says it -> its number.
pub fn addr(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim().trim_start_matches("0x").trim_start_matches("0X"), 16).ok()
}

/// The object's name: node and PCI address (`forge1-0000-01-00-0`).
pub fn object_name(node: &str, pci: &str) -> String {
    let s: String = format!("{node}-{pci}").to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect();
    s.trim_matches('-').to_string()
}

/// A drive of this node, as the card's rows are joined to it.
#[derive(Debug, Clone, Default)]
pub struct DriveRef {
    pub id: String,
    pub name: String,
    pub sas_address: Option<u64>,
    pub pci: Option<String>,
}

/// The HostBusAdapter object (`storage.storm.io/v1`) for one card.
pub fn object(r: &CardReport, node: &str, sas_address: Option<&str>, drives: &[DriveRef], shelves: &BTreeMap<u64, String>) -> Value {
    let name = object_name(node, &r.pci);
    let nv = |w: u32| format!("{:02x}.{:02x}.{:02x}.{:02x}", w >> 24, (w >> 16) & 0xFF, (w >> 8) & 0xFF, w & 0xFF);
    let device = |h: u16| r.devices.iter().find(|d| d.handle == h);
    let drive_of = |a: u64| drives.iter().find(|d| d.sas_address == Some(a));
    let phys: Vec<Value> = r
        .phys
        .iter()
        .map(|p| {
            let att = (p.attached != 0).then(|| device(p.attached)).flatten();
            json!({
                "phy": p.num,
                "port": p.port,
                "rate": mpt::rate_name(p.rate),
                "rateMbps": mpt::rate_mbps(p.rate),
                "maxRateMbps": p.max_rate.and_then(mpt::rate_mbps),
                "minRateMbps": p.min_rate.and_then(mpt::rate_mbps),
                "disabled": p.disabled,
                "attached": att.map(|d| json!({
                    "handle": d.handle,
                    "sasAddress": hex(d.sas_address),
                    "kind": d.kind(),
                    "drive": drive_of(d.sas_address).map(|x| x.id.clone()),
                    "name": drive_of(d.sas_address).map(|x| x.name.clone()),
                })),
            })
        })
        .collect();
    let encl_of = |h: u16| r.enclosures.iter().find(|e| e.handle == h);
    let devices: Vec<Value> = r
        .devices
        .iter()
        .map(|d| {
            let e = encl_of(d.enclosure);
            json!({
                "handle": d.handle,
                "sasAddress": hex(d.sas_address),
                "kind": d.kind(),
                "present": d.present(),
                "access": d.access(),
                "enclosure": e.map(|e| hex(e.logical_id)),
                "slot": d.slot,
                "phy": d.phy,
                "drive": drive_of(d.sas_address).map(|x| x.id.clone()),
                "name": drive_of(d.sas_address).map(|x| x.name.clone()),
            })
        })
        .collect();
    let enclosures: Vec<Value> = r
        .enclosures
        .iter()
        .map(|e| json!({ "logicalId": hex(e.logical_id), "slots": e.num_slots, "startSlot": e.start_slot, "shelf": shelves.get(&e.logical_id) }))
        .collect();
    let images: Vec<Value> = r
        .images
        .iter()
        .map(|i| json!({ "region": i.region.as_str(), "part": i.part, "size": i.size, "sha256": i.sha256, "version": i.version, "productId": i.product_id.map(|p| format!("{p:04x}")), "name": i.name }))
        .collect();
    let mut problems: Vec<String> = r.facts.as_ref().map(|f| f.exceptions().iter().map(|s| s.to_string()).collect()).unwrap_or_default();
    for d in &r.devices {
        if let Some(a) = d.access() {
            problems.push(format!("device {} (slot {}): {a}", hex(d.sas_address), d.slot));
        }
    }
    let labels = json!({
        "storm.io/node": crate::controller::label_value(node),
        "storage.storm.io/pci": crate::controller::label_value(&r.pci.replace(':', "-")),
    });
    let rfc = crate::controller::rfc3339;
    json!({
        "apiVersion": format!("{}/{}", crate::api::kube::GROUP, crate::api::kube::VERSION),
        "kind": "HostBusAdapter",
        "metadata": { "name": name, "labels": labels },
        "status": {
            "node": node,
            "present": true,
            "pci": r.pci,
            "scsiHost": r.host,
            "ioc": r.ioc,
            "controlDevice": r.ctl,
            "adapter": r.info.adapter(),
            "pciDevice": format!("{:04x}", r.info.pci_device),
            "subsystem": format!("{:04x}:{:04x}", r.info.subsystem_vendor, r.info.subsystem_device),
            "driver": r.info.driver_version,
            "chip": r.man0.as_ref().and_then(|m| m.chip_name.clone()),
            "chipRevision": r.man0.as_ref().and_then(|m| m.chip_revision.clone()),
            "board": r.man0.as_ref().and_then(|m| m.board_name.clone()),
            "boardAssembly": r.man0.as_ref().and_then(|m| m.board_assembly.clone()),
            "boardTracer": r.man0.as_ref().and_then(|m| m.board_tracer.clone()),
            "sasAddress": sas_address,
            "firmwareVersion": mpt::version(r.info.firmware_version),
            "biosVersion": r.bios_version.map(mpt::version),
            "nvdataVersion": r.nvdata.map(|(_, p)| nv(p)),
            "nvdataDefaultVersion": r.nvdata.map(|(d, _)| nv(d)),
            "facts": r.facts.as_ref().map(|f| json!({
                "mpiVersion": f.mpi_version(),
                "productId": format!("{:04x}", f.product_id),
                "capabilities": format!("{:08x}", f.capabilities),
                "ports": f.number_of_ports,
                "maxTargets": f.max_targets,
                "maxInitiators": f.max_initiators,
                "maxExpanders": f.max_expanders,
                "maxEnclosures": f.max_enclosures,
                "maxVolumes": f.max_volumes,
                "requestCredit": f.request_credit,
            })),
            "phys": phys,
            "linkedPhys": r.phys.iter().filter(|p| p.linked()).count(),
            "devices": devices,
            "drives": r.devices.iter().filter(|d| !d.is_expander() && d.kind().ends_with("drive")).count(),
            "enclosures": enclosures,
            "firmware": { "images": images, "readAt": r.images_read.map(rfc) },
            "lastFlash": r.last_flash.as_ref().map(|f| json!({
                "operation": f.operation, "region": f.region.as_str(), "image": f.image, "sha256": f.sha256,
                "before": f.before, "after": f.after, "started": rfc(f.started), "finished": rfc(f.finished), "result": f.result,
            })),
            "problems": problems,
            "errors": r.errors,
        },
    })
}

/// What the object's status says, without the parts that change on their
/// own: whether it needs writing.
pub fn fingerprint(o: &Value) -> String {
    o["status"].to_string()
}

// ---- the flash ----

/// What a CardOperation asks (`spec`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FlashSpec {
    pub node: String,
    /// The HostBusAdapter's name or the card's PCI address.
    pub card: String,
    pub op: FlashOp,
    pub region: Region,
    pub image: GoldenRef,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlashOp {
    Flash,
}

/// The golden the image is (compliance-ops#23): its name in the node's
/// firmware store, its sha256 and where it came from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoldenRef {
    pub name: String,
    pub sha256: String,
    pub source: String,
}

/// The image is the golden named, and fits this card. -> the version the
/// image carries, when it says one.
pub fn check_image(spec: &FlashSpec, image: &[u8], card: &CardReport) -> Result<Option<String>, String> {
    let want = spec.image.sha256.trim().to_lowercase();
    if want.len() != 64 || !want.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("image.sha256: 64 hex digits".into());
    }
    let src = spec.image.source.trim();
    if !(src.starts_with("https://") || src.starts_with("http://")) {
        return Err("image.source: the URL the golden was recorded from (compliance-ops#23)".into());
    }
    let got = crate::firmware::sha256_hex(image);
    if got != want {
        return Err(format!("image {}: sha256 {got}, not the golden's {want}", spec.image.name));
    }
    match spec.region {
        Region::Firmware => {
            let h = mpt::parse_fw_header(image).ok_or_else(|| format!("image {}: no MPI firmware header", spec.image.name))?;
            if h.vendor_id != 0x1000 {
                return Err(format!("image {}: vendor {:04x}, the card's is 1000", spec.image.name, h.vendor_id));
            }
            let facts = card.facts.as_ref().ok_or("the card's IOC Facts were not read: its product cannot be checked")?;
            if h.product_id != facts.product_id {
                return Err(format!("image {}: product {:04x}, the card is {:04x}", spec.image.name, h.product_id, facts.product_id));
            }
            if (h.image_size as usize) > image.len() {
                return Err(format!("image {}: its header says {} bytes, the file has {}", spec.image.name, h.image_size, image.len()));
            }
            Ok(Some(mpt::version(h.fw_version)))
        }
        Region::Bios => {
            let roms = mpt::parse_rom(image);
            if roms.is_empty() {
                return Err(format!("image {}: not an option ROM (no 55 AA / PCIR image)", spec.image.name));
            }
            if let Some(r) = roms.iter().find(|r| r.vendor_id != 0x1000) {
                return Err(format!("image {}: its {} image is for vendor {:04x}, the card's is 1000", spec.image.name, r.kind(), r.vendor_id));
            }
            Ok(None)
        }
        Region::FirmwareBackup | Region::Manufacturing => Err(format!("region {}: a flash writes firmware or bios", spec.region.as_str())),
    }
}

/// Where a flash is (the operation's `status.step`).
pub trait Progress: Sync {
    fn step(&self, step: &str, done: u64, total: u64);
}

/// How the flash ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flashed {
    pub before: Option<String>,
    pub after: Option<String>,
    /// The new image is running (or the card answers with the same version
    /// it ran: a same-version flash).
    pub reset: bool,
    pub message: String,
}

fn region_version(p: &dyn Port, ioc: u32, region: Region) -> Option<String> {
    match region {
        Region::Bios => mpt::page(p, ioc, mpt::PAGETYPE_BIOS, 0, 3, 0).ok().flatten().map(|b| mpt::version(mpt::parse_bios3(&b))),
        _ => p.info(ioc).ok().map(|i| mpt::version(i.firmware_version)),
    }
}

/// Write `image` to the card's region, read it back, and when it matches
/// and `idle()` says the card's drives have no I/O in flight, reset the
/// card and read its version again.
pub fn flash(p: &dyn Port, ioc: u32, mpi25: bool, region: Region, image: &[u8], idle: &dyn Fn() -> Result<(), String>, progress: &dyn Progress) -> Result<Flashed, String> {
    let before = region_version(p, ioc, region);
    progress.step("download", 0, image.len() as u64);
    mpt::download(p, ioc, mpi25, region, image, &|d, t| progress.step("download", d, t))?;
    progress.step("verify", 0, image.len() as u64);
    let back = mpt::upload(p, ioc, mpi25, region, &|d, t| progress.step("verify", d, t))?.ok_or("read back: the card has no such region")?;
    if back.len() < image.len() {
        return Err(format!("read back {} bytes of {}: the card was not reset and runs what it ran", back.len(), image.len()));
    }
    if let Some(i) = image.iter().zip(&back).position(|(a, b)| a != b) {
        return Err(format!("read back differs at byte {i} (0x{:02x}, written 0x{:02x}): the card was not reset and runs what it ran", back[i], image[i]));
    }
    progress.step("quiesce", 0, 0);
    if let Err(why) = idle() {
        return Ok(Flashed {
            before,
            after: None,
            reset: false,
            message: format!("written and read back; not reset: {why}. The new image runs after the card's next reset (a reboot, or this operation again)"),
        });
    }
    progress.step("reset", 0, 0);
    p.hard_reset(ioc).map_err(|e| format!("written and read back; the reset failed: {e}"))?;
    progress.step("ready", 0, 0);
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if mpt::ioc_facts(p, ioc).is_ok() {
            break;
        }
        if Instant::now() >= deadline {
            return Err("written, read back and reset; the card has not answered IOC Facts in 180 s".into());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    let after = region_version(p, ioc, region);
    let message = match (&before, &after) {
        (Some(b), Some(a)) if a == b => format!("written, read back, reset: {} {a} (the same version as before)", region.label()),
        (Some(b), Some(a)) => format!("written, read back, reset: {} {b} → {a}", region.label()),
        _ => "written, read back, reset".to_string(),
    };
    Ok(Flashed { before, after, reset: true, message })
}

/// The block devices of the drives on a card: `/sys/block/<dev>/inflight`
/// says what each has in flight (reads, writes).
pub fn inflight(sys: &Path, devs: &[String]) -> Vec<(String, u64, u64)> {
    devs.iter()
        .filter_map(|d| {
            let s = std::fs::read_to_string(sys.join("block").join(d).join("inflight")).ok()?;
            let mut it = s.split_whitespace().map(|n| n.parse::<u64>().unwrap_or(0));
            Some((d.clone(), it.next().unwrap_or(0), it.next().unwrap_or(0)))
        })
        .collect()
}

/// Waits up to `wait` for one moment with nothing in flight on any of
/// `devs`. The driver's reset then holds new I/O itself while it runs.
pub fn wait_idle(sys: &Path, devs: &[String], wait: Duration) -> Result<(), String> {
    let deadline = Instant::now() + wait;
    loop {
        let busy: Vec<String> = inflight(sys, devs).into_iter().filter(|(_, r, w)| r + w > 0).map(|(d, r, w)| format!("{d} {r} read(s) {w} write(s)")).collect();
        if busy.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("I/O still in flight after {} s ({})", wait.as_secs(), busy.join(", ")));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpt::fake::{self, Fake};
    use std::sync::Mutex;

    struct Steps(Mutex<Vec<String>>);
    impl Progress for Steps {
        fn step(&self, s: &str, _: u64, _: u64) {
            let mut v = self.0.lock().unwrap();
            if v.last().map(String::as_str) != Some(s) {
                v.push(s.to_string());
            }
        }
    }

    fn spec(region: Region, image: &[u8]) -> FlashSpec {
        FlashSpec {
            node: "forge1".into(),
            card: "0000:01:00.0".into(),
            op: FlashOp::Flash,
            region,
            image: GoldenRef { name: "hba330.fw".into(), sha256: crate::firmware::sha256_hex(image), source: "https://example.invalid/hba330.fw".into() },
            dry_run: false,
        }
    }

    #[test]
    fn the_report_and_its_object() {
        let c = Fake::new(fake::hba330());
        let r = read(&c, 0, "0000:01:00.0", "host0", true).unwrap();
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(r.phys[2].max_rate, Some(0xA));
        // firmware, bios + its two images, manufacturing; no backup region.
        let kinds: Vec<(&str, Option<&str>)> = r.images.iter().map(|i| (i.region.as_str(), i.part.as_deref())).collect();
        assert_eq!(kinds, [("firmware", None), ("bios", None), ("bios", Some("legacy_bios")), ("bios", Some("uefi")), ("manufacturing", None)]);
        assert_eq!(r.images[0].version.as_deref(), Some("16.00.01.00"));
        let drives = [DriveRef { id: "d-1".into(), name: "sdb".into(), sas_address: Some(0x5000_cca2_5e8b_1c11), pci: Some("0000:01:00.0".into()) }];
        let shelves = BTreeMap::from([(0x5006_05b0_0c8e_1200u64, "internal-forge1".to_string())]);
        let o = object(&r, "forge1", Some("0x500605b00c8e1200"), &drives, &shelves);
        assert_eq!(o["metadata"]["name"], "forge1-0000-01-00-0");
        let s = &o["status"];
        assert_eq!((s["board"].as_str(), s["firmwareVersion"].as_str(), s["biosVersion"].as_str()), (Some("HBA330"), Some("16.00.01.00"), Some("08.18.07.00")));
        assert_eq!(s["nvdataVersion"], "0e.01.00.07");
        assert_eq!(s["facts"]["productId"], "2221");
        assert_eq!(s["phys"][1]["attached"]["drive"], "d-1");
        assert_eq!(s["phys"][1]["rateMbps"], 12000);
        assert_eq!(s["phys"][2]["maxRateMbps"], 6000);
        assert_eq!(s["devices"][0]["enclosure"], "500605b00c8e1200");
        assert_eq!(s["enclosures"][0]["shelf"], "internal-forge1");
        assert_eq!((s["linkedPhys"].as_u64(), s["drives"].as_u64()), (Some(1), Some(1)));
        assert_eq!(s["firmware"]["images"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn a_same_version_flash_writes_reads_back_and_resets() {
        let c = Fake::new(fake::hba330());
        let img = c.with(|s| s.flash[&1].clone());
        let r = read(&c, 0, "0000:01:00.0", "host0", false).unwrap();
        assert_eq!(check_image(&spec(Region::Firmware, &img), &img, &r).unwrap().as_deref(), Some("16.00.01.00"));
        let steps = Steps(Mutex::new(Vec::new()));
        let f = flash(&c, 0, true, Region::Firmware, &img, &|| Ok(()), &steps).unwrap();
        assert!(f.reset);
        assert_eq!((f.before.as_deref(), f.after.as_deref()), (Some("16.00.01.00"), Some("16.00.01.00")));
        assert!(f.message.contains("the same version"), "{}", f.message);
        assert_eq!(*steps.0.lock().unwrap(), ["download", "verify", "quiesce", "reset", "ready"]);
        assert_eq!(c.with(|s| s.resets), 1);
    }

    #[test]
    fn a_new_version_runs_after_the_reset() {
        let c = Fake::new(fake::hba330());
        let img = fake::fw_image(0x1000_0200, 0x2221, 180_000);
        let f = flash(&c, 0, true, Region::Firmware, &img, &|| Ok(()), &Steps(Mutex::new(Vec::new()))).unwrap();
        assert_eq!((f.before.as_deref(), f.after.as_deref()), (Some("16.00.01.00"), Some("16.00.02.00")));
        assert!(c.with(|s| s.commands.contains(&0x09)));
    }

    #[test]
    fn a_bad_read_back_is_never_reset() {
        let c = Fake::new(fake::hba330());
        c.with(|s| s.corrupt_at = Some(70_000));
        let img = fake::fw_image(0x1000_0200, 0x2221, 100_000);
        let e = flash(&c, 0, true, Region::Firmware, &img, &|| Ok(()), &Steps(Mutex::new(Vec::new()))).unwrap_err();
        assert!(e.contains("differs at byte 70000") && e.contains("not reset"), "{e}");
        assert_eq!(c.with(|s| s.resets), 0);
    }

    #[test]
    fn io_in_flight_holds_the_reset() {
        let c = Fake::new(fake::hba330());
        let img = fake::rom_image();
        let f = flash(&c, 0, true, Region::Bios, &img, &|| Err("I/O still in flight after 30 s (sdb 1 read(s) 0 write(s))".into()), &Steps(Mutex::new(Vec::new()))).unwrap();
        assert!(!f.reset && f.message.contains("not reset"), "{}", f.message);
        assert_eq!(c.with(|s| s.resets), 0);
    }

    #[test]
    fn an_image_that_is_not_the_golden_or_not_for_this_card_is_refused() {
        let c = Fake::new(fake::hba330());
        let r = read(&c, 0, "0000:01:00.0", "host0", false).unwrap();
        let img = fake::fw_image(0x1000_0200, 0x2221, 4096);
        let mut s = spec(Region::Firmware, &img);
        s.image.sha256 = "00".repeat(32);
        assert!(check_image(&s, &img, &r).unwrap_err().contains("not the golden's"));
        let mut s = spec(Region::Firmware, &img);
        s.image.source = String::new();
        assert!(check_image(&s, &img, &r).unwrap_err().contains("source"));
        let other = fake::fw_image(0x1000_0200, 0x2228, 4096);
        assert!(check_image(&spec(Region::Firmware, &other), &other, &r).unwrap_err().contains("product 2228"));
        let rom = fake::rom_image();
        assert!(check_image(&spec(Region::Firmware, &rom), &rom, &r).unwrap_err().contains("no MPI firmware header"));
        assert_eq!(check_image(&spec(Region::Bios, &rom), &rom, &r).unwrap(), None);
        assert!(check_image(&spec(Region::Bios, &img), &img, &r).unwrap_err().contains("not an option ROM"));
        let mut nofacts = r.clone();
        nofacts.facts = None;
        assert!(check_image(&spec(Region::Firmware, &img), &img, &nofacts).unwrap_err().contains("IOC Facts"));
    }

    #[test]
    fn spec_parses_and_refuses_unknown_fields() {
        let v = json!({ "node": "forge1", "card": "forge1-0000-01-00-0", "op": "flash", "region": "firmware", "image": { "name": "a.fw", "sha256": "ab", "source": "https://x" }, "dryRun": true });
        let s: FlashSpec = serde_json::from_value(v).unwrap();
        assert!(s.dry_run && s.region == Region::Firmware);
        assert!(serde_json::from_value::<FlashSpec>(json!({ "node": "a", "card": "b", "op": "erase", "region": "bios", "image": { "name": "a", "sha256": "b", "source": "c" } })).is_err());
    }

    #[test]
    fn idle_reads_inflight() {
        let d = std::env::temp_dir().join(format!("sd-inflight-{}", std::process::id()));
        std::fs::create_dir_all(d.join("block/sdb")).unwrap();
        std::fs::write(d.join("block/sdb/inflight"), "       0        0\n").unwrap();
        assert!(wait_idle(&d, &["sdb".into()], Duration::from_millis(10)).is_ok());
        std::fs::write(d.join("block/sdb/inflight"), "       2        1\n").unwrap();
        assert!(wait_idle(&d, &["sdb".into()], Duration::from_millis(10)).unwrap_err().contains("sdb 2 read(s) 1 write(s)"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn names_and_addresses() {
        assert_eq!(object_name("Forge1", "0000:c1:00.0"), "forge1-0000-c1-00-0");
        assert_eq!(addr("0x5000CCA2"), Some(0x5000_cca2));
        assert_eq!(addr("x"), None);
    }
}
