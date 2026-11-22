//! The release's firmware manifest (#80, #29 decided): which image golden
//! applies to which drive or IOM.
//!
//! Vendor drive and IOM firmware images are recorded goldens with their
//! source; the release carries a manifest naming, per model and current
//! revision, the image that replaces it. stormdrive reads it, says on each
//! drive and IOM what is available (`firmware_available`), and applies it
//! only when asked (`"manifest": true` on the firmware APIs) — never by
//! itself. The image is read from the golden beside the manifest and its
//! sha256 checked against the manifest every time; an upload to the store
//! stays for a one-off.
//!
//! ```toml
//! [[image]]
//! kind = "drive"                  # drive | iom
//! vendor = "SEAGATE"              # optional: INQUIRY vendor, any case
//! model = "ST1200MM0098"          # INQUIRY product / NVMe model / the shelf's SES product
//! from = ["N003", "N004"]         # revisions it replaces; empty = any but `version`
//! version = "N005"                # the revision the image installs
//! file = "ST1200MM0098-N005.lod"  # beside the manifest (a plain name)
//! sha256 = "…64 hex…"
//! source = "https://vendor.example/…"  # where the golden was recorded from
//! golden = "golden-fw-…"          # optional: the golden's name
//! ```
//!
//! Revision strings are the vendor's and not ordered, so "newer" is what the
//! manifest says: an entry applies to a drive whose revision is in `from`
//! (or, with no `from`, any revision but `version`). Two entries matching
//! one drive is a manifest error, reported and neither used.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Drive,
    Iom,
}

/// One manifest entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    pub model: String,
    #[serde(default)]
    pub from: Vec<String>,
    pub version: String,
    pub file: String,
    pub sha256: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub golden: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(default, rename = "image")]
    pub images: Vec<Entry>,
}

/// What a drive or IOM is offered: the entry's version and its golden.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Available {
    pub version: String,
    pub file: String,
    pub sha256: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub golden: Option<String>,
}

impl From<&Entry> for Available {
    fn from(e: &Entry) -> Self {
        Self { version: e.version.clone(), file: e.file.clone(), sha256: e.sha256.clone(), source: e.source.clone(), golden: e.golden.clone() }
    }
}

fn eq(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

impl Entry {
    fn check(&self) -> Result<(), String> {
        let who = format!("{} {}", self.model, self.version);
        if self.model.trim().is_empty() || self.version.trim().is_empty() {
            return Err(format!("{who}: model and version are required"));
        }
        if !crate::firmware::valid_image_name(&self.file) {
            return Err(format!("{who}: file {:?} is not a plain file name", self.file));
        }
        let h = self.sha256.trim();
        if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{who}: sha256 must be 64 hex digits"));
        }
        let s = self.source.trim();
        if !(s.starts_with("https://") || s.starts_with("http://")) {
            return Err(format!("{who}: source must be the URL the golden was recorded from"));
        }
        if self.from.iter().any(|f| eq(f, &self.version)) {
            return Err(format!("{who}: from names the version it installs"));
        }
        Ok(())
    }

    /// Whether this entry replaces `revision` on a `kind` of this vendor
    /// and model.
    pub fn applies(&self, kind: Kind, vendor: Option<&str>, model: &str, revision: &str) -> bool {
        if self.kind != kind || !eq(&self.model, model) || revision.trim().is_empty() {
            return false;
        }
        if let Some(v) = &self.vendor {
            if !vendor.is_some_and(|x| eq(v, x)) {
                return false;
            }
        }
        if self.from.is_empty() {
            !eq(&self.version, revision)
        } else {
            self.from.iter().any(|f| eq(f, revision))
        }
    }
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self, String> {
        let m: Manifest = toml::from_str(text).map_err(|e| e.to_string())?;
        for e in &m.images {
            e.check()?;
        }
        Ok(m)
    }

    /// The one entry for this device, `Ok(None)` when none, an error when
    /// the manifest names two.
    pub fn find(&self, kind: Kind, vendor: Option<&str>, model: &str, revision: &str) -> Result<Option<&Entry>, String> {
        let hits: Vec<&Entry> = self.images.iter().filter(|e| e.applies(kind, vendor, model, revision)).collect();
        match hits.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(*one)),
            many => Err(format!(
                "the firmware manifest names {} images for {model} at {revision}: {}",
                many.len(),
                many.iter().map(|e| e.file.as_str()).collect::<Vec<_>>().join(", ")
            )),
        }
    }
}

/// The manifest as read from its path: where, when, what, or why not.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Loaded {
    pub path: Option<PathBuf>,
    /// The file is there and parsed.
    pub loaded: bool,
    /// Why it is not: unreadable or invalid. An absent file is no error:
    /// a release without firmware goldens has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub manifest: Manifest,
    #[serde(skip)]
    stamp: Option<(SystemTime, u64)>,
}

