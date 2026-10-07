//! Drive discovery: enumerate /sys/block, classify, identify.
//!
//! Pure-policy pieces (name eligibility, mount matching) live here as
//! portable functions with tests; the sysfs walk itself is Linux-only.

use crate::config::{wildcard_match, DiscoveryConfig};
use crate::drive::DriveKind;

/// What a scan saw for one physical drive, before it is merged into the
/// inventory.
#[derive(Debug, Clone)]
pub struct Observed {
    pub name: String,
    pub path: String,
    pub kind: DriveKind,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub wwid: Option<String>,
    pub capacity_bytes: u64,
    /// Logical block length the drive reports (READ CAPACITY(16) when we
    /// can ask; sysfs otherwise).
    pub block_size: u32,
    pub physical_block_size: u32,
    /// The kernel exposes a non-zero capacity — I/O through /dev works.
    /// False for the sector sizes sd refuses (520, 528, …).
    pub usable: bool,
    /// Whose data is on it (a stormblock slab), read off the drive —
    /// see `contents`.
    pub in_use_by: Option<String>,
    /// Anything a destructive step would destroy (`contents::holds`: a
    /// filesystem, a slab, a RAID member, on the disk or in a partition) —
    /// None = blank, or not readable (#42).
    pub contents: Option<String>,
}

/// Is a sector size one the kernel will drive?
pub fn kernel_usable_block_size(bs: u32) -> bool {
    matches!(bs, 512 | 1024 | 2048 | 4096)
}

/// Kernel names that are never physical drives (or are somebody else's
/// export surface — ublkb* is stormblock's own).
const BUILTIN_EXCLUDE: &[&str] = &[
    "loop", "ram", "zram", "dm-", "md", "sr", "fd", "nbd", "ublkb", "zd", "pmem", "drbd",
];

/// `nvme0c1n1`: one controller's path to a namespace under native NVMe
/// multipath. Hidden, no /dev node; the namespace is `nvme0n1`.
pub fn is_nvme_path_node(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("nvme") else { return false };
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let Some((subsys, rest)) = rest.split_once('c') else { return false };
    let Some((ctrl, ns)) = rest.split_once('n') else { return false };
    digits(subsys) && digits(ctrl) && digits(ns)
}

pub fn name_eligible(cfg: &DiscoveryConfig, name: &str) -> bool {
    if BUILTIN_EXCLUDE.iter().any(|p| name.starts_with(p)) || is_nvme_path_node(name) {
        return false;
    }
    if cfg.exclude.iter().any(|p| wildcard_match(p, name)) {
        return false;
    }
    if !cfg.include.is_empty() && !cfg.include.iter().any(|p| wildcard_match(p, name)) {
        return false;
    }
    true
}

/// Does `source` (a /proc/mounts device field) refer to disk `name` or one
/// of its partitions? "/dev/sda" matches "/dev/sda" and "/dev/sda1", never
/// "/dev/sdab"; "/dev/nvme0n1" matches "/dev/nvme0n1p2".
pub fn mount_source_is_disk(source: &str, name: &str) -> bool {
    let Some(rest) = source.strip_prefix("/dev/") else {
        return false;
    };
    let Some(tail) = rest.strip_prefix(name) else {
        return false;
    };
    if tail.is_empty() {
        return true;
    }
    // Names ending in a digit (nvme0n1, md0) take a 'p' separator before the
    // partition number — a bare digit tail is a *different* device
    // (nvme0n10). Names ending in a letter (sda) append the number directly.
    let name_ends_digit = name.chars().last().is_some_and(|c| c.is_ascii_digit());
    if name_ends_digit {
        tail.len() > 1
            && tail.starts_with('p')
            && tail[1..].chars().all(|c| c.is_ascii_digit())
    } else {
        tail.chars().all(|c| c.is_ascii_digit())
    }
}

/// Is the disk (or any of its partitions) mounted right now? False on
/// non-Linux. Checked at discovery and re-checked immediately before any
/// destructive test.
pub fn is_mounted(name: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
            return false;
        };
        mounted_in(&mounts, name)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = name;
        false
    }
}

/// Is `name` mounted according to this /proc/mounts text? A scan reads
/// the table once, not once per drive.
pub fn mounted_in(mounts: &str, name: &str) -> bool {
    mounts
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .any(|src| mount_source_is_disk(src, name))
}

