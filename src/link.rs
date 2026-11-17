//! A SAS drive's link, both sides, as an alarm before writes fail (#149,
//! stormblock#642; owner on forge1 bay 3: "We need to monitor the sas metrics
//! on the sas buss", "alarms when something is not normal, instead of when
//! writes fail"). Bay 3's writes failed for a week on its link — loss of
//! DWORD sync climbing about 500 a minute at 12 Gbit — and nobody read the
//! counter.
//!
//! On every health sample, two readings:
//!
//! - **the drive's side**: its Protocol-Specific Port log page (0x18), one
//!   descriptor per phy: negotiated rate, attached SAS address, and the four
//!   SPL error counters (invalid DWORDs, running disparity errors, loss of
//!   DWORD synchronization, phy reset problems) — what the drive's receiver
//!   saw;
//! - **the host's side**: the `sas_phy` facing the drive in sysfs (the phy
//!   of the port its `end_device-` hangs off: the HBA's, or the shelf
//!   expander's), the same four counters — what the host's receiver saw —
//!   and its negotiated, maximum and hardware maximum rates.
//!
//! [`step`] turns a reading and the previous state into rates per minute and
//! a level: `settling` (the first reading, or a counter went back), `ok`
//! (nothing moving), `degraded` (anything moving), `failing` (loss of sync or
//! phy resets moving two samples running). [`recommendation`] names the
//! likely part. `spec.linkRate` on the Drive object holds the facing phy at
//! a rate ([`apply`]); nothing here ever sets one by itself.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::drive::DriveId;
use crate::smart::scsi::log_params;

/// Protocol-Specific Port log page.
pub const LOG_SAS_PORT: u8 = 0x18;
/// SPC's protocol identifier for SAS.
const PROTOCOL_SAS: u8 = 6;

/// One phy's counters, either side.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Phy {
    /// The drive's phy identifier ("0", "1"), or the sysfs phy ("phy-1:2").
    pub phy: String,
    /// Negotiated rate, Mb/s; None with no link.
    pub rate_mbps: Option<u32>,
    /// The SAS address on the other end (drive side only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attached_sas: Option<String>,
    pub invalid_dwords: u64,
    pub disparity_errors: u64,
    pub loss_of_sync: u64,
    pub phy_resets: u64,
}

/// One phy's counters per minute since the previous sample.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Rate {
    pub phy: String,
    pub invalid_dwords: f64,
    pub disparity_errors: f64,
    pub loss_of_sync: f64,
    pub phy_resets: f64,
}

impl Rate {
    fn moving(&self) -> bool {
        self.invalid_dwords > 0.0 || self.disparity_errors > 0.0 || self.sync_moving()
    }

    fn sync_moving(&self) -> bool {
        self.loss_of_sync > 0.0 || self.phy_resets > 0.0
    }
}

/// One side's totals and rates.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Side {
    pub phys: Vec<Phy>,
    pub rates: Vec<Rate>,
}

impl Side {
    fn moving(&self) -> bool {
        self.rates.iter().any(Rate::moving)
    }

    fn sync_moving(&self) -> bool {
        self.rates.iter().any(Rate::sync_moving)
    }
}

/// The facing host phy's rates, from sysfs (Mb/s).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostRates {
    pub negotiated: Option<u32>,
    pub maximum: Option<u32>,
    pub maximum_hw: Option<u32>,
    pub minimum_hw: Option<u32>,
}

/// One sample's raw reading.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reading {
    /// Log page 0x18's phys; None when the drive was not read (not SAS,
    /// no page, the read failed).
    pub drive: Option<Vec<Phy>>,
    /// The facing sysfs phys; empty when there is no SAS transport.
    pub host: Vec<Phy>,
    pub host_rates: HostRates,
}

impl Reading {
    pub fn is_empty(&self) -> bool {
        self.drive.is_none() && self.host.is_empty()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    #[default]
    Settling,
    Ok,
    Degraded,
    Failing,
}

impl Level {
    pub fn word(self) -> &'static str {
        match self {
            Level::Settling => "settling",
            Level::Ok => "ok",
            Level::Degraded => "degraded",
            Level::Failing => "failing",
        }
    }

    /// For /metrics: settling and ok 0, degraded 1, failing 2.
    pub fn gauge(self) -> f64 {
        match self {
            Level::Settling | Level::Ok => 0.0,
            Level::Degraded => 1.0,
            Level::Failing => 2.0,
        }
    }
}

/// A condition as the Drive object carries it: true or not, and since when.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cond {
    pub on: bool,
    pub since: u64,
}

/// The link as the Drive object shows it.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkState {
    /// Unix seconds of this sample.
    pub at: u64,
    pub level: Level,
    /// Unix seconds the level was first seen.
    pub since: u64,
    /// Samples running with anything moving.
    pub streak: u32,
    /// Samples running with loss of sync or phy resets moving.
    pub sync_streak: u32,
    /// The sysfs phy facing the drive, e.g. `phy-1:2` (`phy-1:0:5` behind
    /// an expander).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_phy: Option<String>,
    /// The rate the link runs at, Mb/s: the host's negotiated rate, else the
    /// drive's fastest phy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub negotiated_mbps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_maximum_mbps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_maximum_hw_mbps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drive: Option<Side>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<Side>,
    /// What is moving, in words.
    pub message: String,
    /// The likely part, when the level is not ok.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    #[serde(skip)]
    pub degraded: Cond,
    #[serde(skip)]
    pub failing: Cond,
    /// The last level that was not settling: what a change is judged by.
    #[serde(skip)]
    pub last_level: Option<Level>,
}

