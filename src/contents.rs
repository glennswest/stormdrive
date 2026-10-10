//! What is on a drive that a destructive operation would destroy.
//!
//! `/proc/mounts` is not enough on stormcos: the root filesystem sits on a
//! ublk device that stormblock serves out of slabs on the system disk, so
//! that disk never appears as mounted and stormblock does not open it
//! `O_EXCL`. Its `/api/v1/drives` does not list it either (the slabs attach
//! by path). The disk itself says so, though — a stormcos disk is a GPT
//! whose partitions start with a stormblock slab header — so we read it.
//!
//! The drive worker (#5) asks a wider question: does the drive hold a
//! stormblock slab **or a filesystem** (anything blkid would name) — on the
//! whole disk or at the start of any GPT partition? [`holds`] answers it.
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
    /// Partition type GUID as GPT stores it (mixed-endian).
    pub type_guid: [u8; 16],
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
                type_guid: e[..16].try_into().unwrap(),
            })
        })
        .collect()
}

/// How much of a region [`fs_signature`] needs: btrfs keeps its superblock
/// at 64 KiB.
pub const SIGNATURE_BYTES: usize = 64 * 1024 + 4096;

/// What a region (a whole disk, or a partition from its first byte) holds,
/// by on-disk signature. The same magic numbers blkid uses; enough to say
/// "somebody's data is here", not to identify every format there is.
pub fn fs_signature(buf: &[u8]) -> Option<&'static str> {
    let at = |off: usize, magic: &[u8]| buf.len() >= off + magic.len() && &buf[off..off + magic.len()] == magic;
    if at(0, SLAB_MAGIC) {
        return Some("stormblock slab");
    }
    if at(0, STORMRAID_MAGIC) {
        return Some("stormraid member");
    }
    if at(0, b"LUKS\xba\xbe") {
        return Some("LUKS");
    }
    if at(0, b"XFSB") {
        return Some("xfs");
    }
    if at(3, b"NTFS    ") {
        return Some("ntfs");
    }
    if at(0x438, &[0x53, 0xEF]) {
        return Some("ext2/3/4");
    }
    if at(0x10040, b"_BHRfS_M") {
        return Some("btrfs");
    }
    if at(0x218, b"LVM2 001") || at(0x018, b"LVM2 001") {
        return Some("LVM2");
    }
    if at(4096 - 10, b"SWAPSPACE2") || at(4096 - 10, b"SWAP-SPACE") {
        return Some("swap");
    }
    // md superblock 1.2 (4 KiB in) and 1.1 (at 0), magic a92b4efc LE.
    if at(4096, &[0xfc, 0x4e, 0x2b, 0xa9]) || at(0, &[0xfc, 0x4e, 0x2b, 0xa9]) {
        return Some("linux_raid");
    }
    if at(510, &[0x55, 0xAA]) && (at(82, b"FAT32   ") || at(54, b"FAT16   ") || at(54, b"FAT12   ")) {
        return Some("vfat");
    }
    None
}

/// The sentence for [`holds`]: "xfs (whole drive)", "ext2/3/4 in
/// partition 1 'root'"…
pub fn describe_holdings(found: &[(Option<String>, &str)]) -> Option<String> {
    if found.is_empty() {
        return None;
    }
    Some(
        found
            .iter()
            .map(|(part, what)| match part {
                None => format!("{what} (whole drive)"),
                Some(p) => format!("{what} in partition {p}"),
            })
            .collect::<Vec<_>>()
            .join(", "),
    )
}

/// Everything on the drive a destructive step would destroy: a stormblock
/// slab or a filesystem, on the whole disk or at the start of any GPT
/// partition. None when nothing is recognised (or it cannot be read).
pub fn holds(path: &str) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        linux::holds(path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        None
    }
}

/// Blocks read at the start of a drive the kernel cannot read (#81).
pub const RAW_BLOCKS: u32 = 128;

/// What the first blocks of a drive sd cannot read hold (#81: a NetApp
/// 520-byte drive, which may carry an ONTAP label). Read through SG at the
/// drive's own sector size. Blank (all zero) → None. Anything else is
/// somebody's data — "foreign data (520-byte sectors): first 128 blocks
/// not blank; strings: "…"" — with up to three printable runs of 6+
/// characters as a hint of whose. Not a format identification.
pub fn describe_raw(buf: &[u8], block_len: u32) -> Option<String> {
    if buf.iter().all(|b| *b == 0) {
        return None;
    }
    let mut strings: Vec<String> = vec![];
    let mut run = String::new();
    for &b in buf.iter().chain(std::iter::once(&0u8)) {
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':' | b'/') {
            run.push(b as char);
            continue;
        }
        if run.len() >= 6 && !strings.contains(&run) && strings.len() < 3 {
            strings.push(run.clone());
        }
        run.clear();
    }
    let blocks = buf.len() / block_len.max(1) as usize;
    let mut s = format!("foreign data ({block_len}-byte sectors): first {blocks} blocks not blank");
    if !strings.is_empty() {
        s.push_str(&format!("; strings: {}", strings.iter().map(|x| format!("{x:?}")).collect::<Vec<_>>().join(", ")));
    }
    Some(s)
}

