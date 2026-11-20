//! Spread spectrum clocking on a SAS drive's phys (#164; owner: "we are in a
//! basement, with a metal enclosure and soil, so dont care about emi very
//! much"; "should be able to set / clear it").
//!
//! SSC spreads a link's clock to lower radiated emissions, at the cost of
//! some signal margin. Where emissions do not matter, turning it off gives
//! the margin back: on a path that only just holds at 12 Gbit (forge1 bay 1,
//! stormblock#675) that is the first thing to try before stepping the rate
//! down.
//!
//! The drive's side is SPL's **enhanced phy control** mode subpage (0x19/3):
//! per phy, the programmed phy capabilities (what the phy offers in SNW-3
//! speed negotiation), the current ones, the attached phy's (the HBA's or the
//! expander's, as it offered them), and the negotiated SSC bit beside the
//! negotiated rate. In a capabilities word each rate G1..G5 has two bits, the
//! high one "without SSC", the low one "with SSC" (G1 23/22 … G5 15/14). SNW-3
//! settles SSC for the link from both phys' offers, so a drive that offers no
//! with-SSC setting gets a link without SSC, whatever the card offers; the
//! negotiated SSC bit says what the link actually runs.
//!
//! The card's own SSC has no control in MPI2's public config pages (it lives
//! in vendor NVDATA), so the card's side is only reported, from the
//! attached capabilities the drive saw (owner/master on #164: option 1).
//!
//! [`plan`] makes the MODE SELECT list for `spec.spreadSpectrum`:
//! - `off`: every with-SSC bit cleared, with that rate's without-SSC bit set
//!   so no rate is lost;
//! - `on`: the with-SSC bit set on every rate the phy offers;
//! - `default`: the programmed word as it was before stormdrive first changed
//!   it (the prior, kept on the Drive object so a clear is exact).
//!
//! Refused by name when the drive does not mark those bits changeable.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::drive::DriveId;
use crate::link::gbit;

/// The enhanced phy control subpage of mode page 0x19.
pub const MODE_ENHANCED_SUB: u8 = 0x03;
const PROTOCOL_SAS: u8 = 6;
/// The first phy descriptor, and the shortest one SPL defines.
const DESC_AT: usize = 8;
const DESC_MIN: usize = 20;

/// Each rate, Mb/s, with its (without SSC, with SSC) bits in a phy
/// capabilities word.
const RATES: [(u32, u32, u32); 5] = [(1_500, 23, 22), (3_000, 21, 20), (6_000, 19, 18), (12_000, 17, 16), (22_500, 15, 14)];

/// Every with-SSC bit.
const WITH_MASK: u32 = (1 << 22) | (1 << 20) | (1 << 18) | (1 << 16) | (1 << 14);

/// One phy of subpage 0x19/3.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SscPhy {
    pub phy: String,
    /// The descriptor's offset in the page.
    #[serde(skip)]
    pub at: usize,
    /// What this phy offers (SNW-3 phy capabilities), and what it offered in
    /// the last negotiation.
    pub programmed: u32,
    pub current: u32,
    /// What the phy on the other end (the HBA's or the expander's) offered.
    pub attached: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub negotiated_mbps: Option<u32>,
    /// The link runs with SSC.
    pub negotiated_ssc: bool,
}

/// The drive's SSC page, read: every phy, which programmed bits MODE SELECT
/// may change (per phy; None when the changeable page was not read), and
/// whether the page is savable (PS).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriveSsc {
    pub phys: Vec<SscPhy>,
    pub changeable: Option<Vec<u32>>,
    pub savable: bool,
}

impl DriveSsc {
    /// The phy that carries the link (a negotiated rate), else the first.
    pub fn main(&self) -> Option<&SscPhy> {
        self.phys.iter().find(|p| p.negotiated_mbps.is_some()).or_else(|| self.phys.first())
    }

    /// The with-SSC bits are changeable on every phy (None: mask not read).
    pub fn settable(&self) -> Option<bool> {
        self.changeable.as_ref().map(|m| !m.is_empty() && m.iter().all(|c| c & WITH_MASK == WITH_MASK))
    }
}

/// What `spec.spreadSpectrum` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wanted {
    On,
    Off,
    Default,
}

impl Wanted {
    pub fn word(self) -> &'static str {
        match self {
            Wanted::On => "on",
            Wanted::Off => "off",
            Wanted::Default => "default",
        }
    }
}

