//! Raw SCSI over SG_IO — the commands the kernel does not wrap for us:
//! READ CAPACITY(16) on a drive the sd driver refused (520-byte sectors),
//! MODE SELECT + FORMAT UNIT to change the sector size, TEST UNIT READY
//! for format progress, and RECEIVE/SEND DIAGNOSTIC for SES pages.
//!
//! Everything that can be tested without hardware — sense decoding, CDB
//! and parameter-list construction, response parsing — is portable and
//! unit-tested here. Only `Device` (the fd + ioctl) is Linux-only.

use std::fmt;

/// Commands stormdrive sent that did not end GOOD, per device path:
/// `"<opcode> <outcome>" → count` (#82). The kernel's `ioerr_cnt` counts
/// every command that fails on a device, whoever sent it; this is the part
/// that was ours, so the two can be compared.
static NOT_GOOD: std::sync::Mutex<NotGood> = std::sync::Mutex::new(std::collections::BTreeMap::new());

/// device path → `"<opcode>[/<page>] <outcome>"` → count.
pub type NotGood = std::collections::BTreeMap<String, std::collections::BTreeMap<String, u64>>;

/// Count one command that did not end GOOD.
pub fn record_not_good(path: &str, cdb: &[u8], outcome: &str) {
    let op = cdb.first().copied().unwrap_or(0);
    // LOG SENSE / VPD / diagnostic pages: the page is part of what failed.
    let what = match op {
        0x4D => format!("4d/{:02x} {outcome}", cdb.get(2).map_or(0, |b| b & 0x3f)),
        0x12 if cdb.get(1).is_some_and(|b| b & 1 != 0) => format!("12/{:02x} {outcome}", cdb.get(2).copied().unwrap_or(0)),
        0x85 => format!("85/{:02x} {outcome}", cdb.get(4).copied().unwrap_or(0)),
        _ => format!("{op:02x} {outcome}"),
    };
    let mut m = NOT_GOOD.lock().unwrap_or_else(|e| e.into_inner());
    *m.entry(path.to_string()).or_default().entry(what).or_default() += 1;
}

/// What [`record_not_good`] has counted since start.
pub fn not_good() -> NotGood {
    NOT_GOOD.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// How many commands stormdrive sent to these device paths did not end GOOD.
pub fn not_good_count<'a>(paths: impl Iterator<Item = &'a str>) -> u64 {
    let m = NOT_GOOD.lock().unwrap_or_else(|e| e.into_inner());
    paths.filter_map(|p| m.get(p)).flat_map(|o| o.values()).sum()
}

/// Decoded sense data (fixed or descriptor format).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sense {
    pub key: u8,
    pub asc: u8,
    pub ascq: u8,
    /// Sense-key-specific progress indication (0..=65535 of the whole),
    /// present while a FORMAT UNIT runs (key NOT READY, 04/04).
    pub progress: Option<u16>,
    /// ILLEGAL REQUEST: the byte (and bit) the drive objects to, from the
    /// sense-key-specific field pointer (SPC "field pointer", #129).
    pub field: Option<FieldPointer>,
}

/// SPC sense-key-specific field pointer: which byte of the CDB or of the
/// parameter list an ILLEGAL REQUEST is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldPointer {
    /// C/D = 1: the CDB; 0: the parameter list (data out).
    pub in_cdb: bool,
    /// BPV = 1: the bit within the byte (7 = most significant).
    pub bit: Option<u8>,
    pub byte: u16,
}

impl FieldPointer {
    /// The three sense-key-specific bytes (SKSV in the first's bit 7).
    pub fn from_sks(b: [u8; 3]) -> Option<Self> {
        (b[0] & 0x80 != 0).then(|| FieldPointer {
            in_cdb: b[0] & 0x40 != 0,
            bit: (b[0] & 0x08 != 0).then_some(b[0] & 0x07),
            byte: u16::from_be_bytes([b[1], b[2]]),
        })
    }
}

impl fmt::Display for FieldPointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} byte {}", if self.in_cdb { "cdb" } else { "parameter list" }, self.byte)?;
        if let Some(b) = self.bit {
            write!(f, " bit {b}")?;
        }
        Ok(())
    }
}

impl Sense {
    /// Sense with no sense-key-specific data.
    pub fn new(key: u8, asc: u8, ascq: u8) -> Self {
        Sense { key, asc, ascq, progress: None, field: None }
    }

    pub fn key_name(&self) -> &'static str {
        match self.key {
            0x0 => "no sense",
            0x1 => "recovered error",
            0x2 => "not ready",
            0x3 => "medium error",
            0x4 => "hardware error",
            0x5 => "illegal request",
            0x6 => "unit attention",
            0x7 => "data protect",
            0x8 => "blank check",
            0xb => "aborted command",
            0xd => "volume overflow",
            0xe => "miscompare",
            _ => "reserved",
        }
    }

    pub fn is_format_in_progress(&self) -> bool {
        self.key == 0x2 && self.asc == 0x04 && self.ascq == 0x04
    }

    pub fn is_illegal_request(&self) -> bool {
        self.key == 0x5
    }

    pub fn is_unit_attention(&self) -> bool {
        self.key == 0x6
    }

    /// 31/00 MEDIUM FORMAT CORRUPTED (any key; drives say it as MEDIUM
    /// ERROR or NOT READY): a format that did not finish (#131). Another
    /// format is the cure.
    pub fn is_medium_format_corrupted(&self) -> bool {
        self.asc == 0x31 && self.ascq == 0x00
    }

    /// Progress as a percentage, when the device reported one.
    pub fn progress_pct(&self) -> Option<u8> {
        self.progress.map(|p| ((p as u32 * 100) / 65536) as u8)
    }
}

impl fmt::Display for Sense {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (key 0x{:x}, asc/ascq 0x{:02x}/0x{:02x}",
            self.key_name(),
            self.key,
            self.asc,
            self.ascq
        )?;
        if let Some(p) = self.field {
            write!(f, ", field pointer: {p}")?;
        }
        write!(f, ")")
    }
}

