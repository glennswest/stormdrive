//! A RAID controller's backplane is a shelf too (#65; owner: "off a perc by
//! definition it should go to a shelf. just in this case shelf is internal").
//!
//! `megaraid_sas` (a PERC, a MegaRAID) shows Linux its pass-through drives and
//! no SES device, so nothing in sysfs names their enclosure or slot. The
//! controller knows both: it answers the MFI management commands storcli and
//! smartctl send, over `MEGASAS_IOC_FIRMWARE` on the driver's char device.
//! Only reads are sent: the physical drive list and one drive's info.
//!
//! Each enclosure on the controller becomes one [`ShelfReport`] with `through`
//! set, in position `internal`: the engine sees no enclosure there and says the
//! same of those drives. A controller that cannot be read is still an internal
//! shelf, with no bays, and says why.
//!
//! Layouts are the kernel's `drivers/scsi/megaraid/megaraid_sas.h`. Everything
//! but the ioctl itself is portable and tested against a fake firmware.

use crate::drive::{Position, Shelf};
use crate::ses::ShelfReport;
use crate::topology::Shelves;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::SystemTime;

/// MR_DCMD_PD_GET_LIST: every physical device, enclosures included.
pub const DCMD_PD_LIST: u32 = 0x0201_0000;
/// MR_DCMD_PD_GET_INFO: one device, by device id in the mailbox.
pub const DCMD_PD_INFO: u32 = 0x0202_0000;
/// sizeof(struct MR_PD_INFO) as the driver reads it.
const PD_INFO_LEN: usize = 512;
/// Room for the most a controller lists: 8-byte header + 24 bytes × 512.
const PD_LIST_LEN: usize = 8 + 24 * 512;
const SCSI_TYPE_DISK: u8 = 0x00;
const SCSI_TYPE_ENCLOSURE: u8 = 0x0d;
/// `enclDeviceId` of a drive in no enclosure.
const NO_ENCLOSURE: u16 = 0xffff;
/// The driver's MEGASAS_MAX_DEV_PER_CHANNEL: device id = channel × this + id.
const DEVICES_PER_CHANNEL: u16 = 128;
/// Channels 0 and 1 are physical devices; 2 and 3 logical drives.
const PD_CHANNELS: u16 = 2;
const DRIVER: &str = "megaraid_sas";

/// How a shelf with no SES device was read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Through {
    pub driver: String,
    pub scsi_host: String,
    #[serde(default)]
    pub pcie_addr: Option<String>,
    /// The controller's id for the enclosure ("32" in storcli's `/e32`);
    /// None for drives the controller places in no enclosure.
    #[serde(default)]
    pub enclosure_device_id: Option<u16>,
    /// Physical device id → slot.
    #[serde(default)]
    pub device_bays: BTreeMap<u16, u32>,
    /// Devices the controller lists in no enclosure, placed on this one as
    /// the controller's only enclosure (#122: forge1's PERC lists two of the
    /// BP14G+'s four drives with enclosure 0xffff). Their bay is the slot the
    /// controller gives.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub no_enclosure_devices: Vec<u16>,
    /// Devices on this shelf whose slot the controller does not give (0xff):
    /// on the shelf, with no bay — "slot not matched", never a shelf of their
    /// own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slot_not_matched: Vec<u16>,
    /// Why the controller could not be read: the shelf has no bays then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One entry of the physical drive list (struct MR_PD_ADDRESS).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pd {
    pub device_id: u16,
    pub enclosure: u16,
    pub slot: u8,
    pub scsi_type: u8,
    pub sas_address: Option<String>,
}

/// struct MR_PD_LIST: size, count, then 24-byte entries.
pub fn parse_pd_list(raw: &[u8]) -> Option<Vec<Pd>> {
    let word = |o: usize| raw.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let count = word(4)? as usize;
    let mut out = Vec::with_capacity(count.min(512));
    for e in raw.get(8..)?.chunks_exact(24).take(count) {
        let le16 = |o: usize| u16::from_le_bytes([e[o], e[o + 1]]);
        // sasAddr[2]: one per port, the other zero on a single-ported path.
        let sas = [8, 16]
            .iter()
            .map(|&o| u64::from_le_bytes([e[o], e[o + 1], e[o + 2], e[o + 3], e[o + 4], e[o + 5], e[o + 6], e[o + 7]]))
            .find(|a| *a != 0)
            .map(|a| format!("{a:016x}"));
        out.push(Pd { device_id: le16(0), enclosure: le16(2), slot: e[5], scsi_type: e[6], sas_address: sas });
    }
    Some(out)
}

