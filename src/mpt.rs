//! The SAS card's own interface (#166): mpt2sas/mpt3sas through the driver's
//! control device, `/dev/mpt3ctl` (SAS3) or `/dev/mpt2ctl` (SAS2).
//!
//! Owner, 2026-11-19 (compliance-ops#252, A): no Broadcom binaries in the
//! release; storcli's inventory and sas3flash's flash are ours. Both are
//! front ends to the same three ioctls (`mpt3sas_ctl.h`):
//! - `MPT3IOCINFO`: what the driver knows of the IOC (adapter type, PCI ids,
//!   firmware and BIOS versions as the driver read them at its last reset);
//! - `MPT3COMMAND`: one MPI2 request frame to the IOC, with data in and/or
//!   out: IOC Facts, Config (every page this module reads), FW Upload (an
//!   image read back from the card's flash) and FW Download (an image
//!   written to it);
//! - `MPT3HARDRESET`: the driver's own host reset (diag reset), which is how
//!   a downloaded firmware starts running. The driver blocks the SCSI host,
//!   lets outstanding commands finish or retries them, resets the IOC and
//!   brings the host back: the same pause a link reset gives a drive.
//!
//! The config-page path is stormblock's (`src/drive/mpt.rs`, #611/#642, read
//! on forge1's HBA330). Layouts are Linux's `drivers/scsi/mpt3sas/mpi/`
//! (mpi2.h, mpi2_ioc.h, mpi2_cnfg.h, mpi2_image.h).
//!
//! Every request goes through a [`Port`]; only [`Ctl`] is the device. The
//! frames, parsers and the flows over them (inventory, upload, download)
//! are portable and run against a fake card in the tests.

use std::io;

// ---- MPI2 constants ----

const FUNCTION_IOC_FACTS: u8 = 0x03;
const FUNCTION_CONFIG: u8 = 0x04;
const FUNCTION_FW_DOWNLOAD: u8 = 0x09;
const FUNCTION_FW_UPLOAD: u8 = 0x12;

const ACTION_PAGE_HEADER: u8 = 0x00;
const ACTION_READ_CURRENT: u8 = 0x01;

pub const PAGETYPE_IO_UNIT: u8 = 0x00;
pub const PAGETYPE_BIOS: u8 = 0x02;
pub const PAGETYPE_MANUFACTURING: u8 = 0x09;
const PAGETYPE_EXTENDED: u8 = 0x0F;
pub const EXTPAGE_SAS_IO_UNIT: u8 = 0x10;
pub const EXTPAGE_SAS_DEVICE: u8 = 0x12;
pub const EXTPAGE_ENCLOSURE: u8 = 0x15;

const IOCSTATUS_MASK: u16 = 0x7FFF;
pub const IOCSTATUS_SUCCESS: u16 = 0x0000;
pub const IOCSTATUS_INVALID_FUNCTION: u16 = 0x0001;
pub const IOCSTATUS_INVALID_FIELD: u16 = 0x0007;
pub const IOCSTATUS_CONFIG_INVALID_PAGE: u16 = 0x0022;

const FW_DOWNLOAD_LAST_SEGMENT: u8 = 0x01;

/// The config request's SGE offset (`PageBufferSGE`, 0x1C), in dwords.
pub const CONFIG_SGE_DWORDS: u32 = 0x1C / 4;
/// IOC Facts has no data: the frame is 0x0C bytes.
pub const FACTS_DWORDS: u32 = 0x0C / 4;

/// The `MPT3IOCINFO` adapter types that speak MPI 2.5 or later (SAS3,
/// SAS3.5): their FW Upload/Download frames carry offset and size in the
/// frame (`MPI25_FW_*_REQUEST`, SGL at 0x20); MPI 2.0 (SAS2) carries them
/// in a transaction-context element before the SGL.
pub const ADAPTER_SAS3: u32 = 0x06;
pub const ADAPTER_SAS35: u32 = 0x07;

/// A transfer chunk for upload and download: the driver allocates each
/// ioctl's data as one coherent DMA buffer, so it stays small.
pub const CHUNK: usize = 64 * 1024;

pub fn le16(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]])).unwrap_or(0)
}
pub fn le32(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).unwrap_or(0)
}
pub fn le64(b: &[u8], o: usize) -> u64 {
    ((le32(b, o + 4) as u64) << 32) | (le32(b, o) as u64)
}

fn text(b: &[u8], r: std::ops::Range<usize>) -> Option<String> {
    let s = String::from_utf8_lossy(b.get(r)?).trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string();
    (!s.is_empty()).then_some(s)
}

/// An MPI version word (`MPI2_VERSION_UNION`: Dev, Unit, Minor, Major from
/// the low byte), as the driver prints it: `16.00.01.00`.
pub fn version(w: u32) -> String {
    format!("{:02}.{:02}.{:02}.{:02}", w >> 24, (w >> 16) & 0xFF, (w >> 8) & 0xFF, w & 0xFF)
}

/// IOCStatus in the spec's words.
pub fn ioc_status_name(s: u16) -> String {
    match s {
        0x0000 => "success".into(),
        0x0001 => "invalid function".into(),
        0x0002 => "busy".into(),
        0x0003 => "invalid SGL".into(),
        0x0004 => "internal error".into(),
        0x0006 => "insufficient resources".into(),
        0x0007 => "invalid field".into(),
        0x0008 => "invalid state".into(),
        0x0020 => "config: invalid action".into(),
        0x0021 => "config: invalid type".into(),
        0x0022 => "config: invalid page".into(),
        0x0023 => "config: invalid data".into(),
        0x0024 => "config: no defaults".into(),
        0x0025 => "config: can't commit".into(),
        s => format!("IOCStatus 0x{s:04x}"),
    }
}

// ---- the port ----

/// `MPT3IOCINFO`: the driver's view of one IOC.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IocInfo {
    pub adapter_type: u32,
    pub port_number: u32,
    pub pci_device: u32,
    pub hw_rev: u32,
    pub subsystem_device: u32,
    pub subsystem_vendor: u32,
    /// The IOC Facts firmware version the driver read at its last reset.
    pub firmware_version: u32,
    /// BIOS Page 3's version the driver read.
    pub bios_version: u32,
    pub driver_version: String,
    pub pci_bus: u32,
    pub pci_device_number: u32,
    pub pci_function: u32,
    pub pci_segment: u32,
}

impl IocInfo {
    /// MPI 2.5 frames (SAS3 and later).
    pub fn mpi25(&self) -> bool {
        matches!(self.adapter_type, ADAPTER_SAS3 | ADAPTER_SAS35)
    }
    pub fn adapter(&self) -> &'static str {
        match self.adapter_type {
            0x04 => "SAS2",
            0x05 => "SAS2 (SSS6200)",
            ADAPTER_SAS3 => "SAS3",
            ADAPTER_SAS35 => "SAS3.5",
            _ => "unknown",
        }
    }
    /// `DDDD:BB:DD.F`.
    pub fn bdf(&self) -> String {
        format!("{:04x}:{:02x}:{:02x}.{:x}", self.pci_segment, self.pci_bus, self.pci_device_number, self.pci_function)
    }
}

