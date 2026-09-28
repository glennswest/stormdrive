//! A GPT with one stormblock partition — the drive worker's `partition`
//! step (#5). Built as bytes here (portable, tested); written and re-read
//! by the kernel in `worker`.
//!
//! The layout matches what stormblock's node installer lays down, so a disk
//! stormdrive prepared reads the same to every tool: the partition type is
//! stormblock's slab type GUID (`SLAB_DATA` for a data slab, `SLAB` for a
//! system one — stormblock `src/image/mod.rs` `type_guid`), aligned to
//! 1 MiB at both ends, one partition covering the rest of the drive.

/// stormblock's slab partition type, `4C9A7B2E-1D63-4F8A-9E51-0B7C2A6D3F14`
/// (mixed-endian, as GPT stores it).
pub const TYPE_SLAB: [u8; 16] = [
    0x2E, 0x7B, 0x9A, 0x4C, 0x63, 0x1D, 0x8A, 0x4F, 0x9E, 0x51, 0x0B, 0x7C, 0x2A, 0x6D, 0x3F, 0x14,
];
/// stormblock's data slab partition type, `7D3E5A91-6C24-4B8F-A05D-2E9147BC6F38`.
pub const TYPE_SLAB_DATA: [u8; 16] = [
    0x91, 0x5A, 0x3E, 0x7D, 0x24, 0x6C, 0x8F, 0x4B, 0xA0, 0x5D, 0x2E, 0x91, 0x47, 0xBC, 0x6F, 0x38,
];

const ENTRIES: u32 = 128;
const ENTRY_SIZE: u32 = 128;
const ALIGN_BYTES: u64 = 1 << 20;

/// CRC-32 (IEEE 802.3), as GPT uses it.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// What to write, and where the partition landed.
#[derive(Debug, Clone)]
pub struct Layout {
    /// LBA 0 (protective MBR), LBA 1 (header), the entry array.
    pub primary: Vec<u8>,
    /// The backup entry array and header, written at `backup_offset`.
    pub backup: Vec<u8>,
    pub backup_offset: u64,
    pub first_lba: u64,
    pub last_lba: u64,
    pub block_size: u64,
}

impl Layout {
    pub fn partition_bytes(&self) -> u64 {
        (self.last_lba - self.first_lba + 1) * self.block_size
    }
}

#[allow(clippy::too_many_arguments)]
fn header(
    my_lba: u64,
    alt_lba: u64,
    first_usable: u64,
    last_usable: u64,
    disk_guid: &[u8; 16],
    entries_lba: u64,
    entries_crc: u32,
    block_size: usize,
) -> Vec<u8> {
    let mut h = vec![0u8; block_size];
    h[0..8].copy_from_slice(b"EFI PART");
    h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
    h[12..16].copy_from_slice(&92u32.to_le_bytes());
    h[24..32].copy_from_slice(&my_lba.to_le_bytes());
    h[32..40].copy_from_slice(&alt_lba.to_le_bytes());
    h[40..48].copy_from_slice(&first_usable.to_le_bytes());
    h[48..56].copy_from_slice(&last_usable.to_le_bytes());
    h[56..72].copy_from_slice(disk_guid);
    h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
    h[80..84].copy_from_slice(&ENTRIES.to_le_bytes());
    h[84..88].copy_from_slice(&ENTRY_SIZE.to_le_bytes());
    h[88..92].copy_from_slice(&entries_crc.to_le_bytes());
    let crc = crc32(&h[..92]);
    h[16..20].copy_from_slice(&crc.to_le_bytes());
    h
}