/// struct MR_PD_INFO starts with a 4-byte reference and the device's standard
/// INQUIRY data: its vendor and product.
pub fn parse_pd_identity(raw: &[u8]) -> (Option<String>, Option<String>) {
    let text = |r: std::ops::Range<usize>| {
        raw.get(r).map(|b| String::from_utf8_lossy(b).trim_matches(|c: char| c.is_whitespace() || c == '\0').to_string()).filter(|s| !s.is_empty())
    };
    (text(4 + 8..4 + 16), text(4 + 16..4 + 32))
}

/// The 128-byte frame of a read DCMD (struct megasas_dcmd_frame). The kernel
/// fills the SGE's address from the packet's iovec; the length stays here.
pub fn dcmd_frame(opcode: u32, mbox: Option<u16>, len: u32) -> [u8; 128] {
    let m = mbox.map(u16::to_le_bytes);
    dcmd_frame_with(opcode, m.as_ref().map_or(&[][..], |b| &b[..]), Some(len))
}

/// A DCMD frame with a mailbox of up to 12 bytes; `read` = Some(length) for
/// a command that returns data, None for one that moves none (a state
/// change: MFI_FRAME_DIR_NONE, no SGE).
pub fn dcmd_frame_with(opcode: u32, mbox: &[u8], read: Option<u32>) -> [u8; 128] {
    let mut f = [0u8; 128];
    f[0x00] = 0x05; // MFI_CMD_DCMD
    f[0x18..0x1c].copy_from_slice(&opcode.to_le_bytes());
    let n = mbox.len().min(12);
    f[0x1c..0x1c + n].copy_from_slice(&mbox[..n]);
    if let Some(len) = read {
        f[0x07] = 1; // sge_count
        f[0x10..0x12].copy_from_slice(&0x0010u16.to_le_bytes()); // MFI_FRAME_DIR_READ
        f[0x14..0x18].copy_from_slice(&len.to_le_bytes());
        f[0x2c..0x30].copy_from_slice(&len.to_le_bytes()); // sge length
    }
    f
}

/// Offset of the scatter-gather list in the frame.
pub const FRAME_SGL_OFFSET: u32 = 0x28;

/// "/sys/…/target0:0:4/0:0:4:0/block/sda" → (channel, target).
pub fn scsi_address(real: &std::path::Path) -> Option<(u16, u16)> {
    real.components().find_map(|c| {
        let c = c.as_os_str().to_str()?;
        let mut p = c.split(':');
        let (_h, ch, t, _l) = (p.next()?.parse::<u32>().ok()?, p.next()?.parse().ok()?, p.next()?.parse().ok()?, p.next()?.parse::<u64>().ok()?);
        p.next().is_none().then_some((ch, t))
    })
}

/// A physical device's id from its SCSI address; None on a logical drive's
/// channel.
pub fn device_id(channel: u16, target: u16) -> Option<u16> {
    (channel < PD_CHANNELS).then_some(channel * DEVICES_PER_CHANNEL + target)
}

/// The shelf and bay of a drive on `host` at SCSI address `addr`, from the
/// controller-read reports. A device the list does not name (a RAID volume, or
/// a controller that was not read) is on the controller's shelf with no bay,
/// when it has just one to be on.
pub fn place(shelves: &Shelves, host: &str, addr: (u16, u16)) -> Option<(Shelf, Option<u32>)> {
    let mine: Vec<&ShelfReport> = shelves.values().filter(|r| r.through.as_ref().is_some_and(|t| t.scsi_host == host)).collect();
    if let Some(id) = device_id(addr.0, addr.1) {
        for r in &mine {
            let Some(t) = r.through.as_ref() else { continue };
            if let Some(bay) = t.device_bays.get(&id) {
                return Some((r.shelf.clone(), Some(*bay)));
            }
            if t.slot_not_matched.contains(&id) {
                return Some((r.shelf.clone(), None));
            }
        }
    }
    match mine.as_slice() {
        [only] => Some((only.shelf.clone(), None)),
        _ => None,
    }
}

/// A name for a shelf with no address of its own: unique across the cluster
/// (a Shelf object is cluster-wide) and fit to be an object name.
pub(crate) fn local_key(node: &str, pcie: Option<&str>, host: &str, enclosure: Option<u16>) -> String {
    let mut k = format!("internal-{node}-{}", pcie.unwrap_or(host));
    if let Some(e) = enclosure {
        k.push_str(&format!("-e{e}"));
    }
    k.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '-' }).collect()
}