/// "12 Gbit", "1.5 Gbit".
pub fn gbit(mbps: u32) -> String {
    format!("{} Gbit", f64::from(mbps) / 1000.0)
}

/// SPL's negotiated logical link rate code, Mb/s.
fn rate_code(code: u8) -> Option<u32> {
    match code {
        0x8 => Some(1_500),
        0x9 => Some(3_000),
        0xa => Some(6_000),
        0xb => Some(12_000),
        0xc => Some(22_500),
        _ => None,
    }
}

fn be32(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 4).map_or(0, |x| u64::from(u32::from_be_bytes([x[0], x[1], x[2], x[3]])))
}

/// Log page 0x18: every SAS phy of every port, with its counters (SPL SAS
/// phy log descriptor: phy id at 1, rate at 5, attached SAS address at 16,
/// invalid DWORDs 32, running disparity 36, loss of DWORD sync 40, phy reset
/// problems 44).
pub fn parse_port_log(raw: &[u8]) -> Vec<Phy> {
    let mut out = Vec::new();
    for (_, p) in log_params(raw, LOG_SAS_PORT) {
        if p.first().map(|b| b & 0x0f) != Some(PROTOCOL_SAS) {
            continue;
        }
        let phys = p.get(3).copied().unwrap_or(0);
        let mut off = 4;
        for _ in 0..phys {
            let Some(d) = p.get(off..) else { break };
            let len = d.get(3).map_or(48, |l| if *l < 44 { 48 } else { *l as usize + 4 });
            if d.len() < 48 {
                break;
            }
            let attached = &d[16..24];
            out.push(Phy {
                phy: d[1].to_string(),
                rate_mbps: rate_code(d[5] & 0x0f),
                attached_sas: attached.iter().any(|b| *b != 0).then(|| format!("0x{}", attached.iter().map(|b| format!("{b:02x}")).collect::<String>())),
                invalid_dwords: be32(d, 32),
                disparity_errors: be32(d, 36),
                loss_of_sync: be32(d, 40),
                phy_resets: be32(d, 44),
            });
            off += len;
        }
    }
    out
}

/// A sysfs link rate ("12.0 Gbit", "1.5 Gbit") in Mb/s; None for
/// "Unknown", "Phy disabled", …
pub fn parse_sysfs_rate(s: &str) -> Option<u32> {
    let g: f64 = s.trim().strip_suffix("Gbit")?.trim().parse().ok()?;
    Some((g * 1000.0).round() as u32)
}

/// The sysfs spelling of a rate: "6.0 Gbit", "22.5 Gbit".
pub fn sysfs_rate(mbps: u32) -> String {
    format!("{:.1} Gbit", f64::from(mbps) / 1000.0)
}

/// What `spec.linkRate` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wanted {
    /// Hold the phy at most at this rate, Mb/s.
    Max(u32),
    /// Back to the phy's hardware maximum.
    Hw,
}

impl Wanted {
    pub fn word(self) -> String {
        match self {
            Wanted::Max(m) => gbit(m),
            Wanted::Hw => "hw".into(),
        }
    }
}

/// `spec.linkRate`: 6, "6", "6G", "6 Gbit", "6.0 Gbit", 1.5, 3, 12, 22.5, or
/// "hw". Anything else is refused with why.
pub fn parse_wanted(v: &serde_json::Value) -> Result<Option<Wanted>, String> {
    let s = match v {
        serde_json::Value::Null => return Ok(None),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.trim().to_string(),
        other => return Err(format!("linkRate {other}: a rate in Gbit (1.5, 3, 6, 12, 22.5) or \"hw\"")),
    };
    if s.is_empty() {
        return Ok(None);
    }
    if s.eq_ignore_ascii_case("hw") {
        return Ok(Some(Wanted::Hw));
    }
    let lower = s.to_ascii_lowercase();
    let num = ["gbit/s", "gb/s", "gbit", "gbps", "gb", "g"]
        .iter()
        .find_map(|suf| lower.strip_suffix(suf))
        .unwrap_or(&lower)
        .trim();
    let mbps = num.parse::<f64>().ok().map(|g| (g * 1000.0).round() as u32);
    match mbps {
        Some(m @ (1_500 | 3_000 | 6_000 | 12_000 | 22_500)) => Ok(Some(Wanted::Max(m))),
        _ => Err(format!("linkRate {s:?}: a rate in Gbit (1.5, 3, 6, 12, 22.5) or \"hw\"")),
    }
}

fn per_minute(now: u64, then: u64, elapsed_secs: u64) -> Option<f64> {
    let d = now.checked_sub(then)?;
    let m = elapsed_secs.max(1) as f64 / 60.0;
    Some(((d as f64 / m) * 10.0).round() / 10.0)
}

/// Rates for one side against the previous totals. None when a counter went
/// back or a phy is new: the side starts again.
fn side_rates(now: &[Phy], prev: Option<&Side>, elapsed: u64) -> Option<Vec<Rate>> {
    let prev = prev?;
    now.iter()
        .map(|p| {
            let q = prev.phys.iter().find(|q| q.phy == p.phy)?;
            Some(Rate {
                phy: p.phy.clone(),
                invalid_dwords: per_minute(p.invalid_dwords, q.invalid_dwords, elapsed)?,
                disparity_errors: per_minute(p.disparity_errors, q.disparity_errors, elapsed)?,
                loss_of_sync: per_minute(p.loss_of_sync, q.loss_of_sync, elapsed)?,
                phy_resets: per_minute(p.phy_resets, q.phy_resets, elapsed)?,
            })
        })
        .collect()
}