/// The bytes after the 12-byte header of `struct mpt3_ioctl_iocinfo`
/// (92 bytes in all) -> its fields.
pub fn parse_iocinfo(b: &[u8]) -> IocInfo {
    let pci = le32(b, 84);
    IocInfo {
        adapter_type: le32(b, 12),
        port_number: le32(b, 16),
        pci_device: le32(b, 20),
        hw_rev: le32(b, 24),
        subsystem_device: le32(b, 28),
        subsystem_vendor: le32(b, 32),
        firmware_version: le32(b, 40),
        bios_version: le32(b, 44),
        driver_version: text(b, 48..80).unwrap_or_default(),
        pci_device_number: pci & 0x1F,
        pci_function: (pci >> 5) & 0x7,
        pci_bus: pci >> 8,
        pci_segment: le32(b, 88),
    }
}

/// What a card is asked through: the driver's control device, or a fake.
pub trait Port: Send + Sync {
    /// One request frame (`sge_dwords` dwords of it are copied) with data
    /// in and/or out. -> the reply frame (all zero: a context reply, a
    /// success with no reply frame).
    fn command(&self, ioc: u32, frame: &[u8], sge_dwords: u32, data_in: &mut [u8], data_out: &[u8], timeout_s: u32) -> io::Result<Vec<u8>>;
    /// `MPT3IOCINFO`.
    fn info(&self, ioc: u32) -> io::Result<IocInfo>;
    /// `MPT3HARDRESET`: the driver's host reset of the IOC.
    fn hard_reset(&self, ioc: u32) -> io::Result<()>;
    /// What to call it in a message (`mpt3ctl`).
    fn name(&self) -> String;
}

/// A reply's IOCStatus (the log-info bit masked off).
pub fn reply_status(r: &[u8]) -> u16 {
    le16(r, 0x0E) & IOCSTATUS_MASK
}

// ---- IOC Facts ----

pub fn facts_request() -> Vec<u8> {
    let mut f = vec![0u8; (FACTS_DWORDS * 4) as usize];
    f[0x03] = FUNCTION_IOC_FACTS;
    f
}

/// IOC Facts (`MPI2_IOC_FACTS_REPLY`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    pub msg_version: u16,
    pub header_version: u16,
    pub ioc_number: u8,
    pub ioc_exceptions: u16,
    pub number_of_ports: u8,
    pub request_credit: u16,
    pub product_id: u16,
    pub capabilities: u32,
    pub fw_version: u32,
    pub max_initiators: u16,
    pub max_targets: u16,
    pub max_expanders: u16,
    pub max_enclosures: u16,
    pub protocol_flags: u16,
    pub max_volumes: u8,
    pub max_dev_handle: u16,
}

pub fn parse_facts(r: &[u8]) -> Facts {
    Facts {
        msg_version: le16(r, 0x00),
        header_version: le16(r, 0x04),
        ioc_number: r.get(0x06).copied().unwrap_or(0),
        ioc_exceptions: le16(r, 0x0C),
        number_of_ports: r.get(0x16).copied().unwrap_or(0),
        request_credit: le16(r, 0x18),
        product_id: le16(r, 0x1A),
        capabilities: le32(r, 0x1C),
        fw_version: le32(r, 0x20),
        max_initiators: le16(r, 0x28),
        max_targets: le16(r, 0x2A),
        max_expanders: le16(r, 0x2C),
        max_enclosures: le16(r, 0x2E),
        protocol_flags: le16(r, 0x30),
        max_volumes: r.get(0x37).copied().unwrap_or(0),
        max_dev_handle: le16(r, 0x38),
    }
}

impl Facts {
    /// `IOCExceptions` in words (mpi2_ioc.h), empty when none.
    pub fn exceptions(&self) -> Vec<&'static str> {
        let e = self.ioc_exceptions;
        let mut v = Vec::new();
        for (bit, what) in [
            (0x0400, "PCIe disabled"),
            (0x0200, "partial memory failure"),
            (0x0100, "IR foreign config max"),
            (0x0010, "metadata unsupported"),
            (0x0008, "manufacturing checksum failed"),
            (0x0004, "firmware checksum failed"),
            (0x0002, "RAID config invalid"),
            (0x0001, "config checksum failed"),
        ] {
            if e & bit != 0 {
                v.push(what);
            }
        }
        match e & 0x00E0 {
            0x0020 => v.push("booted from the backup image"),
            0x0040 => v.push("booted from a restored image"),
            0x0060 => v.push("backup image corrupt"),
            _ => {}
        }
        v
    }
    pub fn mpi_version(&self) -> String {
        format!("{}.{}", self.msg_version >> 8, self.msg_version & 0xFF)
    }
}

pub fn ioc_facts(p: &dyn Port, ioc: u32) -> Result<Facts, String> {
    let r = p.command(ioc, &facts_request(), FACTS_DWORDS, &mut [], &[], 10).map_err(|e| format!("IOC Facts: {e}"))?;
    match reply_status(&r) {
        IOCSTATUS_SUCCESS if r.iter().any(|b| *b != 0) => Ok(parse_facts(&r)),
        IOCSTATUS_SUCCESS => Err("IOC Facts: no reply frame".into()),
        s => Err(format!("IOC Facts: {}", ioc_status_name(s))),
    }
}

// ---- config pages ----

/// A config request (`MPI2_CONFIG_REQUEST`, 0x1C bytes before its SGE).
/// `page_type` is a standard type, or [`PAGETYPE_EXTENDED`] with `ext_type`.
/// `version` and the lengths are 0 for the header action and what the
/// header said for a read.
#[allow(clippy::too_many_arguments)]
pub fn config_request(action: u8, page_type: u8, ext_type: u8, page: u8, address: u32, version: u8, length: u8, ext_len: u16) -> Vec<u8> {
    let mut f = vec![0u8; (CONFIG_SGE_DWORDS * 4) as usize];
    f[0x00] = action;
    f[0x03] = FUNCTION_CONFIG;
    f[0x04..0x06].copy_from_slice(&ext_len.to_le_bytes());
    f[0x06] = ext_type;
    f[0x14] = version;
    f[0x15] = length;
    f[0x16] = page;
    f[0x17] = page_type;
    f[0x18..0x1C].copy_from_slice(&address.to_le_bytes());
    f
}

/// One config page, standard (`ext_type` 0) or extended. None when the card
/// has no such page (the end of a GET_NEXT_HANDLE walk, a page this card
/// does not keep).
pub fn page(p: &dyn Port, ioc: u32, page_type: u8, ext_type: u8, number: u8, address: u32) -> Result<Option<Vec<u8>>, String> {
    let name = || if page_type == PAGETYPE_EXTENDED { format!("config page 0x{ext_type:02x}/{number}") } else { format!("config page {page_type}/{number}") };
    let f = config_request(ACTION_PAGE_HEADER, page_type, ext_type, number, address, 0, 0, 0);
    let r = p.command(ioc, &f, CONFIG_SGE_DWORDS, &mut [], &[], 10).map_err(|e| format!("{} header: {e}", name()))?;
    let st = reply_status(&r);
    if st == IOCSTATUS_CONFIG_INVALID_PAGE {
        return Ok(None);
    }
    let (ver, len, ext_len) = (r.get(0x14).copied().unwrap_or(0), r.get(0x15).copied().unwrap_or(0), le16(&r, 0x04));
    let bytes = if page_type == PAGETYPE_EXTENDED { ext_len as usize * 4 } else { len as usize * 4 };
    if st != IOCSTATUS_SUCCESS || bytes == 0 {
        return Err(format!("{} header: {}", name(), ioc_status_name(st)));
    }
    let mut buf = vec![0u8; bytes];
    let f = config_request(ACTION_READ_CURRENT, page_type, ext_type, number, address, ver, len, ext_len);
    let r = p.command(ioc, &f, CONFIG_SGE_DWORDS, &mut buf, &[], 10).map_err(|e| format!("{} read: {e}", name()))?;
    match reply_status(&r) {
        IOCSTATUS_CONFIG_INVALID_PAGE => Ok(None),
        IOCSTATUS_SUCCESS => Ok(Some(buf)),
        s => Err(format!("{} read: {}", name(), ioc_status_name(s))),
    }
}

