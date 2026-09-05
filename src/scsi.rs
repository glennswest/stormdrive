//! Raw SCSI over SG_IO — the commands the kernel does not wrap for us:
//! READ CAPACITY(16) on a drive the sd driver refused (520-byte sectors),
//! MODE SELECT + FORMAT UNIT to change the sector size, TEST UNIT READY
//! for format progress, and RECEIVE/SEND DIAGNOSTIC for SES pages.
//!
//! Everything that can be tested without hardware — sense decoding, CDB
//! and parameter-list construction, response parsing — is portable and
//! unit-tested here. Only `Device` (the fd + ioctl) is Linux-only.

use std::fmt;

/// Decoded sense data (fixed or descriptor format).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sense {
    pub key: u8,
    pub asc: u8,
    pub ascq: u8,
    /// Sense-key-specific progress indication (0..=65535 of the whole),
    /// present while a FORMAT UNIT runs (key NOT READY, 04/04).
    pub progress: Option<u16>,
}

impl Sense {
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

    /// Progress as a percentage, when the device reported one.
    pub fn progress_pct(&self) -> Option<u8> {
        self.progress.map(|p| ((p as u32 * 100) / 65536) as u8)
    }
}

impl fmt::Display for Sense {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (key 0x{:x}, asc/ascq 0x{:02x}/0x{:02x})",
            self.key_name(),
            self.key,
            self.asc,
            self.ascq
        )
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
            Some(Sense {
                key,
                asc,
                ascq,
                progress,
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
                }
                i += 2 + dlen;
            }
            Some(Sense {
                key,
                asc,
                ascq,
                progress,
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

/// The MODE SELECT parameter list that sets a new logical block length:
/// an 8-byte MODE SELECT(10) header (mode data length reserved, block
/// descriptor length 8) and one short block descriptor whose NUMBER OF
/// LOGICAL BLOCKS is 0 — "all remaining blocks take these characteristics"
/// (SBC). Density code and medium type are kept from what the drive
/// reported; everything else is zero.
pub fn mode_select10_block_length(block_len: u32, medium_type: u8, density: u8) -> Vec<u8> {
    let mut v = vec![0u8; 16];
    v[2] = medium_type;
    v[7] = 8;
    v[8] = density;
    v[13] = ((block_len >> 16) & 0xff) as u8;
    v[14] = ((block_len >> 8) & 0xff) as u8;
    v[15] = (block_len & 0xff) as u8;
    v
}

/// Same, in MODE SELECT(6) shape (4-byte header) for drives that reject
/// the 10-byte form.
pub fn mode_select6_block_length(block_len: u32, medium_type: u8, density: u8) -> Vec<u8> {
    let mut v = vec![0u8; 12];
    v[1] = medium_type;
    v[3] = 8;
    v[4] = density;
    v[9] = ((block_len >> 16) & 0xff) as u8;
    v[10] = ((block_len >> 8) & 0xff) as u8;
    v[11] = (block_len & 0xff) as u8;
    v
}

/// FORMAT UNIT short parameter list header: FOV=0 (drive defaults for
/// DPRY/DCRT/STPF/IP), IMMED as asked, no defect list.
pub fn format_unit_param(immed: bool) -> [u8; 4] {
    [0, if immed { 0x02 } else { 0 }, 0, 0]
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
    pub fn read_capacity16(alloc: u32) -> [u8; 16] {
        let a = alloc.to_be_bytes();
        [0x9e, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, a[0], a[1], a[2], a[3], 0, 0]
    }
    /// Current values, page 0x01 (mandatory), block descriptors included.
    pub fn mode_sense10(page: u8, alloc: u16) -> [u8; 10] {
        let [hi, lo] = alloc.to_be_bytes();
        [0x5a, 0x00, page & 0x3f, 0, 0, 0, 0, hi, lo, 0]
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
        [0x04, 0x10, 0, 0, 0, 0]
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
            let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), SG_IO, &mut hdr as *mut SgIoHdr) };
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
                    // A CHECK CONDITION with a recovered error is success.
                    if s.key == 0x1 {
                        let got = (hdr.dxfer_len as i64 - hdr.resid as i64).max(0) as usize;
                        return Ok(got.min(data.len()));
                    }
                    return Err(Error::Sense(s));
                }
            }
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
        let mut p = format_unit_param(immed);
        let t = if immed { T_FORMAT_IMMED } else { T_FORMAT_BLOCKING };
        self.io(&cdb::format_unit(), Dir::ToDevice, &mut p, t).map(|_| ())
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
        let mut d = page.to_vec();
        self.io(&cdb::send_diagnostic(d.len() as u16), Dir::ToDevice, &mut d, T_SHORT)
            .map(|_| ())
    }
}

/// The /dev/sgN node for a block device, when the sg driver exposes one.
/// SG_IO also works on the block node, but sg is the classic door and
/// keeps working when sd has given up on the device (size 0).
pub fn sg_path_for_block(name: &str) -> Option<String> {
    sg_path_in(&format!("/sys/block/{name}/device/scsi_generic"))
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

    #[test]
    fn mode_select_lists_carry_block_length_and_zero_count() {
        let v = mode_select10_block_length(4096, 0, 0);
        assert_eq!(v.len(), 16);
        assert_eq!(&v[0..2], &[0, 0], "mode data length reserved");
        assert_eq!(v[7], 8, "block descriptor length");
        assert_eq!(&v[9..12], &[0, 0, 0], "number of blocks 0 = all");
        assert_eq!(u32::from_be_bytes([0, v[13], v[14], v[15]]), 4096);

        let v6 = mode_select6_block_length(512, 0, 0);
        assert_eq!(v6.len(), 12);
        assert_eq!(v6[3], 8);
        assert_eq!(u32::from_be_bytes([0, v6[9], v6[10], v6[11]]), 512);
    }

    #[test]
    fn format_unit_param_immed_bit() {
        assert_eq!(format_unit_param(true), [0, 0x02, 0, 0]);
        assert_eq!(format_unit_param(false), [0, 0, 0, 0]);
        assert_eq!(cdb::format_unit()[1] & 0x10, 0x10, "FMTDATA");
    }

    #[test]
    fn cdb_shapes() {
        assert_eq!(cdb::inquiry(Some(0x80), 255), [0x12, 1, 0x80, 0, 255, 0]);
        assert_eq!(cdb::read_capacity16(32)[0..2], [0x9e, 0x10]);
        assert_eq!(cdb::receive_diagnostic(0x02, 0xfffc), [0x1c, 1, 2, 0xff, 0xfc, 0]);
        assert_eq!(cdb::send_diagnostic(0x100), [0x1d, 0x10, 0, 1, 0, 0]);
        assert_eq!(cdb::mode_select10(16), [0x55, 0x10, 0, 0, 0, 0, 0, 0, 16, 0]);
        assert_eq!(cdb::mode_sense10(1, 252)[2], 0x01, "current values, page 1");
    }
}
