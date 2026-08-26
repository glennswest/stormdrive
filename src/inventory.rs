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
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TrendSample {
    pub unix_secs: u64,
    pub wear_pct: Option<u8>,
    pub media_errors: u64,
}

const MAX_TREND_SAMPLES: usize = 512;

impl Inventory {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(serde_json::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomic write: tmp + rename, so a crash mid-write never leaves a torn
    /// inventory.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = PathBuf::from(format!("{}.tmp", path.display()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
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
        self.drives.values().find(|d| {
            d.name == handle
                || d.path == handle
                || d.paths.iter().any(|p| p == handle)
                || d.serial == handle
                || d.wwid.as_deref() == Some(handle)
        })
    }
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
            wwid: None,
            capacity_bytes: 1 << 30,
            block_size: 512,
            location: Location::default(),
            membership: Membership::Out,
            designation: Designation::None,
            activity: Activity::Idle,
            health: HealthReport::default(),
            first_seen: SystemTime::now(),
            last_seen: SystemTime::now(),
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

        let loaded = Inventory::load(&path).unwrap();
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
    fn missing_file_is_empty_inventory() {
        let inv = Inventory::load(Path::new("/definitely/not/here.json")).unwrap();
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
}