/// Sends one command (opcode, mailbox) and fills the buffer.
pub(crate) type Dcmd<'a> = dyn FnMut(u32, Option<u16>, &mut [u8]) -> Result<(), String> + 'a;

struct Controller<'a> {
    node: &'a str,
    host: &'a str,
    pcie: Option<&'a str>,
}

impl Controller<'_> {
    fn report(&self, enclosure: Option<u16>, shelf: Shelf, device_bays: BTreeMap<u16, u32>, slots: BTreeMap<u32, Vec<String>>, error: Option<String>) -> ShelfReport {
        self.report_with(enclosure, shelf, Placed { device_bays, slots, ..Default::default() }, error)
    }

    fn report_with(&self, enclosure: Option<u16>, shelf: Shelf, placed: Placed, error: Option<String>) -> ShelfReport {
        let Placed { device_bays, slots, no_enclosure_devices, slot_not_matched } = placed;
        let id = local_key(self.node, self.pcie, self.host, enclosure);
        let shelf = Shelf { id: Some(id), position: Some(Position::Internal), ..shelf };
        ShelfReport {
            key: shelf.key().unwrap_or_default(),
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
                driver: DRIVER.into(),
                scsi_host: self.host.into(),
                pcie_addr: self.pcie.map(str::to_string),
                enclosure_device_id: enclosure,
                device_bays,
                no_enclosure_devices,
                slot_not_matched,
                error,
            }),
            collected_at: SystemTime::now(),
            status_raw: Vec::new(),
        }
    }

    /// One report per enclosure on the controller. `dcmd` sends one command
    /// and fills the buffer.
    fn read(&self, dcmd: &mut Dcmd<'_>) -> Vec<ShelfReport> {
        let mut list = vec![0u8; PD_LIST_LEN];
        let pds = match dcmd(DCMD_PD_LIST, None, &mut list).and_then(|()| parse_pd_list(&list).ok_or_else(|| "short drive list".to_string())) {
            Ok(p) => p,
            Err(e) => return vec![self.report(None, Shelf::default(), BTreeMap::new(), BTreeMap::new(), Some(e))],
        };
        // Every enclosure entry, and every enclosure a disk says it is in.
        let mut enclosures: BTreeMap<Option<u16>, Shelf> = BTreeMap::new();
        for p in pds.iter().filter(|p| p.scsi_type == SCSI_TYPE_ENCLOSURE) {
            let mut info = vec![0u8; PD_INFO_LEN];
            let (vendor, model) = match dcmd(DCMD_PD_INFO, Some(p.device_id), &mut info) {
                Ok(()) => parse_pd_identity(&info),
                Err(e) => {
                    tracing::debug!(host = self.host, enclosure = p.device_id, "enclosure info: {e}");
                    (None, None)
                }
            };
            let shelf = Shelf { vendor, model, sas_address: p.sas_address.clone(), logical_id: p.sas_address.clone(), ..Default::default() };
            enclosures.insert(Some(p.device_id), shelf);
        }
        let disks: Vec<&Pd> = pds.iter().filter(|p| p.scsi_type == SCSI_TYPE_DISK).collect();
        let enclosure_of = |d: &Pd| (d.enclosure != NO_ENCLOSURE).then_some(d.enclosure);
        // A disk the controller lists in no enclosure, on a controller with
        // exactly one: that enclosure is the backplane it sits in (#122). With
        // none or several, it stays on a shelf of the controller's own.
        let listed: Vec<u16> = enclosures.keys().flatten().copied().collect();
        let only = match listed.as_slice() {
            [e] => Some(*e),
            _ => None,
        };
        let shelf_of = |d: &Pd| enclosure_of(d).or(only);
        for d in &disks {
            enclosures.entry(shelf_of(d)).or_default();
        }
        if enclosures.is_empty() {
            enclosures.insert(None, Shelf::default());
        }
        enclosures
            .into_iter()
            .map(|(e, shelf)| {
                let mut placed = Placed::default();
                for d in disks.iter().filter(|d| shelf_of(d) == e) {
                    if e.is_some() && enclosure_of(d).is_none() {
                        placed.no_enclosure_devices.push(d.device_id);
                    }
                    if d.slot == 0xff {
                        placed.slot_not_matched.push(d.device_id);
                        continue;
                    }
                    placed.device_bays.insert(d.device_id, u32::from(d.slot));
                    placed.slots.insert(u32::from(d.slot), d.sas_address.iter().cloned().collect());
                }
                if !placed.no_enclosure_devices.is_empty() {
                    tracing::debug!(host = self.host, enclosure = ?e, devices = ?placed.no_enclosure_devices, "listed in no enclosure: placed on the controller's only one");
                }
                self.report_with(e, shelf, placed, None)
            })
            .collect()
    }
}