/// One partition of `type_guid`, named `name`, from the first 1 MiB
/// boundary to the last one before the backup table. `disk_guid` and
/// `part_guid` are the GPT-ordered bytes of fresh random GUIDs.
pub fn layout(
    capacity_bytes: u64,
    block_size: u64,
    type_guid: [u8; 16],
    name: &str,
    disk_guid: [u8; 16],
    part_guid: [u8; 16],
) -> Result<Layout, String> {
    if !matches!(block_size, 512 | 4096) {
        return Err(format!("block size {block_size}: a GPT here needs 512 or 4096"));
    }
    let bs = block_size as usize;
    let total = capacity_bytes / block_size;
    let entry_lbas = (ENTRIES as u64 * ENTRY_SIZE as u64).div_ceil(block_size);
    let first_usable = 2 + entry_lbas;
    let last_usable = total.checked_sub(2 + entry_lbas).ok_or("drive too small for a GPT")?;
    let align = ALIGN_BYTES / block_size;
    let first = first_usable.div_ceil(align) * align;
    let last = ((last_usable + 1) / align) * align;
    if last <= first + align {
        return Err(format!("drive too small: {capacity_bytes} bytes leaves no aligned partition"));
    }
    let last = last - 1;

    let mut entries = vec![0u8; (ENTRIES * ENTRY_SIZE) as usize];
    entries[0..16].copy_from_slice(&type_guid);
    entries[16..32].copy_from_slice(&part_guid);
    entries[32..40].copy_from_slice(&first.to_le_bytes());
    entries[40..48].copy_from_slice(&last.to_le_bytes());
    for (i, u) in name.encode_utf16().take(36).enumerate() {
        entries[56 + 2 * i..58 + 2 * i].copy_from_slice(&u.to_le_bytes());
    }
    let entries_crc = crc32(&entries);
    let mut entries_padded = entries.clone();
    entries_padded.resize(entry_lbas as usize * bs, 0);

    // Protective MBR: one 0xEE partition covering the disk (capped at 2^32-1).
    let mut mbr = vec![0u8; bs];
    let p = &mut mbr[446..462];
    p[1..4].copy_from_slice(&[0x00, 0x02, 0x00]);
    p[4] = 0xEE;
    p[5..8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    p[8..12].copy_from_slice(&1u32.to_le_bytes());
    p[12..16].copy_from_slice(&((total - 1).min(u32::MAX as u64) as u32).to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;

    let backup_header_lba = total - 1;
    let backup_entries_lba = total - 1 - entry_lbas;
    let mut primary = mbr;
    primary.extend(header(1, backup_header_lba, first_usable, last_usable, &disk_guid, 2, entries_crc, bs));
    primary.extend(&entries_padded);
    let mut backup = entries_padded;
    backup.extend(header(backup_header_lba, 1, first_usable, last_usable, &disk_guid, backup_entries_lba, entries_crc, bs));
    Ok(Layout {
        primary,
        backup,
        backup_offset: backup_entries_lba * block_size,
        first_lba: first,
        last_lba: last,
        block_size,
    })
}

/// The kernel's name for partition 1 of a disk: `sdb` → `sdb1`,
/// `nvme0n1` → `nvme0n1p1` (a name ending in a digit takes a `p`).
pub fn partition_name(disk: &str, n: u32) -> String {
    if disk.ends_with(|c: char| c.is_ascii_digit()) {
        format!("{disk}p{n}")
    } else {
        format!("{disk}{n}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contents::{parse_gpt_entries, parse_gpt_header};

    #[test]
    fn crc32_known_vectors() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    fn check(capacity: u64, bs: u64) {
        let l = layout(capacity, bs, TYPE_SLAB_DATA, "stormblock-data", [1; 16], [2; 16]).unwrap();
        let b = bs as usize;
        // Protective MBR.
        assert_eq!(&l.primary[510..512], &[0x55, 0xAA]);
        assert_eq!(l.primary[446 + 4], 0xEE);
        // Primary header parses with our own reader and its CRCs hold.
        let hdr = &l.primary[b..2 * b];
        let h = parse_gpt_header(hdr).expect("primary header");
        assert_eq!((h.entries_lba, h.entries, h.entry_size), (2, 128, 128));
        let mut zeroed = hdr[..92].to_vec();
        zeroed[16..20].fill(0);
        assert_eq!(crc32(&zeroed), u32::from_le_bytes(hdr[16..20].try_into().unwrap()));
        let table = &l.primary[2 * b..2 * b + 128 * 128];
        assert_eq!(crc32(table), u32::from_le_bytes(hdr[88..92].try_into().unwrap()));
        let parts = parse_gpt_entries(table, &h);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].first_lba, l.first_lba);
        assert_eq!(parts[0].name, "stormblock-data");
        assert_eq!(&table[..16], &TYPE_SLAB_DATA);
        // 1 MiB aligned at both ends, inside the usable range.
        assert_eq!((l.first_lba * bs) % (1 << 20), 0);
        assert_eq!(((l.last_lba + 1) * bs) % (1 << 20), 0);
        let total = capacity / bs;
        let entry_lbas = (128 * 128) / bs;
        assert!(l.last_lba <= total - 2 - entry_lbas);
        // The backup ends on the last LBA, its header points back at LBA 1.
        assert_eq!(l.backup_offset + l.backup.len() as u64, total * bs);
        let bh = &l.backup[l.backup.len() - b..];
        assert_eq!(u64::from_le_bytes(bh[24..32].try_into().unwrap()), total - 1);
        assert_eq!(u64::from_le_bytes(bh[32..40].try_into().unwrap()), 1);
        assert_eq!(u64::from_le_bytes(bh[72..80].try_into().unwrap()) * bs, l.backup_offset);
    }

    #[test]
    fn layouts_for_512_and_4k_drives() {
        check(1_200_243_695_616, 512); // ST1200MM0098
        check(1_200_243_695_616, 4096); // the same drive reformatted to 4K
        check(2_000_398_934_016, 512); // the R230's WD20EFAX
        check(64 << 20, 512); // small
    }

    #[test]
    fn refusals() {
        assert!(layout(1 << 30, 520, TYPE_SLAB, "x", [0; 16], [0; 16]).is_err(), "520-byte drives need a reformat first");
        assert!(layout(1 << 20, 512, TYPE_SLAB, "x", [0; 16], [0; 16]).is_err(), "too small");
    }

    #[test]
    fn partition_names() {
        assert_eq!(partition_name("sdb", 1), "sdb1");
        assert_eq!(partition_name("sdaa", 1), "sdaa1");
        assert_eq!(partition_name("nvme0n1", 1), "nvme0n1p1");
    }
}
