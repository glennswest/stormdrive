//! Persistent drive registry. Deliberately the opposite of stormblock's
//! in-memory `Vec<DriveInfo>`: identity, first_seen, and health history
//! survive restarts via atomic writes to `<data_dir>/inventory.json`.

use crate::drive::{Drive, DriveId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Inventory {
    pub drives: HashMap<DriveId, Drive>,
    /// Wear-trend samples per drive: (unix_secs, wear_pct, media_errors).
    #[serde(default)]
    pub trends: HashMap<DriveId, Vec<TrendSample>>,
    /// Bays whose fault LED stormdrive lit for a failed RAID set member
    /// (#44), `<shelf key>/<bay>`: kept so a restart can still clear them.
    #[serde(default)]
    pub fault_bays: std::collections::BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TrendSample {
    pub unix_secs: u64,
    pub wear_pct: Option<u8>,
    pub media_errors: u64,
}

const MAX_TREND_SAMPLES: usize = 512;

impl Inventory {
    /// The kept inventory, or an empty one when it is absent or cannot be
    /// read (#102): discovery finds the drives again, so a file a reset cut
    /// must not stop the service. The note says what happened to the file.
    pub fn load(path: &Path) -> (Self, Option<String>) {
        let (inv, note) = load_state(path);
        (inv.unwrap_or_default(), note)
    }

    /// Atomic write: a crash or a reset mid-write never leaves a torn
    /// inventory (see [`write_atomic`]).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        write_atomic(path, &self.to_bytes()?)
    }

    /// Compact JSON: at 160 drives the file is rewritten every tick, and
    /// pretty-printing roughly doubled it.
    pub fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn record_trend(&mut self, id: DriveId, sample: TrendSample) {
        let v = self.trends.entry(id).or_default();
        v.push(sample);
        if v.len() > MAX_TREND_SAMPLES {
            let excess = v.len() - MAX_TREND_SAMPLES;
            v.drain(..excess);
        }
    }

    /// Resolve an API handle: DriveId uuid, kernel name, /dev path, serial,
    /// or wwid.
    pub fn resolve(&self, handle: &str) -> Option<&Drive> {
        if let Ok(u) = handle.parse::<uuid::Uuid>() {
            if let Some(d) = self.drives.get(&DriveId(u)) {
                return Some(d);
            }
        }
        // Most specific first: a WWN names one namespace, where NVMe-oF
        // namespaces can share a controller serial (stormblock#136).
        let by = |f: &dyn Fn(&Drive) -> bool| self.drives.values().find(|d| f(d));
        by(&|d| d.wwid.as_deref().is_some_and(|w| w.eq_ignore_ascii_case(handle)))
            .or_else(|| by(&|d| d.name == handle || d.path == handle || d.paths.iter().any(|p| p == handle)))
            .or_else(|| by(&|d| !d.serial.is_empty() && d.serial == handle))
    }
}

/// Temp file, fsync, rename, fsync of the directory. Without the first
/// fsync a reset can keep the rename and lose the data, leaving the file
/// empty or full of NULs where the old one was (#102).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    write_synced(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        sync_dir(dir);
    }
    Ok(())
}

/// Write a file and wait for its data to reach the disk.
pub fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Make a rename in `dir` durable. Best effort: the data is already safe.
pub fn sync_dir(dir: &Path) {
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Nothing a writer meant to keep: no bytes, or only NULs and whitespace.
pub fn is_blank(bytes: &[u8]) -> bool {
    bytes.iter().all(|b| *b == 0 || b.is_ascii_whitespace())
}

/// Read one of our JSON state files at start. Never fails (#102): a state
/// file that cannot be read must not stop the service.
/// - absent: `(None, None)`
/// - blank: `(None, note)`; the next write replaces it
/// - unreadable or not what we wrote: `(None, note)`, moved to
///   `<file>.unreadable` so it can be looked at and the next write does not
///   destroy it
///
/// The note names the file and is also logged; the caller raises it as an
/// event.
pub fn load_state<T: serde::de::DeserializeOwned>(path: &Path) -> (Option<T>, Option<String>) {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (None, None),
        Err(e) => return (None, Some(set_aside(path, &e.to_string()))),
    };
    if is_blank(&bytes) {
        let note = format!("{}: blank ({} bytes), starting without it", path.display(), bytes.len());
        tracing::warn!("{note}");
        return (None, Some(note));
    }
    match serde_json::from_slice(&bytes) {
        Ok(v) => (Some(v), None),
        Err(e) => {
            let start: Vec<String> = bytes.iter().take(16).map(|b| format!("{b:02x}")).collect();
            (None, Some(set_aside(path, &format!("{e} ({} bytes, starts {})", bytes.len(), start.join(" ")))))
        }
    }
}