/// Parse sense data. Fixed format (0x70/0x71) and descriptor format
/// (0x72/0x73) both occur in the wild; the progress indication lives in
/// different places in each.
pub fn parse_sense(raw: &[u8]) -> Option<Sense> {
    if raw.is_empty() {
        return None;
    }
    match raw[0] & 0x7f {
        0x70 | 0x71 => {
            if raw.len() < 14 {
                return None;
            }
            let key = raw[2] & 0x0f;
            let asc = raw[12];
            let ascq = raw[13];
            let progress = if raw.len() >= 18 && raw[15] & 0x80 != 0 && (key == 0x0 || key == 0x2)
            {
                Some(u16::from_be_bytes([raw[16], raw[17]]))
            } else {
                None
            };
            let field = if raw.len() >= 18 && key == 0x5 {
                FieldPointer::from_sks([raw[15], raw[16], raw[17]])
            } else {
                None
            };
            Some(Sense {
                key,
                asc,
                ascq,
                progress,
                field,
            })
        }
        0x72 | 0x73 => {
            if raw.len() < 8 {
                return None;
            }
            let key = raw[1] & 0x0f;
            let asc = raw[2];
            let ascq = raw[3];
            let add_len = raw[7] as usize;
            let end = (8 + add_len).min(raw.len());
            let mut progress = None;
            let mut field = None;
            let mut i = 8;
            while i + 2 <= end {
                let dtype = raw[i];
                let dlen = raw[i + 1] as usize;
                if dtype == 0x02 && dlen >= 6 && i + 2 + dlen <= raw.len() {
                    // Sense key specific descriptor: SKSV in byte 4 bit 7,
                    // progress in bytes 5-6.
                    if raw[i + 4] & 0x80 != 0 && (key == 0x0 || key == 0x2) {
                        progress = Some(u16::from_be_bytes([raw[i + 5], raw[i + 6]]));
                    }
                    if key == 0x5 {
                        field = FieldPointer::from_sks([raw[i + 4], raw[i + 5], raw[i + 6]]);
                    }
                }
                i += 2 + dlen;
            }
            Some(Sense {
                key,
                asc,
                ascq,
                progress,
                field,
            })
        }
        _ => None,
    }
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// CHECK CONDITION with decodable sense.
    Sense(Sense),
    /// Transport-level failure (host/driver status) or a SCSI status other
    /// than GOOD with no usable sense.
    Transport {
        status: u8,
        host: u16,
        driver: u16,
    },
    /// A response too short to hold what we asked for.
    Short(usize),
    Unsupported(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Sense(s) => write!(f, "{s}"),
            Error::Transport {
                status,
                host,
                driver,
            } => write!(
                f,
                "scsi status 0x{status:02x}, host 0x{host:02x}, driver 0x{driver:02x}"
            ),
            Error::Short(n) => write!(f, "short response ({n} bytes)"),
            Error::Unsupported(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// READ CAPACITY(16) response, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    pub last_lba: u64,
    pub block_len: u32,
    /// Logical blocks per physical block, as a power-of-two exponent.
    pub lbppbe: u8,
    /// Protection type in use (0 = none), from P_TYPE/PROT_EN.
    pub prot_type: u8,
}

impl Capacity {
    pub fn blocks(&self) -> u64 {
        self.last_lba.wrapping_add(1)
    }
    pub fn bytes(&self) -> u64 {
        self.blocks() * self.block_len as u64
    }
    pub fn physical_block_len(&self) -> u32 {
        self.block_len << self.lbppbe.min(16)
    }
}

pub fn parse_read_capacity16(raw: &[u8]) -> Result<Capacity> {
    if raw.len() < 16 {
        return Err(Error::Short(raw.len()));
    }
    let last_lba = u64::from_be_bytes(raw[0..8].try_into().unwrap());
    let block_len = u32::from_be_bytes(raw[8..12].try_into().unwrap());
    let prot_en = raw[12] & 0x01 != 0;
    let p_type = (raw[12] >> 1) & 0x07;
    let prot_type = if prot_en { p_type + 1 } else { 0 };
    let lbppbe = raw[13] & 0x0f;
    Ok(Capacity {
        last_lba,
        block_len,
        lbppbe,
        prot_type,
    })
}

/// Standard INQUIRY, the parts we use.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inquiry {
    /// Peripheral device type: 0 disk, 13 enclosure.
    pub device_type: u8,
    pub vendor: String,
    pub product: String,
    pub revision: String,
}

pub fn parse_inquiry(raw: &[u8]) -> Result<Inquiry> {
    if raw.len() < 36 {
        return Err(Error::Short(raw.len()));
    }
    let s = |r: std::ops::Range<usize>| String::from_utf8_lossy(&raw[r]).trim().to_string();
    Ok(Inquiry {
        device_type: raw[0] & 0x1f,
        vendor: s(8..16),
        product: s(16..32),
        revision: s(32..36),
    })
}

/// MODE SENSE(10) response header + the first block descriptor, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeHeader10 {
    pub medium_type: u8,
    pub device_specific: u8,
    pub long_lba: bool,
    pub block_descriptor: Option<Vec<u8>>,
}

pub fn parse_mode_sense10(raw: &[u8]) -> Result<ModeHeader10> {
    if raw.len() < 8 {
        return Err(Error::Short(raw.len()));
    }
    let long_lba = raw[4] & 0x01 != 0;
    let bd_len = u16::from_be_bytes([raw[6], raw[7]]) as usize;
    let bd = if bd_len > 0 && raw.len() >= 8 + bd_len {
        Some(raw[8..8 + bd_len].to_vec())
    } else {
        None
    };
    Ok(ModeHeader10 {
        medium_type: raw[2],
        device_specific: raw[3],
        long_lba,
        block_descriptor: bd,
    })
}

/// The MODE SELECT(10) parameter list that sets a new logical block
/// length, built from what the drive's MODE SENSE(10) returned (sg_format's
/// way, #129): an 8-byte header with the mode data length 0 (reserved on
/// select), the medium type kept, the device-specific byte 0 (WP/DPOFUA are
/// reserved on select) and LONGLBA as the drive returned it; then the
/// drive's own block descriptor, 8 bytes or 16 (long LBA), with NUMBER OF
/// LOGICAL BLOCKS 0 — with a changed length SBC sets the most blocks the
/// drive has at that length — and the new LOGICAL BLOCK LENGTH. No mode
/// pages follow. `long: false` sends a short descriptor whatever came back.
///
/// SBC's short descriptor for a disk has no density code: byte 0 is the top
/// byte of the block count. The list before #129 copied it back as a
/// "density", so a drive past 2^32 blocks (short count FFFFFFFFh) was asked
/// for FF000000h blocks: more than a 10 TB drive has at 4096 → 26/00.
pub fn mode_select10_list(h: &ModeHeader10, block_len: u32, long: bool) -> Vec<u8> {
    let long = long && h.long_lba && h.block_descriptor.as_ref().is_some_and(|bd| bd.len() >= 16);
    let bd_len = if long { 16 } else { 8 };
    let mut v = vec![0u8; 8 + bd_len];
    v[2] = h.medium_type;
    v[4] = u8::from(long);
    v[7] = bd_len as u8;
    let own = h.block_descriptor.as_deref().unwrap_or(&[]);
    if own.len() == bd_len {
        v[8..].copy_from_slice(own);
    }
    let bd = &mut v[8..];
    if long {
        bd[0..8].fill(0);
        bd[12..16].copy_from_slice(&block_len.to_be_bytes());
    } else {
        bd[0..4].fill(0);
        bd[5..8].copy_from_slice(&block_len.to_be_bytes()[1..]);
    }
    v
}

