//! The node's system drive(s), where the install can see them (#66).
//!
//! The owner's install rule 3 (stormdrive#58, stormblock#351): the system
//! drive is the one already holding this node's system slab, else "a drive
//! designated `system` in stormdrive", else one unambiguous drive, else the
//! install stops. stormdrive's inventory lives in `data_dir`, which an
//! install wipes, so the designation is also written to system-data, the
//! volume every install keeps: `<history.dir>/stormdrive/system-drives.json`,
//! each drive by WWN and serial with its model, kind, size, shelf and bay.
//!
//! The file is rewritten (atomically) whenever the set changes. After an
//! install stormdrive reads it back once and designates the drives it names
//! again, matched by WWN, else model + serial; from then on the inventory is
//! the truth. A drive named in the file and not seen since keeps its line,
//! `present: false`, so a drive pulled during an install is not forgotten.

use crate::drive::{Activity, Designation, Drive, DriveId};
use crate::events::Severity;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Under the system-data directory.
pub const FILE: &str = "stormdrive/system-drives.json";
/// The file's shape; bumped on an incompatible change.
pub const VERSION: u32 = 1;

/// One system-designated drive, as the install matches it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// stormdrive's stable drive id (uuid5 of the WWN).
    pub id: String,
    /// The drive's WWN as sysfs `wwid` gives it; the first thing to match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wwn: Option<String>,
    pub serial: String,
    pub model: String,
    pub kind: String,
    pub capacity_bytes: u64,
    /// The shelf (SES logical id or serial) and bay, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shelf: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bay: Option<u32>,
    /// /dev path when last seen; never a way to match (it moves).
    pub path: String,
    /// Seen by this run of stormdrive. False: kept from the file.
    pub present: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct File {
    pub version: u32,
    pub node: String,
    pub updated: String,
    pub stormdrive: String,
    pub drives: Vec<Entry>,
}

/// The line for a drive.
pub fn entry(d: &Drive) -> Entry {
    Entry {
        id: d.id.0.to_string(),
        wwn: d.wwid.clone().filter(|w| !w.trim().is_empty()),
        serial: d.serial.clone(),
        model: d.model.clone(),
        kind: serde_json::to_value(d.kind).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default(),
        capacity_bytes: d.capacity_bytes,
        shelf: d.location.shelf.as_ref().and_then(|s| s.key()),
        bay: d.location.bay,
        path: d.path.clone(),
        present: d.activity != Activity::Missing,
    }
}

/// Is `d` the drive this line names? WWN when both have one; else model and
/// serial (a serial alone is not unique across vendors).
pub fn names(e: &Entry, d: &Drive) -> bool {
    if let (Some(a), Some(b)) = (e.wwn.as_deref(), d.wwid.as_deref().map(str::trim).filter(|w| !w.is_empty())) {
        return a.trim().eq_ignore_ascii_case(b);
    }
    !e.serial.trim().is_empty() && e.serial.trim() == d.serial.trim() && e.model.trim() == d.model.trim()
}

/// What one sync does, decided from the inventory and the lines still kept
/// from the file.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// Drives to designate `system` again (they had no designation).
    pub designate: Vec<DriveId>,
    /// Kept lines whose drive was found with another designation: left
    /// as the operator set it, said in an event.
    pub differs: Vec<(DriveId, Designation)>,
    /// Kept lines with no drive in the inventory yet.
    pub kept: Vec<Entry>,
    /// The file's drives after this sync.
    pub drives: Vec<Entry>,
}

pub fn plan<'a>(inventory: impl Iterator<Item = &'a Drive> + Clone, kept: &[Entry]) -> Plan {
    let mut p = Plan::default();
    for e in kept {
        match inventory.clone().find(|d| names(e, d)) {
            Some(d) if d.designation == Designation::None => p.designate.push(d.id),
            Some(d) if d.designation == Designation::System => {}
            Some(d) => p.differs.push((d.id, d.designation)),
            None => p.kept.push(e.clone()),
        }
    }
    let mut drives: Vec<Entry> = inventory
        .filter(|d| d.designation == Designation::System || p.designate.contains(&d.id))
        .map(entry)
        .collect();
    drives.extend(p.kept.iter().map(|e| Entry { present: false, ..e.clone() }));
    drives.sort_by(|a, b| a.id.cmp(&b.id));
    p.drives = drives;
    p
}

#[derive(Default)]
struct Inner {
    /// The file has been read (once, the first time system-data is there).
    loaded: bool,
    /// Lines from the file not yet matched to a drive.
    kept: Vec<Entry>,
    /// The drives last written, so an unchanged set is not rewritten.
    written: Option<Vec<Entry>>,
    last_error: Option<String>,
}

/// The system-drives record. Call [`sync`] after each discovery pass and
/// whenever a designation changes.
pub struct SystemDrives {
    root: PathBuf,
    inner: Mutex<Inner>,
}