/// [`describe_raw`] of a drive through its sg node: READ(16) of the first
/// [`RAW_BLOCKS`] blocks at `block_len`. None when blank or unreadable.
pub fn raw_holds(sg: &str, block_len: u32) -> Option<String> {
    let dev = crate::scsi::Device::open(sg).ok()?;
    let buf = dev.read16(0, RAW_BLOCKS, block_len).ok()?;
    describe_raw(&buf, block_len)
}

/// What a drive holds, whichever way it can be read: through the block
/// device when the kernel can ([`holds`]), else through SG at the drive's
/// own sector size ([`raw_holds`]).
pub fn holds_drive(path: &str, name: &str, usable: bool, block_size: u32) -> Option<String> {
    if usable {
        return holds(path);
    }
    let sg = crate::scsi::sg_path_for_block(name).unwrap_or_else(|| path.to_string());
    raw_holds(&sg, block_size)
}

/// stormraid's superblock (copy A at block 0 of every member; stormraid
/// `src/format.rs` SB_MAGIC).
pub const STORMRAID_MAGIC: &[u8; 8] = b"STORMRD1";

/// The sentence for a stormraid member, as `in_use_by` carries it.
pub const STORMRAID_HOLDER: &str = "stormraid (a RAID set member)";

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

/// One stormblock slab found on the disk (#58): where, and which half of
/// the node it belongs to, so it can be held against the engine's own
/// report of where its slabs are.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SlabPart {
    /// GPT partition number; None for a slab on the whole drive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<usize>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// `system` or `data` from the partition type GUID (stormblock's
    /// `SLAB` / `SLAB_DATA`); `unknown` for a whole-drive slab or another
    /// type.
    pub role: String,
    /// Byte offset of the slab on the drive, as the engine names a local
    /// partition slab (`/dev/sda@1048576`).
    pub offset_bytes: u64,
}

/// The half a slab partition belongs to, by its type GUID.
pub fn slab_role(type_guid: &[u8; 16]) -> &'static str {
    if type_guid == &crate::gpt::TYPE_SLAB_DATA {
        "data"
    } else if type_guid == &crate::gpt::TYPE_SLAB {
        "system"
    } else {
        "unknown"
    }
}

/// Read the drive and say who holds it, or None when it carries no
/// stormblock slab (or cannot be read — a 520-byte drive, a drive mid
/// format; those have nothing the kernel could have put there).
pub fn probe(path: &str) -> Option<String> {
    probe_slabs(path).0
}