/// The same in MODE SELECT(6) shape (4-byte header, a short descriptor),
/// for drives that reject the 10-byte form.
pub fn mode_select6_list(h: &ModeHeader10, block_len: u32) -> Vec<u8> {
    let ten = mode_select10_list(h, block_len, false);
    let mut v = vec![0u8; 4];
    v[1] = h.medium_type;
    v[3] = 8;
    v.extend_from_slice(&ten[8..16]);
    v
}

/// FORMAT UNIT short parameter list header: FOV=0 (drive defaults for
/// DPRY/DCRT/STPF/IP), IMMED as asked, no defect list.
pub fn format_unit_param(immed: bool) -> [u8; 4] {
    [0, if immed { 0x02 } else { 0 }, 0, 0]
}

/// What REPORT SUPPORTED OPERATION CODES says of one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneCommand {
    /// SUPPORT: 001b not supported, 011b supported as the standard says,
    /// 101b supported in a vendor-specific way; 000b no data.
    pub support: u8,
    /// CDB USAGE DATA: byte 0 the opcode, then a bit set for every CDB bit
    /// the drive looks at.
    pub usage: Vec<u8>,
}

impl OneCommand {
    pub fn supported(&self) -> bool {
        matches!(self.support, 0b011 | 0b101)
    }
}

/// The one-command format of REPORT SUPPORTED OPERATION CODES: byte 1
/// SUPPORT (2:0), bytes 2-3 CDB SIZE, the usage data after.
pub fn parse_one_command(raw: &[u8]) -> Option<OneCommand> {
    if raw.len() < 4 {
        return None;
    }
    let size = u16::from_be_bytes([raw[2], raw[3]]) as usize;
    let usage = raw.get(4..4 + size).unwrap_or(&raw[4..]).to_vec();
    Some(OneCommand { support: raw[1] & 0x07, usage })
}

/// Whether a drive takes FORMAT UNIT's FFMT field, from its usage data for
/// opcode 0x04: Some(true) when a bit of byte 4's 1:0 is set, Some(false)
/// when the drive lists FORMAT UNIT and none is, None when it said nothing
/// we can read (then only trying tells).
pub fn ffmt_supported(c: &OneCommand) -> Option<bool> {
    if !c.supported() || c.usage.len() < 5 {
        return None;
    }
    Some(c.usage[4] & 0x03 != 0)
}

// ---------------------------------------------------------------- CDBs

pub mod cdb {
    pub fn test_unit_ready() -> [u8; 6] {
        [0x00, 0, 0, 0, 0, 0]
    }
    pub fn inquiry(evpd: Option<u8>, alloc: u16) -> [u8; 6] {
        let [hi, lo] = alloc.to_be_bytes();
        match evpd {
            Some(page) => [0x12, 0x01, page, hi, lo, 0],
            None => [0x12, 0x00, 0x00, hi, lo, 0],
        }
    }
    /// READ(16) of `blocks` logical blocks from `lba` (#81: a 520-byte
    /// drive's first blocks, which sd will not read).
    /// REQUEST SENSE, fixed format. GOOD even while the drive formats, with
    /// NOT READY 04/04 and the progress indication in the data (SPC).
    pub fn request_sense(alloc: u8) -> [u8; 6] {
        [0x03, 0, 0, 0, alloc, 0]
    }
    /// START STOP UNIT, IMMED=0, START=1 (#142: a drive NOT READY 04/02,
    /// "initializing command required", wants it before anything else).
    pub fn start_unit() -> [u8; 6] {
        [0x1b, 0, 0, 0, 0x01, 0]
    }

    pub fn read16(lba: u64, blocks: u32) -> [u8; 16] {
        let l = lba.to_be_bytes();
        let n = blocks.to_be_bytes();
        [0x88, 0, l[0], l[1], l[2], l[3], l[4], l[5], l[6], l[7], n[0], n[1], n[2], n[3], 0, 0]
    }
    pub fn read_capacity16(alloc: u32) -> [u8; 16] {
        let a = alloc.to_be_bytes();
        [0x9e, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, a[0], a[1], a[2], a[3], 0, 0]
    }
    /// Current values, page 0x01 (mandatory), block descriptors included.
    pub fn mode_sense10(page: u8, alloc: u16) -> [u8; 10] {
        mode_sense10_llbaa(page, alloc, false)
    }
    /// LLBAA=1 (byte 1 bit 4): a long LBA (16-byte) block descriptor may
    /// come back (#129).
    pub fn mode_sense10_llbaa(page: u8, alloc: u16, llbaa: bool) -> [u8; 10] {
        let [hi, lo] = alloc.to_be_bytes();
        [0x5a, if llbaa { 0x10 } else { 0 }, page & 0x3f, 0, 0, 0, 0, hi, lo, 0]
    }
    pub fn mode_select10(len: u16) -> [u8; 10] {
        let [hi, lo] = len.to_be_bytes();
        [0x55, 0x10, 0, 0, 0, 0, 0, hi, lo, 0]
    }
    pub fn mode_select6(len: u8) -> [u8; 6] {
        [0x15, 0x10, 0, 0, len, 0]
    }
    /// FMTDATA=1 (a parameter list follows), defect list format 0.
    pub fn format_unit() -> [u8; 6] {
        format_unit_pi(0)
    }
    /// The same, with FMTPINFO (byte 1 bits 7:6; #85): 00b no protection
    /// information, 10b type 1 (with PFU 000b in the parameter list).
    pub fn format_unit_pi(fmtpinfo: u8) -> [u8; 6] {
        format_unit_ffmt(fmtpinfo, 0)
    }
    /// FORMAT UNIT with FAST FORMAT (SBC-4, CDB byte 4 bits 1:0): 00b a
    /// normal format, 01b the drive initialises its own resources without
    /// writing the medium (#136: the initramfs's sector size / PI change).
    pub fn format_unit_ffmt(fmtpinfo: u8, ffmt: u8) -> [u8; 6] {
        [0x04, ((fmtpinfo & 0x03) << 6) | 0x10, 0, 0, ffmt & 0x03, 0]
    }
    /// REPORT SUPPORTED OPERATION CODES (MAINTENANCE IN 0xA3/0x0C),
    /// reporting options 001b: the one command `op` (no service action),
    /// with its CDB usage data.
    pub fn report_supported_opcode(op: u8, alloc: u32) -> [u8; 12] {
        let a = alloc.to_be_bytes();
        [0xa3, 0x0c, 0x01, op, 0, 0, a[0], a[1], a[2], a[3], 0, 0]
    }
    /// PCV=1: the named diagnostic page.
    pub fn receive_diagnostic(page: u8, alloc: u16) -> [u8; 6] {
        let [hi, lo] = alloc.to_be_bytes();
        [0x1c, 0x01, page, hi, lo, 0]
    }
    /// PF=1: the data is a diagnostic page.
    pub fn send_diagnostic(len: u16) -> [u8; 6] {
        let [hi, lo] = len.to_be_bytes();
        [0x1d, 0x10, 0, hi, lo, 0]
    }
    /// WRITE BUFFER: `mode` (0x05 whole image + save, 0x07 offsets + save,
    /// 0x0e offsets + save + defer, 0x0f activate deferred), buffer id 0.
    pub fn write_buffer(mode: u8, offset: u32, len: u32) -> [u8; 10] {
        let o = offset.to_be_bytes();
        let l = len.to_be_bytes();
        [0x3b, mode & 0x1f, 0, o[1], o[2], o[3], l[1], l[2], l[3], 0]
    }
    /// READ BUFFER mode 0x03 (descriptor): offset boundary + capacity.
    pub fn read_buffer_descriptor() -> [u8; 10] {
        [0x3c, 0x03, 0, 0, 0, 0, 0, 0, 4, 0]
    }
}