/// The likely part for a link that is not ok.
pub fn recommendation(level: Level, drive_moving: bool, host_moving: bool) -> Option<String> {
    if !matches!(level, Level::Degraded | Level::Failing) {
        return None;
    }
    Some(
        match (drive_moving, host_moving) {
            (true, true) => "both sides count errors: the cable or the expander port",
            (true, false) => "the drive's receiver or its slot: reseat; if it continues, swap bays or replace",
            _ => "the host side counts errors on what the drive sends: the drive's transmitter or its slot: reseat; if it continues, swap bays or replace",
        }
        .into(),
    )
}

fn said(side: &str, s: &Side) -> Vec<String> {
    s.rates
        .iter()
        .filter(|r| r.moving())
        .map(|r| {
            let mut parts = Vec::new();
            for (n, v) in [("loss of sync", r.loss_of_sync), ("phy resets", r.phy_resets), ("invalid DWORDs", r.invalid_dwords), ("disparity errors", r.disparity_errors)] {
                if v > 0.0 {
                    parts.push(format!("{n} {v}/min"));
                }
            }
            format!("{side} phy {}: {}", r.phy, parts.join(", "))
        })
        .collect()
}

/// The next state from the previous one and a reading at `now` (unix secs).
pub fn step(prev: Option<&LinkState>, r: &Reading, now: u64) -> LinkState {
    let elapsed = prev.map_or(0, |p| now.saturating_sub(p.at));
    let drive_rates = r.drive.as_ref().map(|phys| side_rates(phys, prev.and_then(|p| p.drive.as_ref()), elapsed));
    let host_rates = (!r.host.is_empty()).then(|| side_rates(&r.host, prev.and_then(|p| p.host.as_ref()), elapsed));
    // Settling: the first reading, or one side started again (a counter
    // went back: a power cycle, a reset; a phy came or went).
    let settling = prev.is_none() || matches!(drive_rates, Some(None)) || matches!(host_rates, Some(None)) || elapsed == 0;
    let drive = r.drive.as_ref().map(|phys| Side { phys: phys.clone(), rates: drive_rates.flatten().unwrap_or_default() });
    let host = (!r.host.is_empty()).then(|| Side { phys: r.host.clone(), rates: host_rates.flatten().unwrap_or_default() });
    let (dm, hm) = (drive.as_ref().is_some_and(Side::moving), host.as_ref().is_some_and(Side::moving));
    let sync = drive.as_ref().is_some_and(Side::sync_moving) || host.as_ref().is_some_and(Side::sync_moving);
    let (streak, sync_streak) = match (settling, prev) {
        (false, Some(p)) => (if dm || hm { p.streak + 1 } else { 0 }, if sync { p.sync_streak + 1 } else { 0 }),
        _ => (0, 0),
    };
    let level = if settling {
        Level::Settling
    } else if !(dm || hm) {
        Level::Ok
    } else if sync_streak >= 2 {
        Level::Failing
    } else {
        Level::Degraded
    };
    let since = match prev {
        Some(p) if p.level == level => p.since,
        _ => now,
    };
    let cond = |was: Option<Cond>, on: bool| match was {
        Some(c) if c.on == on => c,
        _ => Cond { on, since: now },
    };
    // A settling sample keeps the conditions as they were: a reset of the
    // counters is not a recovery.
    let (degraded, failing) = if settling {
        (prev.map(|p| p.degraded).unwrap_or(Cond { on: false, since: now }), prev.map(|p| p.failing).unwrap_or(Cond { on: false, since: now }))
    } else {
        (cond(prev.map(|p| p.degraded), level >= Level::Degraded), cond(prev.map(|p| p.failing), level == Level::Failing))
    };
    let mut words: Vec<String> = Vec::new();
    if let Some(d) = &drive {
        words.extend(said("drive", d));
    }
    if let Some(h) = &host {
        words.extend(said("host", h));
    }
    let message = match level {
        Level::Settling => "first reading of the counters: rates from the next sample".to_string(),
        Level::Ok => "no link errors moving".to_string(),
        _ => words.join("; "),
    };
    let drive_rate = r.drive.as_ref().and_then(|p| p.iter().filter_map(|x| x.rate_mbps).max());
    LinkState {
        at: now,
        level,
        since,
        streak,
        sync_streak,
        host_phy: (!r.host.is_empty()).then(|| r.host.iter().map(|p| p.phy.as_str()).collect::<Vec<_>>().join(",")),
        negotiated_mbps: r.host_rates.negotiated.or(drive_rate),
        host_maximum_mbps: r.host_rates.maximum,
        host_maximum_hw_mbps: r.host_rates.maximum_hw,
        drive,
        host,
        message,
        recommendation: recommendation(level, dm, hm),
        degraded,
        failing,
        last_level: if level == Level::Settling { prev.and_then(|p| p.last_level) } else { Some(level) },
    }
}