/// `spec.spreadSpectrum`: `on`, `off`, `default` (any case; true/false as
/// on/off). None when absent or null.
pub fn parse_wanted(v: &serde_json::Value) -> Result<Option<Wanted>, String> {
    let s = match v {
        serde_json::Value::Null => return Ok(None),
        serde_json::Value::Bool(b) => return Ok(Some(if *b { Wanted::On } else { Wanted::Off })),
        serde_json::Value::String(s) => s.trim().to_ascii_lowercase(),
        other => return Err(format!("spreadSpectrum {other}: on, off or default")),
    };
    match s.as_str() {
        "on" => Ok(Some(Wanted::On)),
        "off" => Ok(Some(Wanted::Off)),
        "default" => Ok(Some(Wanted::Default)),
        _ => Err(format!("spreadSpectrum {s:?}: on, off or default")),
    }
}

fn be16(b: &[u8], at: usize) -> Option<usize> {
    Some(usize::from(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?])))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Every phy of subpage 0x19/3, the page from its first byte (SPF header:
/// page, subpage, 2-byte length; protocol at 5, phys at 7; per descriptor:
/// phy id at 1, descriptor length at 2 (the bytes after the first four),
/// programmed capabilities at 4, current at 8, attached at 12, negotiated
/// SSC (bit 4) and rate (bits 3:0) at 18). Empty for another page or
/// protocol.
pub fn parse(p: &[u8]) -> Vec<SscPhy> {
    if p.len() < DESC_AT || p[0] & 0x3f != crate::link::MODE_PHY_CONTROL || p[0] & 0x40 == 0 || p[1] != MODE_ENHANCED_SUB || p[5] & 0x0f != PROTOCOL_SAS {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut at = DESC_AT;
    for _ in 0..p[7] {
        let Some(len) = be16(p, at + 2).map(|l| l + 4) else { break };
        if len < DESC_MIN || p.len() < at + DESC_MIN {
            break;
        }
        let (Some(programmed), Some(current), Some(attached)) = (be32(p, at + 4), be32(p, at + 8), be32(p, at + 12)) else { break };
        out.push(SscPhy {
            phy: p[at + 1].to_string(),
            at,
            programmed,
            current,
            attached,
            negotiated_mbps: crate::link::rate_code(p[at + 18] & 0x0f),
            negotiated_ssc: p[at + 18] & 0x10 != 0,
        });
        at += len;
    }
    out
}

/// The page read: the current page and, when read, the changeable one.
pub fn read_pages(current: &[u8], changeable: Option<&[u8]>) -> Option<DriveSsc> {
    let phys = parse(current);
    if phys.is_empty() {
        return None;
    }
    let changeable = changeable.map(|c| phys.iter().map(|p| be32(c, p.at + 4).unwrap_or(0)).collect());
    Some(DriveSsc { phys, changeable, savable: current[0] & 0x80 != 0 })
}

/// The rates a capabilities word offers with SSC, and without.
fn rates(caps: u32) -> (Vec<u32>, Vec<u32>) {
    let with = RATES.iter().filter(|(_, _, w)| caps & (1 << w) != 0).map(|r| r.0).collect();
    let without = RATES.iter().filter(|(_, wo, _)| caps & (1 << wo) != 0).map(|r| r.0).collect();
    (with, without)
}

/// "on", "off" (no rate with SSC), "partial" (some rates), "none" (no
/// rate offered at all).
pub fn setting(caps: u32) -> &'static str {
    let (with, without) = rates(caps);
    let offered = RATES.iter().filter(|(m, _, _)| with.contains(m) || without.contains(m)).count();
    match (offered, with.len()) {
        (0, _) => "none",
        (_, 0) => "off",
        (o, w) if w == o => "on",
        _ => "partial",
    }
}

/// "with SSC 1.5–12 Gbit; without SSC 1.5–12 Gbit".
pub fn caps_said(caps: u32) -> String {
    let span = |v: &[u32]| match (v.first(), v.last()) {
        (Some(a), Some(b)) if a == b => gbit(*a),
        (Some(a), Some(b)) => format!("{}–{}", crate::link::gbit(*a).trim_end_matches(" Gbit"), gbit(*b)),
        _ => "none".to_string(),
    };
    let (with, without) = rates(caps);
    format!("with SSC {}; without SSC {}", span(&with), span(&without))
}

/// The programmed word for `want` from `cur`: `off` moves every with-SSC
/// rate to without; `on` adds with-SSC to every offered rate; `default`
/// is the prior (None: nothing recorded, leave it).
pub fn target(cur: u32, want: Wanted, prior: Option<u32>) -> Option<u32> {
    let t = match want {
        Wanted::Off => RATES.iter().fold(cur, |c, (_, wo, w)| if c & (1 << w) != 0 { (c & !(1 << w)) | (1 << wo) } else { c }),
        Wanted::On => RATES.iter().fold(cur, |c, (_, wo, w)| if c & ((1 << w) | (1 << wo)) != 0 { c | (1 << w) | (1 << wo) } else { c }),
        Wanted::Default => prior?,
    };
    (t != cur).then_some(t)
}

/// The MODE SELECT(10) list that programs each phy's `targets` word, or
/// None when no phy changes: an 8-byte header with no block descriptor,
/// then the page as the drive gave it with PS cleared and only the
/// programmed capabilities changed. Refused by name when a bit to change is
/// not in the changeable mask (or the mask was not read).
pub fn plan(page: &[u8], changeable: Option<&[u8]>, targets: &[Option<u32>]) -> Result<Option<Vec<u8>>, String> {
    let s = read_pages(page, changeable).ok_or("mode page 0x19/3 lists no SAS phy")?;
    if targets.iter().all(Option::is_none) {
        return Ok(None);
    }
    let mut p = page.to_vec();
    p[0] &= 0x7f;
    for (i, phy) in s.phys.iter().enumerate() {
        let Some(t) = targets.get(i).copied().flatten() else { continue };
        let diff = t ^ phy.programmed;
        let mask = s.changeable.as_ref().and_then(|m| m.get(i).copied());
        match mask {
            None => return Err("the drive's changeable page 0x19/3 was not read".to_string()),
            Some(m) if diff & !m != 0 => {
                return Err(format!("the drive does not let phy {} change its SSC settings (changeable 0x{m:08x}, needs 0x{diff:08x})", phy.phy));
            }
            Some(_) => {}
        }
        p[phy.at + 4..phy.at + 8].copy_from_slice(&t.to_be_bytes());
    }
    let mut list = vec![0u8; 8];
    list.extend(p);
    Ok(Some(list))
}

/// What a sample read, kept per drive.
static STATES: std::sync::OnceLock<Mutex<HashMap<DriveId, DriveSsc>>> = std::sync::OnceLock::new();
/// `spec.spreadSpectrum` as the controller last read it.
static POLICIES: std::sync::OnceLock<Mutex<HashMap<DriveId, Wanted>>> = std::sync::OnceLock::new();
/// The programmed word of each phy before stormdrive first changed it.
static PRIORS: std::sync::OnceLock<Mutex<HashMap<DriveId, Vec<(String, u32)>>>> = std::sync::OnceLock::new();

fn lock<T>(m: &'static std::sync::OnceLock<Mutex<T>>) -> std::sync::MutexGuard<'static, T>
where
    T: Default,
{
    m.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner())
}

pub fn observe(id: DriveId, s: DriveSsc) {
    lock(&STATES).insert(id, s);
}

pub fn state(id: &DriveId) -> Option<DriveSsc> {
    lock(&STATES).get(id).cloned()
}

pub fn policy(id: &DriveId) -> Option<Wanted> {
    lock(&POLICIES).get(id).copied()
}

pub fn set_policy(id: DriveId, w: Option<Wanted>) {
    let mut m = lock(&POLICIES);
    match w {
        Some(w) => m.insert(id, w),
        None => m.remove(&id),
    };
}

pub fn prior(id: &DriveId) -> Option<Vec<(String, u32)>> {
    lock(&PRIORS).get(id).cloned()
}

/// Keep the prior; `None` forgets it (after `default` put it back).
pub fn set_prior(id: DriveId, p: Option<Vec<(String, u32)>>) {
    let mut m = lock(&PRIORS);
    match p {
        Some(p) => m.insert(id, p),
        None => m.remove(&id),
    };
}

/// The prior as `status.link.ssc.prior` carries it: `[{phy, programmed}]`
/// with the word in hex. Read back from the object at start.
pub fn prior_json(p: &[(String, u32)]) -> serde_json::Value {
    serde_json::Value::Array(p.iter().map(|(phy, w)| serde_json::json!({ "phy": phy, "programmed": format!("0x{w:08x}") })).collect())
}

pub fn prior_from_json(v: &serde_json::Value) -> Option<Vec<(String, u32)>> {
    let out: Vec<(String, u32)> = v
        .as_array()?
        .iter()
        .filter_map(|e| Some((e["phy"].as_str()?.to_string(), u32::from_str_radix(e["programmed"].as_str()?.trim_start_matches("0x"), 16).ok()?)))
        .collect();
    (!out.is_empty()).then_some(out)
}

/// `status.link.ssc`, or None when nothing is known: the drive's setting in
/// words, settable or why not, what the link negotiated, what the attached
/// phy (the card's or the expander's) offers, the policy and the prior.
pub fn status(id: &DriveId) -> Option<serde_json::Value> {
    let st = state(id);
    let pol = policy(id);
    let pri = prior(id);
    if st.is_none() && pol.is_none() && pri.is_none() {
        return None;
    }
    // `prior: null` when none: the status is merge-patched, so a prior put
    // back by `default` must be cleared on the object, not left behind.
    let mut v = serde_json::json!({ "policy": pol.map(Wanted::word), "prior": null });
    if let Some(p) = &pri {
        v["prior"] = prior_json(p);
    }
    match st.as_ref().and_then(|s| s.main().map(|m| (s, m))) {
        Some((s, m)) => {
            v["drive"] = serde_json::json!(setting(m.programmed));
            v["driveOffers"] = serde_json::json!(caps_said(m.programmed));
            v["negotiated"] = serde_json::json!(if m.negotiated_mbps.is_some() { Some(if m.negotiated_ssc { "on" } else { "off" }) } else { None });
            v["attached"] = serde_json::json!(setting(m.attached));
            v["attachedOffers"] = serde_json::json!(caps_said(m.attached));
            v["settable"] = serde_json::json!(s.settable());
            if s.settable() == Some(false) {
                v["reason"] = serde_json::json!("the drive does not mark its with-SSC bits changeable (mode page 0x19/3)");
            }
            v["savable"] = serde_json::json!(s.savable);
            v["phys"] = serde_json::to_value(&s.phys).unwrap_or_default();
            v["message"] = serde_json::json!(said(s));
        }
        None => {
            v["settable"] = serde_json::json!(false);
            v["reason"] = serde_json::json!("the drive has no enhanced phy control page (0x19/3)");
        }
    }
    Some(v)
}

/// "link 12 Gbit with SSC; drive offers SSC on; attached phy offers SSC on".
pub fn said(s: &DriveSsc) -> String {
    let Some(m) = s.main() else { return String::new() };
    let link = match m.negotiated_mbps {
        Some(r) => format!("link {} {} SSC", gbit(r), if m.negotiated_ssc { "with" } else { "without" }),
        None => "no link".to_string(),
    };
    format!("{link}; drive offers SSC {}; attached phy offers SSC {}", setting(m.programmed), setting(m.attached))
}

/// Mode page 0x19/3 with page control `pc` (0 current, 1 changeable).
#[cfg(target_os = "linux")]
fn sense(dev: &crate::scsi::Device, pc: u8) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; 1024];
    let cdb = crate::scsi::cdb::mode_sense10_sub(crate::link::MODE_PHY_CONTROL, MODE_ENHANCED_SUB, pc, 1024);
    let n = dev.io(&cdb, crate::scsi::Dir::FromDevice, &mut buf, 10_000).map_err(|e| format!("MODE SENSE 0x19/3: {e}"))?;
    buf.truncate(n);
    crate::pages::mode_page_of(&buf).map(<[u8]>::to_vec).ok_or_else(|| "MODE SENSE 0x19/3: an answer with no subpage in it".to_string())
}