/// What a controller's list puts on one shelf.
#[derive(Default)]
struct Placed {
    device_bays: BTreeMap<u16, u32>,
    slots: BTreeMap<u32, Vec<String>>,
    no_enclosure_devices: Vec<u16>,
    slot_not_matched: Vec<u16>,
}

fn into_table(reports: Vec<ShelfReport>) -> Shelves {
    reports.into_iter().map(|r| (r.key.clone(), r)).collect()
}

/// The shelves behind every `megaraid_sas` controller on the node. A node
/// without one sends nothing and makes no device node.
pub fn scan(node: &str) -> Shelves {
    #[cfg(target_os = "linux")]
    {
        linux::scan(node)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = node;
        Shelves::new()
    }
}

#[cfg(target_os = "linux")]
pub(crate) mod linux {
    use super::*;
    use std::fs::File;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    const NODE: &str = "/dev/megaraid_sas_ioctl_node";
    const CHRDEV: &str = "megaraid_sas_ioctl";

    /// struct megasas_iocpacket, packed; 404 bytes on a 64-bit kernel, the
    /// size `_IOWR('M', 1, …)` carries.
    #[repr(C, packed)]
    struct IocPacket {
        host_no: u16,
        pad: u16,
        sgl_off: u32,
        sge_count: u32,
        sense_off: u32,
        sense_len: u32,
        frame: [u8; 128],
        sgl: [libc::iovec; 16],
    }

    #[cfg(target_pointer_width = "64")]
    const _: () = assert!(std::mem::size_of::<IocPacket>() == 404);

    /// _IOWR('M', 1, struct megasas_iocpacket)
    const MEGASAS_IOC_FIRMWARE: libc::c_ulong =
        (3 << 30) | ((std::mem::size_of::<IocPacket>() as libc::c_ulong) << 16) | ((b'M' as libc::c_ulong) << 8) | 1;

    fn dcmd(file: &File, host_no: u16, opcode: u32, mbox: Option<u16>, buf: &mut [u8]) -> Result<(), String> {
        let m = mbox.map(u16::to_le_bytes);
        dcmd_raw(file, host_no, opcode, m.as_ref().map_or(&[][..], |b| &b[..]), Some(buf)).map_err(|(_, e)| e)
    }