/// "maximum 6 Gbit (policy)", "maximum 12 Gbit (hardware)", "maximum 6 Gbit
/// (set, hardware 12 Gbit)".
pub fn maximum_said(st: &LinkState, policy: Option<Wanted>) -> Option<String> {
    let max = st.host_maximum_mbps?;
    let why = match (policy, st.host_maximum_hw_mbps) {
        (Some(Wanted::Max(_)), _) => "policy".to_string(),
        (_, Some(hw)) if hw == max => "hardware".to_string(),
        (_, Some(hw)) => format!("set, hardware {}", gbit(hw)),
        (_, None) => "hardware".to_string(),
    };
    Some(format!("maximum {} ({why})", gbit(max)))
}

/// `status.link` and the two conditions for the Drive object.
pub fn status(id: &DriveId) -> (Option<serde_json::Value>, Vec<serde_json::Value>) {
    let st = state(id);
    let policy = policy(id);
    if st.is_none() && policy.is_none() {
        return (None, Vec::new());
    }
    let mut v = st.as_ref().map(|s| serde_json::to_value(s).unwrap_or_default()).unwrap_or_else(|| serde_json::json!({}));
    v["policy"] = serde_json::json!(policy.map(Wanted::word));
    if let Some(s) = &st {
        v["maximum"] = serde_json::json!(maximum_said(s, policy));
    }
    let conds = st.map(|s| conditions(&s)).unwrap_or_default();
    (Some(v), conds)
}

/// `LinkDegraded` (anything moving, failing included) and `LinkFailing`.
pub fn conditions(s: &LinkState) -> Vec<serde_json::Value> {
    let c = |kind: &str, c: Cond, reason_on: &str| {
        let (reason, message) = if c.on {
            let mut m = s.message.clone();
            if let Some(r) = &s.recommendation {
                m.push_str(&format!(" — {r}"));
            }
            (reason_on.to_string(), m)
        } else {
            ("LinkOk".to_string(), format!("link {}", s.level.word()))
        };
        serde_json::json!({
            "type": kind,
            "status": if c.on { "True" } else { "False" },
            "reason": reason,
            "message": message,
            "lastTransitionTime": crate::controller::rfc3339(c.since),
        })
    };
    vec![c("LinkDegraded", s.degraded, "ErrorsMoving"), c("LinkFailing", s.failing, "SyncLost")]
}

/// A level change worth an event: into degraded or failing, or back to ok
/// from either. Settling and the first ok are not.
pub fn noteworthy(from: Option<Level>, to: Level) -> bool {
    match to {
        Level::Degraded | Level::Failing => from != Some(to),
        Level::Ok => matches!(from, Some(Level::Degraded | Level::Failing)),
        Level::Settling => false,
    }
}

static STATES: std::sync::OnceLock<Mutex<HashMap<DriveId, LinkState>>> = std::sync::OnceLock::new();
static POLICIES: std::sync::OnceLock<Mutex<HashMap<DriveId, Wanted>>> = std::sync::OnceLock::new();

fn states() -> &'static Mutex<HashMap<DriveId, LinkState>> {
    STATES.get_or_init(Default::default)
}

fn policies() -> &'static Mutex<HashMap<DriveId, Wanted>> {
    POLICIES.get_or_init(Default::default)
}

pub fn state(id: &DriveId) -> Option<LinkState> {
    states().lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned()
}

/// Take a reading: the new state, and the level it left when that change
/// is worth an event ([`noteworthy`]).
pub fn observe(id: DriveId, r: &Reading, now: u64) -> (LinkState, Option<Option<Level>>) {
    let mut m = states().lock().unwrap_or_else(|e| e.into_inner());
    let prev = m.get(&id).cloned();
    // Judged against the last level that was not settling, so a reset of
    // the counters between two equal levels raises nothing.
    let st = step(prev.as_ref(), r, now);
    let from = prev.as_ref().and_then(|p| p.last_level);
    let changed = noteworthy(from, st.level).then_some(from);
    m.insert(id, st.clone());
    (st, changed)
}

pub fn policy(id: &DriveId) -> Option<Wanted> {
    policies().lock().unwrap_or_else(|e| e.into_inner()).get(id).copied()
}

pub fn set_policy(id: DriveId, w: Option<Wanted>) {
    let mut m = policies().lock().unwrap_or_else(|e| e.into_inner());
    match w {
        Some(w) => m.insert(id, w),
        None => m.remove(&id),
    };
}

/// The sysfs phy directories of the port a drive's end device hangs off
/// (`…/port-1:2/end_device-1:2/…` → `port-1:2/phy-1:2`), from the drive's
/// canonical device path. Empty with no SAS transport.
pub fn facing_phys(real: &std::path::Path) -> Vec<std::path::PathBuf> {
    let comps: Vec<std::path::Component> = real.components().collect();
    let Some(ed) = comps.iter().position(|c| c.as_os_str().to_string_lossy().starts_with("end_device-")) else {
        return Vec::new();
    };
    if ed < 1 || !comps[ed - 1].as_os_str().to_string_lossy().starts_with("port-") {
        return Vec::new();
    }
    let port: std::path::PathBuf = comps[..ed].iter().collect();
    let mut phys: Vec<std::path::PathBuf> = std::fs::read_dir(&port)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("phy-"))
        .map(|e| e.path())
        .collect();
    phys.sort();
    phys
}