/// What `GET /api/v1/system-drives` reports.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub file: String,
    pub written: bool,
    pub drives: Vec<Entry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl SystemDrives {
    pub fn new(system_data: impl Into<PathBuf>) -> Self {
        Self { root: system_data.into(), inner: Mutex::new(Inner::default()) }
    }

    pub fn path(&self) -> PathBuf {
        self.root.join(FILE)
    }

    pub fn status(&self) -> Status {
        let i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Status {
            file: self.path().display().to_string(),
            written: i.written.is_some(),
            drives: i.written.clone().unwrap_or_default(),
            last_error: i.last_error.clone(),
        }
    }
}

/// Read the file's drives. None when it is not there.
pub fn read(path: &Path) -> anyhow::Result<Option<Vec<Entry>>> {
    match std::fs::read(path) {
        Ok(b) => {
            let f: File = serde_json::from_slice(&b)?;
            Ok(Some(f.drives))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Write the file: temp file, fsync, rename, so the install never reads a
/// half-written one.
pub fn write(path: &Path, f: &File) -> anyhow::Result<()> {
    use std::io::Write;
    let dir = path.parent().ok_or_else(|| anyhow::anyhow!("no parent directory"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("json.tmp");
    let mut out = std::fs::File::create(&tmp)?;
    out.write_all(&serde_json::to_vec_pretty(f)?)?;
    out.sync_all()?;
    std::fs::rename(&tmp, path)?;
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// One sync: read the file the first time system-data is there, designate
/// the drives it names again, write the file when the set changed.
pub async fn sync(state: &Arc<crate::api::AppState>) {
    let sd = state.system_drives.clone();
    let h = state.history.clone();
    let path = sd.path();
    // The first read, off the async threads.
    let need_load = !sd.inner.lock().unwrap_or_else(|e| e.into_inner()).loaded;
    if need_load {
        let p = path.clone();
        let res = tokio::task::spawn_blocking(move || h.available().then(|| read(&p))).await;
        let mut i = sd.inner.lock().unwrap_or_else(|e| e.into_inner());
        match res {
            Ok(None) => return, // system-data not mounted: nothing to read or write
            Ok(Some(Ok(lines))) => {
                i.loaded = true;
                i.kept = lines.clone().unwrap_or_default();
                // What is on disk counts as written: unchanged, not rewritten.
                i.written = lines;
            }
            Ok(Some(Err(e))) => {
                // Unreadable: said, and replaced by what the inventory says.
                tracing::warn!(file = %path.display(), "system drives: not read: {e:#}");
                i.loaded = true;
                i.last_error = Some(format!("read: {e:#}"));
            }
            Err(e) => {
                tracing::warn!("system drives: {e}");
                return;
            }
        }
    } else if !state.history.available() {
        return;
    }

    let kept = sd.inner.lock().unwrap_or_else(|e| e.into_inner()).kept.clone();
    let (plan, names) = {
        let mut inv = state.inventory.write().await;
        let plan = plan(inv.drives.values(), &kept);
        let mut names = vec![];
        for id in &plan.designate {
            if let Some(d) = inv.drives.get_mut(id) {
                d.designation = Designation::System;
                names.push((*id, format!("{} ({} {})", d.name, d.model, d.serial)));
            }
        }
        for (id, _) in &plan.differs {
            if let Some(d) = inv.drives.get(id) {
                names.push((*id, format!("{} ({} {})", d.name, d.model, d.serial)));
            }
        }
        (plan, names)
    };
    if !names.is_empty() {
        let file = path.display().to_string();
        let mut log = state.events.write().await;
        for (id, who) in &names {
            match plan.differs.iter().find(|(d, _)| d == id) {
                Some((_, des)) => log.push(
                    Some(*id),
                    Severity::Warning,
                    "designation",
                    format!("{who}: {file} names it the system drive, but it is designated {} here: left as it is", des.word()),
                ),
                None => log.push(Some(*id), Severity::Info, "designation", format!("{who}: none → system (kept in {file})")),
            }
        }
    }

    let unchanged = {
        let mut i = sd.inner.lock().unwrap_or_else(|e| e.into_inner());
        i.kept = plan.kept.clone();
        i.written.as_ref() == Some(&plan.drives)
    };
    if unchanged {
        return;
    }
    let f = File {
        version: VERSION,
        node: state.node_name.clone(),
        updated: crate::controller::rfc3339(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()),
        stormdrive: crate::VERSION.to_string(),
        drives: plan.drives.clone(),
    };
    let p = path.clone();
    let res = tokio::task::spawn_blocking(move || write(&p, &f)).await;
    let mut i = sd.inner.lock().unwrap_or_else(|e| e.into_inner());
    match res {
        Ok(Ok(())) => {
            tracing::info!(file = %path.display(), drives = plan.drives.len(), "system drives written");
            i.written = Some(plan.drives);
            i.last_error = None;
        }
        Ok(Err(e)) => {
            tracing::warn!(file = %path.display(), "system drives: not written: {e:#}");
            i.last_error = Some(format!("write: {e:#}"));
        }
        Err(e) => i.last_error = Some(format!("write: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::Membership;

    fn drive(wwn: Option<&str>, serial: &str) -> Drive {
        let mut d: Drive = serde_json::from_value(serde_json::json!({
            "id": DriveId::derive(wwn, "M", serial).0,
            "name": "sda", "path": "/dev/sda", "kind": "sata_ssd",
            "model": "M", "serial": serial, "firmware": "1", "capacity_bytes": 256_000_000_000u64, "block_size": 512,
            "first_seen": {"secs_since_epoch": 0, "nanos_since_epoch": 0},
            "last_seen": {"secs_since_epoch": 0, "nanos_since_epoch": 0},
        }))
        .unwrap();
        d.wwid = wwn.map(String::from);
        d
    }

    #[test]
    fn a_line_names_a_drive_by_wwn_else_model_and_serial() {
        let d = drive(Some("naa.5000c500aaaa0001"), "S1");
        let mut e = entry(&d);
        e.wwn = Some("NAA.5000C500AAAA0001".into());
        assert!(names(&e, &d), "wwn, any case");
        e.serial = "other".into();
        assert!(names(&e, &d), "the wwn decides");
        e.wwn = Some("naa.5000c500aaaa0002".into());
        e.serial = "S1".into();
        assert!(!names(&e, &d), "another wwn is another drive, same serial or not");
        let bare = drive(None, "S1");
        let mut e = entry(&bare);
        assert!(names(&e, &bare));
        e.model = "N".into();
        assert!(!names(&e, &bare), "a serial alone is not unique");
        e.model = "M".into();
        e.serial = " ".into();
        assert!(!names(&e, &drive(None, " ")), "a blank serial names nothing");
    }

    #[test]
    fn after_an_install_the_file_designates_its_drives_again() {
        // The X9 blade (#66): SSD system, HDD data; data_dir wiped.
        let ssd = drive(Some("naa.1"), "SSD1");
        let hdd = drive(Some("naa.2"), "HDD1");
        let mut was = ssd.clone();
        was.designation = Designation::System;
        let pulled = entry(&drive(Some("naa.9"), "GONE"));
        let kept = vec![entry(&was), pulled.clone()];
        let inv = [ssd.clone(), hdd.clone()];
        let p = plan(inv.iter(), &kept);
        assert_eq!(p.designate, vec![ssd.id]);
        assert!(p.differs.is_empty());
        assert_eq!(p.kept, vec![pulled.clone()], "a drive not seen keeps its line");
        let ids: Vec<_> = p.drives.iter().map(|e| (e.id.clone(), e.present)).collect();
        assert!(ids.contains(&(ssd.id.0.to_string(), true)));
        assert!(ids.contains(&(pulled.id.clone(), false)));
        assert_eq!(p.drives.len(), 2, "the HDD is not a system drive");
    }

    #[test]
    fn the_inventory_wins_once_matched() {
        let mut d = drive(Some("naa.1"), "S");
        d.designation = Designation::Spare;
        let mut e = entry(&d);
        e.present = true;
        let p = plan([d.clone()].iter(), &[e]);
        assert!(p.designate.is_empty());
        assert_eq!(p.differs, vec![(d.id, Designation::Spare)]);
        assert!(p.drives.is_empty(), "spare here: no longer a system drive in the file");

        // Cleared by the operator after the match: gone from the file.
        let none = drive(Some("naa.1"), "S");
        let p = plan([none].iter(), &[]);
        assert!(p.drives.is_empty());

        // Designated here, nothing kept: written; a missing one is not present.
        let mut sys = drive(Some("naa.3"), "T");
        sys.designation = Designation::System;
        sys.activity = Activity::Missing;
        sys.membership = Membership::Out;
        let p = plan([sys.clone()].iter(), &[]);
        assert_eq!(p.drives.len(), 1);
        assert!(!p.drives[0].present);
    }

    #[test]
    fn the_file_round_trips_through_an_atomic_write() {
        let dir = std::env::temp_dir().join(format!("stormdrive-sysdrives-{}", std::process::id()));
        let path = dir.join(FILE);
        assert!(read(&path).unwrap().is_none());
        let mut d = drive(Some("naa.1"), "S");
        d.designation = Designation::System;
        let f = File { version: VERSION, node: "n".into(), updated: "t".into(), stormdrive: "v".into(), drives: vec![entry(&d)] };
        write(&path, &f).unwrap();
        assert_eq!(read(&path).unwrap().unwrap(), f.drives);
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
