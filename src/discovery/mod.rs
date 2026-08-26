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
    pub block_size: u32,
}

/// Kernel names that are never physical drives (or are somebody else's
/// export surface — ublkb* is stormblock's own).
const BUILTIN_EXCLUDE: &[&str] = &[
    "loop", "ram", "zram", "dm-", "md", "sr", "fd", "nbd", "ublkb", "zd", "pmem", "drbd",
];

pub fn name_eligible(cfg: &DiscoveryConfig, name: &str) -> bool {
    if BUILTIN_EXCLUDE.iter().any(|p| name.starts_with(p)) {
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

    fn has_mounted_partition(name: &str) -> bool {
        let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
            return false;
        };
        mounts
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .any(|src| mount_source_is_disk(src, name))
    }

    fn classify(base: &Path, name: &str) -> DriveKind {
        if name.starts_with("nvme") {
            return DriveKind::NvmeSsd;
        }
        let rotational = read_trim(&base.join("queue/rotational")).as_deref() == Some("1");
        let is_sas = base.join("device/sas_address").exists();
        match (is_sas, rotational) {
            (true, false) => DriveKind::SasSsd,
            (true, true) => DriveKind::SasHdd,
            (false, false) => DriveKind::SataSsd,
            (false, true) => DriveKind::SataHdd,
        }
    }

    pub fn scan(cfg: &DiscoveryConfig) -> Vec<Observed> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir("/sys/block") else {
            return out;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name_eligible(cfg, &name) {
                continue;
            }
            if !cfg.manage_mounted && has_mounted_partition(&name) {
                tracing::debug!(%name, "skipping drive with mounted partitions");
                continue;
            }
            let base = e.path();
            let sectors: u64 = read_trim(&base.join("size"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if sectors == 0 {
                continue; // empty card reader slots etc.
            }
            let block_size: u32 = read_trim(&base.join("queue/logical_block_size"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(512);
            let dev = base.join("device");
            let model = read_trim(&dev.join("model")).unwrap_or_default();
            let serial = read_trim(&dev.join("serial"))
                .or_else(|| read_trim(&base.join("serial")))
                .unwrap_or_default();
            let firmware = read_trim(&dev.join("firmware_rev"))
                .or_else(|| read_trim(&dev.join("rev")))
                .unwrap_or_default();
            let wwid = read_trim(&base.join("wwid")).or_else(|| read_trim(&dev.join("wwid")));
            out.push(Observed {
                path: format!("/dev/{name}"),
                kind: classify(&base, &name),
                model,
                serial,
                firmware,
                wwid,
                capacity_bytes: sectors * 512,
                block_size,
                name,
            });
        }
        out
    }
}

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