/// READ BUFFER descriptor: (offset boundary in bytes, buffer capacity).
pub fn parse_read_buffer_descriptor(raw: &[u8]) -> Option<(u32, u32)> {
    if raw.len() < 4 {
        return None;
    }
    let boundary = if raw[0] == 0 || raw[0] > 24 { 0 } else { 1u32 << raw[0] };
    let cap = u32::from_be_bytes([0, raw[1], raw[2], raw[3]]);
    Some((boundary, cap))
}

// ------------------------------------------------------------- Device

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    None,
    ToDevice,
    FromDevice,
}

/// How long to give a command, in milliseconds.
pub const T_SHORT: u32 = 30_000;
/// START UNIT waits for the spindle (#142): a large SAS HDD takes ~1 min.
pub const T_START: u32 = 3 * 60 * 1000;
pub const T_FORMAT_IMMED: u32 = 20 * 60 * 1000;
/// A full format without IMMED support: the command blocks until the
/// drive is done. Hours, on a large HDD.
pub const T_FORMAT_BLOCKING: u32 = 24 * 60 * 60 * 1000;

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::os::unix::io::AsRawFd;

    const SG_IO: libc::c_ulong = 0x2285;
    const SG_DXFER_NONE: i32 = -1;
    const SG_DXFER_TO_DEV: i32 = -2;
    const SG_DXFER_FROM_DEV: i32 = -3;
    const SENSE_LEN: usize = 64;

    #[repr(C)]
    struct SgIoHdr {
        interface_id: i32,
        dxfer_direction: i32,
        cmd_len: u8,
        mx_sb_len: u8,
        iovec_count: u16,
        dxfer_len: u32,
        dxferp: *mut libc::c_void,
        cmdp: *mut u8,
        sbp: *mut u8,
        timeout: u32,
        flags: u32,
        pack_id: i32,
        usr_ptr: *mut libc::c_void,
        status: u8,
        masked_status: u8,
        msg_status: u8,
        sb_len_wr: u8,
        host_status: u16,
        driver_status: u16,
        resid: i32,
        duration: u32,
        info: u32,
    }

    pub struct Device {
        file: std::fs::File,
        pub path: String,
    }

    impl Device {
        pub fn open(path: &str) -> Result<Self> {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?;
            Ok(Self {
                file,
                path: path.to_string(),
            })
        }

        /// Issue one command. Returns the number of data bytes actually
        /// transferred (dxfer_len - resid) for FROM_DEVICE transfers.
        pub fn io(&self, cdb: &[u8], dir: Dir, data: &mut [u8], timeout_ms: u32) -> Result<usize> {
            let mut sense = [0u8; SENSE_LEN];
            let mut cdb_buf = cdb.to_vec();
            let mut hdr = SgIoHdr {
                interface_id: b'S' as i32,
                dxfer_direction: match dir {
                    Dir::None => SG_DXFER_NONE,
                    Dir::ToDevice => SG_DXFER_TO_DEV,
                    Dir::FromDevice => SG_DXFER_FROM_DEV,
                },
                cmd_len: cdb.len() as u8,
                mx_sb_len: SENSE_LEN as u8,
                iovec_count: 0,
                dxfer_len: if dir == Dir::None { 0 } else { data.len() as u32 },
                dxferp: if dir == Dir::None {
                    std::ptr::null_mut()
                } else {
                    data.as_mut_ptr() as *mut libc::c_void
                },
                cmdp: cdb_buf.as_mut_ptr(),
                sbp: sense.as_mut_ptr(),
                timeout: timeout_ms,
                flags: 0,
                pack_id: 0,
                usr_ptr: std::ptr::null_mut(),
                status: 0,
                masked_status: 0,
                msg_status: 0,
                sb_len_wr: 0,
                host_status: 0,
                driver_status: 0,
                resid: 0,
                duration: 0,
                info: 0,
            };
            // SAFETY: hdr points at live buffers for the duration of the
            // call; the kernel only reads/writes within the lengths given.
            // `as _`, not a bare constant: glibc types the ioctl request as
            // c_ulong and musl as c_int, so passing a fixed-width value
            // compiles against one and not the other. The node's binaries are
            // musl-static, which is exactly where this broke. The NVMe path in
            // `smart/nvme.rs` already does the same thing.
            let rc = unsafe {
                libc::ioctl(self.file.as_raw_fd(), SG_IO as _, &mut hdr as *mut SgIoHdr)
            };
            if rc < 0 {
                return Err(Error::Io(std::io::Error::last_os_error()));
            }
            let driver_masked = hdr.driver_status & 0x0f;
            if hdr.status == 0 && hdr.host_status == 0 && driver_masked == 0 {
                let got = (hdr.dxfer_len as i64 - hdr.resid as i64).max(0) as usize;
                return Ok(got.min(data.len()));
            }
            if hdr.sb_len_wr > 0 {
                if let Some(s) = parse_sense(&sense[..hdr.sb_len_wr as usize]) {
                    record_not_good(&self.path, cdb, &format!("sense {:x}/{:02x}{:02x}", s.key, s.asc, s.ascq));
                    // A CHECK CONDITION with a recovered error is success.
                    if s.key == 0x1 {
                        let got = (hdr.dxfer_len as i64 - hdr.resid as i64).max(0) as usize;
                        return Ok(got.min(data.len()));
                    }
                    return Err(Error::Sense(s));
                }
            }
            record_not_good(&self.path, cdb, &format!("status {:02x} host {:x} driver {:x}", hdr.status, hdr.host_status, hdr.driver_status));
            Err(Error::Transport {
                status: hdr.status,
                host: hdr.host_status,
                driver: hdr.driver_status,
            })
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::Device;

#[cfg(not(target_os = "linux"))]
pub struct Device {
    pub path: String,
}

#[cfg(not(target_os = "linux"))]
impl Device {
    pub fn open(path: &str) -> Result<Self> {
        let _ = path;
        Err(Error::Unsupported("SG_IO requires Linux"))
    }
    pub fn io(&self, _cdb: &[u8], _dir: Dir, _data: &mut [u8], _timeout_ms: u32) -> Result<usize> {
        Err(Error::Unsupported("SG_IO requires Linux"))
    }
}

impl Device {
    pub fn test_unit_ready(&self) -> Result<()> {
        self.io(&cdb::test_unit_ready(), Dir::None, &mut [], T_SHORT)
            .map(|_| ())
    }

    /// START UNIT (`cdb::start_unit`), waiting for the drive to spin up.
    pub fn start_unit(&self) -> Result<()> {
        self.io(&cdb::start_unit(), Dir::None, &mut [], T_START).map(|_| ())
    }

    /// The drive's current sense data (`cdb::request_sense`).
    pub fn request_sense(&self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 252];
        let n = self.io(&cdb::request_sense(252), Dir::FromDevice, &mut buf, T_SHORT)?;
        buf.truncate(n);
        Ok(buf)
    }

    pub fn inquiry(&self) -> Result<Inquiry> {
        let mut buf = vec![0u8; 96];
        let n = self.io(&cdb::inquiry(None, 96), Dir::FromDevice, &mut buf, T_SHORT)?;
        parse_inquiry(&buf[..n])
    }

    pub fn vpd(&self, page: u8) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 1024];
        let n = self.io(&cdb::inquiry(Some(page), 1024), Dir::FromDevice, &mut buf, T_SHORT)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// READ(16): `blocks` × `block_len` bytes from `lba`.
    pub fn read16(&self, lba: u64, blocks: u32, block_len: u32) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; blocks as usize * block_len as usize];
        let n = self.io(&cdb::read16(lba, blocks), Dir::FromDevice, &mut buf, T_SHORT)?;
        buf.truncate(n);
        Ok(buf)
    }

    pub fn read_capacity16(&self) -> Result<Capacity> {
        let mut buf = [0u8; 32];
        let n = self.io(&cdb::read_capacity16(32), Dir::FromDevice, &mut buf, T_SHORT)?;
        parse_read_capacity16(&buf[..n])
    }

    pub fn mode_sense10(&self, page: u8) -> Result<ModeHeader10> {
        let mut buf = vec![0u8; 252];
        let n = self.io(&cdb::mode_sense10(page, 252), Dir::FromDevice, &mut buf, T_SHORT)?;
        parse_mode_sense10(&buf[..n])
    }

    /// MODE SENSE(10) with LLBAA=1, so a drive past 2^32 blocks returns its
    /// long LBA descriptor; asked again without when the drive refuses the
    /// bit (#129).
    pub fn mode_sense10_long(&self, page: u8) -> Result<ModeHeader10> {
        let mut buf = vec![0u8; 252];
        match self.io(&cdb::mode_sense10_llbaa(page, 252, true), Dir::FromDevice, &mut buf, T_SHORT) {
            Ok(n) => parse_mode_sense10(&buf[..n]),
            Err(Error::Sense(s)) if s.is_illegal_request() => self.mode_sense10(page),
            Err(e) => Err(e),
        }
    }

    pub fn mode_select10(&self, data: &[u8]) -> Result<()> {
        let mut d = data.to_vec();
        self.io(&cdb::mode_select10(d.len() as u16), Dir::ToDevice, &mut d, T_SHORT)
            .map(|_| ())
    }

    pub fn mode_select6(&self, data: &[u8]) -> Result<()> {
        let mut d = data.to_vec();
        self.io(&cdb::mode_select6(d.len() as u8), Dir::ToDevice, &mut d, T_SHORT)
            .map(|_| ())
    }

    /// FORMAT UNIT with a parameter list. With `immed`, returns once the
    /// drive accepts the command; poll `test_unit_ready` for progress.
    pub fn format_unit(&self, immed: bool) -> Result<()> {
        self.format_unit_pi(immed, 0)
    }

    /// FORMAT UNIT with FMTPINFO (see `cdb::format_unit_pi`).
    pub fn format_unit_pi(&self, immed: bool, fmtpinfo: u8) -> Result<()> {
        let mut p = format_unit_param(immed);
        let t = if immed { T_FORMAT_IMMED } else { T_FORMAT_BLOCKING };
        self.io(&cdb::format_unit_pi(fmtpinfo), Dir::ToDevice, &mut p, t).map(|_| ())
    }

    /// FORMAT UNIT with FMTPINFO and FAST FORMAT (`cdb::format_unit_ffmt`).
    pub fn format_unit_ffmt(&self, immed: bool, fmtpinfo: u8, ffmt: u8) -> Result<()> {
        let mut p = format_unit_param(immed);
        let t = if immed { T_FORMAT_IMMED } else { T_FORMAT_BLOCKING };
        self.io(&cdb::format_unit_ffmt(fmtpinfo, ffmt), Dir::ToDevice, &mut p, t).map(|_| ())
    }

    /// REPORT SUPPORTED OPERATION CODES for one opcode.
    pub fn supported_opcode(&self, op: u8) -> Result<OneCommand> {
        let mut buf = vec![0u8; 64];
        let n = self.io(&cdb::report_supported_opcode(op, 64), Dir::FromDevice, &mut buf, T_SHORT)?;
        parse_one_command(&buf[..n]).ok_or(Error::Short(n))
    }

    pub fn receive_diagnostic(&self, page: u8) -> Result<Vec<u8>> {
        const ALLOC: usize = 0xfffc;
        let mut buf = vec![0u8; ALLOC];
        let n = self.io(
            &cdb::receive_diagnostic(page, ALLOC as u16),
            Dir::FromDevice,
            &mut buf,
            T_SHORT,
        )?;
        buf.truncate(n);
        Ok(buf)
    }

    pub fn send_diagnostic(&self, page: &[u8]) -> Result<()> {
        self.send_diagnostic_timeout(page, T_SHORT)
    }

    /// SEND DIAGNOSTIC with its own timeout: a Download Microcode chunk may
    /// be written to flash before the enclosure answers (#35).
    pub fn send_diagnostic_timeout(&self, page: &[u8], timeout_ms: u32) -> Result<()> {
        let mut d = page.to_vec();
        self.io(&cdb::send_diagnostic(d.len() as u16), Dir::ToDevice, &mut d, timeout_ms)
            .map(|_| ())
    }

    /// One WRITE BUFFER chunk (or the data-less activate, mode 0x0f).
    pub fn write_buffer(&self, mode: u8, offset: u32, data: &[u8], timeout_ms: u32) -> Result<()> {
        let mut d = data.to_vec();
        let dir = if d.is_empty() { Dir::None } else { Dir::ToDevice };
        self.io(&cdb::write_buffer(mode, offset, d.len() as u32), dir, &mut d, timeout_ms)
            .map(|_| ())
    }

    pub fn read_buffer_descriptor(&self) -> Result<(u32, u32)> {
        let mut buf = [0u8; 4];
        let n = self.io(&cdb::read_buffer_descriptor(), Dir::FromDevice, &mut buf, T_SHORT)?;
        parse_read_buffer_descriptor(&buf[..n]).ok_or(Error::Short(n))
    }
}