/// Move a state file out of the way of the next write, and say so.
fn set_aside(path: &Path, why: &str) -> String {
    let aside = PathBuf::from(format!("{}.unreadable", path.display()));
    let note = match std::fs::rename(path, &aside) {
        Ok(()) => format!("{}: {why}; moved to {}, starting without it", path.display(), aside.display()),
        Err(e) => format!("{}: {why}; could not move it to {} ({e}), starting without it", path.display(), aside.display()),
    };
    tracing::error!("{note}");
    note
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::*;
    use std::time::SystemTime;

    fn drive(name: &str, serial: &str) -> Drive {
        Drive {
            id: DriveId::derive(None, "M", serial),
            path: format!("/dev/{name}"),
            name: name.into(),
            paths: vec![format!("/dev/{name}")],
            kind: DriveKind::SataSsd,
            model: "M".into(),
            serial: serial.into(),
            firmware: "1.0".into(),
            vendor: None,
            supports: None,
            link_rate_mbps: None,
            health_ack: None,
            history: None,
            wwid: None,
            capacity_bytes: 1 << 30,
            block_size: 512,
            physical_block_size: 512,
            usable: true,
            in_use_by: None,
            contents: None,
            slab_parts: vec![],
            engine_finding: None,
            enrolable: false,
            wear_projection: None,
            format: None,
            firmware_update: None,
            location: Location::default(),
            membership: Membership::Out,
            designation: Designation::None,
            overcommit: Default::default(),
            activity: Activity::Idle,
            health: HealthReport::default(),
            first_seen: SystemTime::now(),
            last_seen: SystemTime::now(),
            pushed_labels: Vec::new(),
            pushed_health: None,
            pushed_overcommit: None,
            replaces: None,
            drain: None,
            usage: None,
            fleet_partition: None,
        }
    }

    #[test]
    fn roundtrip_and_resolve() {
        let dir = std::env::temp_dir().join(format!("sd-inv-{}", std::process::id()));
        let path = dir.join("inventory.json");
        let mut inv = Inventory::default();
        let d = drive("sda", "S1");
        let id = d.id;
        inv.drives.insert(id, d);
        inv.record_trend(
            id,
            TrendSample {
                unix_secs: 1,
                wear_pct: Some(3),
                media_errors: 0,
            },
        );
        inv.save(&path).unwrap();

        let (loaded, note) = Inventory::load(&path);
        assert_eq!(note, None);
        assert_eq!(loaded.drives.len(), 1);
        assert_eq!(loaded.trends[&id].len(), 1);
        assert!(loaded.resolve("sda").is_some());
        assert!(loaded.resolve("/dev/sda").is_some());
        assert!(loaded.resolve("S1").is_some());
        assert!(loaded.resolve(&id.to_string()).is_some());
        assert!(loaded.resolve("nope").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_prefers_the_wwn_over_a_shared_serial() {
        // NVMe-oF namespaces share a controller serial; the WWN tells them
        // apart, and it is what stormblock's placement names.
        let mut inv = Inventory::default();
        for (name, wwn) in [("nvme1n1", "uuid.aaaa"), ("nvme1n2", "uuid.bbbb")] {
            let mut d = drive(name, "SB010A");
            d.wwid = Some(wwn.into());
            d.id = DriveId::derive(Some(wwn), "M", "SB010A");
            inv.drives.insert(d.id, d);
        }
        assert_eq!(inv.resolve("uuid.bbbb").unwrap().name, "nvme1n2");
        assert_eq!(inv.resolve("UUID.AAAA").unwrap().name, "nvme1n1");
        assert_eq!(inv.resolve("/dev/nvme1n2").unwrap().name, "nvme1n2");
        assert!(inv.resolve("SB010A").is_some());
    }

    #[test]
    fn missing_file_is_empty_inventory() {
        let (inv, note) = Inventory::load(Path::new("/definitely/not/here.json"));
        assert_eq!(note, None, "absent is the normal first start");
        assert!(inv.drives.is_empty());
    }

    #[test]
    fn trend_ring_caps() {
        let mut inv = Inventory::default();
        let id = DriveId::derive(None, "m", "s");
        for i in 0..600u64 {
            inv.record_trend(
                id,
                TrendSample {
                    unix_secs: i,
                    wear_pct: None,
                    media_errors: 0,
                },
            );
        }
        assert_eq!(inv.trends[&id].len(), MAX_TREND_SAMPLES);
        assert_eq!(inv.trends[&id][0].unix_secs, 600 - MAX_TREND_SAMPLES as u64);
    }

    /// What forge1 printed (#102): NULs, not an empty file, read that way.
    #[test]
    fn the_message_on_forge1_is_a_file_starting_with_a_non_json_byte() {
        let nuls = serde_json::from_slice::<Inventory>(&[0u8; 64]).unwrap_err().to_string();
        assert_eq!(nuls, "expected value at line 1 column 1");
        let empty = serde_json::from_slice::<Inventory>(b"").unwrap_err().to_string();
        assert_eq!(empty, "EOF while parsing a value at line 1 column 0");
    }

    fn state_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("stormdrive-state-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_blank_inventory_is_absent_and_said() {
        let dir = state_dir("blank");
        let path = dir.join("inventory.json");
        for blank in [&b""[..], &[0u8; 4096][..], &b" \n\0\n"[..]] {
            std::fs::write(&path, blank).unwrap();
            let (inv, note) = Inventory::load(&path);
            assert!(inv.drives.is_empty());
            let note = note.expect("said");
            assert!(note.contains("inventory.json") && note.contains("blank"), "{note}");
            assert!(!dir.join("inventory.json.unreadable").exists(), "nothing there to keep");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unparseable_inventory_is_kept_aside_and_the_next_save_works() {
        let dir = state_dir("garbage");
        let path = dir.join("inventory.json");
        for bad in [&b"\0\0not json"[..], &b"{\"drives\": {"[..], &b"[1,2]"[..], &[0xff, 0xfe, 0x00][..]] {
            std::fs::write(&path, bad).unwrap();
            let (inv, note) = Inventory::load(&path);
            assert!(inv.drives.is_empty());
            let note = note.expect("said");
            assert!(note.contains("inventory.json") && note.contains(".unreadable"), "{note}");
            assert_eq!(std::fs::read(dir.join("inventory.json.unreadable")).unwrap(), bad, "kept as it was");
            assert!(!path.exists());
        }
        Inventory::default().save(&path).unwrap();
        assert_eq!(Inventory::load(&path).1, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// EISDIR stands in for any read error (EIO on a bad block, EACCES).
    #[test]
    fn a_file_that_cannot_be_read_does_not_stop_the_load() {
        let dir = state_dir("noread");
        let path = dir.join("inventory.json");
        std::fs::create_dir(&path).unwrap();
        let (inv, note) = Inventory::load(&path);
        assert!(inv.drives.is_empty());
        assert!(note.expect("said").contains("inventory.json"));
        assert!(dir.join("inventory.json.unreadable").is_dir());
        Inventory::default().save(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_atomic_leaves_the_data_and_no_temp_file() {
        let dir = state_dir("atomic");
        let path = dir.join("sub/x.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert!(!dir.join("sub/x.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