/// The drive's SSC page, read now. None when it has none.
#[cfg(target_os = "linux")]
pub fn read(dev: &crate::scsi::Device) -> Option<DriveSsc> {
    let cur = sense(dev, 0).ok()?;
    let chg = sense(dev, 1).ok();
    read_pages(&cur, chg.as_deref())
}

/// `spec.spreadSpectrum` on drive `name`: the drive's programmed
/// capabilities set for `want` (MODE SELECT, SP when savable, read back),
/// then the facing phys' link reset so the link renegotiates, and what it
/// negotiated. `prior` is the word to restore for `default`. Ok((said,
/// the prior to keep: Some when this changed the drive and none was kept,
/// so the caller records it)).
#[cfg(target_os = "linux")]
pub fn set(sys: &std::path::Path, name: &str, want: Wanted, prior: Option<&[(String, u32)]>) -> Result<(String, Option<Vec<(String, u32)>>), String> {
    let dev = crate::link::open_sg(name).ok_or_else(|| format!("{name}: no SCSI device to open"))?;
    let page = sense(&dev, 0).map_err(|e| format!("the drive has no enhanced phy control page: {e}"))?;
    let chg = sense(&dev, 1).ok();
    let before = read_pages(&page, chg.as_deref()).ok_or("the drive's page 0x19/3 lists no SAS phy")?;
    let targets: Vec<Option<u32>> = before
        .phys
        .iter()
        .map(|p| target(p.programmed, want, prior.and_then(|pr| pr.iter().find(|(id, _)| *id == p.phy).map(|(_, w)| *w))))
        .collect();
    let was = before.main().map(|m| format!("drive offered SSC {}", setting(m.programmed))).unwrap_or_default();
    let Some(mut list) = plan(&page, chg.as_deref(), &targets).map_err(|e| format!("{was}: {e}"))? else {
        return Ok((format!("{was}, left as it is; {}", said(&before)), None));
    };
    let cdb = crate::scsi::cdb::mode_select10_sp(list.len() as u16, before.savable);
    dev.io(&cdb, crate::scsi::Dir::ToDevice, &mut list, 30_000).map_err(|e| format!("{was}: MODE SELECT 0x19/3 refused: {e}"))?;
    let after = sense(&dev, 0).ok().and_then(|p| read_pages(&p, None)).ok_or_else(|| format!("{was}: MODE SELECT 0x19/3 taken, the page not read back"))?;
    for (i, t) in targets.iter().enumerate() {
        if let (Some(t), Some(p)) = (t, after.phys.get(i)) {
            if p.programmed != *t {
                return Err(format!("{was}: MODE SELECT 0x19/3 taken, but phy {} now offers 0x{:08x} (asked 0x{t:08x})", p.phy, p.programmed));
            }
        }
    }
    let keep = (prior.is_none() && want != Wanted::Default).then(|| before.phys.iter().map(|p| (p.phy.clone(), p.programmed)).collect());
    let kept = if before.savable { "saved" } else { "current only: the page is not savable, a power cycle loses it" };
    let now = after.main().map(|m| setting(m.programmed)).unwrap_or("?");
    let reset = crate::link::reset_host(sys, name).unwrap_or_else(|e| format!("no link reset: {e}"));
    // The link renegotiates: what it runs now, read once it is up (15 s).
    let mut fin = after;
    for _ in 0..15 {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if let Some(s) = sense(&dev, 0).ok().and_then(|p| read_pages(&p, None)) {
            let up = s.main().is_some_and(|m| m.negotiated_mbps.is_some());
            fin = s;
            if up {
                break;
            }
        }
    }
    Ok((format!("{was} → {now} ({kept}); {reset}; {}", said(&fin)), keep))
}