    /// One DCMD: `buf` Some for a read, None for a command that moves no
    /// data. The error carries the controller's MFI status when it answered.
    pub(crate) fn dcmd_raw(file: &File, host_no: u16, opcode: u32, mbox: &[u8], buf: Option<&mut [u8]>) -> Result<(), (Option<u8>, String)> {
        // SAFETY: all-zero is valid for the packet: integers and null iovecs.
        let mut ioc: IocPacket = unsafe { std::mem::zeroed() };
        ioc.host_no = host_no;
        ioc.sgl_off = FRAME_SGL_OFFSET;
        match buf {
            Some(buf) => {
                ioc.sge_count = 1;
                ioc.frame = dcmd_frame_with(opcode, mbox, Some(buf.len() as u32));
                let mut sgl = ioc.sgl;
                sgl[0] = libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: buf.len() };
                ioc.sgl = sgl;
            }
            None => ioc.frame = dcmd_frame_with(opcode, mbox, None),
        }
        // `as _`: glibc and musl type the ioctl request differently.
        // SAFETY: the packet is the driver's layout, the iovec (if any)
        // points at a buffer of its stated length, alive for the call.
        let rc = unsafe { libc::ioctl(file.as_raw_fd(), MEGASAS_IOC_FIRMWARE as _, &mut ioc as *mut IocPacket) };
        if rc < 0 {
            return Err((None, format!("{NODE}: {}", std::io::Error::last_os_error())));
        }
        let frame = ioc.frame;
        match frame[2] {
            0 => Ok(()),
            st => Err((Some(st), format!("controller status 0x{st:02x} ({}) on command 0x{opcode:08x}", crate::raidctl::mfi_status_name(st)))),
        }
    }

    /// The char device has a dynamic major and no devtmpfs entry: whoever
    /// wants it makes the node, from /proc/devices (as smartctl and storcli).
    pub(crate) fn open_node() -> Result<File, String> {
        let devices = std::fs::read_to_string("/proc/devices").map_err(|e| format!("/proc/devices: {e}"))?;
        let major = major_of(&devices).ok_or_else(|| format!("{CHRDEV} not in /proc/devices"))?;
        let path = Path::new(NODE);
        let current = std::fs::metadata(path).ok().is_some_and(|m| m.file_type().is_char_device() && (m.rdev() >> 8) & 0xfff == u64::from(major));
        if !current {
            let _ = std::fs::remove_file(path);
            let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
            // SAFETY: a valid C string; the device number is major << 8, minor 0.
            let rc = unsafe { libc::mknod(c.as_ptr(), libc::S_IFCHR | 0o600, (u64::from(major) << 8) as libc::dev_t) };
            if rc != 0 {
                return Err(format!("mknod {NODE}: {}", std::io::Error::last_os_error()));
            }
        }
        std::fs::OpenOptions::new().read(true).write(true).open(path).map_err(|e| format!("{NODE}: {e}"))
    }

    pub fn scan(node: &str) -> Shelves {
        let hosts = hosts(Path::new("/sys"));
        if hosts.is_empty() {
            return Shelves::new();
        }
        let file = open_node();
        let mut reports = Vec::new();
        for (host, pcie) in &hosts {
            let c = Controller { node, host, pcie: pcie.as_deref() };
            let number = host.trim_start_matches("host").parse::<u16>();
            match (&file, number) {
                (Ok(f), Ok(n)) => reports.extend(c.read(&mut |op, mbox, buf| dcmd(f, n, op, mbox, buf))),
                (Err(e), _) => reports.push(c.report(None, Shelf::default(), BTreeMap::new(), BTreeMap::new(), Some(e.clone()))),
                (_, Err(e)) => reports.push(c.report(None, Shelf::default(), BTreeMap::new(), BTreeMap::new(), Some(format!("{host}: {e}")))),
            }
        }
        into_table(reports)
    }

    /// Every SCSI host of this driver, with its PCI function.
    pub(crate) fn hosts(sys: &Path) -> Vec<(String, Option<String>)> {
        let Ok(rd) = std::fs::read_dir(sys.join("class/scsi_host")) else { return Vec::new() };
        let mut out: Vec<(String, Option<String>)> = rd
            .flatten()
            .filter(|e| std::fs::read_to_string(e.path().join("proc_name")).is_ok_and(|s| s.trim() == DRIVER))
            .map(|e| {
                let pcie = std::fs::canonicalize(e.path()).ok().and_then(|r| {
                    r.components().map(|c| c.as_os_str().to_string_lossy().to_string()).rfind(|c| crate::topology::is_pci_bdf(c))
                });
                (e.file_name().to_string_lossy().to_string(), pcie)
            })
            .collect();
        out.sort();
        out
    }
}