fn read_trim(p: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// One facing phy's counters and rates. Its counters live under
/// `sas_phy/<name>` beside the phy directory (the class device).
pub fn read_host_phy(dir: &std::path::Path) -> Option<(Phy, HostRates)> {
    let name = dir.file_name()?.to_string_lossy().to_string();
    let attrs = [dir.join("sas_phy").join(&name), dir.to_path_buf()].into_iter().find(|d| d.join("invalid_dword_count").exists())?;
    let n = |a: &str| read_trim(&attrs.join(a)).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let r = |a: &str| read_trim(&attrs.join(a)).and_then(|s| parse_sysfs_rate(&s));
    let rates = HostRates { negotiated: r("negotiated_linkrate"), maximum: r("maximum_linkrate"), maximum_hw: r("maximum_linkrate_hw"), minimum_hw: r("minimum_linkrate_hw") };
    Some((
        Phy {
            phy: name,
            rate_mbps: rates.negotiated,
            attached_sas: None,
            invalid_dwords: n("invalid_dword_count"),
            disparity_errors: n("running_disparity_error_count"),
            loss_of_sync: n("loss_of_dword_sync_count"),
            phy_resets: n("phy_reset_problem_count"),
        },
        rates,
    ))
}

/// The host side of a drive by its kernel name.
pub fn read_host(sys: &std::path::Path, name: &str) -> (Vec<Phy>, HostRates) {
    let Ok(real) = std::fs::canonicalize(sys.join("block").join(name).join("device")) else {
        return (Vec::new(), HostRates::default());
    };
    let mut phys = Vec::new();
    let mut rates = HostRates::default();
    for dir in facing_phys(&real) {
        if let Some((p, r)) = read_host_phy(&dir) {
            if rates == HostRates::default() {
                rates = r;
            }
            phys.push(p);
        }
    }
    (phys, rates)
}

/// Hold the phys facing a drive at `want` (`maximum_linkrate`, then a link
/// reset so it renegotiates), as `stormblock hba linkrate` does. A phy
/// already there is left alone. Ok: what was done, in words.
pub fn apply(sys: &std::path::Path, name: &str, want: Wanted) -> Result<String, String> {
    let real = std::fs::canonicalize(sys.join("block").join(name).join("device")).map_err(|e| format!("{name}: no device: {e}"))?;
    let dirs = facing_phys(&real);
    if dirs.is_empty() {
        return Err(format!("{name}: no SAS phy faces it (no SAS transport in sysfs)"));
    }
    let mut done = Vec::new();
    for dir in dirs {
        let pname = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let attrs = [dir.join("sas_phy").join(&pname), dir.clone()].into_iter().find(|d| d.join("maximum_linkrate").exists()).ok_or_else(|| format!("{pname}: no maximum_linkrate"))?;
        let cur = read_trim(&attrs.join("maximum_linkrate")).and_then(|s| parse_sysfs_rate(&s));
        let hw = read_trim(&attrs.join("maximum_linkrate_hw")).and_then(|s| parse_sysfs_rate(&s));
        let min_hw = read_trim(&attrs.join("minimum_linkrate_hw")).and_then(|s| parse_sysfs_rate(&s));
        let target = match want {
            Wanted::Max(m) => {
                if hw.is_some_and(|h| m > h) || min_hw.is_some_and(|l| m < l) {
                    return Err(format!("{pname}: {} is outside what the phy can do ({}–{})", gbit(m), min_hw.map_or("?".into(), gbit), hw.map_or("?".into(), gbit)));
                }
                m
            }
            Wanted::Hw => hw.ok_or_else(|| format!("{pname}: no maximum_linkrate_hw"))?,
        };
        if cur == Some(target) {
            done.push(format!("{pname} already at maximum {}", gbit(target)));
            continue;
        }
        std::fs::write(attrs.join("maximum_linkrate"), sysfs_rate(target)).map_err(|e| format!("{pname}: maximum_linkrate {}: {e}", sysfs_rate(target)))?;
        std::fs::write(attrs.join("link_reset"), "1").map_err(|e| format!("{pname}: link_reset: {e}"))?;
        done.push(format!("{pname}: maximum {} → {}, link reset", cur.map_or("?".into(), gbit), gbit(target)));
    }
    Ok(done.join("; "))
}

/// The drive side over SG: log page 0x18, when the drive lists it.
#[cfg(target_os = "linux")]
pub fn read_drive(dev: &crate::scsi::Device) -> Option<Vec<Phy>> {
    use crate::smart::scsi::log_sense_cdb;
    let log = |page: u8| {
        let mut buf = vec![0u8; 4096];
        let n = dev.io(&log_sense_cdb(page, 4096), crate::scsi::Dir::FromDevice, &mut buf, 10_000).ok()?;
        buf.truncate(n);
        Some(buf)
    };
    let listed = log(0x00).is_some_and(|raw| {
        let end = raw.len().min(4 + raw.get(2..4).map_or(0, |l| u16::from_be_bytes([l[0], l[1]]) as usize));
        raw.get(4..end).is_some_and(|pages| pages.contains(&LOG_SAS_PORT))
    });
    if !listed {
        return None;
    }
    let phys = parse_port_log(&log(LOG_SAS_PORT)?);
    (!phys.is_empty()).then_some(phys)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One port of one or more phys: (phy id, rate code, counters).
    fn port(code: u16, phys: &[(u8, u8, [u32; 4])]) -> Vec<u8> {
        let mut d = vec![PROTOCOL_SAS, 0, 1, phys.len() as u8];
        for (id, rate, c) in phys {
            let mut p = vec![0u8; 48];
            p[1] = *id;
            p[3] = 44;
            p[4] = 0x10;
            p[5] = 0x90 | rate;
            if *rate != 0 {
                p[16..24].copy_from_slice(&[0x50, 0x0a, 0x09, 0x80, 0, 0, 0, *id]);
            }
            for (i, v) in c.iter().enumerate() {
                p[32 + i * 4..36 + i * 4].copy_from_slice(&v.to_be_bytes());
            }
            d.extend_from_slice(&p);
        }
        let mut out = code.to_be_bytes().to_vec();
        out.extend_from_slice(&[0, d.len() as u8]);
        out.extend_from_slice(&d);
        out
    }

    fn page(params: &[Vec<u8>]) -> Vec<u8> {
        let body = params.concat();
        let mut raw = vec![LOG_SAS_PORT, 0];
        raw.extend_from_slice(&(body.len() as u16).to_be_bytes());
        raw.extend_from_slice(&body);
        raw
    }

    #[test]
    fn the_port_log_page_gives_every_phy_and_its_counters() {
        // forge1 bay 3: dual ported, the link on phy 0, loss of sync counting.
        let raw = page(&[port(1, &[(0, 0xb, [12, 3, 4_123, 7])]), port(2, &[(1, 0, [0, 0, 0, 0])])]);
        let phys = parse_port_log(&raw);
        assert_eq!(phys.len(), 2);
        assert_eq!(phys[0], Phy { phy: "0".into(), rate_mbps: Some(12_000), attached_sas: Some("0x500a098000000000".into()), invalid_dwords: 12, disparity_errors: 3, loss_of_sync: 4_123, phy_resets: 7 });
        assert_eq!(phys[1].rate_mbps, None, "no link on the second port");
        assert_eq!(phys[1].attached_sas, None);
        assert!(parse_port_log(&[0x2f, 0, 0, 0]).is_empty(), "another page");
        // A truncated descriptor is not read past.
        let mut short = raw.clone();
        short.truncate(40);
        assert!(parse_port_log(&short).len() <= 1);
    }

    #[test]
    fn sysfs_rates_and_the_spec() {
        assert_eq!(parse_sysfs_rate("12.0 Gbit"), Some(12_000));
        assert_eq!(parse_sysfs_rate("1.5 Gbit\n"), Some(1_500));
        assert_eq!(parse_sysfs_rate("Unknown"), None);
        assert_eq!(parse_sysfs_rate("Phy disabled"), None);
        assert_eq!(sysfs_rate(6_000), "6.0 Gbit");
        assert_eq!(sysfs_rate(22_500), "22.5 Gbit");
        use serde_json::json;
        for (v, w) in [(json!(6), Wanted::Max(6_000)), (json!("6"), Wanted::Max(6_000)), (json!("6 Gbit"), Wanted::Max(6_000)), (json!("6.0 Gbit"), Wanted::Max(6_000)), (json!("3G"), Wanted::Max(3_000)), (json!(1.5), Wanted::Max(1_500)), (json!("12Gb/s"), Wanted::Max(12_000)), (json!("HW"), Wanted::Hw)] {
            assert_eq!(parse_wanted(&v), Ok(Some(w)), "{v}");
        }
        assert_eq!(parse_wanted(&json!(null)), Ok(None));
        assert_eq!(parse_wanted(&json!("")), Ok(None));
        for bad in [json!(5), json!("fast"), json!(true), json!("0")] {
            assert!(parse_wanted(&bad).is_err(), "{bad}");
        }
    }

    fn phy(name: &str, c: [u64; 4]) -> Phy {
        Phy { phy: name.into(), rate_mbps: Some(12_000), attached_sas: None, invalid_dwords: c[0], disparity_errors: c[1], loss_of_sync: c[2], phy_resets: c[3] }
    }

    fn reading(drive: [u64; 4], host: [u64; 4]) -> Reading {
        Reading {
            drive: Some(vec![phy("0", drive)]),
            host: vec![phy("phy-1:2", host)],
            host_rates: HostRates { negotiated: Some(12_000), maximum: Some(12_000), maximum_hw: Some(12_000), minimum_hw: Some(1_500) },
        }
    }

    #[test]
    fn levels_from_settling_to_failing_and_back() {
        // First reading: settling, no rates.
        let s0 = step(None, &reading([0, 0, 1000, 0], [0; 4]), 1_000);
        assert_eq!(s0.level, Level::Settling);
        assert!(s0.drive.as_ref().unwrap().rates.is_empty());
        assert_eq!(s0.host_phy.as_deref(), Some("phy-1:2"));
        // Nothing moved in a minute: ok.
        let s1 = step(Some(&s0), &reading([0, 0, 1000, 0], [0; 4]), 1_060);
        assert_eq!(s1.level, Level::Ok);
        assert_eq!((s1.since, s1.streak), (1_060, 0));
        assert!(s1.recommendation.is_none());
        // Bay 3: loss of sync ~500/min on the drive side only.
        let s2 = step(Some(&s1), &reading([0, 0, 1500, 0], [0; 4]), 1_120);
        assert_eq!(s2.level, Level::Degraded, "one sample moving is degraded");
        assert_eq!(s2.drive.as_ref().unwrap().rates[0].loss_of_sync, 500.0);
        assert!(s2.message.contains("drive phy 0: loss of sync 500/min"), "{}", s2.message);
        assert!(s2.recommendation.as_deref().unwrap().starts_with("the drive's receiver or its slot"));
        assert!(s2.degraded.on && !s2.failing.on);
        let s3 = step(Some(&s2), &reading([0, 0, 2000, 0], [0; 4]), 1_180);
        assert_eq!(s3.level, Level::Failing, "two samples running");
        assert_eq!((s3.streak, s3.sync_streak, s3.since), (2, 2, 1_180));
        assert_eq!(s3.degraded.since, 1_120, "degraded since it first moved");
        assert!(s3.failing.on);
        // Still failing: since holds.
        let s4 = step(Some(&s3), &reading([0, 0, 2500, 0], [0; 4]), 1_240);
        assert_eq!((s4.level, s4.since), (Level::Failing, 1_180));
        // Invalid DWORDs only, sync stopped: degraded, not failing.
        let s5 = step(Some(&s4), &reading([30, 0, 2500, 0], [0; 4]), 1_300);
        assert_eq!(s5.level, Level::Degraded);
        assert!(!s5.failing.on && s5.degraded.on && s5.degraded.since == 1_120);
        // Quiet: ok again, both conditions off.
        let s6 = step(Some(&s5), &reading([30, 0, 2500, 0], [0; 4]), 1_360);
        assert_eq!(s6.level, Level::Ok);
        assert!(!s6.degraded.on && s6.degraded.since == 1_360);
    }

    #[test]
    fn both_sides_moving_is_the_cable() {
        let s0 = step(None, &reading([0; 4], [0; 4]), 0);
        let s1 = step(Some(&s0), &reading([10, 0, 0, 0], [0, 4, 0, 0]), 120);
        assert_eq!(s1.level, Level::Degraded);
        assert_eq!(s1.drive.as_ref().unwrap().rates[0].invalid_dwords, 5.0, "per minute over two");
        assert_eq!(s1.host.as_ref().unwrap().rates[0].disparity_errors, 2.0);
        assert!(s1.recommendation.as_deref().unwrap().contains("the cable or the expander port"));
        // Host side only.
        let s2 = step(Some(&s1), &reading([10, 0, 0, 0], [0, 8, 0, 0]), 180);
        assert!(s2.recommendation.as_deref().unwrap().contains("transmitter"));
    }

    #[test]
    fn counters_going_back_settle_and_keep_the_conditions() {
        let s0 = step(None, &reading([0, 0, 100, 0], [0; 4]), 0);
        let s1 = step(Some(&s0), &reading([0, 0, 200, 0], [0; 4]), 60);
        assert!(s1.degraded.on);
        // A power cycle: the drive's counters start from 0.
        let s2 = step(Some(&s1), &reading([0, 0, 5, 0], [0; 4]), 120);
        assert_eq!(s2.level, Level::Settling);
        assert!(s2.degraded.on, "a reset of the counters is not a recovery");
        assert_eq!(s2.streak, 0);
        // A new phy on the host side settles too.
        let mut r = reading([0, 0, 5, 0], [0; 4]);
        r.host[0].phy = "phy-1:3".into();
        assert_eq!(step(Some(&s2), &r, 180).level, Level::Settling);
        // A drive side that could not be read this time: the host alone.
        let s3 = step(Some(&s2), &reading([0, 0, 5, 0], [0; 4]), 180);
        let mut r = reading([0; 4], [0; 4]);
        r.drive = None;
        let s4 = step(Some(&s3), &r, 240);
        assert_eq!(s4.level, Level::Ok);
        assert!(s4.drive.is_none());
    }

    #[test]
    fn events_on_the_changes_that_matter() {
        assert!(noteworthy(None, Level::Degraded));
        assert!(noteworthy(Some(Level::Ok), Level::Failing));
        assert!(noteworthy(Some(Level::Degraded), Level::Failing));
        assert!(noteworthy(Some(Level::Failing), Level::Ok));
        assert!(!noteworthy(None, Level::Ok), "the first ok");
        assert!(!noteworthy(Some(Level::Ok), Level::Ok));
        assert!(!noteworthy(Some(Level::Failing), Level::Failing));
        assert!(!noteworthy(Some(Level::Ok), Level::Settling));
        // observe: settling between two levels does not hide the change.
        let id = DriveId(uuid::Uuid::from_u128(0x149));
        assert_eq!(observe(id, &reading([0; 4], [0; 4]), 0).1, None);
        assert_eq!(observe(id, &reading([0; 4], [0; 4]), 60).1, None, "settling → ok");
        assert_eq!(observe(id, &reading([0, 0, 9, 0], [0; 4]), 120).1, Some(Some(Level::Ok)));
        assert_eq!(observe(id, &reading([0, 0, 9, 0], [0; 4]), 180).1, Some(Some(Level::Degraded)), "degraded → ok");
        // Degraded, a power cycle (settling), degraded again: one event.
        assert!(observe(id, &reading([0, 0, 19, 0], [0; 4]), 240).1.is_some());
        assert_eq!(observe(id, &reading([0, 0, 1, 0], [0; 4]), 300).1, None, "settling");
        assert_eq!(observe(id, &reading([0, 0, 3, 0], [0; 4]), 360).1, None, "degraded again: no new event");
    }

    #[test]
    fn the_maximum_in_words() {
        let mut s = step(None, &reading([0; 4], [0; 4]), 0);
        assert_eq!(maximum_said(&s, None).as_deref(), Some("maximum 12 Gbit (hardware)"));
        s.host_maximum_mbps = Some(6_000);
        assert_eq!(maximum_said(&s, Some(Wanted::Max(6_000))).as_deref(), Some("maximum 6 Gbit (policy)"));
        assert_eq!(maximum_said(&s, None).as_deref(), Some("maximum 6 Gbit (set, hardware 12 Gbit)"));
        s.host_maximum_mbps = None;
        assert_eq!(maximum_said(&s, None), None);
    }

    #[test]
    fn conditions_carry_the_message_and_the_part() {
        let s0 = step(None, &reading([0; 4], [0; 4]), 0);
        let s1 = step(Some(&s0), &reading([0, 0, 500, 0], [0; 4]), 60);
        let s2 = step(Some(&s1), &reading([0, 0, 1000, 0], [0; 4]), 120);
        let c = conditions(&s2);
        assert_eq!(c[0]["type"], "LinkDegraded");
        assert_eq!(c[0]["status"], "True");
        assert_eq!(c[1]["type"], "LinkFailing");
        assert_eq!(c[1]["status"], "True");
        assert_eq!(c[1]["reason"], "SyncLost");
        assert_eq!(c[1]["lastTransitionTime"], "1970-01-01T00:02:00Z");
        let m = c[1]["message"].as_str().unwrap();
        assert!(m.contains("loss of sync 500/min") && m.contains("reseat"), "{m}");
        let ok = conditions(&s0);
        assert_eq!(ok[0]["status"], "False");
    }

    /// A shelf drive behind an expander, and a direct-attached one: the phys
    /// of the port before `end_device-`, with the class attributes.
    #[cfg(unix)]
    #[test]
    fn the_facing_phy_from_sysfs_and_holding_it() {
        let root = std::env::temp_dir().join(format!("stormdrive-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let host = root.join("devices/pci0000:00/host1");
        let port = host.join("port-1:2");
        let ed = port.join("end_device-1:2/target1:0:2/1:0:2:0");
        std::fs::create_dir_all(&ed).unwrap();
        let attrs = port.join("phy-1:2/sas_phy/phy-1:2");
        std::fs::create_dir_all(&attrs).unwrap();
        for (a, v) in [
            ("invalid_dword_count", "3"),
            ("running_disparity_error_count", "1"),
            ("loss_of_dword_sync_count", "4123"),
            ("phy_reset_problem_count", "0"),
            ("negotiated_linkrate", "12.0 Gbit"),
            ("maximum_linkrate", "12.0 Gbit"),
            ("maximum_linkrate_hw", "12.0 Gbit"),
            ("minimum_linkrate_hw", "1.5 Gbit"),
        ] {
            std::fs::write(attrs.join(a), format!("{v}\n")).unwrap();
        }
        std::fs::create_dir_all(root.join("block/sdd")).unwrap();
        std::os::unix::fs::symlink(&ed, root.join("block/sdd/device")).unwrap();
        let (phys, rates) = read_host(&root, "sdd");
        assert_eq!(phys.len(), 1);
        assert_eq!((phys[0].phy.as_str(), phys[0].loss_of_sync, phys[0].invalid_dwords), ("phy-1:2", 4123, 3));
        assert_eq!(rates, HostRates { negotiated: Some(12_000), maximum: Some(12_000), maximum_hw: Some(12_000), minimum_hw: Some(1_500) });
        // Hold it at 6: the maximum written, a link reset asked for.
        let said = apply(&root, "sdd", Wanted::Max(6_000)).unwrap();
        assert!(said.contains("12 Gbit → 6 Gbit"), "{said}");
        assert_eq!(std::fs::read_to_string(attrs.join("maximum_linkrate")).unwrap(), "6.0 Gbit");
        assert_eq!(std::fs::read_to_string(attrs.join("link_reset")).unwrap(), "1");
        // Again: already there, nothing written.
        std::fs::remove_file(attrs.join("link_reset")).unwrap();
        assert!(apply(&root, "sdd", Wanted::Max(6_000)).unwrap().contains("already"));
        assert!(!attrs.join("link_reset").exists());
        // Back to the hardware's.
        apply(&root, "sdd", Wanted::Hw).unwrap();
        assert_eq!(std::fs::read_to_string(attrs.join("maximum_linkrate")).unwrap(), "12.0 Gbit");
        // Past what the phy can do.
        std::fs::write(attrs.join("maximum_linkrate_hw"), "6.0 Gbit\n").unwrap();
        assert!(apply(&root, "sdd", Wanted::Max(12_000)).unwrap_err().contains("outside"));
        // No SAS transport.
        let plain = root.join("devices/pci0000:00/ata1/host2/target2:0:0/2:0:0:0");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::create_dir_all(root.join("block/sda")).unwrap();
        std::os::unix::fs::symlink(&plain, root.join("block/sda/device")).unwrap();
        assert!(read_host(&root, "sda").0.is_empty());
        assert!(apply(&root, "sda", Wanted::Max(6_000)).is_err());
        // Behind an expander: the expander's phy.
        let exp_port = host.join("port-1:0/expander-1:0/port-1:0:5");
        let ed2 = exp_port.join("end_device-1:0:5/target1:0:9/1:0:9:0");
        std::fs::create_dir_all(&ed2).unwrap();
        std::fs::create_dir_all(exp_port.join("phy-1:0:5")).unwrap();
        assert_eq!(facing_phys(&ed2), vec![exp_port.join("phy-1:0:5")]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