/// What discovery asks the drive itself (READ CAPACITY(16), the slab probe
/// off LBA 0 and the GPT), kept between passes (#15). At 160 drives on a
/// 30 s pass that was ~320 reads a minute of drive I/O to learn nothing
/// new. Asked again when the device changes (a new dev_t, a new size, a new
/// WWID — a swap, a rescan, a reformat) or when the answer is older than
/// `PROBE_REFRESH`. Destructive operations re-read the disk themselves
/// right before they start; this never gates them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProbeKey {
    pub name: String,
    /// `/sys/block/<name>/dev`, `maj:min`.
    pub dev: String,
    pub sectors: u64,
    pub wwid: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Probed {
    pub block_size: u32,
    pub physical_block_size: u32,
    pub capacity_bytes: u64,
    pub in_use_by: Option<String>,
    pub contents: Option<String>,
    pub at: std::time::Instant,
}

pub const PROBE_REFRESH: std::time::Duration = std::time::Duration::from_secs(600);

/// The probe cache, by device name.
#[derive(Debug, Default)]
pub struct ProbeCache {
    by_name: std::collections::HashMap<String, (ProbeKey, Probed)>,
}

impl ProbeCache {
    /// The cached answer, if it is for this very device and still fresh.
    pub fn get(&self, key: &ProbeKey, now: std::time::Instant) -> Option<&Probed> {
        self.by_name
            .get(&key.name)
            .filter(|(k, p)| k == key && now.duration_since(p.at) < PROBE_REFRESH)
            .map(|(_, p)| p)
    }

    pub fn put(&mut self, key: ProbeKey, p: Probed) {
        self.by_name.insert(key.name.clone(), (key, p));
    }

