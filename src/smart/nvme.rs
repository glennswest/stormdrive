//! NVMe SMART/Health via the kernel admin-command passthrough
//! (`NVME_IOCTL_ADMIN_CMD`, Get Log Page 0x02). Same mechanism as
//! stormblock's must-gather collector (stormblock `src/main.rs:2342`), but
//! feeding the live health pipeline instead of a text bundle.

use super::Sample;

/// Decode a SMART/Health Information log page (512 bytes, Log Identifier
/// 02h) into a Sample. Portable and unit-tested; the ioctl that fetches the
/// page is Linux-only below.
pub fn decode_smart_page(page: &[u8; 512]) -> Sample {
    let u128_at = |off: usize| -> u64 {
        let mut b = [0u8; 16];
        b.copy_from_slice(&page[off..off + 16]);
        u128::from_le_bytes(b).min(u64::MAX as u128) as u64
    };
    let kelvin = u16::from_le_bytes([page[1], page[2]]);
    let temperature_c = if kelvin == 0 {
        None
    } else {
        Some(kelvin as i32 - 273)
    };
    Sample {
        critical_warning: page[0],
        temperature_c,
        available_spare_pct: Some(page[3].min(100)),
        wear_pct: Some(page[5]),
        power_on_hours: Some(u128_at(128)),
        media_errors: u128_at(160),
        kernel_ok: true,
        messages: Vec::new(),
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::os::fd::AsRawFd;

    /// struct nvme_admin_cmd from linux/nvme_ioctl.h — 72 bytes.
    #[repr(C)]
    #[derive(Default)]
    struct NvmeAdminCmd {
        opcode: u8,
        flags: u8,
        rsvd1: u16,
        nsid: u32,
        cdw2: u32,
        cdw3: u32,
        metadata: u64,
        addr: u64,
        metadata_len: u32,
        data_len: u32,
        cdw10: u32,
        cdw11: u32,
        cdw12: u32,
        cdw13: u32,
        cdw14: u32,
        cdw15: u32,
        timeout_ms: u32,
        result: u32,
    }

    /// _IOWR('N', 0x41, struct nvme_admin_cmd) with sizeof == 72.
    /// Kept as u64 and cast at the call site: libc's ioctl request type is
    /// c_ulong on glibc but c_int on musl.
    const NVME_IOCTL_ADMIN_CMD: u64 = 0xC048_4E41;

    pub fn fetch_smart_page(path: &str) -> std::io::Result<[u8; 512]> {
        let f = std::fs::File::open(path)?;
        let mut page = [0u8; 512];
        let mut cmd = NvmeAdminCmd {
            opcode: 0x02, // Get Log Page
            nsid: 0xFFFF_FFFF,
            addr: page.as_mut_ptr() as u64,
            data_len: 512,
            // LID 0x02 | NUMDL (dwords - 1) in bits 16..31
            cdw10: 0x02 | (((512 / 4 - 1) as u32) << 16),
            timeout_ms: 5_000,
            ..Default::default()
        };
        let ret = unsafe {
            libc::ioctl(
                f.as_raw_fd(),
                NVME_IOCTL_ADMIN_CMD as _,
                &mut cmd as *mut NvmeAdminCmd,
            )
        };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if ret > 0 {
            return Err(std::io::Error::other(format!("nvme status 0x{ret:x}")));
        }
        Ok(page)
    }

    const _: () = assert!(std::mem::size_of::<NvmeAdminCmd>() == 72);
}

/// Collect health for an NVMe namespace block device. `name` is only used
/// for messages.
pub fn collect(path: &str, name: &str) -> Sample {
    #[cfg(target_os = "linux")]
    {
        match linux::fetch_smart_page(path) {
            Ok(page) => decode_smart_page(&page),
            Err(e) => Sample {
                kernel_ok: false,
                messages: vec![format!("{name}: SMART log page read failed: {e}")],
                ..Default::default()
            },
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Sample {
            kernel_ok: true,
            messages: vec![format!("{name}: NVMe SMART unavailable on this platform")],
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_synthetic_smart_page() {
        let mut page = [0u8; 512];
        page[0] = 0x04; // reliability degraded
        let kelvin = (42 + 273) as u16;
        page[1..3].copy_from_slice(&kelvin.to_le_bytes());
        page[3] = 87; // available spare
        page[5] = 13; // percentage used
        page[128..144].copy_from_slice(&1234u128.to_le_bytes()); // POH
        page[160..176].copy_from_slice(&7u128.to_le_bytes()); // media errors

        let s = decode_smart_page(&page);
        assert_eq!(s.critical_warning, 0x04);
        assert_eq!(s.temperature_c, Some(42));
        assert_eq!(s.available_spare_pct, Some(87));
        assert_eq!(s.wear_pct, Some(13));
        assert_eq!(s.power_on_hours, Some(1234));
        assert_eq!(s.media_errors, 7);
        assert!(s.kernel_ok);
    }

    #[test]
    fn zero_temperature_reads_as_unknown() {
        let page = [0u8; 512];
        let s = decode_smart_page(&page);
        assert_eq!(s.temperature_c, None);
        assert_eq!(s.available_spare_pct, Some(0));
    }
}
