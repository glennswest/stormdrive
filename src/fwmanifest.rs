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
//!
//! **Cards (#184).** A SAS card's entry is `kind = "card"` with a `region`:
//!
//! ```toml
//! [[image]]
//! kind = "card"
//! region = "firmware"             # firmware | bios | uefi | nvdata
//! model = "HBA330"                # the board name (Manufacturing Page 0) or the PCI id "1000:0097"
//! vendor = "1028:1f45"            # optional: the card's PCI subsystem (vendor:device)
//! from = ["16.00.01.00"]          # the region's versions it replaces
//! version = "16.17.01.00"
//! file = "HBA330-16.17.01.00.bin"
//! sha256 = "…"
//! source = "https://…"
//! ```
//!
//! The region's current version is the card's own: `firmwareVersion`,
//! `biosVersion`, `nvdataVersion` (persistent). The card reports no UEFI
//! driver version, so a `uefi` entry is recorded and listed but offered to
//! no card. A CardOperation flash writes the `firmware` and `bios` regions
//! (#166): only those entries are `flashable`; NVDATA travels inside the
//! firmware image and the UEFI driver inside the bios option ROM.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Drive,
    Iom,
    Card,
}

/// A card's firmware region a manifest entry is for (#184).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardRegion {
    Firmware,
    Bios,
    Uefi,
    Nvdata,
}

impl CardRegion {
    pub const ALL: [CardRegion; 4] = [CardRegion::Firmware, CardRegion::Bios, CardRegion::Uefi, CardRegion::Nvdata];
    pub fn as_str(self) -> &'static str {
        match self {
            CardRegion::Firmware => "firmware",
            CardRegion::Bios => "bios",
            CardRegion::Uefi => "uefi",
            CardRegion::Nvdata => "nvdata",
        }
    }
    /// The region a CardOperation flash writes for it, when it writes one.
    pub fn flash_region(self) -> Option<crate::mpt::Region> {
        match self {
            CardRegion::Firmware => Some(crate::mpt::Region::Firmware),
            CardRegion::Bios => Some(crate::mpt::Region::Bios),
            CardRegion::Uefi | CardRegion::Nvdata => None,
        }
    }
    pub fn of_flash(r: crate::mpt::Region) -> Option<CardRegion> {
        match r {
            crate::mpt::Region::Firmware => Some(CardRegion::Firmware),
            crate::mpt::Region::Bios => Some(CardRegion::Bios),
            _ => None,
        }
    }
}

/// One manifest entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub kind: Kind,
    /// A card's region (#184); only for `kind = "card"`, and required there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<CardRegion>,
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
    /// A card entry's region (#184).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<CardRegion>,
    pub version: String,
    pub file: String,
    pub sha256: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub golden: Option<String>,
}

impl From<&Entry> for Available {
    fn from(e: &Entry) -> Self {
        Self { region: e.region, version: e.version.clone(), file: e.file.clone(), sha256: e.sha256.clone(), source: e.source.clone(), golden: e.golden.clone() }
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
        match (self.kind, self.region) {
            (Kind::Card, None) => return Err(format!("{who}: a card entry names its region (firmware, bios, uefi, nvdata)")),
            (Kind::Drive | Kind::Iom, Some(_)) => return Err(format!("{who}: region is for a card entry")),
            _ => {}
        }
        if self.from.iter().any(|f| eq(f, &self.version)) {
            return Err(format!("{who}: from names the version it installs"));
        }
        Ok(())
    }

    /// Whether this entry replaces `revision` on a `kind` of this vendor
    /// and model.
    pub fn applies(&self, kind: Kind, vendor: Option<&str>, model: &str, revision: &str) -> bool {
        self.kind == kind && self.region.is_none() && eq(&self.model, model) && self.applies_to(vendor, revision)
    }

    /// Whether this card entry replaces `revision` in `region` of a card
    /// known by any of `models` (its board name, its PCI id) with this PCI
    /// subsystem (#184).
    pub fn applies_card(&self, region: CardRegion, subsystem: Option<&str>, models: &[String], revision: &str) -> bool {
        self.kind == Kind::Card && self.region == Some(region) && models.iter().any(|m| eq(&self.model, m)) && self.applies_to(subsystem, revision)
    }