    /// Forget devices that are gone.
    pub fn retain_names(&mut self, seen: &std::collections::HashSet<String>) {
        self.by_name.retain(|n, _| seen.contains(n));
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

static PROBES: std::sync::OnceLock<std::sync::Mutex<ProbeCache>> = std::sync::OnceLock::new();

/// The daemon's probe cache.
pub fn probe_cache() -> &'static std::sync::Mutex<ProbeCache> {
    PROBES.get_or_init(Default::default)
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::path::Path;

    fn read_trim(p: &Path) -> Option<String> {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn classify(base: &Path, name: &str) -> DriveKind {
        if name.starts_with("nvme") {
            return DriveKind::NvmeSsd;
        }
        let rotational = read_trim(&base.join("queue/rotational")).as_deref() == Some("1");
        // A SATA drive behind a SAS HBA has a sas_address too (the HBA's
        // end-device address for it); the SAT layer reports vendor "ATA".
        let is_ata = read_trim(&base.join("device/vendor")).as_deref() == Some("ATA");
        let is_sas = !is_ata && base.join("device/sas_address").exists();
        match (is_sas, rotational) {
            (true, false) => DriveKind::SasSsd,
            (true, true) => DriveKind::SasHdd,
            (false, false) => DriveKind::SataSsd,
            (false, true) => DriveKind::SataHdd,
        }
    }

    pub fn scan(cfg: &DiscoveryConfig) -> Vec<Observed> {
        let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
        scan_in(Path::new("/sys"), Path::new("/dev"), &mounts, cfg)
    }

    /// [`scan`] of a sysfs tree at `sys`, opening devices under `dev` (#31:
    /// a simulated chassis is a tree in a directory).
    pub fn scan_in(sys: &Path, dev_root: &Path, mounts: &str, cfg: &DiscoveryConfig) -> Vec<Observed> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(sys.join("block")) else {
            return out;
        };
        let node = |name: &str| dev_root.join(name).to_string_lossy().to_string();
        let mut cache = probe_cache().lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        let mut seen = std::collections::HashSet::new();
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name_eligible(cfg, &name) {
                continue;
            }
            // Hidden gendisks (NVMe multipath paths, whatever the name).
            if read_trim(&e.path().join("hidden")).as_deref() == Some("1") {
                continue;
            }
            if !cfg.manage_mounted && super::mounted_in(mounts, &name) {
                tracing::debug!(%name, "skipping drive with mounted partitions");
                continue;
            }
            let base = e.path();
            let sectors: u64 = read_trim(&base.join("size"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let dev = base.join("device");
            if sectors == 0 {
                // A 0-block device is either an empty removable slot (card
                // reader, USB floppy) — skip — or a real SCSI disk whose
                // sector size sd refused ("Unsupported sector size 520").
                // Those must be seen: the reformat is how they become
                // drives.
                let removable = read_trim(&base.join("removable")).as_deref() == Some("1");
                let is_scsi_disk = read_trim(&dev.join("type")).as_deref() == Some("0");
                if removable || !is_scsi_disk {
                    continue;
                }
            }
            let sysfs_lbs: u32 = read_trim(&base.join("queue/logical_block_size"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(512);
            let sysfs_pbs: u32 = read_trim(&base.join("queue/physical_block_size"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(sysfs_lbs);
            let wwid = read_trim(&base.join("wwid")).or_else(|| read_trim(&dev.join("wwid")));
            seen.insert(name.clone());
            let key = ProbeKey {
                name: name.clone(),
                dev: read_trim(&base.join("dev")).unwrap_or_default(),
                sectors,
                wwid: wwid.clone(),
            };
            let model = read_trim(&dev.join("model")).unwrap_or_default();
            let serial = read_trim(&dev.join("serial"))
                .or_else(|| read_trim(&base.join("serial")))
                .unwrap_or_default();
            let firmware = read_trim(&dev.join("firmware_rev"))
                .or_else(|| read_trim(&dev.join("rev")))
                .unwrap_or_default();
            if let Some(p) = cache.get(&key, now) {
                out.push(Observed {
                    path: format!("/dev/{name}"),
                    kind: classify(&base, &name),
                    model,
                    serial,
                    firmware,
                    wwid,
                    capacity_bytes: p.capacity_bytes,
                    block_size: p.block_size,
                    physical_block_size: p.physical_block_size,
                    usable: sectors > 0,
                    in_use_by: p.in_use_by.clone(),
                    contents: p.contents.clone(),
                    name,
                });
                continue;
            }
            // Ask the drive itself when we can: for a 520-byte drive the
            // kernel's logical_block_size is a 512 it fell back to, and
            // physical_block_size is the only hint. READ CAPACITY(16) is
            // the truth; it fails (NOT READY) mid-format, and then sysfs
            // stands in.
            let asked = if name.starts_with("sd") {
                crate::scsi::sg_path_in(&sys.join("block").join(&name).join("device/scsi_generic").to_string_lossy())
                    .or_else(|| Some(node(&name)))
                    .and_then(|p| crate::scsi::Device::open(&p).ok())
                    .and_then(|d| d.read_capacity16().ok())
            } else {
                None
            };
            // Only a real answer is cached: a drive that did not answer
            // (mid-format) is asked again next pass.
            let answered = asked.as_ref().is_some_and(|c| c.block_len > 0) || !name.starts_with("sd");
            let (block_size, physical_block_size, capacity_bytes) = match asked {
                Some(c) if c.block_len > 0 => (c.block_len, c.physical_block_len(), c.bytes()),
                _ => (
                    if sectors == 0 && !kernel_usable_block_size(sysfs_pbs) {
                        sysfs_pbs
                    } else {
                        sysfs_lbs
                    },
                    sysfs_pbs,
                    sectors * 512,
                ),
            };
            let in_use_by = if sectors > 0 {
                crate::contents::probe(&node(&name))
            } else {
                None
            };
            let contents = if sectors > 0 {
                crate::contents::holds(&node(&name))
            } else {
                None
            };
            if answered {
                cache.put(
                    key,
                    Probed { block_size, physical_block_size, capacity_bytes, in_use_by: in_use_by.clone(), contents: contents.clone(), at: now },
                );
            }
            out.push(Observed {
                path: format!("/dev/{name}"),
                kind: classify(&base, &name),
                model,
                serial,
                firmware,
                wwid,
                capacity_bytes,
                block_size,
                physical_block_size,
                usable: sectors > 0,
                in_use_by,
                contents,
                name,
            });
        }
        cache.retain_names(&seen);
        out
    }
}

/// [`scan`] over a sysfs tree at `sys` with device nodes under `dev` (#31,
/// the simulated 160-bay chassis). Linux only.
#[cfg(target_os = "linux")]
pub use linux::scan_in;

/// Scan the node for physical drives. Empty on non-Linux (build-on-dev rule:
/// the real path only exists there).
pub fn scan(cfg: &DiscoveryConfig) -> Vec<Observed> {
    #[cfg(target_os = "linux")]
    {
        linux::scan(cfg)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cfg;
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DiscoveryConfig {
        DiscoveryConfig::default()
    }

    #[test]
    fn builtin_exclusions() {
        for n in ["loop0", "ram1", "zram0", "dm-3", "md0", "sr0", "nbd2", "ublkb0", "zd16"] {
            assert!(!name_eligible(&cfg(), n), "{n} must be excluded");
        }
        for n in ["sda", "sdab", "nvme0n1"] {
            assert!(name_eligible(&cfg(), n), "{n} must be eligible");
        }
    }

    #[test]
    fn config_exclude_and_include() {
        let mut c = cfg();
        c.exclude = vec!["sda".into()];
        assert!(!name_eligible(&c, "sda"));
        assert!(name_eligible(&c, "sdb"));

        let mut c = cfg();
        c.include = vec!["nvme*".into()];
        assert!(name_eligible(&c, "nvme0n1"));
        assert!(!name_eligible(&c, "sda"));
    }

    #[test]
    fn nvme_multipath_path_nodes_are_not_drives() {
        for n in ["nvme0c0n1", "nvme12c3n45"] {
            assert!(is_nvme_path_node(n) && !name_eligible(&cfg(), n), "{n}");
        }
        for n in ["nvme0n1", "nvme10n2", "nvmec0n1", "nvme0c0n", "sdc"] {
            assert!(!is_nvme_path_node(n), "{n}");
        }
    }

    #[test]
    fn one_mount_table_answers_for_every_drive() {
        let m = "/dev/sda2 / ext4 rw 0 0\ntmpfs /tmp tmpfs rw 0 0\n/dev/nvme3n1p1 /data xfs rw 0 0\n";
        assert!(mounted_in(m, "sda") && mounted_in(m, "nvme3n1"));
        assert!(!mounted_in(m, "sdb") && !mounted_in(m, "nvme3n10"));
    }

    #[test]
    fn a_probe_is_reused_until_the_device_changes_or_it_ages() {
        let t0 = std::time::Instant::now();
        let key = ProbeKey { name: "sdc".into(), dev: "8:32".into(), sectors: 1000, wwid: Some("naa.1".into()) };
        let mut c = ProbeCache::default();
        c.put(key.clone(), Probed { block_size: 4096, physical_block_size: 4096, capacity_bytes: 1, in_use_by: None, contents: None, at: t0 });
        assert!(c.get(&key, t0 + std::time::Duration::from_secs(30)).is_some());
        assert!(c.get(&key, t0 + PROBE_REFRESH).is_none(), "refreshed every ten minutes");
        let swapped = ProbeKey { wwid: Some("naa.2".into()), ..key.clone() };
        assert!(c.get(&swapped, t0).is_none(), "a different drive under the same name");
        let reformatted = ProbeKey { sectors: 2000, ..key.clone() };
        assert!(c.get(&reformatted, t0).is_none());
        c.retain_names(&std::collections::HashSet::new());
        assert!(c.is_empty());
    }

    #[test]
    fn kernel_sector_sizes() {
        assert!(kernel_usable_block_size(512));
        assert!(kernel_usable_block_size(4096));
        assert!(!kernel_usable_block_size(520));
        assert!(!kernel_usable_block_size(528));
    }

    #[test]
    fn mount_matching() {
        assert!(mount_source_is_disk("/dev/sda", "sda"));
        assert!(mount_source_is_disk("/dev/sda1", "sda"));
        assert!(!mount_source_is_disk("/dev/sdab", "sda"));
        assert!(!mount_source_is_disk("/dev/sdab1", "sda"));
        assert!(mount_source_is_disk("/dev/nvme0n1p2", "nvme0n1"));
        assert!(!mount_source_is_disk("/dev/nvme0n10", "nvme0n1"));
        assert!(!mount_source_is_disk("tmpfs", "sda"));
    }
}