pub fn ext_page(p: &dyn Port, ioc: u32, ext_type: u8, number: u8, address: u32) -> Result<Option<Vec<u8>>, String> {
    page(p, ioc, PAGETYPE_EXTENDED, ext_type, number, address)
}

/// Every page of a GET_NEXT_HANDLE walk (SAS Device 0, Enclosure 0); the
/// page's own handle is at `handle_at`.
pub fn walk(p: &dyn Port, ioc: u32, ext_type: u8, handle_at: usize) -> Result<Vec<Vec<u8>>, String> {
    let mut v = Vec::new();
    let mut h: u32 = 0xFFFF;
    for _ in 0..1024 {
        let Some(pg) = ext_page(p, ioc, ext_type, 0, h)? else { break };
        let next = le16(&pg, handle_at) as u32;
        v.push(pg);
        if next == h || next == 0xFFFF {
            break;
        }
        h = next;
    }
    Ok(v)
}

/// Manufacturing Page 0: the board.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Man0 {
    pub chip_name: Option<String>,
    pub chip_revision: Option<String>,
    pub board_name: Option<String>,
    pub board_assembly: Option<String>,
    pub board_tracer: Option<String>,
}

pub fn parse_man0(p: &[u8]) -> Man0 {
    Man0 {
        chip_name: text(p, 0x04..0x14),
        chip_revision: text(p, 0x14..0x1C),
        board_name: text(p, 0x1C..0x2C),
        board_assembly: text(p, 0x2C..0x3C),
        board_tracer: text(p, 0x3C..0x4C),
    }
}

/// IO Unit Page 0: the NVDATA versions (default, persistent). `U64` is
/// 4-aligned in MPI (mpi2_type.h), so they sit at 0x0C and 0x10.
pub fn parse_io_unit0(p: &[u8]) -> (u32, u32) {
    (le32(p, 0x0C), le32(p, 0x10))
}

/// BIOS Page 3's `BiosVersion`.
pub fn parse_bios3(p: &[u8]) -> u32 {
    le32(p, 0x08)
}

/// A rate code (the nibble MPI uses for negotiated, max and min rates).
pub fn rate_mbps(code: u8) -> Option<u32> {
    match code {
        0x8 => Some(1500),
        0x9 => Some(3000),
        0xA => Some(6000),
        0xB => Some(12000),
        0xC => Some(22500),
        _ => None,
    }
}

pub fn rate_name(code: u8) -> &'static str {
    match code {
        0x0 => "unknown",
        0x1 => "disabled",
        0x2 => "negotiation failed",
        0x3 => "SATA OOB complete",
        0x4 => "port selector",
        0x5 => "SMP reset in progress",
        0x6 => "unsupported phy",
        0x8 => "1.5 Gbit",
        0x9 => "3.0 Gbit",
        0xA => "6.0 Gbit",
        0xB => "12.0 Gbit",
        0xC => "22.5 Gbit",
        _ => "?",
    }
}

/// One phy of the card: SAS IO Unit Page 0 (negotiated, attached) joined to
/// Page 1 (what the card is set to: max/min rate, disabled).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Phy {
    pub num: u8,
    pub port: u8,
    /// The negotiated logical rate code (high nibble of NegotiatedLinkRate).
    pub rate: u8,
    pub attached: u16,
    /// SAS IO Unit Page 1's MaxMinLinkRate nibbles, when it was read.
    pub max_rate: Option<u8>,
    pub min_rate: Option<u8>,
    pub disabled: bool,
}

impl Phy {
    pub fn linked(&self) -> bool {
        (0x8..=0xC).contains(&self.rate)
    }
}

/// SAS IO Unit Page 0 -> its phys (`PhyData` at 0x10, 0x14 bytes each).
pub fn parse_sas_io_unit0(p: &[u8]) -> Vec<Phy> {
    let n = p.get(0x0C).copied().unwrap_or(0) as usize;
    (0..n)
        .filter_map(|i| {
            let o = 0x10 + i * 0x14;
            p.get(o..o + 0x14).map(|d| Phy { num: i as u8, port: d[0], rate: d[3] >> 4, attached: le16(d, 0x08), ..Default::default() })
        })
        .collect()
}

/// SAS IO Unit Page 1 -> per phy (max, min, disabled) (`PhyData` at 0x14,
/// 0x0C bytes each).
pub fn parse_sas_io_unit1(p: &[u8]) -> Vec<(u8, u8, bool)> {
    let n = p.get(0x10).copied().unwrap_or(0) as usize;
    (0..n)
        .filter_map(|i| {
            let o = 0x14 + i * 0x0C;
            p.get(o..o + 0x0C).map(|d| (d[3] >> 4, d[3] & 0x0F, d[2] & 0x08 != 0))
        })
        .collect()
}

/// One device the card lists (SAS Device Page 0).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Device {
    pub handle: u16,
    pub sas_address: u64,
    pub enclosure: u16,
    pub slot: u16,
    pub parent: u16,
    pub phy: u8,
    pub access_status: u8,
    pub info: u32,
    pub flags: u16,
}

const DI_TYPE_MASK: u32 = 0x7;
const DI_END_DEVICE: u32 = 0x1;
const DI_SATA_DEVICE: u32 = 0x80;
const DI_STP_TARGET: u32 = 0x200;
const DI_SSP_TARGET: u32 = 0x400;
const DI_SEP: u32 = 0x4000;

impl Device {
    pub fn is_expander(&self) -> bool {
        matches!(self.info & DI_TYPE_MASK, 2 | 3)
    }
    pub fn present(&self) -> bool {
        self.flags & 0x0001 != 0
    }
    pub fn kind(&self) -> &'static str {
        if self.is_expander() {
            "expander"
        } else if self.info & DI_SEP != 0 {
            "enclosure processor"
        } else if self.info & DI_SSP_TARGET != 0 {
            "SAS drive"
        } else if self.info & (DI_SATA_DEVICE | DI_STP_TARGET) != 0 {
            "SATA drive"
        } else if self.info & DI_TYPE_MASK == DI_END_DEVICE {
            "end device"
        } else {
            "unknown"
        }
    }
    /// `AccessStatus` in the card's words, None when 0.
    pub fn access(&self) -> Option<String> {
        Some(
            match self.access_status {
                0x00 => return None,
                0x01 => "SATA needs initialization",
                0x02 => "SATA capability failed",
                0x03 => "SATA affiliation conflict",
                0x04 => "route not addressable",
                0x05 => "SMP error, not addressable",
                0x06 => "device blocked",
                0x10..=0x1F => "SATA initialization failed",
                _ => return Some(format!("access status 0x{:02x}", self.access_status)),
            }
            .to_string(),
        )
    }
}