#[cfg(not(target_os = "linux"))]
pub fn set(_sys: &std::path::Path, name: &str, _want: Wanted, _prior: Option<&[(String, u32)]>) -> Result<(String, Option<Vec<(String, u32)>>), String> {
    Err(format!("{name}: SCSI mode pages are read on Linux only"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every rate 1.5–12 Gbit offered with and without SSC (G1–G4), as a
    /// 12 Gbit SAS drive gives it; START and TX SSC TYPE clear.
    const ALL_12: u32 = 0x00ff_0000;

    /// A 0x19/3 page: two phys, each a 20-byte descriptor; phy 0 linked at
    /// 12 Gbit with `neg_ssc`, its attached phy offering `attached`.
    fn page(programmed: [u32; 2], attached: u32, neg_ssc: bool, ps: bool) -> Vec<u8> {
        let mut p = vec![0x40 | crate::link::MODE_PHY_CONTROL | if ps { 0x80 } else { 0 }, MODE_ENHANCED_SUB, 0, 44, 0, PROTOCOL_SAS, 0, 2];
        for (id, w) in programmed.iter().enumerate() {
            let mut d = vec![0u8; 20];
            d[1] = id as u8;
            d[3] = 0x10;
            d[4..8].copy_from_slice(&w.to_be_bytes());
            d[8..12].copy_from_slice(&w.to_be_bytes());
            if id == 0 {
                d[12..16].copy_from_slice(&attached.to_be_bytes());
                d[18] = 0x0b | if neg_ssc { 0x10 } else { 0 };
            }
            p.extend(d);
        }
        p
    }

    #[test]
    fn the_page_and_the_words() {
        let cur = page([ALL_12, ALL_12], ALL_12, true, true);
        let phys = parse(&cur);
        assert_eq!(phys.len(), 2);
        assert_eq!((phys[0].phy.as_str(), phys[0].at, phys[0].programmed, phys[0].negotiated_mbps, phys[0].negotiated_ssc), ("0", 8, ALL_12, Some(12_000), true));
        assert_eq!((phys[1].at, phys[1].negotiated_mbps, phys[1].negotiated_ssc), (28, None, false));
        assert_eq!(setting(ALL_12), "on");
        assert_eq!(setting(0x00aa_0000), "off");
        assert_eq!(setting(0x00ae_0000), "partial");
        assert_eq!(setting(0), "none");
        assert_eq!(caps_said(ALL_12), "with SSC 1.5–12 Gbit; without SSC 1.5–12 Gbit");
        assert_eq!(caps_said(0x00aa_0000), "with SSC none; without SSC 1.5–12 Gbit");
        let s = read_pages(&cur, Some(&page([0x00ff_c000, 0x00ff_c000], 0, false, false))).unwrap();
        assert_eq!((s.settable(), s.savable, s.main().unwrap().phy.as_str()), (Some(true), true, "0"));
        assert_eq!(said(&s), "link 12 Gbit with SSC; drive offers SSC on; attached phy offers SSC on");
        assert_eq!(read_pages(&cur, Some(&page([0, 0], 0, false, false))).unwrap().settable(), Some(false));
        assert_eq!(read_pages(&cur, None).unwrap().settable(), None);
        // Another subpage, another protocol, short: nothing.
        let mut sub1 = cur.clone();
        sub1[1] = 1;
        assert!(parse(&sub1).is_empty());
        let mut fc = cur.clone();
        fc[5] = 0;
        assert!(parse(&fc).is_empty());
        assert_eq!(parse(&cur[..20]).len(), 0);
        // A longer descriptor is stepped by its length.
        let mut long = vec![0x40 | 0x19, 3, 0, 52, 0, PROTOCOL_SAS, 0, 2];
        for id in 0..2u8 {
            let mut d = vec![0u8; 24];
            d[1] = id;
            d[3] = 0x14;
            d[4..8].copy_from_slice(&ALL_12.to_be_bytes());
            long.extend(d);
        }
        assert_eq!(parse(&long).iter().map(|p| p.at).collect::<Vec<_>>(), vec![8, 32]);
    }

    #[test]
    fn off_on_and_back_to_default() {
        // off: no with-SSC left, every rate still offered without.
        let off = target(ALL_12, Wanted::Off, None).unwrap();
        assert_eq!(off, 0x00aa_0000);
        assert_eq!(target(off, Wanted::Off, None), None, "already off: nothing");
        // A rate offered only with SSC keeps its rate, without.
        assert_eq!(target(0x0055_0000, Wanted::Off, None), Some(0x00aa_0000));
        // on: with SSC on every offered rate, none added.
        assert_eq!(target(0x002a_0000, Wanted::On, None), Some(0x003f_0000));
        assert_eq!(target(ALL_12, Wanted::On, None), None);
        // default: the prior, or nothing to do.
        assert_eq!(target(off, Wanted::Default, Some(ALL_12)), Some(ALL_12));
        assert_eq!(target(off, Wanted::Default, None), None);
        assert_eq!(target(ALL_12, Wanted::Default, Some(ALL_12)), None);
        // Other bits (TX SSC type, the rest) untouched.
        assert_eq!(target(ALL_12 | 0x4000_0003, Wanted::Off, None), Some(0x00aa_0000 | 0x4000_0003));
    }

    #[test]
    fn the_mode_select_list() {
        let cur = page([ALL_12, ALL_12], ALL_12, true, true);
        let chg = page([0x00ff_c000, 0x00ff_c000], 0, false, false);
        let list = plan(&cur, Some(&chg), &[Some(0x00aa_0000), Some(0x00aa_0000)]).unwrap().unwrap();
        assert_eq!(&list[..8], &[0; 8]);
        let p = &list[8..];
        assert_eq!(p.len(), cur.len());
        assert_eq!(p[0], 0x40 | 0x19, "PS cleared");
        assert_eq!((be32(p, 12), be32(p, 32)), (Some(0x00aa_0000), Some(0x00aa_0000)));
        let mut rest = p.to_vec();
        rest[0] = cur[0];
        rest[12..16].copy_from_slice(&ALL_12.to_be_bytes());
        rest[32..36].copy_from_slice(&ALL_12.to_be_bytes());
        assert_eq!(rest, cur, "nothing else changed");
        // One phy only.
        let one = plan(&cur, Some(&chg), &[Some(0x00aa_0000), None]).unwrap().unwrap();
        assert_eq!((be32(&one[8..], 12), be32(&one[8..], 32)), (Some(0x00aa_0000), Some(ALL_12)));
        assert_eq!(plan(&cur, Some(&chg), &[None, None]).unwrap(), None);
        // Not changeable, or the mask not read: refused by name.
        let e = plan(&cur, Some(&page([0x00aa_0000, 0], 0, false, false)), &[Some(0x00aa_0000), None]).unwrap_err();
        assert!(e.contains("phy 0") && e.contains("0x00aa0000") && e.contains("needs 0x00550000"), "{e}");
        assert!(plan(&cur, None, &[Some(0), None]).unwrap_err().contains("not read"));
        assert!(plan(&[0x59, 3, 0, 4, 0, 6, 0, 0], Some(&chg), &[Some(0)]).is_err());
        assert_eq!(crate::scsi::cdb::mode_sense10_sub(0x19, 3, 0, 1024)[3], 3);
    }

    #[test]
    fn the_spec_and_the_status() {
        assert_eq!(parse_wanted(&serde_json::json!("Off")).unwrap(), Some(Wanted::Off));
        assert_eq!(parse_wanted(&serde_json::json!(" on ")).unwrap(), Some(Wanted::On));
        assert_eq!(parse_wanted(&serde_json::json!("default")).unwrap(), Some(Wanted::Default));
        assert_eq!(parse_wanted(&serde_json::json!(false)).unwrap(), Some(Wanted::Off));
        assert_eq!(parse_wanted(&serde_json::Value::Null).unwrap(), None);
        assert!(parse_wanted(&serde_json::json!("half")).is_err());
        assert!(parse_wanted(&serde_json::json!(3)).is_err());
        // The prior round-trips through the object.
        let p = vec![("0".to_string(), ALL_12), ("1".to_string(), 0x00aa_0000)];
        let j = prior_json(&p);
        assert_eq!(j[0]["programmed"], "0x00ff0000");
        assert_eq!(prior_from_json(&j), Some(p.clone()));
        assert_eq!(prior_from_json(&serde_json::json!([])), None);
        // status.link.ssc.
        let id = DriveId(uuid::Uuid::from_u128(0x164));
        assert_eq!(status(&id), None);
        set_policy(id, Some(Wanted::Off));
        let v = status(&id).unwrap();
        assert_eq!((v["policy"].as_str(), v["settable"].as_bool()), (Some("off"), Some(false)));
        assert!(v["reason"].as_str().unwrap().contains("no enhanced phy control page"));
        let cur = page([0x00aa_0000, 0x00aa_0000], ALL_12, false, true);
        observe(id, read_pages(&cur, Some(&page([0x00ff_c000, 0x00ff_c000], 0, false, false))).unwrap());
        set_prior(id, Some(p));
        let v = status(&id).unwrap();
        assert_eq!((v["drive"].as_str(), v["negotiated"].as_str(), v["attached"].as_str(), v["settable"].as_bool()), (Some("off"), Some("off"), Some("on"), Some(true)));
        assert_eq!(v["message"], "link 12 Gbit without SSC; drive offers SSC off; attached phy offers SSC on");
        assert_eq!(v["prior"][1]["programmed"], "0x00aa0000");
        assert_eq!(v["phys"][0]["programmed"], 0x00aa_0000);
        assert!(v.get("reason").is_none());
        set_policy(id, None);
        set_prior(id, None);
    }
}