/// The /dev/sgN node for a block device, when the sg driver exposes one.
/// SG_IO also works on the block node, but sg is the classic door and
/// keeps working when sd has given up on the device (size 0).
pub fn sg_path_for_block(name: &str) -> Option<String> {
    sg_path_in(&format!("/sys/block/{name}/device/scsi_generic"))
}

/// A drive that is on the HBA and answers, but NOT READY (#141): what it
/// says, honestly. forge1's unformatted SAS drives are this: present (the
/// iDRAC lists them, INQUIRY and VPD 0x83 answer), with no usable disk
/// until they are formatted, which is the install's to do (stormblock#592).
/// None for any other answer.
pub fn not_ready_reason(s: &Sense) -> Option<String> {
    if s.key != 0x2 || s.is_medium_format_corrupted() {
        return None;
    }
    let what = match (s.asc, s.ascq) {
        (0x04, 0x04) => "a format is running",
        (0x04, 0x01) => "becoming ready",
        (0x04, 0x02) => "needs a START UNIT",
        (0x04, 0x03) => "needs manual intervention",
        (0x04, 0x1b) | (0x04, 0x1c) => "a sanitize is running",
        (0x04, _) => "not ready, cause in the code",
        (0x3a, _) => "no medium",
        _ => "not ready",
    };
    Some(format!(
        "present on the HBA, not ready ({:x}/{:02x}/{:02x}: {what}): unformatted or mid-format; no usable disk until it is formatted",
        s.key, s.asc, s.ascq
    ))
}