pub fn parse_sas_device0(p: &[u8]) -> Device {
    Device {
        slot: le16(p, 0x08),
        enclosure: le16(p, 0x0A),
        sas_address: le64(p, 0x0C),
        parent: le16(p, 0x14),
        phy: p.get(0x16).copied().unwrap_or(0),
        access_status: p.get(0x17).copied().unwrap_or(0),
        handle: le16(p, 0x18),
        info: le32(p, 0x1C),
        flags: le16(p, 0x20),
    }
}

/// An enclosure the card knows (Enclosure Page 0).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Enclosure {
    pub handle: u16,
    pub logical_id: u64,
    pub num_slots: u16,
    pub start_slot: u16,
}

pub fn parse_enclosure0(p: &[u8]) -> Enclosure {
    Enclosure { logical_id: le64(p, 0x0C), handle: le16(p, 0x16), num_slots: le16(p, 0x18), start_slot: le16(p, 0x1A) }
}

// ---- firmware images ----

/// A flash region of the card, by its FW Upload / FW Download image types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Region {
    /// The IOC firmware (upload FW_FLASH 0x01, download FW 0x01).
    Firmware,
    /// The option ROM: the legacy BIOS and UEFI driver images (0x02).
    Bios,
    /// The backup firmware image (upload only, 0x05).
    FirmwareBackup,
    /// Manufacturing pages (0x06).
    Manufacturing,
}

impl Region {
    pub const ALL: [Region; 4] = [Region::Firmware, Region::Bios, Region::FirmwareBackup, Region::Manufacturing];
    pub fn upload_type(self) -> u8 {
        match self {
            Region::Firmware => 0x01,
            Region::Bios => 0x02,
            Region::FirmwareBackup => 0x05,
            Region::Manufacturing => 0x06,
        }
    }
    /// The download image type, for the regions a flash writes.
    pub fn download_type(self) -> Option<u8> {
        match self {
            Region::Firmware => Some(0x01),
            Region::Bios => Some(0x02),
            Region::FirmwareBackup | Region::Manufacturing => None,
        }
    }
    /// In a sentence: `firmware`, `BIOS`.
    pub fn label(self) -> &'static str {
        match self {
            Region::Firmware => "firmware",
            Region::Bios => "BIOS",
            Region::FirmwareBackup => "backup firmware",
            Region::Manufacturing => "manufacturing",
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Region::Firmware => "firmware",
            Region::Bios => "bios",
            Region::FirmwareBackup => "firmware_backup",
            Region::Manufacturing => "manufacturing",
        }
    }
}

/// An FW Upload or FW Download frame for one chunk. MPI 2.5: offset and
/// size in the frame, the SGL at 0x20. MPI 2.0: a transaction-context
/// element at 0x14 carries them, the SGL after it at 0x24. -> the frame and
/// its SGE offset in dwords.
pub fn transfer_request(download: bool, mpi25: bool, image_type: u8, total: u32, offset: u32, size: u32, last: bool) -> (Vec<u8>, u32) {
    let len = if mpi25 { 0x20 } else { 0x24 };
    let mut f = vec![0u8; len];
    f[0x00] = image_type;
    f[0x03] = if download { FUNCTION_FW_DOWNLOAD } else { FUNCTION_FW_UPLOAD };
    if download {
        if last {
            f[0x07] = FW_DOWNLOAD_LAST_SEGMENT;
        }
        f[0x0C..0x10].copy_from_slice(&total.to_le_bytes());
    }
    if mpi25 {
        f[0x18..0x1C].copy_from_slice(&offset.to_le_bytes());
        f[0x1C..0x20].copy_from_slice(&size.to_le_bytes());
    } else {
        // MPI2_FW_*_TCSGE: ContextSize 0, DetailsLength 12, Flags 0
        // (a transaction element), then ImageOffset, ImageSize.
        f[0x16] = 12;
        f[0x1C..0x20].copy_from_slice(&offset.to_le_bytes());
        f[0x20..0x24].copy_from_slice(&size.to_le_bytes());
    }
    (f, (len / 4) as u32)
}

/// A region read back from the card's flash. None when the card has no such
/// region (it answers the upload "invalid function/field", or 0 bytes).
pub fn upload(p: &dyn Port, ioc: u32, mpi25: bool, region: Region, progress: &dyn Fn(u64, u64)) -> Result<Option<Vec<u8>>, String> {
    let ty = region.upload_type();
    let mut out: Vec<u8> = Vec::new();
    let mut total: Option<usize> = None;
    for _ in 0..4096 {
        let offset = out.len();
        let want = match total {
            Some(t) => (t - offset).min(CHUNK),
            None => CHUNK,
        };
        let mut buf = vec![0u8; want];
        let (f, sge) = transfer_request(false, mpi25, ty, 0, offset as u32, want as u32, false);
        let r = p.command(ioc, &f, sge, &mut buf, &[], 60).map_err(|e| format!("upload {}: {e}", region.as_str()))?;
        let st = reply_status(&r);
        if matches!(st, IOCSTATUS_INVALID_FUNCTION | IOCSTATUS_INVALID_FIELD) && offset == 0 {
            return Ok(None);
        }
        if st != IOCSTATUS_SUCCESS {
            return Err(format!("upload {} at {offset}: {}", region.as_str(), ioc_status_name(st)));
        }
        let actual = le32(&r, 0x14) as usize;
        if total.is_none() {
            if actual == 0 {
                return Ok(None);
            }
            total = Some(actual);
        }
        let t = total.unwrap_or(0);
        let got = want.min(t.saturating_sub(offset));
        out.extend_from_slice(&buf[..got]);
        progress(out.len() as u64, t as u64);
        if out.len() >= t || got == 0 {
            break;
        }
    }
    Ok(Some(out))
}

/// Writes an image to a region of the card's flash, in [`CHUNK`]s, the last
/// one marked. The card does not run it until it is reset.
pub fn download(p: &dyn Port, ioc: u32, mpi25: bool, region: Region, image: &[u8], progress: &dyn Fn(u64, u64)) -> Result<(), String> {
    let ty = region.download_type().ok_or_else(|| format!("the {} region is not written by a flash", region.as_str()))?;
    if image.is_empty() {
        return Err("an empty image".into());
    }
    let total = image.len();
    let mut offset = 0usize;
    while offset < total {
        let n = (total - offset).min(CHUNK);
        let last = offset + n == total;
        let (f, sge) = transfer_request(true, mpi25, ty, total as u32, offset as u32, n as u32, last);
        let r = p.command(ioc, &f, sge, &mut [], &image[offset..offset + n], 120).map_err(|e| format!("download {} at {offset}: {e}", region.as_str()))?;
        let st = reply_status(&r);
        if st != IOCSTATUS_SUCCESS {
            return Err(format!("download {} at {offset} of {total}: the card said {} (log info 0x{:08x})", region.as_str(), ioc_status_name(st), le32(&r, 0x10)));
        }
        offset += n;
        progress(offset as u64, total as u64);
    }
    Ok(())
}

