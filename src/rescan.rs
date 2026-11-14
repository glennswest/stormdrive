//! Rescanning the SAS hosts after start and while a known drive is
//! missing (#141).
//!
//! A SAS drive that spins up after the HBA's driver loaded (forge1: three
//! unformatted drives behind an IT-mode mpt3sas, arriving at varying times
//! after boot) is not always added by the driver on its own: its first
//! INQUIRY or TEST UNIT READY can fail while it is still becoming ready,
//! and then no SCSI device, no sg node and no block node appear until
//! something asks the host to scan again. The boot-time count is never
//! final, so for a while after start, and whenever the inventory holds a
//! drive it cannot see, the monitor asks every SAS host to scan (`- - -`
//! to `scsi_host/hostN/scan`, every `discovery.rescan_missing_secs`). A
//! drive that answers then comes up through the usual uevents and
//! discovery pass, and the inventory takes it back (missing → seen, stale state cleared: `monitor`).
//!
//! A scan of a host whose drives are all present is harmless: the SCSI
//! midlayer skips the LUNs it already has.

use std::path::Path;
use std::time::{Duration, Instant};

/// The SAS hosts in a sysfs tree at `sys`: `class/sas_host/hostN`, sorted
/// by number. Only SAS: a USB stick's or an AHCI port's host is no
/// business of this.
pub fn sas_hosts(sys: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(sys.join("class/sas_host")) else { return vec![] };
    let mut out: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.starts_with("host")).collect();
    out.sort_by_key(|n| (n.len(), n.clone()));
    out
}

/// Ask each host to scan every channel, target and LUN. One result per
/// host: a refused write (not root, a host going away) is said, not fatal.
pub fn scan(sys: &Path, hosts: &[String]) -> Vec<(String, Result<(), String>)> {
    hosts
        .iter()
        .map(|h| {
            let r = std::fs::write(sys.join("class/scsi_host").join(h).join("scan"), "- - -\n").map_err(|e| e.to_string());
            (h.clone(), r)
        })
        .collect()
}

/// For this long after stormdrive starts, the hosts are rescanned even
/// with nothing missing: a drive never seen before (a fresh install's
/// inventory is empty) cannot be missing, and unformatted drives "can vary
/// when they come up" (#141).
pub const AFTER_START: Duration = Duration::from_secs(15 * 60);

/// Whether a rescan is due: something is missing or stormdrive started
/// less than [`AFTER_START`] ago, rescans are on (`every` > 0) and the last
/// one was at least `every` seconds ago.
pub fn due(missing: usize, up: Duration, every: u64, last: Option<Instant>, now: Instant) -> bool {
    (missing > 0 || up < AFTER_START)
        && every > 0
        && match last {
            None => true,
            Some(t) => now.saturating_duration_since(t) >= Duration::from_secs(every),
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rescans_only_while_something_is_missing() {
        let now = Instant::now();
        let long = AFTER_START;
        assert!(!due(0, long, 60, None, now), "nothing missing, long up");
        assert!(due(0, Duration::from_secs(30), 60, None, now), "just started: drives still arriving");
        assert!(due(3, long, 60, None, now), "first one at once");
        assert!(!due(3, long, 0, None, now), "0 turns it off");
        let later = now + Duration::from_secs(59);
        assert!(!due(3, long, 60, Some(now), later));
        assert!(due(3, long, 60, Some(now), now + Duration::from_secs(60)));
    }

    #[test]
    fn the_sas_hosts_are_scanned_and_nothing_else() {
        let sys = std::env::temp_dir().join(format!("sd-rescan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sys);
        for h in ["host10", "host2", "host0"] {
            std::fs::create_dir_all(sys.join("class/sas_host").join(h)).unwrap();
        }
        for h in ["host0", "host2", "host10", "host7"] {
            std::fs::create_dir_all(sys.join("class/scsi_host").join(h)).unwrap();
        }
        let hosts = sas_hosts(&sys);
        assert_eq!(hosts, ["host0", "host2", "host10"], "host7 is not SAS");
        let r = scan(&sys, &hosts);
        assert!(r.iter().all(|(_, r)| r.is_ok()), "{r:?}");
        assert_eq!(std::fs::read_to_string(sys.join("class/scsi_host/host2/scan")).unwrap(), "- - -\n");
        assert!(!sys.join("class/scsi_host/host7/scan").exists());
        let gone = scan(&sys, &["host9".to_string()]);
        assert!(gone[0].1.is_err(), "a host that is gone is said");
        assert!(sas_hosts(&sys.join("nowhere")).is_empty());
        let _ = std::fs::remove_dir_all(&sys);
    }
}