/// Why a SCSI disk the kernel gave no block node needs a format (#131),
/// from its answer to READ CAPACITY(16) over sg: an interrupted format
/// (31/00), NOT READY (#141), or whatever else it said.
pub fn no_block_node_reason(cap: &Result<Capacity>) -> String {
    match cap {
        Err(Error::Sense(s)) if s.is_medium_format_corrupted() => {
            format!("medium format corrupted ({:x}/{:02x}/{:02x}): an interrupted format; format it again", s.key, s.asc, s.ascq)
        }
        Err(Error::Sense(s)) if not_ready_reason(s).is_some() => not_ready_reason(s).unwrap_or_default(),
        Err(e) => format!("no block device; read capacity over sg: {e}; a format may bring it back"),
        Ok(c) => format!("no block device; the drive reports {}-byte blocks over sg", c.block_len),
    }
}

/// The /dev/sgN node for a SCSI device directory (…/scsi_generic/sgN).
pub fn sg_path_in(scsi_generic_dir: &str) -> Option<String> {
    let entries = std::fs::read_dir(scsi_generic_dir).ok()?;
    for e in entries.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with("sg") {
            return Some(format!("/dev/{n}"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_format_cdb_and_ffmt_support() {
        assert_eq!(cdb::format_unit_ffmt(0b10, 1), [0x04, 0x90, 0, 0, 0x01, 0]);
        assert_eq!(cdb::format_unit_pi(0), [0x04, 0x10, 0, 0, 0, 0], "normal format: FFMT 00b");
        assert_eq!(cdb::report_supported_opcode(0x04, 64), [0xa3, 0x0c, 0x01, 0x04, 0, 0, 0, 0, 0, 64, 0, 0]);
        // Supported as the standard says, 6-byte usage with FFMT bits.
        let raw = [0, 0b011, 0, 6, 0x04, 0xff, 0x00, 0x00, 0x03, 0x07];
        let c = parse_one_command(&raw).unwrap();
        assert_eq!(c.usage, vec![0x04, 0xff, 0, 0, 0x03, 0x07]);
        assert_eq!(ffmt_supported(&c), Some(true));
        // Lists FORMAT UNIT, byte 4 reserved → no fast format.
        let raw = [0, 0b011, 0, 6, 0x04, 0xff, 0x00, 0x00, 0x00, 0x07];
        assert_eq!(ffmt_supported(&parse_one_command(&raw).unwrap()), Some(false));
        // Not supported / no data: only trying tells.
        let raw = [0, 0b001, 0, 0];
        assert_eq!(ffmt_supported(&parse_one_command(&raw).unwrap()), None);
        assert!(parse_one_command(&[0, 3]).is_none());
    }

    #[test]
    fn an_interrupted_format_is_named() {
        let r = no_block_node_reason(&Err(Error::Sense(Sense::new(3, 0x31, 0))));
        assert_eq!(r, "medium format corrupted (3/31/00): an interrupted format; format it again");
        assert!(Sense::new(2, 0x31, 0).is_medium_format_corrupted());
        assert!(!Sense::new(3, 0x31, 1).is_medium_format_corrupted(), "31/01 is a format that failed, not one cut off");
        assert!(no_block_node_reason(&Err(Error::Sense(Sense::new(4, 0x44, 0)))).starts_with("no block device"));
    }

    #[test]
    fn a_not_ready_drive_is_said_to_be_present() {
        let r = no_block_node_reason(&Err(Error::Sense(Sense::new(2, 4, 0))));
        assert_eq!(
            r,
            "present on the HBA, not ready (2/04/00: not ready, cause in the code): unformatted or mid-format; no usable disk until it is formatted"
        );
        assert!(no_block_node_reason(&Err(Error::Sense(Sense::new(2, 4, 4)))).contains("a format is running"));
        assert!(not_ready_reason(&Sense::new(2, 0x31, 0)).is_none(), "31/00 is the interrupted format's wording");
        assert!(not_ready_reason(&Sense::new(5, 0x24, 0)).is_none());
    }

    #[test]
    fn not_good_counts_by_opcode_and_page() {
        record_not_good("/dev/sgT", &[0x4D, 0, 0x40 | 0x11, 0, 0, 0, 0, 4, 0, 0], "sense 5/2400");
        record_not_good("/dev/sgT", &[0x4D, 0, 0x40 | 0x11, 0, 0, 0, 0, 4, 0, 0], "sense 5/2400");
        record_not_good("/dev/sgT", &[0x85, 8, 0x0e, 0, 0xD1], "sense 1/0000");
        let m = not_good();
        assert_eq!(m["/dev/sgT"]["4d/11 sense 5/2400"], 2);
        assert_eq!(m["/dev/sgT"]["85/d1 sense 1/0000"], 1);
    }

    #[test]
    fn fixed_sense_with_progress() {
        // NOT READY, format in progress 04/04, SKSV set, progress 0x8000 = 50%.
        let mut raw = vec![0u8; 18];
        raw[0] = 0x70;
        raw[2] = 0x02;
        raw[7] = 10;
        raw[12] = 0x04;
        raw[13] = 0x04;
        raw[15] = 0x80;
        raw[16] = 0x80;
        raw[17] = 0x00;
        let s = parse_sense(&raw).unwrap();
        assert_eq!(s.key, 2);
        assert!(s.is_format_in_progress());
        assert_eq!(s.progress, Some(0x8000));
        assert_eq!(s.progress_pct(), Some(50));
        assert_eq!(s.key_name(), "not ready");
    }

    #[test]
    fn fixed_sense_without_sksv_has_no_progress() {
        let mut raw = vec![0u8; 18];
        raw[0] = 0x70;
        raw[2] = 0x05;
        raw[12] = 0x20;
        let s = parse_sense(&raw).unwrap();
        assert!(s.is_illegal_request());
        assert_eq!(s.progress, None);
    }

    #[test]
    fn descriptor_sense_with_progress() {
        // 0x72, key 2, asc 04, ascq 04, one SKS descriptor (type 2, len 6).
        let raw = [
            0x72, 0x02, 0x04, 0x04, 0, 0, 0, 8, // header, add_len 8
            0x02, 0x06, 0, 0, 0x80, 0x40, 0x00, 0, // SKS: SKSV, progress 0x4000
        ];
        let s = parse_sense(&raw).unwrap();
        assert!(s.is_format_in_progress());
        assert_eq!(s.progress_pct(), Some(25));
    }

    #[test]
    fn garbage_sense_is_none() {
        assert!(parse_sense(&[]).is_none());
        assert!(parse_sense(&[0x11, 0, 0]).is_none());
        assert!(parse_sense(&[0x70, 0, 0]).is_none(), "truncated fixed");
    }

    #[test]
    fn read_capacity16_decodes_520_byte_drive() {
        let mut raw = [0u8; 32];
        // 2344225968 blocks → last LBA 2344225967 = 0x8BBC_1BAF
        raw[0..8].copy_from_slice(&2344225967u64.to_be_bytes());
        raw[8..12].copy_from_slice(&520u32.to_be_bytes());
        let c = parse_read_capacity16(&raw).unwrap();
        assert_eq!(c.block_len, 520);
        assert_eq!(c.blocks(), 2344225968);
        assert_eq!(c.bytes(), 2344225968 * 520);
        assert_eq!(c.physical_block_len(), 520);
        assert_eq!(c.prot_type, 0);
    }

    #[test]
    fn read_capacity16_physical_and_protection() {
        let mut raw = [0u8; 32];
        raw[0..8].copy_from_slice(&999u64.to_be_bytes());
        raw[8..12].copy_from_slice(&512u32.to_be_bytes());
        raw[12] = 0x03; // PROT_EN, P_TYPE 1 → type 2
        raw[13] = 0x03; // 8 logical per physical
        let c = parse_read_capacity16(&raw).unwrap();
        assert_eq!(c.physical_block_len(), 4096);
        assert_eq!(c.prot_type, 2);
        assert!(matches!(parse_read_capacity16(&raw[..10]), Err(Error::Short(10))));
    }

    #[test]
    fn inquiry_decodes_enclosure() {
        let mut raw = vec![0u8; 96];
        raw[0] = 0x0d;
        raw[8..16].copy_from_slice(b"NETAPP  ");
        raw[16..32].copy_from_slice(b"DS22412IOM12A   ");
        raw[32..36].copy_from_slice(b"0401");
        let i = parse_inquiry(&raw).unwrap();
        assert_eq!(i.device_type, 13);
        assert_eq!(i.vendor, "NETAPP");
        assert_eq!(i.product, "DS22412IOM12A");
        assert_eq!(i.revision, "0401");
    }

    #[test]
    fn mode_sense10_pulls_block_descriptor() {
        let raw = [
            0x00, 0x16, 0x00, 0x10, 0x00, 0x00, 0x00, 0x08, // header, bd len 8
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x08, // bd: 520-byte blocks
            0x01, 0x0a, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // page 1
        ];
        let h = parse_mode_sense10(&raw).unwrap();
        assert!(!h.long_lba);
        assert_eq!(h.device_specific, 0x10);
        let bd = h.block_descriptor.unwrap();
        assert_eq!(bd.len(), 8);
        assert_eq!(u32::from_be_bytes([0, bd[5], bd[6], bd[7]]), 520);
        let none = parse_mode_sense10(&[0, 4, 0, 0, 0, 0, 0, 0]).unwrap();
        assert!(none.block_descriptor.is_none());
    }

    fn header(long: bool, bd: Vec<u8>) -> ModeHeader10 {
        ModeHeader10 { medium_type: 0, device_specific: 0x10, long_lba: long, block_descriptor: Some(bd) }
    }

    #[test]
    fn mode_select_lists_carry_block_length_and_zero_count() {
        // A 10 TB drive at 512, short descriptor: count FFFFFFFFh.
        let short = header(false, vec![0xff, 0xff, 0xff, 0xff, 0, 0, 0x02, 0x00]);
        let v = mode_select10_list(&short, 4096, true);
        assert_eq!(v.len(), 16);
        assert_eq!(&v[0..2], &[0, 0], "mode data length reserved");
        assert_eq!(v[3], 0, "device-specific zero on select");
        assert_eq!(v[4], 0, "no LONGLBA");
        assert_eq!(v[7], 8, "block descriptor length");
        assert_eq!(&v[8..12], &[0, 0, 0, 0], "number of blocks 0 = the most at 4096 (not FF000000h, #129)");
        assert_eq!(u32::from_be_bytes([0, v[13], v[14], v[15]]), 4096);

        // The same drive's long descriptor: 19532873728 blocks of 512.
        let mut bd = vec![0u8; 16];
        bd[0..8].copy_from_slice(&19_532_873_728u64.to_be_bytes());
        bd[12..16].copy_from_slice(&512u32.to_be_bytes());
        let long = header(true, bd);
        let v = mode_select10_list(&long, 4096, true);
        assert_eq!(v.len(), 24);
        assert_eq!(v[4], 1, "LONGLBA as the drive returned it");
        assert_eq!(v[7], 16);
        assert_eq!(&v[8..16], &[0; 8], "count 0");
        assert_eq!(u32::from_be_bytes(v[20..24].try_into().unwrap()), 4096);
        // The short fallback from a long answer.
        let v = mode_select10_list(&long, 4096, false);
        assert_eq!((v.len(), v[4], v[7]), (16, 0, 8));
        assert_eq!(u32::from_be_bytes([0, v[13], v[14], v[15]]), 4096);

        let v6 = mode_select6_list(&long, 512);
        assert_eq!(v6.len(), 12);
        assert_eq!(v6[3], 8);
        assert_eq!(&v6[4..8], &[0; 4]);
        assert_eq!(u32::from_be_bytes([0, v6[9], v6[10], v6[11]]), 512);

        // No descriptor came back: a zero one with the length.
        let none = ModeHeader10 { medium_type: 0, device_specific: 0, long_lba: false, block_descriptor: None };
        let v = mode_select10_list(&none, 4096, true);
        assert_eq!(&v[8..13], &[0; 5]);
        assert_eq!(u32::from_be_bytes([0, v[13], v[14], v[15]]), 4096);
    }

    #[test]
    fn illegal_request_names_the_field() {
        // Fixed: 05/26/00, SKSV, C/D=0, BPV bit 4, parameter list byte 8.
        let mut raw = vec![0u8; 18];
        raw[0] = 0x70;
        raw[2] = 0x05;
        raw[7] = 10;
        raw[12] = 0x26;
        raw[15] = 0x80 | 0x08 | 0x04;
        raw[17] = 8;
        let s = parse_sense(&raw).unwrap();
        assert_eq!(s.field, Some(FieldPointer { in_cdb: false, bit: Some(4), byte: 8 }));
        assert_eq!(s.progress, None);
        assert_eq!(
            s.to_string(),
            "illegal request (key 0x5, asc/ascq 0x26/0x00, field pointer: parameter list byte 8 bit 4)"
        );
        // Descriptor: 05/24/00 in the CDB byte 1, no bit.
        let raw = [0x72, 0x05, 0x24, 0x00, 0, 0, 0, 8, 0x02, 0x06, 0, 0, 0x80 | 0x40, 0x00, 0x01, 0];
        let s = parse_sense(&raw).unwrap();
        assert_eq!(s.field, Some(FieldPointer { in_cdb: true, bit: None, byte: 1 }));
        assert!(s.to_string().ends_with("field pointer: cdb byte 1)"), "{s}");
        // No SKSV: nothing named; other keys never read it as a pointer.
        let mut nr = vec![0u8; 18];
        nr[0] = 0x70;
        nr[2] = 0x02;
        nr[15] = 0x80;
        assert_eq!(parse_sense(&nr).unwrap().field, None);
        assert_eq!(cdb::mode_sense10_llbaa(1, 252, true)[1], 0x10, "LLBAA");
    }

    #[test]
    fn format_unit_param_immed_bit() {
        assert_eq!(format_unit_param(true), [0, 0x02, 0, 0]);
        assert_eq!(format_unit_param(false), [0, 0, 0, 0]);
        assert_eq!(cdb::format_unit()[1] & 0x10, 0x10, "FMTDATA");
    }

    #[test]
    fn write_buffer_cdb_and_descriptor() {
        let c = cdb::write_buffer(0x0e, 0x012345, 0x8000);
        assert_eq!(c[0], 0x3b);
        assert_eq!(c[1], 0x0e);
        assert_eq!(&c[3..6], &[0x01, 0x23, 0x45]);
        assert_eq!(&c[6..9], &[0x00, 0x80, 0x00]);
        assert_eq!(cdb::write_buffer(0x0f, 0, 0)[1], 0x0f);
        assert_eq!(parse_read_buffer_descriptor(&[0x0f, 0x10, 0, 0]), Some((32768, 0x100000)));
        assert_eq!(parse_read_buffer_descriptor(&[0, 0, 0x40, 0]), Some((0, 0x4000)));
        assert_eq!(parse_read_buffer_descriptor(&[1]), None);
    }

    #[test]
    fn cdb_shapes() {
        assert_eq!(cdb::inquiry(Some(0x80), 255), [0x12, 1, 0x80, 0, 255, 0]);
        assert_eq!(cdb::read_capacity16(32)[0..2], [0x9e, 0x10]);
        assert_eq!(cdb::start_unit(), [0x1b, 0, 0, 0, 1, 0], "IMMED=0, START=1");
        assert_eq!(cdb::receive_diagnostic(0x02, 0xfffc), [0x1c, 1, 2, 0xff, 0xfc, 0]);
        assert_eq!(cdb::send_diagnostic(0x100), [0x1d, 0x10, 0, 1, 0, 0]);
        assert_eq!(cdb::mode_select10(16), [0x55, 0x10, 0, 0, 0, 0, 0, 0, 16, 0]);
        assert_eq!(cdb::mode_sense10(1, 252)[2], 0x01, "current values, page 1");
    }
}