/// An MPI2 / MPI2.6 firmware image's header (`MPI2_FW_IMAGE_HEADER`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FwHeader {
    pub fw_version: u32,
    pub nvdata_version: u32,
    pub package_version: u32,
    pub vendor_id: u16,
    pub product_id: u16,
    pub image_size: u32,
    pub name: Option<String>,
}

pub fn parse_fw_header(b: &[u8]) -> Option<FwHeader> {
    let sig = le32(b, 0x00) & 0xFF00_0000;
    let (s0, s1, s2) = (le32(b, 0x04), le32(b, 0x08), le32(b, 0x0C));
    let mpi2 = sig == 0xEA00_0000 && s0 == 0x5AFA_A55A && s1 == 0xA55A_FAA5 && s2 == 0x5AA5_5AFA;
    let mpi26 = sig == 0xEB00_0000 && s0 & 0xFFFF_FF00 == 0x5AEA_A500 && s1 == 0xA55A_EAA5 && s2 == 0x5AA5_5AEA;
    if !(mpi2 || mpi26) || b.len() < 0x88 {
        return None;
    }
    Some(FwHeader {
        fw_version: le32(b, 0x14),
        nvdata_version: le32(b, 0x18),
        package_version: le32(b, 0x1C),
        vendor_id: le16(b, 0x20),
        product_id: le16(b, 0x22),
        image_size: le32(b, 0x2C),
        name: text(b, 0x68..0x88),
    })
}

/// One image of a PCI option ROM (the BIOS region): a legacy x86 BIOS or a
/// UEFI driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RomImage {
    pub offset: usize,
    pub len: usize,
    /// PCIR code type: 0 x86 BIOS, 3 UEFI.
    pub code_type: u8,
    pub vendor_id: u16,
    pub device_id: u16,
}

impl RomImage {
    pub fn kind(&self) -> &'static str {
        match self.code_type {
            0 => "legacy_bios",
            1 => "open_firmware",
            2 => "hp_pa_risc",
            3 => "uefi",
            _ => "unknown",
        }
    }
}

/// The images chained in an option ROM (`55 AA`, the PCIR structure, each
/// image's length in 512-byte units, the last-image bit).
pub fn parse_rom(b: &[u8]) -> Vec<RomImage> {
    let mut v = Vec::new();
    let mut off = 0usize;
    while off + 0x1A <= b.len() && b[off] == 0x55 && b[off + 1] == 0xAA {
        let pcir = off + le16(b, off + 0x18) as usize;
        if b.get(pcir..pcir + 4) != Some(b"PCIR") || pcir + 0x18 > b.len() {
            break;
        }
        let len = le16(b, pcir + 0x10) as usize * 512;
        if len == 0 {
            break;
        }
        v.push(RomImage { offset: off, len: len.min(b.len() - off), code_type: b[pcir + 0x14], vendor_id: le16(b, pcir + 4), device_id: le16(b, pcir + 6) });
        if b[pcir + 0x15] & 0x80 != 0 {
            break;
        }
        off += len;
    }
    v
}

// ---- the device ----

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod dev {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::path::Path;

    /// `struct mpt3_ioctl_command` (mpt3sas_ctl.h) on a 64-bit kernel, with
    /// room for the frame the driver copies from `mf` (data_sge_offset
    /// dwords of it).
    #[repr(C)]
    #[allow(dead_code)] // read by the driver
    struct Command {
        ioc_number: u32,
        port_number: u32,
        max_data_size: u32,
        timeout: u32,
        reply_frame_buf_ptr: u64,
        data_in_buf_ptr: u64,
        data_out_buf_ptr: u64,
        sense_data_ptr: u64,
        max_reply_bytes: u32,
        data_in_size: u32,
        data_out_size: u32,
        max_sense_bytes: u32,
        data_sge_offset: u32,
        mf: [u8; 128],
    }

    /// `_IOWR('L', n, size)`.
    const fn iowr(n: u64, size: u64) -> u64 {
        (3 << 30) | (size << 16) | ((b'L' as u64) << 8) | n
    }
    /// The driver checks each size against its own struct: the command is
    /// 72 bytes (`mf[1]` padded), iocinfo 92, the reset's header 12.
    const MPT3COMMAND: u64 = iowr(20, 72);
    const MPT3IOCINFO: u64 = iowr(17, 92);
    const MPT3HARDRESET: u64 = iowr(24, 12);

    /// The driver's control device, open.
    pub struct Ctl {
        file: std::fs::File,
        name: String,
    }

    impl Ctl {
        /// `/dev/mpt3ctl` and `/dev/mpt2ctl`, those that open.
        pub fn open_all(dev: &Path) -> io::Result<Vec<Ctl>> {
            let mut v = Vec::new();
            let mut last = None;
            for n in ["mpt3ctl", "mpt2ctl"] {
                match std::fs::OpenOptions::new().read(true).write(true).open(dev.join(n)) {
                    Ok(file) => v.push(Ctl { file, name: n.to_string() }),
                    Err(e) => last = Some(e),
                }
            }
            if v.is_empty() {
                return Err(last.unwrap_or_else(|| io::Error::from(io::ErrorKind::NotFound)));
            }
            Ok(v)
        }
    }

    impl Port for Ctl {
        fn command(&self, ioc: u32, frame: &[u8], sge_dwords: u32, data_in: &mut [u8], data_out: &[u8], timeout_s: u32) -> io::Result<Vec<u8>> {
            let mut reply = vec![0u8; 128];
            let mut sense = vec![0u8; 96];
            let mut c = Command {
                ioc_number: ioc,
                port_number: 0,
                max_data_size: 0,
                timeout: timeout_s,
                reply_frame_buf_ptr: reply.as_mut_ptr() as u64,
                data_in_buf_ptr: if data_in.is_empty() { 0 } else { data_in.as_mut_ptr() as u64 },
                data_out_buf_ptr: if data_out.is_empty() { 0 } else { data_out.as_ptr() as u64 },
                sense_data_ptr: sense.as_mut_ptr() as u64,
                max_reply_bytes: reply.len() as u32,
                data_in_size: data_in.len() as u32,
                data_out_size: data_out.len() as u32,
                max_sense_bytes: sense.len() as u32,
                data_sge_offset: sge_dwords,
                mf: [0u8; 128],
            };
            let n = frame.len().min(c.mf.len());
            c.mf[..n].copy_from_slice(&frame[..n]);
            // SAFETY: `c` is the driver's struct with the frame after it;
            // every pointer in it names a live buffer of the size beside it.
            let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), MPT3COMMAND as _, &mut c) };
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(reply)
        }

        fn info(&self, ioc: u32) -> io::Result<IocInfo> {
            let mut b = [0u8; 92];
            b[0..4].copy_from_slice(&ioc.to_le_bytes());
            // SAFETY: `b` is the 92-byte struct the driver fills.
            let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), MPT3IOCINFO as _, b.as_mut_ptr()) };
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(parse_iocinfo(&b))
        }

        fn hard_reset(&self, ioc: u32) -> io::Result<()> {
            let mut b = [0u8; 12];
            b[0..4].copy_from_slice(&ioc.to_le_bytes());
            // SAFETY: `b` is the 12-byte header the driver reads.
            let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), MPT3HARDRESET as _, b.as_mut_ptr()) };
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        fn name(&self) -> String {
            self.name.clone()
        }
    }
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub use dev::Ctl;