impl Loaded {
    pub fn read(path: &Path) -> Self {
        let mut l = Loaded { path: Some(path.to_path_buf()), ..Default::default() };
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return l,
            Err(e) => {
                l.error = Some(format!("{}: {e}", path.display()));
                return l;
            }
        };
        l.stamp = Some((meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len()));
        match std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|t| Manifest::parse(&t)) {
            Ok(m) => {
                l.manifest = m;
                l.loaded = true;
            }
            Err(e) => l.error = Some(format!("{}: {e}", path.display())),
        }
        l
    }

    /// The directory the images are in: the manifest's own.
    pub fn dir(&self) -> Option<&Path> {
        self.path.as_deref().and_then(Path::parent)
    }

    /// The entry for this device as `firmware_available`, or the error
    /// that says why none is offered.
    pub fn available(&self, kind: Kind, vendor: Option<&str>, model: &str, revision: &str) -> Result<Option<Available>, String> {
        Ok(self.manifest.find(kind, vendor, model, revision)?.map(Available::from))
    }

    /// The image for this device, read from the golden and checked against
    /// the manifest's sha256. -> (the entry, the bytes).
    pub fn image(&self, kind: Kind, vendor: Option<&str>, model: &str, revision: &str) -> Result<(Entry, Vec<u8>), String> {
        if let Some(e) = &self.error {
            return Err(format!("the firmware manifest is not usable: {e}"));
        }
        let e = self
            .manifest
            .find(kind, vendor, model, revision)?
            .ok_or_else(|| format!("the firmware manifest names no image for {model} at {revision}"))?
            .clone();
        let dir = self.dir().ok_or("no firmware manifest path")?;
        let p = dir.join(&e.file);
        let data = std::fs::read(&p).map_err(|x| format!("image {} ({}): {x}", e.file, p.display()))?;
        if data.is_empty() {
            return Err(format!("image {} is empty", e.file));
        }
        let got = crate::firmware::sha256_hex(&data);
        if got != e.sha256.trim().to_lowercase() {
            return Err(format!("image {}: sha256 {got}, not the manifest's {}", e.file, e.sha256.trim()));
        }
        Ok((e, data))
    }
}

struct Cache {
    path: Option<PathBuf>,
    current: Arc<Loaded>,
}

fn cache() -> &'static Mutex<Cache> {
    static C: OnceLock<Mutex<Cache>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(Cache { path: None, current: Arc::new(Loaded::default()) }))
}

/// Where this daemon's manifest is (`firmware.manifest`; empty = none).
/// Set once at start.
pub fn init(path: &str) {
    let mut c = cache().lock().unwrap();
    c.path = (!path.trim().is_empty()).then(|| PathBuf::from(path));
    c.current = Arc::new(c.path.as_deref().map(Loaded::read).unwrap_or_default());
}

/// The manifest now: re-read when the file changed (a stat per call).
/// Before `init` (unit tests): none.
pub fn current() -> Arc<Loaded> {
    let mut c = cache().lock().unwrap();
    let Some(path) = c.path.clone() else {
        return c.current.clone();
    };
    let stamp = std::fs::metadata(&path).ok().map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
    if stamp != c.current.stamp || (stamp.is_none() && c.current.loaded) {
        c.current = Arc::new(Loaded::read(&path));
    }
    c.current.clone()
}

/// A drive's offer as JSON: the entry, `null`, or `{"error": …}`.
pub fn drive_available(l: &Loaded, d: &crate::drive::Drive) -> serde_json::Value {
    match l.available(Kind::Drive, d.vendor.as_deref(), &d.model, &d.firmware) {
        Ok(a) => serde_json::to_value(a).unwrap_or_default(),
        Err(e) => serde_json::json!({ "error": e }),
    }
}

