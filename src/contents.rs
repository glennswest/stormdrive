//! What is on a drive that a destructive operation would destroy.
//!
//! `/proc/mounts` is not enough on stormcos: the root filesystem sits on a
//! ublk device that stormblock serves out of slabs on the system disk, so
//! that disk never appears as mounted and stormblock does not open it
//! `O_EXCL`. Its `/api/v1/drives` does not list it either (the slabs attach
//! by path). The disk itself says so, though — a stormcos disk is a GPT
//! whose partitions start with a stormblock slab header — so we read it.
//!
//! The parsers are portable and tested; the read is Linux-only.

/// stormblock's slab header magic (stormblock `src/drive/slab.rs`).
pub const SLAB_MAGIC: &[u8; 8] = b"STRMSLAB";

const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";

pub fn is_slab_header(buf: &[u8]) -> bool {
    buf.len() >= 8 && &buf[..8] == SLAB_MAGIC
}

/// The fields of a GPT header we need to walk its entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GptHeader {
    pub entries_lba: u64,
    pub entries: u32,
    pub entry_size: u32,
}

pub fn parse_gpt_header(buf: &[u8]) -> Option<GptHeader> {
    if buf.len() < 92 || &buf[..8] != GPT_SIGNATURE {
        return None;
    }
    let u32_at = |o: usize| u32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
    let h = GptHeader {
        entries_lba: u64::from_le_bytes(buf[72..80].try_into().unwrap()),
        entries: u32_at(80),
        entry_size: u32_at(84),
    };
    // Sanity bounds: a corrupt header must not make us read gigabytes.
    if h.entries_lba < 2 || h.entries == 0 || h.entries > 1024 {
        return None;
    }
    if h.entry_size < 128 || h.entry_size > 4096 || h.entry_size % 8 != 0 {
        return None;
    }
    Some(h)
}

/// One in-use GPT entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptPartition {
    pub index: usize,
    pub first_lba: u64,
    pub name: String,
}

pub fn parse_gpt_entries(buf: &[u8], h: &GptHeader) -> Vec<GptPartition> {
    let size = h.entry_size as usize;
    buf.chunks_exact(size)
        .take(h.entries as usize)
        .enumerate()
        .filter(|(_, e)| e[..16].iter().any(|&b| b != 0))
        .filter_map(|(i, e)| {
            let first = u64::from_le_bytes(e[32..40].try_into().unwrap());
            let last = u64::from_le_bytes(e[40..48].try_into().unwrap());
            if first == 0 || last < first {
                return None;
            }
            let units: Vec<u16> = e[56..128]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&u| u != 0)
                .collect();
            Some(GptPartition {
                index: i + 1,
                first_lba: first,
                name: String::from_utf16_lossy(&units),
            })
        })
        .collect()
}

/// The sentence the API and UI show: whose data is on the drive.
pub fn describe_slabs(whole_drive: bool, partitions: &[String]) -> Option<String> {
    if whole_drive {
        return Some("stormblock (a slab on the whole drive)".into());
    }
    if partitions.is_empty() {
        return None;
    }
    Some(format!("stormblock (slabs in partitions {})", partitions.join(", ")))
}

/// Read the drive and say who holds it, or None when it carries no
/// stormblock slab (or cannot be read — a 520-byte drive, a drive mid
/// format; those have nothing the kernel could have put there).
pub fn probe(path: &str) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        linux::probe(path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        None
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::fs::File;
    use std::os::unix::fs::FileExt;

    fn read(f: &File, off: u64, len: usize) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; len];
        f.read_exact_at(&mut buf, off).ok()?;
        Some(buf)
    }

    pub fn probe(path: &str) -> Option<String> {
        let f = File::open(path).ok()?;
        if is_slab_header(&read(&f, 0, 512)?) {
            return describe_slabs(true, &[]);
        }
        // The GPT header is at LBA 1; which LBA size depends on the drive.
        for lbs in [512u64, 4096] {
            let Some(h) = read(&f, lbs, 512).as_deref().and_then(parse_gpt_header) else {
                continue;
            };
            let len = h.entries as usize * h.entry_size as usize;
            let Some(table) = read(&f, h.entries_lba * lbs, len) else {
                continue;
            };
            let slabs: Vec<String> = parse_gpt_entries(&table, &h)
                .into_iter()
                .filter(|p| {
                    read(&f, p.first_lba * lbs, 512).is_some_and(|b| is_slab_header(&b))
                })
                .map(|p| {
                    if p.name.is_empty() {
                        format!("#{}", p.index)
                    } else {
                        format!("{} '{}'", p.index, p.name)
                    }
                })
                .collect();
            return describe_slabs(false, &slabs);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(entries_lba: u64, entries: u32, entry_size: u32) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[..8].copy_from_slice(GPT_SIGNATURE);
        b[72..80].copy_from_slice(&entries_lba.to_le_bytes());
        b[80..84].copy_from_slice(&entries.to_le_bytes());
        b[84..88].copy_from_slice(&entry_size.to_le_bytes());
        b
    }

    fn entry(first: u64, last: u64, name: &str) -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[0] = 0xAF; // any non-zero type GUID
        e[32..40].copy_from_slice(&first.to_le_bytes());
        e[40..48].copy_from_slice(&last.to_le_bytes());
        for (i, u) in name.encode_utf16().enumerate() {
            e[56 + 2 * i..58 + 2 * i].copy_from_slice(&u.to_le_bytes());
        }
        e
    }

    #[test]
    fn slab_magic() {
        let mut b = vec![0u8; 512];
        assert!(!is_slab_header(&b));
        b[..8].copy_from_slice(SLAB_MAGIC);
        assert!(is_slab_header(&b));
        assert!(!is_slab_header(&b[..4]));
    }

    #[test]
    fn gpt_header_parse_and_bounds() {
        let h = parse_gpt_header(&header(2, 128, 128)).unwrap();
        assert_eq!(h, GptHeader { entries_lba: 2, entries: 128, entry_size: 128 });
        assert!(parse_gpt_header(&vec![0u8; 512]).is_none(), "no signature");
        assert!(parse_gpt_header(&header(2, 1_000_000, 128)).is_none(), "absurd count");
        assert!(parse_gpt_header(&header(2, 128, 64)).is_none(), "entry too small");
        assert!(parse_gpt_header(&header(0, 128, 128)).is_none(), "table over the header");
    }

    #[test]
    fn gpt_entries_skip_unused_and_decode_names() {
        let h = GptHeader { entries_lba: 2, entries: 4, entry_size: 128 };
        let mut t = entry(2048, 4095, "esp");
        t.extend(vec![0u8; 128]); // unused slot
        t.extend(entry(4096, 1 << 20, "data"));
        t.extend(entry(10, 5, "backwards"));
        let p = parse_gpt_entries(&t, &h);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0], GptPartition { index: 1, first_lba: 2048, name: "esp".into() });
        assert_eq!(p[1].index, 3);
        assert_eq!(p[1].name, "data");
    }

    #[test]
    fn description() {
        assert!(describe_slabs(false, &[]).is_none());
        assert!(describe_slabs(true, &[]).unwrap().contains("whole drive"));
        let d = describe_slabs(false, &["2 'data'".into(), "3 'system'".into()]).unwrap();
        assert!(d.starts_with("stormblock") && d.contains("'system'"));
    }
}