/// [`probe`], and each slab it found (#58).
pub fn probe_slabs(path: &str) -> (Option<String>, Vec<SlabPart>) {
    #[cfg(target_os = "linux")]
    {
        linux::probe(path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        (None, vec![])
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

    pub fn holds(path: &str) -> Option<String> {
        let f = File::open(path).ok()?;
        let mut found: Vec<(Option<String>, &'static str)> = vec![];
        if let Some(w) = read(&f, 0, SIGNATURE_BYTES).as_deref().and_then(fs_signature) {
            found.push((None, w));
        }
        for lbs in [512u64, 4096] {
            let Some(h) = read(&f, lbs, 512).as_deref().and_then(parse_gpt_header) else {
                continue;
            };
            let len = h.entries as usize * h.entry_size as usize;
            let Some(table) = read(&f, h.entries_lba * lbs, len) else {
                continue;
            };
            for p in parse_gpt_entries(&table, &h) {
                if let Some(w) = read(&f, p.first_lba * lbs, SIGNATURE_BYTES).as_deref().and_then(fs_signature) {
                    let name = if p.name.is_empty() { format!("{}", p.index) } else { format!("{} '{}'", p.index, p.name) };
                    found.push((Some(name), w));
                }
            }
            break;
        }
        describe_holdings(&found)
    }

    pub fn probe(path: &str) -> (Option<String>, Vec<SlabPart>) {
        let Ok(f) = File::open(path) else { return (None, vec![]) };
        let Some(head) = read(&f, 0, 512) else { return (None, vec![]) };
        if is_slab_header(&head) {
            let whole = SlabPart { partition: None, name: String::new(), role: "unknown".into(), offset_bytes: 0 };
            return (describe_slabs(true, &[]), vec![whole]);
        }
        if head.starts_with(STORMRAID_MAGIC) {
            return (Some(STORMRAID_HOLDER.into()), vec![]);
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
            let parts: Vec<GptPartition> = parse_gpt_entries(&table, &h)
                .into_iter()
                .filter(|p| {
                    read(&f, p.first_lba * lbs, 512).is_some_and(|b| is_slab_header(&b))
                })
                .collect();
            let names: Vec<String> = parts
                .iter()
                .map(|p| {
                    if p.name.is_empty() {
                        format!("#{}", p.index)
                    } else {
                        format!("{} '{}'", p.index, p.name)
                    }
                })
                .collect();
            let slabs = parts
                .iter()
                .map(|p| SlabPart {
                    partition: Some(p.index),
                    name: p.name.clone(),
                    role: slab_role(&p.type_guid).into(),
                    offset_bytes: p.first_lba * lbs,
                })
                .collect();
            return (describe_slabs(false, &names), slabs);
        }
        (None, vec![])
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
    fn raw_blocks_of_an_unreadable_drive() {
        assert_eq!(describe_raw(&vec![0u8; 520 * 4], 520), None, "blank");
        let mut b = vec![0u8; 520 * 4];
        b[16..30].copy_from_slice(b"NETAPP_LABEL_A");
        b[600..606].copy_from_slice(b"RAID_V");
        b[700..703].copy_from_slice(b"abc");
        let d = describe_raw(&b, 520).unwrap();
        assert_eq!(d, "foreign data (520-byte sectors): first 4 blocks not blank; strings: \"NETAPP_LABEL_A\", \"RAID_V\"");
        let mut c = vec![0u8; 520];
        c[3] = 0xff;
        assert_eq!(describe_raw(&c, 520).unwrap(), "foreign data (520-byte sectors): first 1 blocks not blank");
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
        assert_eq!((p[0].index, p[0].first_lba, p[0].name.as_str()), (1, 2048, "esp"));
        assert_eq!(slab_role(&p[0].type_guid), "unknown");
        assert_eq!(slab_role(&crate::gpt::TYPE_SLAB), "system");
        assert_eq!(slab_role(&crate::gpt::TYPE_SLAB_DATA), "data");
        assert_eq!(p[1].index, 3);
        assert_eq!(p[1].name, "data");
    }

    #[test]
    fn filesystem_signatures() {
        let blank = vec![0u8; SIGNATURE_BYTES];
        assert_eq!(fs_signature(&blank), None);
        let with = |off: usize, magic: &[u8]| {
            let mut b = blank.clone();
            b[off..off + magic.len()].copy_from_slice(magic);
            b
        };
        assert_eq!(fs_signature(&with(0, SLAB_MAGIC)), Some("stormblock slab"));
        assert_eq!(fs_signature(&with(0, b"XFSB")), Some("xfs"));
        assert_eq!(fs_signature(&with(0x438, &[0x53, 0xEF])), Some("ext2/3/4"));
        assert_eq!(fs_signature(&with(0x10040, b"_BHRfS_M")), Some("btrfs"));
        assert_eq!(fs_signature(&with(3, b"NTFS    ")), Some("ntfs"));
        assert_eq!(fs_signature(&with(0x218, b"LVM2 001")), Some("LVM2"));
        assert_eq!(fs_signature(&with(4086, b"SWAPSPACE2")), Some("swap"));
        assert_eq!(fs_signature(&with(4096, &[0xfc, 0x4e, 0x2b, 0xa9])), Some("linux_raid"));
        assert_eq!(fs_signature(&with(0, b"LUKS\xba\xbe")), Some("LUKS"));
        let mut fat = with(82, b"FAT32   ");
        assert_eq!(fs_signature(&fat), None, "FAT needs the boot signature too");
        fat[510] = 0x55;
        fat[511] = 0xAA;
        assert_eq!(fs_signature(&fat), Some("vfat"));
        assert_eq!(fs_signature(&blank[..100]), None, "a short read finds nothing, never panics");
    }

    #[test]
    fn holdings_sentence() {
        assert!(describe_holdings(&[]).is_none());
        let s = describe_holdings(&[(None, "xfs"), (Some("2 'data'".into()), "stormblock slab")]).unwrap();
        assert_eq!(s, "xfs (whole drive), stormblock slab in partition 2 'data'");
    }

    #[test]
    fn description() {
        assert!(describe_slabs(false, &[]).is_none());
        assert!(describe_slabs(true, &[]).unwrap().contains("whole drive"));
        let d = describe_slabs(false, &["2 'data'".into(), "3 'system'".into()]).unwrap();
        assert!(d.starts_with("stormblock") && d.contains("'system'"));
    }
}