/// The char device's major in /proc/devices.
pub fn major_of(devices: &str) -> Option<u32> {
    devices.lines().find_map(|l| {
        let (major, name) = l.trim().split_once(' ')?;
        (name.trim() == "megaraid_sas_ioctl").then(|| major.parse().ok())?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(device: u16, enclosure: u16, slot: u8, scsi_type: u8, sas: u64) -> Vec<u8> {
        let mut e = Vec::new();
        e.extend_from_slice(&device.to_le_bytes());
        e.extend_from_slice(&enclosure.to_le_bytes());
        e.extend_from_slice(&[0, slot, scsi_type, 1]);
        e.extend_from_slice(&sas.to_le_bytes());
        e.extend_from_slice(&0u64.to_le_bytes());
        e
    }

    fn list(entries: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = entries.concat();
        let mut raw = ((8 + body.len()) as u32).to_le_bytes().to_vec();
        raw.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        raw.extend_from_slice(&body);
        raw
    }

    fn pd_info(vendor: &str, model: &str) -> Vec<u8> {
        let mut raw = vec![0u8; PD_INFO_LEN];
        raw[4 + 8..4 + 16].copy_from_slice(format!("{vendor:<8}").as_bytes());
        raw[4 + 16..4 + 32].copy_from_slice(format!("{model:<16}").as_bytes());
        raw
    }

    /// forge1: a PERC H730 Mini, enclosure 32 (`DP BP14G+`), four pass-through
    /// drives in bays 0-3, on device ids that are not the slots.
    fn forge1(node: &str) -> Shelves {
        let c = Controller { node, host: "host0", pcie: Some("0000:03:00.0") };
        into_table(c.read(&mut |op, mbox, buf| {
            let reply = match (op, mbox) {
                (DCMD_PD_LIST, None) => list(&[
                    entry(32, NO_ENCLOSURE, 0, SCSI_TYPE_ENCLOSURE, 0x500056b300000001),
                    entry(4, 32, 0, SCSI_TYPE_DISK, 0x5000cca000000004),
                    entry(5, 32, 1, SCSI_TYPE_DISK, 0x5000cca000000005),
                    entry(6, 32, 2, SCSI_TYPE_DISK, 0x5000cca000000006),
                    entry(7, 32, 3, SCSI_TYPE_DISK, 0x5000cca000000007),
                ]),
                (DCMD_PD_INFO, Some(32)) => pd_info("DP", "BP14G+"),
                other => return Err(format!("unexpected {other:?}")),
            };
            buf[..reply.len()].copy_from_slice(&reply);
            Ok(())
        }))
    }

    #[test]
    fn a_percs_backplane_is_one_internal_shelf_with_its_bays() {
        let shelves = forge1("forge1");
        assert_eq!(shelves.len(), 1);
        let r = shelves.get("500056b300000001").expect("keyed by the enclosure's SAS address");
        assert_eq!(r.shelf.position, Some(Position::Internal), "the owner's rule, and the engine's: no enclosure it can see");
        assert_eq!(r.shelf.vendor.as_deref(), Some("DP"));
        assert_eq!(r.shelf.model.as_deref(), Some("BP14G+"));
        assert_eq!(r.slots.len(), 4);
        assert_eq!(r.slots[&2], vec!["5000cca000000006".to_string()]);
        let t = r.through.as_ref().unwrap();
        assert_eq!((t.driver.as_str(), t.enclosure_device_id, t.error.as_ref()), ("megaraid_sas", Some(32), None));
        assert_eq!(r.worst(), crate::ses::ElementStatus::Unknown, "nothing here reports status: not OK");

        // Each drive by its SCSI address: device id, never the slot.
        for (target, bay) in [(4, 0), (5, 1), (6, 2), (7, 3)] {
            let (sh, b) = place(&shelves, "host0", (0, target)).unwrap();
            assert_eq!((sh.key().as_deref(), b), (Some("500056b300000001"), Some(bay)));
        }
        // A RAID volume on the same controller: its shelf, no bay.
        let (_, b) = place(&shelves, "host0", (2, 0)).unwrap();
        assert_eq!(b, None);
        // Another host's drive is not in it.
        assert_eq!(place(&shelves, "host1", (0, 4)), None);
    }

    #[test]
    fn a_controller_not_read_is_still_an_internal_shelf_and_says_why() {
        let c = Controller { node: "Forge1", host: "host2", pcie: Some("0000:03:00.0") };
        let shelves = into_table(c.read(&mut |_, _, _| Err("mknod /dev/megaraid_sas_ioctl_node: EPERM".into())));
        let r = shelves.values().next().unwrap();
        assert_eq!(r.key, "internal-forge1-0000-03-00.0", "per node, and fit to be an object name");
        assert_eq!(r.shelf.position, Some(Position::Internal));
        assert!(r.through.as_ref().unwrap().error.as_deref().unwrap().contains("EPERM"));
        let (sh, bay) = place(&shelves, "host2", (0, 9)).unwrap();
        assert_eq!((sh.key().as_deref(), bay), (Some("internal-forge1-0000-03-00.0"), None));
    }

    #[test]
    fn drives_in_no_enclosure_and_two_enclosures() {
        let c = Controller { node: "n", host: "host0", pcie: None };
        let shelves = into_table(c.read(&mut |op, _, buf| {
            if op != DCMD_PD_LIST {
                return Err("no info".into());
            }
            let reply = list(&[
                entry(1, NO_ENCLOSURE, 1, SCSI_TYPE_DISK, 0),
                entry(32, NO_ENCLOSURE, 0, SCSI_TYPE_ENCLOSURE, 0xa),
                entry(33, NO_ENCLOSURE, 0, SCSI_TYPE_ENCLOSURE, 0),
                entry(130, 33, 7, SCSI_TYPE_DISK, 0xb),
                entry(3, 33, 0xff, SCSI_TYPE_DISK, 0xc),
            ]);
            buf[..reply.len()].copy_from_slice(&reply);
            Ok(())
        }));
        assert_eq!(shelves.len(), 3, "no enclosure, an empty one with an address, one without");
        assert!(shelves.contains_key("000000000000000a"), "an enclosure with no drives is a shelf");
        assert_eq!(place(&shelves, "host0", (0, 1)).unwrap().1, Some(1));
        // Device 130 is channel 1, target 2.
        let (sh, bay) = place(&shelves, "host0", (1, 2)).unwrap();
        assert_eq!((sh.key().as_deref(), bay), (Some("internal-n-host0-e33"), Some(7)));
        // Slot 0xff: on its enclosure, no bay ("slot not matched").
        let (sh, bay) = place(&shelves, "host0", (0, 3)).unwrap();
        assert_eq!((sh.key().as_deref(), bay), (Some("internal-n-host0-e33"), None), "slot 0xff is no bay, still e33");
        assert_eq!(shelves["internal-n-host0-e33"].through.as_ref().unwrap().slot_not_matched, vec![3]);
        // Two enclosures: a drive in none could be in either; it keeps the
        // controller's own shelf rather than a guess.
        assert!(shelves.contains_key("internal-n-host0"));
        // Three shelves: a device nobody lists is on none of them for certain.
        assert_eq!(place(&shelves, "host0", (0, 9)), None);
    }

    /// forge1 as it is (#122, 0.39.0 on 2026-11-07): the PERC on host1
    /// (0000:c1:00.0) lists enclosure 32 (`DP BP14G+`, 5e09406039f0cf00) and
    /// four drives on device ids = slots 0, 3, 4, 7; sda and sdb in enclosure
    /// 32, sdc and sdd in enclosure 0xffff. One backplane: one shelf.
    fn forge1_live(slot_of_sdd: u8) -> Shelves {
        let c = Controller { node: "storm-94a281", host: "host1", pcie: Some("0000:c1:00.0") };
        into_table(c.read(&mut |op, mbox, buf| {
            let reply = match (op, mbox) {
                (DCMD_PD_LIST, None) => list(&[
                    entry(32, NO_ENCLOSURE, 0, SCSI_TYPE_ENCLOSURE, 0x5e09406039f0cf00),
                    entry(0, 32, 0, SCSI_TYPE_DISK, 0x5000cca2680ab8d9),
                    entry(3, 32, 3, SCSI_TYPE_DISK, 0x5000cca268077301),
                    entry(4, NO_ENCLOSURE, 4, SCSI_TYPE_DISK, 0x5000cca26801b649),
                    entry(7, NO_ENCLOSURE, slot_of_sdd, SCSI_TYPE_DISK, 0x5000cca26a819e69),
                ]),
                (DCMD_PD_INFO, Some(32)) => pd_info("DP", "BP14G+"),
                other => return Err(format!("unexpected {other:?}")),
            };
            buf[..reply.len()].copy_from_slice(&reply);
            Ok(())
        }))
    }

    #[test]
    fn forge1s_four_drives_are_one_bp14g_shelf_bays_0_3_4_7() {
        let shelves = forge1_live(7);
        assert_eq!(shelves.keys().collect::<Vec<_>>(), vec!["5e09406039f0cf00"], "one shelf, not an HBA-only second one");
        let r = &shelves["5e09406039f0cf00"];
        assert_eq!((r.shelf.vendor.as_deref(), r.shelf.model.as_deref()), (Some("DP"), Some("BP14G+")));
        assert_eq!(r.shelf.id.as_deref(), Some("internal-storm-94a281-0000-c1-00.0-e32"));
        assert_eq!(r.slots.keys().copied().collect::<Vec<_>>(), vec![0, 3, 4, 7]);
        assert_eq!(r.slots[&4], vec!["5000cca26801b649".to_string()]);
        let t = r.through.as_ref().unwrap();
        assert_eq!(t.no_enclosure_devices, vec![4, 7], "said, not hidden");
        assert!(t.slot_not_matched.is_empty());
        for (name, target, bay) in [("sda", 0, 0), ("sdb", 3, 3), ("sdc", 4, 4), ("sdd", 7, 7)] {
            let (sh, b) = place(&shelves, "host1", (0, target)).unwrap();
            assert_eq!((sh.key().as_deref(), b), (Some("5e09406039f0cf00"), Some(bay)), "{name}");
        }
    }

    #[test]
    fn a_drive_with_no_slot_stays_on_the_enclosure_slot_not_matched() {
        let shelves = forge1_live(0xff);
        assert_eq!(shelves.len(), 1);
        let r = &shelves["5e09406039f0cf00"];
        assert_eq!(r.through.as_ref().unwrap().slot_not_matched, vec![7]);
        assert_eq!(r.slots.keys().copied().collect::<Vec<_>>(), vec![0, 3, 4]);
        let (sh, b) = place(&shelves, "host1", (0, 7)).unwrap();
        assert_eq!((sh.key().as_deref(), b), (Some("5e09406039f0cf00"), None));
    }

    #[test]
    fn frames_and_addresses() {
        let f = dcmd_frame(DCMD_PD_INFO, Some(0x20), 512);
        assert_eq!(f[0], 0x05, "MFI_CMD_DCMD");
        assert_eq!(f[7], 1, "one SGE");
        assert_eq!(u16::from_le_bytes([f[0x10], f[0x11]]), 0x10, "read");
        assert_eq!(u32::from_le_bytes([f[0x14], f[0x15], f[0x16], f[0x17]]), 512);
        assert_eq!(u32::from_le_bytes([f[0x18], f[0x19], f[0x1a], f[0x1b]]), 0x0202_0000);
        assert_eq!(u16::from_le_bytes([f[0x1c], f[0x1d]]), 0x20, "the device id in the mailbox");
        assert_eq!(u32::from_le_bytes([f[0x2c], f[0x2d], f[0x2e], f[0x2f]]), 512);

        let p = std::path::Path::new("/sys/devices/pci0000:00/0000:00:01.1/0000:03:00.0/host0/target0:2:5/0:2:5:0/block/sda");
        assert_eq!(scsi_address(p), Some((2, 5)));
        assert_eq!(scsi_address(std::path::Path::new("/sys/devices/pci0000:00/nvme/nvme0/nvme0n1")), None);
        assert_eq!(device_id(0, 4), Some(4));
        assert_eq!(device_id(1, 2), Some(130));
        assert_eq!(device_id(2, 0), None, "a logical drive");

        assert_eq!(major_of("Character devices:\n  1 mem\n241 megaraid_sas_ioctl\n\nBlock devices:\n  8 sd\n"), Some(241));
        assert_eq!(major_of("Character devices:\n  1 mem\n"), None);
        assert_eq!(parse_pd_list(&[0; 4]), None, "short");
    }

    /// The whole path on a megaraid-shaped sysfs tree: no sas_address, no
    /// enclosure class, the drive on channel 0 of the controller's host.
    #[cfg(target_os = "linux")]
    #[test]
    fn locate_gives_a_megaraid_drive_its_shelf_position_and_bay() {
        let sys = std::env::temp_dir().join(format!("stormdrive-megaraid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sys);
        let host = sys.join("devices/pci0000:00/0000:00:01.1/0000:03:00.0/host0");
        let disk = host.join("target0:0:6/0:0:6:0/block/sdc");
        std::fs::create_dir_all(&disk).unwrap();
        std::fs::create_dir_all(sys.join("block")).unwrap();
        std::os::unix::fs::symlink(&disk, sys.join("block/sdc")).unwrap();
        std::fs::create_dir_all(sys.join("class/scsi_host")).unwrap();
        std::os::unix::fs::symlink(&host, sys.join("class/scsi_host/host0")).unwrap();
        std::fs::write(host.join("proc_name"), "megaraid_sas\n").unwrap();
        assert_eq!(linux::hosts(&sys), vec![("host0".to_string(), Some("0000:03:00.0".to_string()))]);

        let loc = crate::topology::locate_in(&sys, "sdc", &forge1("forge1"));
        assert_eq!(loc.shelf.as_ref().and_then(|s| s.key()).as_deref(), Some("500056b300000001"));
        assert_eq!(loc.bay, Some(2));
        assert_eq!(loc.position, Some(Position::Internal));
        assert_eq!(loc.labels().iter().find(|(k, _)| k == "bay").map(|(_, v)| v.as_str()), Some("2"));

        // Unread controller's absence: the drive is still placed as internal.
        let alone = crate::topology::locate_in(&sys, "sdc", &Shelves::new());
        assert_eq!((alone.shelf, alone.position), (None, Some(Position::Internal)));
        let _ = std::fs::remove_dir_all(&sys);
    }
}