/// A fake card for the tests (#166 test standard): IOC info and facts,
/// standard and extended pages, flash regions it uploads and downloads,
/// and a reset that makes a downloaded firmware the one it runs.
#[cfg(test)]
pub mod fake {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct State {
        pub info: IocInfo,
        pub facts: Option<Vec<u8>>,
        /// (page type, ext type, number, address) -> page bytes; a
        /// GET_NEXT_HANDLE walk (form 0) finds the page after the handle.
        pub pages: BTreeMap<(u8, u8, u8, u32), Vec<u8>>,
        /// upload image type -> the flash's bytes.
        pub flash: BTreeMap<u8, Vec<u8>>,
        /// download image type -> what is being written.
        pub staged: BTreeMap<u8, Vec<u8>>,
        pub resets: u32,
        pub commands: Vec<u8>,
        /// Answer every download with this IOCStatus.
        pub refuse_download: Option<u16>,
        /// Store this byte wrong in the flash (a read-back mismatch).
        pub corrupt_at: Option<usize>,
    }

    pub struct Fake(pub Mutex<State>);

    impl Fake {
        pub fn new(s: State) -> Self {
            Fake(Mutex::new(s))
        }
        pub fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
            f(&mut self.0.lock().unwrap())
        }
    }

    fn reply(function: u8, status: u16) -> Vec<u8> {
        let mut r = vec![0u8; 128];
        r[0x03] = function;
        r[0x0E..0x10].copy_from_slice(&status.to_le_bytes());
        r
    }

    impl Port for Fake {
        fn command(&self, _ioc: u32, f: &[u8], sge: u32, data_in: &mut [u8], data_out: &[u8], _t: u32) -> io::Result<Vec<u8>> {
            let mut s = self.0.lock().unwrap();
            let func = f[0x03];
            s.commands.push(func);
            Ok(match func {
                FUNCTION_IOC_FACTS => match &s.facts {
                    Some(b) => b.clone(),
                    None => reply(func, IOCSTATUS_INVALID_FUNCTION),
                },
                FUNCTION_CONFIG => {
                    assert_eq!(sge, CONFIG_SGE_DWORDS);
                    let (ty, ext, num, addr) = (f[0x17], f[0x06], f[0x16], le32(f, 0x18));
                    let key = (ty, if ty == PAGETYPE_EXTENDED { ext } else { 0 }, num, addr);
                    let found = if addr & 0xF000_0000 == 0 && ty == PAGETYPE_EXTENDED && matches!(ext, EXTPAGE_SAS_DEVICE | EXTPAGE_ENCLOSURE) {
                        // GET_NEXT_HANDLE: the first page whose handle is above `addr`.
                        let at = if ext == EXTPAGE_SAS_DEVICE { 0x18 } else { 0x16 };
                        let after = if addr == 0xFFFF { 0 } else { addr + 1 };
                        s.pages.iter().filter(|((t, e, n, _), _)| (*t, *e, *n) == (ty, ext, num)).map(|(_, p)| p).find(|p| le16(p, at) as u32 >= after).cloned()
                    } else {
                        s.pages.get(&key).cloned()
                    };
                    let Some(p) = found else { return Ok(reply(func, IOCSTATUS_CONFIG_INVALID_PAGE)) };
                    let mut r = reply(func, IOCSTATUS_SUCCESS);
                    if ty == PAGETYPE_EXTENDED {
                        r[0x04..0x06].copy_from_slice(&((p.len() / 4) as u16).to_le_bytes());
                    } else {
                        r[0x15] = (p.len() / 4) as u8;
                    }
                    r[0x14] = 1;
                    if f[0x00] == ACTION_READ_CURRENT {
                        let n = data_in.len().min(p.len());
                        data_in[..n].copy_from_slice(&p[..n]);
                    }
                    r
                }
                FUNCTION_FW_UPLOAD => {
                    let mpi25 = s.info.mpi25();
                    assert_eq!(sge, if mpi25 { 8 } else { 9 });
                    let (off, size) = if mpi25 { (le32(f, 0x18), le32(f, 0x1C)) } else { (le32(f, 0x1C), le32(f, 0x20)) };
                    assert_eq!(size as usize, data_in.len());
                    let Some(img) = s.flash.get(&f[0]) else { return Ok(reply(func, IOCSTATUS_INVALID_FIELD)) };
                    let o = (off as usize).min(img.len());
                    let n = data_in.len().min(img.len() - o);
                    data_in[..n].copy_from_slice(&img[o..o + n]);
                    let mut r = reply(func, IOCSTATUS_SUCCESS);
                    r[0x14..0x18].copy_from_slice(&(img.len() as u32).to_le_bytes());
                    r
                }
                FUNCTION_FW_DOWNLOAD => {
                    if let Some(st) = s.refuse_download {
                        return Ok(reply(func, st));
                    }
                    let mpi25 = s.info.mpi25();
                    let (off, size) = if mpi25 { (le32(f, 0x18), le32(f, 0x1C)) } else { (le32(f, 0x1C), le32(f, 0x20)) };
                    assert_eq!(size as usize, data_out.len());
                    let ty = f[0];
                    let total = le32(f, 0x0C) as usize;
                    let st = s.staged.entry(ty).or_default();
                    assert_eq!(off as usize, st.len(), "chunks in order");
                    st.extend_from_slice(data_out);
                    if f[0x07] & FW_DOWNLOAD_LAST_SEGMENT != 0 {
                        assert_eq!(st.len(), total);
                        let mut img = s.staged.remove(&ty).unwrap_or_default();
                        if let Some(i) = s.corrupt_at {
                            img[i] ^= 0xFF;
                        }
                        // Download FW (1) lands in upload FW_FLASH (1); BIOS (2) in BIOS_FLASH (2).
                        s.flash.insert(ty, img);
                    }
                    reply(func, IOCSTATUS_SUCCESS)
                }
                _ => reply(func, IOCSTATUS_INVALID_FUNCTION),
            })
        }

        fn info(&self, _ioc: u32) -> io::Result<IocInfo> {
            Ok(self.0.lock().unwrap().info.clone())
        }

        fn hard_reset(&self, _ioc: u32) -> io::Result<()> {
            let mut s = self.0.lock().unwrap();
            s.resets += 1;
            // The card now runs what is in its flash.
            if let Some(h) = s.flash.get(&0x01).map(Vec::as_slice).and_then(parse_fw_header) {
                s.info.firmware_version = h.fw_version;
                if let Some(f) = s.facts.as_mut() {
                    f[0x20..0x24].copy_from_slice(&h.fw_version.to_le_bytes());
                }
            }
            Ok(())
        }

        fn name(&self) -> String {
            "fake".into()
        }
    }

    /// A firmware image with an MPI2 header: version, product, `len` bytes.
    pub fn fw_image(version: u32, product: u16, len: usize) -> Vec<u8> {
        let mut b: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
        b[0x00..0x04].copy_from_slice(&0xEA00_0000u32.to_le_bytes());
        b[0x04..0x08].copy_from_slice(&0x5AFA_A55Au32.to_le_bytes());
        b[0x08..0x0C].copy_from_slice(&0xA55A_FAA5u32.to_le_bytes());
        b[0x0C..0x10].copy_from_slice(&0x5AA5_5AFAu32.to_le_bytes());
        b[0x14..0x18].copy_from_slice(&version.to_le_bytes());
        b[0x18..0x1C].copy_from_slice(&0x0E01_0007u32.to_le_bytes());
        b[0x20..0x22].copy_from_slice(&0x1000u16.to_le_bytes());
        b[0x22..0x24].copy_from_slice(&product.to_le_bytes());
        b[0x2C..0x30].copy_from_slice(&(len as u32).to_le_bytes());
        b[0x68..0x88].fill(0);
        b[0x68..0x68 + 9].copy_from_slice(b"SAS3008IT");
        b
    }

    /// An option ROM: a legacy BIOS then a UEFI driver, LSI's ids.
    pub fn rom_image() -> Vec<u8> {
        let mut b = Vec::new();
        for (code, last) in [(0u8, false), (3u8, true)] {
            let mut img = vec![0u8; 1024];
            img[0] = 0x55;
            img[1] = 0xAA;
            img[0x18..0x1A].copy_from_slice(&0x40u16.to_le_bytes());
            img[0x40..0x44].copy_from_slice(b"PCIR");
            img[0x44..0x46].copy_from_slice(&0x1000u16.to_le_bytes());
            img[0x46..0x48].copy_from_slice(&0x0097u16.to_le_bytes());
            img[0x50..0x52].copy_from_slice(&2u16.to_le_bytes());
            img[0x54] = code;
            img[0x55] = if last { 0x80 } else { 0 };
            b.extend(img);
        }
        b
    }

    /// The facts reply of a SAS3008 running `fw`.
    pub fn facts(fw: u32, product: u16) -> Vec<u8> {
        let mut r = reply(FUNCTION_IOC_FACTS, IOCSTATUS_SUCCESS);
        r[0x00..0x02].copy_from_slice(&0x0205u16.to_le_bytes());
        r[0x16] = 1;
        r[0x1A..0x1C].copy_from_slice(&product.to_le_bytes());
        r[0x20..0x24].copy_from_slice(&fw.to_le_bytes());
        r[0x2A..0x2C].copy_from_slice(&1024u16.to_le_bytes());
        r
    }

    /// A SAS3008 (HBA330) like forge1's: 8 phys, phy 1 linked at 12 Gbit to
    /// a SAS drive in enclosure slot 1, phy 2 held at 6 Gbit max.
    pub fn hba330() -> State {
        let fw = 0x1000_0100; // 16.00.01.00
        let mut s = State {
            info: IocInfo { adapter_type: ADAPTER_SAS3, pci_device: 0x0097, subsystem_vendor: 0x1028, subsystem_device: 0x1f45, firmware_version: fw, bios_version: 0x0812_0700, driver_version: "mpt3sas-43.100.00.00".into(), pci_bus: 1, ..Default::default() },
            facts: Some(facts(fw, 0x2221)),
            ..Default::default()
        };
        let mut man0 = vec![0u8; 0x4C];
        man0[0x04..0x0B].copy_from_slice(b"SAS3008");
        man0[0x14..0x16].copy_from_slice(b"C0");
        man0[0x1C..0x22].copy_from_slice(b"HBA330");
        man0[0x2C..0x31].copy_from_slice(b"0D2T3");
        man0[0x3C..0x46].copy_from_slice(b"CNFCP00123");
        s.pages.insert((PAGETYPE_MANUFACTURING, 0, 0, 0), man0);
        let mut io0 = vec![0u8; 0x14];
        io0[0x0C..0x10].copy_from_slice(&0x0E01_0007u32.to_le_bytes());
        io0[0x10..0x14].copy_from_slice(&0x0E01_0007u32.to_le_bytes());
        s.pages.insert((PAGETYPE_IO_UNIT, 0, 0, 0), io0);
        let mut bios3 = vec![0u8; 0x0C];
        bios3[0x08..0x0C].copy_from_slice(&0x0812_0700u32.to_le_bytes());
        s.pages.insert((PAGETYPE_BIOS, 0, 3, 0), bios3);
        let mut sio0 = vec![0u8; 0x10 + 8 * 0x14];
        sio0[0x0C] = 8;
        let o = 0x10 + 0x14;
        sio0[o + 3] = 0xB0;
        sio0[o + 8..o + 10].copy_from_slice(&0x0009u16.to_le_bytes());
        s.pages.insert((PAGETYPE_EXTENDED, EXTPAGE_SAS_IO_UNIT, 0, 0), sio0);
        let mut sio1 = vec![0u8; 0x14 + 8 * 0x0C];
        sio1[0x10] = 8;
        for i in 0..8 {
            sio1[0x14 + i * 0x0C + 3] = if i == 2 { 0xA8 } else { 0xB8 };
        }
        s.pages.insert((PAGETYPE_EXTENDED, EXTPAGE_SAS_IO_UNIT, 1, 0), sio1);
        let mut dev = vec![0u8; 0x2C];
        dev[0x08..0x0A].copy_from_slice(&1u16.to_le_bytes());
        dev[0x0A..0x0C].copy_from_slice(&1u16.to_le_bytes());
        dev[0x0C..0x14].copy_from_slice(&0x5000_cca2_5e8b_1c11u64.to_le_bytes());
        dev[0x16] = 1;
        dev[0x18..0x1A].copy_from_slice(&0x0009u16.to_le_bytes());
        dev[0x1C..0x20].copy_from_slice(&0x401u32.to_le_bytes());
        dev[0x20..0x22].copy_from_slice(&1u16.to_le_bytes());
        s.pages.insert((PAGETYPE_EXTENDED, EXTPAGE_SAS_DEVICE, 0, 9), dev);
        let mut enc = vec![0u8; 0x24];
        enc[0x0C..0x14].copy_from_slice(&0x5006_05b0_0c8e_1200u64.to_le_bytes());
        enc[0x16..0x18].copy_from_slice(&1u16.to_le_bytes());
        enc[0x18..0x1A].copy_from_slice(&8u16.to_le_bytes());
        s.pages.insert((PAGETYPE_EXTENDED, EXTPAGE_ENCLOSURE, 0, 1), enc);
        s.flash.insert(0x01, fw_image(fw, 0x2221, 200_000));
        s.flash.insert(0x02, rom_image());
        s.flash.insert(0x06, vec![0x4D; 4096]);
        s
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{self, Fake};
    use super::*;

    #[test]
    fn iocinfo_is_parsed_at_the_driver_offsets() {
        let mut b = vec![0u8; 92];
        b[12] = 6;
        b[40..44].copy_from_slice(&0x1000_0100u32.to_le_bytes());
        b[48..52].copy_from_slice(b"mpt3");
        b[84..88].copy_from_slice(&((1u32 << 8) | 0x01).to_le_bytes());
        let i = parse_iocinfo(&b);
        assert!(i.mpi25());
        assert_eq!(version(i.firmware_version), "16.00.01.00");
        assert_eq!(i.bdf(), "0000:01:01.0");
        assert_eq!(i.driver_version, "mpt3");
    }

    #[test]
    fn config_request_frames() {
        let f = config_request(ACTION_READ_CURRENT, PAGETYPE_EXTENDED, EXTPAGE_SAS_DEVICE, 0, 0xFFFF, 9, 0, 0x11);
        assert_eq!(f.len(), 28);
        assert_eq!((f[0], f[3], le16(&f, 4), f[6]), (1, 0x04, 0x11, 0x12));
        assert_eq!((f[0x14], f[0x16], f[0x17]), (9, 0, 0x0F));
        let f = config_request(ACTION_READ_CURRENT, PAGETYPE_MANUFACTURING, 0, 0, 0, 2, 0x13, 0);
        assert_eq!((f[0x14], f[0x15], f[0x16], f[0x17], f[6]), (2, 0x13, 0, 9, 0));
    }

    #[test]
    fn transfer_frames_both_generations() {
        let (f, sge) = transfer_request(true, true, 1, 1000, 512, 488, true);
        assert_eq!((f.len(), sge), (0x20, 8));
        assert_eq!((f[0], f[3], f[7]), (1, FUNCTION_FW_DOWNLOAD, 1));
        assert_eq!((le32(&f, 0x0C), le32(&f, 0x18), le32(&f, 0x1C)), (1000, 512, 488));
        let (f, sge) = transfer_request(false, false, 2, 0, 0, 64, false);
        assert_eq!((f.len(), sge), (0x24, 9));
        assert_eq!((f[3], f[7], f[0x16]), (FUNCTION_FW_UPLOAD, 0, 12));
        assert_eq!((le32(&f, 0x1C), le32(&f, 0x20)), (0, 64));
    }

    #[test]
    fn facts_pages_and_walks_on_the_fake() {
        let c = Fake::new(fake::hba330());
        let f = ioc_facts(&c, 0).unwrap();
        assert_eq!((f.mpi_version(), f.product_id, version(f.fw_version), f.max_targets), ("2.5".to_string(), 0x2221, "16.00.01.00".to_string(), 1024));
        assert!(f.exceptions().is_empty());
        let m = parse_man0(&page(&c, 0, PAGETYPE_MANUFACTURING, 0, 0, 0).unwrap().unwrap());
        assert_eq!((m.chip_name.as_deref(), m.board_name.as_deref(), m.board_tracer.as_deref()), (Some("SAS3008"), Some("HBA330"), Some("CNFCP00123")));
        let (d, p) = parse_io_unit0(&page(&c, 0, PAGETYPE_IO_UNIT, 0, 0, 0).unwrap().unwrap());
        assert_eq!((d, p), (0x0E01_0007, 0x0E01_0007));
        assert_eq!(version(parse_bios3(&page(&c, 0, PAGETYPE_BIOS, 0, 3, 0).unwrap().unwrap())), "08.18.07.00");
        let phys = parse_sas_io_unit0(&ext_page(&c, 0, EXTPAGE_SAS_IO_UNIT, 0, 0).unwrap().unwrap());
        assert_eq!(phys.len(), 8);
        assert!(phys[1].linked() && !phys[0].linked());
        assert_eq!((phys[1].rate, phys[1].attached), (0xB, 9));
        let set = parse_sas_io_unit1(&ext_page(&c, 0, EXTPAGE_SAS_IO_UNIT, 1, 0).unwrap().unwrap());
        assert_eq!(set[2], (0xA, 0x8, false));
        assert_eq!(set[1], (0xB, 0x8, false));
        let devs: Vec<Device> = walk(&c, 0, EXTPAGE_SAS_DEVICE, 0x18).unwrap().iter().map(Vec::as_slice).map(parse_sas_device0).collect();
        assert_eq!(devs.len(), 1);
        assert_eq!((devs[0].kind(), devs[0].slot, devs[0].present(), devs[0].access()), ("SAS drive", 1, true, None));
        let encs: Vec<Enclosure> = walk(&c, 0, EXTPAGE_ENCLOSURE, 0x16).unwrap().iter().map(Vec::as_slice).map(parse_enclosure0).collect();
        assert_eq!((encs[0].logical_id, encs[0].num_slots), (0x5006_05b0_0c8e_1200, 8));
        // A page the card does not keep.
        assert_eq!(page(&c, 0, PAGETYPE_MANUFACTURING, 0, 7, 0).unwrap(), None);
    }

    #[test]
    fn upload_reads_each_region_in_chunks() {
        let c = Fake::new(fake::hba330());
        let seen = std::sync::Mutex::new(Vec::new());
        let fw = upload(&c, 0, true, Region::Firmware, &|d, t| seen.lock().unwrap().push((d, t))).unwrap().unwrap();
        assert_eq!(fw.len(), 200_000);
        assert_eq!(*seen.lock().unwrap().last().unwrap(), (200_000, 200_000));
        assert_eq!(seen.lock().unwrap().len(), 200_000usize.div_ceil(CHUNK));
        let h = parse_fw_header(&fw).unwrap();
        assert_eq!((version(h.fw_version), h.product_id, h.name.as_deref()), ("16.00.01.00".to_string(), 0x2221, Some("SAS3008IT")));
        let rom = upload(&c, 0, true, Region::Bios, &|_, _| {}).unwrap().unwrap();
        let imgs = parse_rom(&rom);
        assert_eq!(imgs.iter().map(RomImage::kind).collect::<Vec<_>>(), ["legacy_bios", "uefi"]);
        assert_eq!(imgs[1].offset, 1024);
        // No backup image on this card.
        assert_eq!(upload(&c, 0, true, Region::FirmwareBackup, &|_, _| {}).unwrap(), None);
    }

    #[test]
    fn download_then_reset_runs_the_new_image() {
        let c = Fake::new(fake::hba330());
        let img = fake::fw_image(0x1000_0200, 0x2221, 150_001);
        download(&c, 0, true, Region::Firmware, &img, &|_, _| {}).unwrap();
        assert_eq!(c.with(|s| s.flash[&1].clone()), img);
        assert_eq!(c.with(|s| s.info.firmware_version), 0x1000_0100, "not running until reset");
        c.hard_reset(0).unwrap();
        assert_eq!(version(c.info(0).unwrap().firmware_version), "16.00.02.00");
        assert_eq!(version(ioc_facts(&c, 0).unwrap().fw_version), "16.00.02.00");
        // MPI 2.0 frames on a SAS2 card.
        let c2 = Fake::new(fake::State { info: IocInfo { adapter_type: 0x04, ..Default::default() }, ..Default::default() });
        download(&c2, 0, false, Region::Bios, &fake::rom_image(), &|_, _| {}).unwrap();
        assert_eq!(upload(&c2, 0, false, Region::Bios, &|_, _| {}).unwrap().unwrap(), fake::rom_image());
    }

    #[test]
    fn a_refused_download_says_what_the_card_said() {
        let c = Fake::new(fake::hba330());
        c.with(|s| s.refuse_download = Some(0x0007));
        let e = download(&c, 0, true, Region::Firmware, &[1, 2, 3, 4], &|_, _| {}).unwrap_err();
        assert!(e.contains("invalid field"), "{e}");
        assert!(download(&c, 0, true, Region::Manufacturing, &[1], &|_, _| {}).is_err());
    }

    #[test]
    fn headers_are_recognised_or_not() {
        assert!(parse_fw_header(&[0u8; 256]).is_none());
        assert!(parse_rom(&[0u8; 256]).is_empty());
        let mut b = fake::fw_image(1, 2, 512);
        b[0x04] ^= 1;
        assert!(parse_fw_header(&b).is_none());
    }
}