    fn applies_to(&self, vendor: Option<&str>, revision: &str) -> bool {
        if revision.trim().is_empty() {
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
        one_of(hits, model, revision)
    }

    /// The one entry for a card's region (#184).
    pub fn find_card(&self, region: CardRegion, subsystem: Option<&str>, models: &[String], revision: &str) -> Result<Option<&Entry>, String> {
        let hits: Vec<&Entry> = self.images.iter().filter(|e| e.applies_card(region, subsystem, models, revision)).collect();
        let model = models.first().map(String::as_str).unwrap_or("the card");
        one_of(hits, &format!("{model} {}", region.as_str()), revision)
    }
}

fn one_of<'a>(hits: Vec<&'a Entry>, model: &str, revision: &str) -> Result<Option<&'a Entry>, String> {
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
        checked(&e, data).map(|d| (e, d))
    }

    /// A card region's entry as offered, or why none is (#184).
    pub fn card_available(&self, region: CardRegion, id: &CardId) -> Result<Option<Available>, String> {
        let Some(rev) = id.version(region) else { return Ok(None) };
        Ok(self.manifest.find_card(region, id.subsystem.as_deref(), &id.models, rev)?.map(Available::from))
    }

    /// A card region's image (#184): the entry for the card at its current
    /// version, read from the golden beside the manifest or, when it is not
    /// there, from the node's firmware store (`store`) under the entry's
    /// file name; either way its sha256 must be the manifest's.
    /// -> (the entry, the bytes, where it was read).
    pub fn card_image(&self, region: CardRegion, id: &CardId, store: Option<&Path>) -> Result<(Entry, Vec<u8>, PathBuf), String> {
        if let Some(e) = &self.error {
            return Err(format!("the firmware manifest is not usable: {e}"));
        }
        if !self.loaded {
            let at = self.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "firmware.manifest unset".into());
            return Err(format!("no firmware manifest on this node ({at})"));
        }
        let name = id.models.first().cloned().unwrap_or_else(|| "the card".into());
        let rev = id.version(region).ok_or_else(|| format!("{name}: its {} version is not read, so no manifest entry applies", region.as_str()))?;
        let e = self
            .manifest
            .find_card(region, id.subsystem.as_deref(), &id.models, rev)?
            .ok_or_else(|| format!("the firmware manifest names no {} image for {} at {rev}", region.as_str(), id.models.join(" / ")))?
            .clone();
        let mut tried = Vec::new();
        for dir in [self.dir(), store].into_iter().flatten() {
            let p = dir.join(&e.file);
            match std::fs::read(&p) {
                Ok(data) => return checked(&e, data).map(|d| (e, d, p)),
                Err(x) => tried.push(format!("{}: {x}", p.display())),
            }
        }
        Err(format!("image {}: not beside the manifest nor in the firmware store ({})", e.file, tried.join("; ")))
    }
}

/// The bytes are the entry's: not empty, the manifest's sha256.
fn checked(e: &Entry, data: Vec<u8>) -> Result<Vec<u8>, String> {
    if data.is_empty() {
        return Err(format!("image {} is empty", e.file));
    }
    let got = crate::firmware::sha256_hex(&data);
    if got != e.sha256.trim().to_lowercase() {
        return Err(format!("image {}: sha256 {got}, not the manifest's {}", e.file, e.sha256.trim()));
    }
    Ok(data)
}

/// A SAS card as the manifest matches it (#184): the names an entry's
/// `model` may give (board name, PCI id `1000:dddd`), its PCI subsystem
/// (`vendor`), and each region's current version.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CardId {
    pub models: Vec<String>,
    pub subsystem: Option<String>,
    pub versions: Vec<(CardRegion, String)>,
}

impl CardId {
    pub fn version(&self, region: CardRegion) -> Option<&str> {
        self.versions.iter().find(|(r, _)| *r == region).map(|(_, v)| v.as_str())
    }

    pub fn of(r: &crate::card::CardReport) -> Self {
        let mut models = Vec::new();
        if let Some(b) = r.man0.as_ref().and_then(|m| m.board_name.clone()).filter(|b| !b.trim().is_empty()) {
            models.push(b);
        }
        models.push(format!("1000:{:04x}", r.info.pci_device));
        let mut versions = vec![(CardRegion::Firmware, crate::mpt::version(r.info.firmware_version))];
        if let Some(b) = r.bios_version {
            versions.push((CardRegion::Bios, crate::mpt::version(b)));
        }
        if let Some((_, p)) = r.nvdata {
            versions.push((CardRegion::Nvdata, crate::card::nvdata_version(p)));
        }
        CardId { models, subsystem: Some(format!("{:04x}:{:04x}", r.info.subsystem_vendor, r.info.subsystem_device)), versions }
    }
}