/// An IOM's offer: the shelf's SES vendor/product, the IOM's revision.
pub fn iom_available(l: &Loaded, shelf: &crate::drive::Shelf, revision: Option<&str>) -> serde_json::Value {
    let (Some(model), Some(rev)) = (shelf.model.as_deref(), revision) else {
        return serde_json::Value::Null;
    };
    match l.available(Kind::Iom, shelf.vendor.as_deref(), model, rev) {
        Ok(a) => serde_json::to_value(a).unwrap_or_default(),
        Err(e) => serde_json::json!({ "error": e }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn entry(model: &str, from: &[&str], version: &str) -> String {
        format!(
            "[[image]]\nkind = \"drive\"\nmodel = \"{model}\"\nfrom = [{}]\nversion = \"{version}\"\nfile = \"{model}-{version}.lod\"\nsha256 = \"{SHA}\"\nsource = \"https://vendor.example/{model}\"\n",
            from.iter().map(|f| format!("\"{f}\"")).collect::<Vec<_>>().join(", ")
        )
    }

    #[test]
    fn an_entry_applies_to_its_model_at_a_listed_revision_only() {
        let m = Manifest::parse(&entry("ST1200MM0098", &["N003", "N004"], "N005")).unwrap();
        assert_eq!(m.find(Kind::Drive, None, " st1200mm0098 ", "N004").unwrap().unwrap().version, "N005");
        assert!(m.find(Kind::Drive, None, "ST1200MM0098", "N005").unwrap().is_none(), "already at it");
        assert!(m.find(Kind::Drive, None, "ST1200MM0098", "N002").unwrap().is_none(), "not a revision it replaces");
        assert!(m.find(Kind::Drive, None, "ST2000NM0045", "N004").unwrap().is_none(), "another model");
        assert!(m.find(Kind::Iom, None, "ST1200MM0098", "N004").unwrap().is_none(), "another kind");
        assert!(m.find(Kind::Drive, None, "ST1200MM0098", "").unwrap().is_none(), "no revision read");
    }

    #[test]
    fn no_from_means_any_revision_but_the_one_installed() {
        let m = Manifest::parse(&entry("X425_HCBEP1T2A10", &[], "NA04")).unwrap();
        assert!(m.find(Kind::Drive, None, "X425_HCBEP1T2A10", "NA02").unwrap().is_some());
        assert!(m.find(Kind::Drive, None, "X425_HCBEP1T2A10", "na04").unwrap().is_none());
    }

    #[test]
    fn a_vendor_when_named_must_match() {
        let t = entry("DS2246", &["0401"], "0500").replace("kind = \"drive\"", "kind = \"iom\"\nvendor = \"NETAPP\"");
        let m = Manifest::parse(&t).unwrap();
        assert!(m.find(Kind::Iom, Some("NetApp  "), "DS2246", "0401").unwrap().is_some());
        assert!(m.find(Kind::Iom, Some("LSI"), "DS2246", "0401").unwrap().is_none());
        assert!(m.find(Kind::Iom, None, "DS2246", "0401").unwrap().is_none());
    }

    #[test]
    fn two_images_for_one_drive_is_an_error_and_neither_is_offered() {
        let m = Manifest::parse(&(entry("M", &["A"], "B") + &entry("M", &[], "C"))).unwrap();
        assert!(m.find(Kind::Drive, None, "M", "A").unwrap_err().contains("2 images"));
        assert_eq!(m.find(Kind::Drive, None, "M", "B").unwrap().unwrap().version, "C");
    }

    #[test]
    fn bad_entries_are_refused() {
        assert!(Manifest::parse(&entry("M", &["A"], "B").replace(SHA, "abc")).unwrap_err().contains("sha256"));
        assert!(Manifest::parse(&entry("M", &["A"], "B").replace("https://", "file://")).unwrap_err().contains("source"));
        assert!(Manifest::parse(&entry("M", &["A"], "B").replace("M-B.lod", "../x")).unwrap_err().contains("plain file"));
        assert!(Manifest::parse(&entry("M", &["B"], "B")).unwrap_err().contains("installs"));
        assert!(Manifest::parse(&(entry("M", &["A"], "B") + "colour = \"red\"\n")).is_err(), "unknown keys");
        assert_eq!(Manifest::parse("").unwrap().images.len(), 0);
    }

    #[test]
    fn the_image_comes_from_the_golden_beside_the_manifest_and_its_sha256_is_checked() {
        let dir = std::env::temp_dir().join(format!("sd-fwm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let img = b"firmware bytes".to_vec();
        let sha = crate::firmware::sha256_hex(&img);
        std::fs::write(dir.join("M-B.lod"), &img).unwrap();
        let path = dir.join("manifest.toml");
        assert!(!Loaded::read(&path).loaded && Loaded::read(&path).error.is_none(), "absent: no manifest, no error");
        std::fs::write(&path, entry("M", &["A"], "B").replace(SHA, &sha)).unwrap();
        let l = Loaded::read(&path);
        assert!(l.loaded, "{:?}", l.error);
        let (e, data) = l.image(Kind::Drive, None, "M", "A").unwrap();
        assert_eq!((e.version.as_str(), data), ("B", img.clone()));
        assert!(l.image(Kind::Drive, None, "M", "B").unwrap_err().contains("names no image"));
        std::fs::write(dir.join("M-B.lod"), b"tampered").unwrap();
        assert!(l.image(Kind::Drive, None, "M", "A").unwrap_err().contains("not the manifest's"));
        std::fs::remove_file(dir.join("M-B.lod")).unwrap();
        assert!(l.image(Kind::Drive, None, "M", "A").is_err(), "the golden missing");
        std::fs::write(&path, "[[image]]\nkind = \"tape\"\n").unwrap();
        let bad = Loaded::read(&path);
        assert!(!bad.loaded && bad.error.is_some());
        assert!(bad.image(Kind::Drive, None, "M", "A").unwrap_err().contains("not usable"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