/// A card's offers (HostBusAdapter `status.firmwareAvailable`, #184): per
/// region its current version, the entry that applies (or null, or the
/// error) and whether a CardOperation flash writes it.
pub fn card_available(l: &Loaded, r: &crate::card::CardReport) -> serde_json::Value {
    let id = CardId::of(r);
    let mut out = serde_json::Map::new();
    for region in CardRegion::ALL {
        let mut v = serde_json::json!({ "current": id.version(region), "flashable": region.flash_region().is_some() });
        match l.card_available(region, &id) {
            Ok(a) => v["image"] = serde_json::to_value(a).unwrap_or_default(),
            Err(e) => v["error"] = serde_json::json!(e),
        }
        out.insert(region.as_str().to_string(), v);
    }
    serde_json::Value::Object(out)
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

    fn card_entry(region: &str, model: &str, from: &[&str], version: &str, sha: &str) -> String {
        format!(
            "[[image]]\nkind = \"card\"\nregion = \"{region}\"\nmodel = \"{model}\"\nfrom = [{}]\nversion = \"{version}\"\nfile = \"{model}-{region}-{version}.bin\"\nsha256 = \"{sha}\"\nsource = \"https://vendor.example/{model}\"\n",
            from.iter().map(|f| format!("\"{f}\"")).collect::<Vec<_>>().join(", ")
        )
    }

    fn hba330() -> CardId {
        CardId {
            models: vec!["HBA330".into(), "1000:0097".into()],
            subsystem: Some("1028:1f45".into()),
            versions: vec![(CardRegion::Firmware, "16.00.01.00".into()), (CardRegion::Bios, "08.18.07.00".into()), (CardRegion::Nvdata, "0e.01.00.07".into())],
        }
    }

    #[test]
    fn a_card_entry_names_its_region_and_matches_board_or_pci_id_per_region() {
        assert!(Manifest::parse(&card_entry("fw", "HBA330", &[], "1", SHA)).is_err(), "fw is not a region name");
        assert!(Manifest::parse(&card_entry("firmware", "HBA330", &[], "1", SHA).replace("region = \"firmware\"\n", "")).unwrap_err().contains("names its region"));
        assert!(Manifest::parse(&entry("M", &["A"], "B").replace("kind = \"drive\"", "kind = \"drive\"\nregion = \"bios\"")).unwrap_err().contains("for a card"));
        let m = Manifest::parse(
            &(card_entry("firmware", "HBA330", &["16.00.01.00"], "16.17.01.00", SHA)
                + &card_entry("bios", "1000:0097", &[], "08.37.00.00", SHA)
                + &card_entry("nvdata", "HBA330", &["0e.01.00.06"], "0e.01.00.07", SHA)
                + &card_entry("uefi", "HBA330", &[], "18.00.00.00", SHA)),
        )
        .unwrap();
        let l = Loaded { loaded: true, manifest: m.clone(), ..Default::default() };
        let id = hba330();
        assert_eq!(l.card_available(CardRegion::Firmware, &id).unwrap().unwrap().version, "16.17.01.00", "by board name");
        assert_eq!(l.card_available(CardRegion::Bios, &id).unwrap().unwrap().version, "08.37.00.00", "by PCI id, any version but its own");
        assert!(l.card_available(CardRegion::Nvdata, &id).unwrap().is_none(), "already at it");
        assert!(l.card_available(CardRegion::Uefi, &id).unwrap().is_none(), "no UEFI version read: never offered");
        assert!(m.find(Kind::Card, None, "HBA330", "16.00.01.00").unwrap().is_none(), "a card entry is not a drive's");
        assert!(m.find_card(CardRegion::Firmware, None, &["SAS9300-8i".into(), "1000:0097".into()], "16.00.01.00").unwrap().is_none(), "another board, firmware named by board");
        // a subsystem, when named, must match: Dell's firmware is not LSI's
        let dell = Manifest::parse(&card_entry("firmware", "1000:0097", &[], "16.17.01.00", SHA).replace("kind = \"card\"", "kind = \"card\"\nvendor = \"1028:1F45\"")).unwrap();
        assert!(dell.find_card(CardRegion::Firmware, Some("1028:1f45"), &id.models, "16.00.01.00").unwrap().is_some());
        assert!(dell.find_card(CardRegion::Firmware, Some("1000:3020"), &id.models, "16.00.01.00").unwrap().is_none());
        // two for one region: an error, neither offered
        let two = Manifest::parse(&(card_entry("firmware", "HBA330", &[], "16.17.01.00", SHA) + &card_entry("firmware", "1000:0097", &[], "16.17.02.00", SHA))).unwrap();
        let l2 = Loaded { loaded: true, manifest: two, ..Default::default() };
        assert!(l2.card_available(CardRegion::Firmware, &id).unwrap_err().contains("2 images"));
    }

    #[test]
    fn a_card_image_comes_from_beside_the_manifest_or_the_store_and_its_sha256_is_checked() {
        let dir = std::env::temp_dir().join(format!("sd-fwm-card-{}", std::process::id()));
        let store = dir.join("store");
        std::fs::create_dir_all(&store).unwrap();
        let img = b"card firmware".to_vec();
        let sha = crate::firmware::sha256_hex(&img);
        let path = dir.join("manifest.toml");
        assert!(Loaded::read(&path).card_image(CardRegion::Firmware, &hba330(), None).unwrap_err().contains("no firmware manifest"));
        std::fs::write(&path, card_entry("firmware", "HBA330", &[], "16.17.01.00", &sha) + &card_entry("nvdata", "HBA330", &[], "0e.02.00.00", &sha)).unwrap();
        let l = Loaded::read(&path);
        assert!(l.loaded, "{:?}", l.error);
        let file = "HBA330-firmware-16.17.01.00.bin";
        assert!(l.card_image(CardRegion::Firmware, &hba330(), Some(&store)).unwrap_err().contains("not beside the manifest nor in the firmware store"));
        std::fs::write(store.join(file), &img).unwrap();
        let (e, data, at) = l.card_image(CardRegion::Firmware, &hba330(), Some(&store)).unwrap();
        assert_eq!((e.version.as_str(), data, at), ("16.17.01.00", img.clone(), store.join(file)), "from the store");
        std::fs::write(dir.join(file), &img).unwrap();
        assert_eq!(l.card_image(CardRegion::Firmware, &hba330(), Some(&store)).unwrap().2, dir.join(file), "beside the manifest first");
        std::fs::write(dir.join(file), b"tampered").unwrap();
        assert!(l.card_image(CardRegion::Firmware, &hba330(), Some(&store)).unwrap_err().contains("not the manifest's"));
        let mut old = hba330();
        old.versions.clear();
        assert!(l.card_image(CardRegion::Firmware, &old, Some(&store)).unwrap_err().contains("not read"));
        assert!(l.card_image(CardRegion::Bios, &hba330(), Some(&store)).unwrap_err().contains("names no bios image"));
        // the HostBusAdapter's view: per region, the current version, the offer, flashable
        let mut r = crate::card::CardReport::default();
        r.info.pci_device = 0x97;
        r.info.firmware_version = 0x1000_0100;
        r.info.subsystem_vendor = 0x1028;
        r.info.subsystem_device = 0x1f45;
        r.man0 = Some(crate::mpt::Man0 { board_name: Some("HBA330".into()), ..Default::default() });
        r.nvdata = Some((0x0e01_0007, 0x0e01_0007));
        let v = card_available(&l, &r);
        assert_eq!(v["firmware"]["current"], "16.00.01.00");
        assert_eq!((v["firmware"]["image"]["version"].as_str(), v["firmware"]["image"]["region"].as_str(), v["firmware"]["flashable"].as_bool()), (Some("16.17.01.00"), Some("firmware"), Some(true)));
        assert_eq!((v["nvdata"]["current"].as_str(), v["nvdata"]["image"]["version"].as_str(), v["nvdata"]["flashable"].as_bool()), (Some("0e.01.00.07"), Some("0e.02.00.00"), Some(false)));
        assert!(v["bios"]["current"].is_null() && v["bios"]["image"].is_null() && v["uefi"]["image"].is_null());
        std::fs::remove_dir_all(&dir).ok();
    }
}
