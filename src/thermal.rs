//! Thermal management (#125): every component the node can read a
//! temperature or a fan from, its limits, its level, alerts, the cooling the
//! node asks its BMC for, and what a hot drive means for the engine.
//!
//! **Readings**, every `thermal.interval_secs` (30 s):
//! - every drive: its own hwmon (NVMe, drivetemp) when it has one, read
//!   fresh, else the health sampler's latest reading;
//! - every other hwmon chip: CPU (coretemp, k10temp), DIMMs (jc42, spd5118),
//!   NICs (a chip on a network device), HBAs, GPUs, chipset, board, PSUs, and
//!   every fan's RPM;
//! - every shelf's SES temperature sensors and fans.
//!
//! Inlet, exhaust and the BMC's fans come from stormipmi (the BMC is its);
//! a board's own hwmon labelled inlet/ambient/exhaust is read here.
//!
//! **Limits**: the device's own first (hwmon `_max`/`_crit`, which for NVMe
//! is its WCTEMP/CCTEMP and for drivetemp the drive's SCT limits; SAS LOG
//! SENSE 0x0D's reference temperature; a shelf sensor's SES thresholds), a
//! live setting (`PUT /api/v1/thermal/limits`) where the device gives none
//! — or lower than the device's, never higher — else the class default.
//!
//! **Levels** ok → near (within `near_c` of warning) → warning → critical,
//! each left only `hysteresis_c` below where it was entered. An alert (event
//! + log line) on warning and again on critical, and when it is over.
//!
//! **Acting**: the node's cooling request (auto | raise | max) for the BMC;
//! a drive at warning asks the engine to slow its background I/O, at
//! critical to place nothing new on it, and critical for
//! `replace_after_mins` proposes its replacement.

use crate::drive::DriveId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};

/// What a component is, for its default limits and for grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Drive,
    Cpu,
    Dimm,
    Hba,
    Nic,
    Gpu,
    Chipset,
    Board,
    Psu,
    Inlet,
    Exhaust,
    Shelf,
    Other,
}

impl Class {
    pub const ALL: [Class; 13] = [
        Class::Drive,
        Class::Cpu,
        Class::Dimm,
        Class::Hba,
        Class::Nic,
        Class::Gpu,
        Class::Chipset,
        Class::Board,
        Class::Psu,
        Class::Inlet,
        Class::Exhaust,
        Class::Shelf,
        Class::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Class::Drive => "drive",
            Class::Cpu => "cpu",
            Class::Dimm => "dimm",
            Class::Hba => "hba",
            Class::Nic => "nic",
            Class::Gpu => "gpu",
            Class::Chipset => "chipset",
            Class::Board => "board",
            Class::Psu => "psu",
            Class::Inlet => "inlet",
            Class::Exhaust => "exhaust",
            Class::Shelf => "shelf",
            Class::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Class> {
        Class::ALL.into_iter().find(|c| c.as_str() == s)
    }

    /// (warning, critical) °C when neither the device nor a setting says.
    /// A drive's are `monitor.temp_warn_c`/`temp_crit_c` (the caller's).
    /// Inlet: ASHRAE A2's 35 °C allowable maximum.
    pub fn default_limits(self) -> (f64, f64) {
        match self {
            Class::Drive => (55.0, 70.0),
            Class::Cpu => (85.0, 95.0),
            Class::Dimm => (80.0, 90.0),
            Class::Hba => (85.0, 100.0),
            Class::Nic => (90.0, 105.0),
            Class::Gpu => (85.0, 95.0),
            Class::Chipset => (80.0, 95.0),
            Class::Board => (70.0, 85.0),
            Class::Psu => (70.0, 85.0),
            Class::Inlet => (35.0, 42.0),
            Class::Exhaust => (60.0, 70.0),
            Class::Shelf => (50.0, 60.0),
            Class::Other => (80.0, 95.0),
        }
    }
}

/// Where a limit came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitSource {
    /// The device's own (hwmon, NVMe WCTEMP/CCTEMP, SAS reference
    /// temperature, SES thresholds).
    Device,
    /// A live setting (`PUT /api/v1/thermal/limits`) or `[thermal.classes]`.
    Setting,
    /// The class default.
    Class,
}

/// What the device says its own limits are; either may be absent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_c: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crit_c: Option<f64>,
}

impl DeviceLimits {
    /// Only plausible limits count: firmware that has none often reports
    /// 0, 127, 255 or a placeholder in the hundreds.
    pub fn sane(self) -> Self {
        let ok = |v: Option<f64>| v.filter(|v| v.is_finite() && *v > 0.0 && *v <= 150.0);
        let (warn_c, crit_c) = (ok(self.warn_c), ok(self.crit_c));
        match (warn_c, crit_c) {
            (Some(w), Some(c)) if w >= c => Self { warn_c: None, crit_c: Some(c) },
            _ => Self { warn_c, crit_c },
        }
    }
}

/// A live limit for a class or one component (either side may be unset).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitSetting {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_c: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crit_c: Option<f64>,
}

impl LimitSetting {
    pub fn validate(&self) -> Result<(), String> {
        for v in [self.warn_c, self.crit_c].into_iter().flatten() {
            if !(v.is_finite() && (0.0..=150.0).contains(&v)) {
                return Err(format!("{v} °C: a limit is between 0 and 150 °C"));
            }
        }
        if let (Some(w), Some(c)) = (self.warn_c, self.crit_c) {
            if w >= c {
                return Err(format!("warning {w} °C must be below critical {c} °C"));
            }
        }
        Ok(())
    }
}

/// The live settings (`<data_dir>/thermal-limits.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// By class name (`drive`, `cpu`, …).
    pub classes: BTreeMap<String, LimitSetting>,
    /// By component id (`drive:<uuid>`, `hwmon:coretemp.0:temp1`, …).
    pub components: BTreeMap<String, LimitSetting>,
}

impl Settings {
    pub fn validate(&self) -> Result<(), String> {
        for (k, v) in &self.classes {
            if Class::parse(k).is_none() {
                let names: Vec<_> = Class::ALL.iter().map(|c| c.as_str()).collect();
                return Err(format!("class {k:?}: one of {}", names.join(", ")));
            }
            v.validate().map_err(|e| format!("class {k}: {e}"))?;
        }
        for (k, v) in &self.components {
            v.validate().map_err(|e| format!("component {k}: {e}"))?;
        }
        Ok(())
    }
}

/// A component's effective limits.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    pub warn_c: f64,
    pub crit_c: f64,
    /// Where the warning limit came from.
    pub warn_source: LimitSource,
    /// Where the critical limit came from.
    pub crit_source: LimitSource,
}

/// Gap kept between a warning limit derived from a critical one and that
/// critical one, and the other way round.
pub const DERIVED_GAP_C: f64 = 5.0;

/// Device first; a setting fills what the device does not give and may
/// lower what it does, never raise it; else the class default (`base`).
/// A warning limit at or above the critical one is put `DERIVED_GAP_C` under.
pub fn resolve(device: DeviceLimits, setting: Option<LimitSetting>, base: (f64, f64)) -> Limits {
    let device = device.sane();
    let pick = |dev: Option<f64>, set: Option<f64>, def: f64| match (dev, set) {
        (Some(d), Some(s)) if s < d => (s, LimitSource::Setting),
        (Some(d), _) => (d, LimitSource::Device),
        (None, Some(s)) => (s, LimitSource::Setting),
        (None, None) => (def, LimitSource::Class),
    };
    let set = setting.unwrap_or_default();
    let (crit_c, crit_source) = pick(device.crit_c, set.crit_c, base.1);
    // A device that gives only its critical limit: warn that far under it,
    // unless a setting or the class default is lower still.
    let (mut warn_c, mut warn_source) = match (device.warn_c, set.warn_c) {
        (None, None) if device.crit_c.is_some() && base.0 > crit_c - DERIVED_GAP_C => (crit_c - DERIVED_GAP_C, LimitSource::Device),
        _ => pick(device.warn_c, set.warn_c, base.0),
    };
    if warn_c >= crit_c {
        warn_c = crit_c - DERIVED_GAP_C;
        warn_source = crit_source;
    }
    Limits { warn_c, crit_c, warn_source, crit_source }
}

/// How hot a component is against its limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// No reading.
    Unknown,
    Ok,
    /// Within `near_c` of its warning limit: the fans should rise.
    Near,
    Warning,
    Critical,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Unknown => "unknown",
            Level::Ok => "ok",
            Level::Near => "near",
            Level::Warning => "warning",
            Level::Critical => "critical",
        }
    }
}

/// The level for a reading, given the previous one: a level is entered at
/// its limit and left only `hyst` below it, so a reading on the line does
/// not flap.
pub fn level(prev: Level, t: Option<f64>, l: &Limits, near: f64, hyst: f64) -> Level {
    let Some(t) = t else { return Level::Unknown };
    let held = |at: Level, limit: f64| t >= limit || (prev >= at && t > limit - hyst);
    if held(Level::Critical, l.crit_c) {
        Level::Critical
    } else if held(Level::Warning, l.warn_c) {
        Level::Warning
    } else if held(Level::Near, l.warn_c - near) {
        Level::Near
    } else {
        Level::Ok
    }
}

/// One temperature as read this pass.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub id: String,
    pub class: Class,
    pub label: String,
    /// `hwmon:<chip>`, `smart`, `ses`.
    pub source: String,
    pub drive_id: Option<DriveId>,
    pub shelf: Option<String>,
    pub temp_c: Option<f64>,
    pub device: DeviceLimits,
}

/// One fan as read this pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fan {
    pub id: String,
    pub label: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shelf: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_rpm: Option<u64>,
    /// The chip or the enclosure flags it (alarm, fault, SES critical).
    #[serde(default)]
    pub failed: bool,
    /// Why it counts as not cooling, when it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// A fan that is stopped, under its minimum or flagged does not cool.
pub fn fan_problem(f: &Fan) -> Option<String> {
    if f.failed {
        return Some(format!("{} reports a fault", f.label));
    }
    match (f.rpm, f.min_rpm) {
        (Some(0), Some(m)) if m > 0 => Some(format!("{} stopped (minimum {m} RPM)", f.label)),
        (Some(r), Some(m)) if r < m => Some(format!("{} at {r} RPM, under its minimum {m} RPM", f.label)),
        _ => None,
    }
}

/// A component after this pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Component {
    pub id: String,
    pub class: Class,
    pub label: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive_id: Option<DriveId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shelf: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_c: Option<f64>,
    pub limits: Limits,
    /// What the device itself said, before settings and defaults.
    pub device_limits: DeviceLimits,
    pub level: Level,
    /// Since when it is at `level` (unix seconds).
    pub since_unix: u64,
    /// Its hottest reading this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_c: Option<f64>,
}

/// What the node asks its BMC for (stormipmi acts on it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoolingLevel {
    /// The BMC's own fan curve: nothing is near a limit.
    Auto,
    /// Something is near its warning limit, or over it, or a fan has
    /// stopped: more air.
    Raise,
    /// Something is at its critical limit: full speed.
    Max,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cooling {
    pub request: CoolingLevel,
    /// The components and fans behind it, hottest first.
    pub why: Vec<String>,
    pub since_unix: u64,
}

/// The cooling request from this pass's components and fans.
pub fn cooling(components: &[Component], fans: &[Fan]) -> (CoolingLevel, Vec<String>) {
    let mut hot: Vec<&Component> = components.iter().filter(|c| c.level >= Level::Near).collect();
    hot.sort_by(|a, b| b.level.cmp(&a.level).then(margin(a).total_cmp(&margin(b))));
    let mut why: Vec<String> = hot
        .iter()
        .map(|c| {
            let limit = if c.level == Level::Critical { ("critical", c.limits.crit_c) } else { ("warning", c.limits.warn_c) };
            format!("{} {:.0} °C ({} {:.0} °C)", c.label, c.temp_c.unwrap_or_default(), limit.0, limit.1)
        })
        .collect();
    why.extend(fans.iter().filter_map(|f| f.problem.clone()));
    let level = if hot.iter().any(|c| c.level == Level::Critical) {
        CoolingLevel::Max
    } else if !why.is_empty() {
        CoolingLevel::Raise
    } else {
        CoolingLevel::Auto
    };
    (level, why)
}

/// Degrees under its warning limit (negative = over).
fn margin(c: &Component) -> f64 {
    c.limits.warn_c - c.temp_c.unwrap_or(f64::NEG_INFINITY)
}

/// A component's level changed in a way someone is told about.
#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub id: String,
    pub label: String,
    pub drive_id: Option<DriveId>,
    pub from: Level,
    pub to: Level,
    pub temp_c: Option<f64>,
    pub limits: Limits,
}

impl Alert {
    /// Up into warning or critical, or down out of them.
    pub fn worth_telling(from: Level, to: Level) -> bool {
        let alarm = |l: Level| l >= Level::Warning;
        (to > from && alarm(to)) || (to < from && alarm(from) && to != Level::Unknown)
    }

    pub fn severity(&self) -> crate::events::Severity {
        match self.to {
            Level::Critical => crate::events::Severity::Error,
            Level::Warning => crate::events::Severity::Warning,
            _ => crate::events::Severity::Info,
        }
    }

    pub fn message(&self) -> String {
        let t = self.temp_c.map_or("no reading".into(), |t| format!("{t:.0} °C"));
        let l = &self.limits;
        let what = match self.to {
            Level::Critical => format!("over its critical limit {:.0} °C", l.crit_c),
            Level::Warning if self.from == Level::Critical => format!("back under critical {:.0} °C, still over warning {:.0} °C", l.crit_c, l.warn_c),
            Level::Warning => format!("over its warning limit {:.0} °C (critical {:.0} °C)", l.warn_c, l.crit_c),
            _ => format!("back under its warning limit {:.0} °C", l.warn_c),
        };
        format!("{}: {t}, {what}", self.label)
    }
}

/// What a hot drive means, on the drive (`Drive.thermal`) and to the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriveThermal {
    pub level: Level,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_c: Option<f64>,
    pub limits: Limits,
    pub since_unix: u64,
    /// At warning or over: the engine slows the drive's background I/O
    /// (scrub, rebuild, flow-over) first.
    pub throttle_background: bool,
    /// At critical: the engine places nothing new on it.
    pub exclude_new_placement: bool,
    /// Critical for `thermal.replace_after_mins`: replace it.
    pub replace_proposed: bool,
}

impl DriveThermal {
    pub fn from_component(c: &Component, now: u64, replace_after_secs: u64) -> Self {
        Self {
            level: c.level,
            temp_c: c.temp_c,
            limits: c.limits,
            since_unix: c.since_unix,
            throttle_background: c.level >= Level::Warning,
            exclude_new_placement: c.level == Level::Critical,
            replace_proposed: c.level == Level::Critical && now.saturating_sub(c.since_unix) >= replace_after_secs,
        }
    }

    /// What the engine is told; a push is due when this changes.
    pub fn engine_word(&self) -> String {
        format!("{}:{}:{}:{}", self.level.as_str(), self.throttle_background, self.exclude_new_placement, self.replace_proposed)
    }
}

/// One line of the thermal history in system-data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryLine {
    pub unix: u64,
    pub boot_id: String,
    pub components: Vec<HistoryPoint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fans: Vec<FanPoint>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryPoint {
    pub id: String,
    pub class: Class,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_c: Option<f64>,
    pub level: Level,
    pub warn_c: f64,
    pub crit_c: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FanPoint {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u64>,
}

/// Tunables of a pass (from `[thermal]`).
#[derive(Debug, Clone, Copy)]
pub struct PassCfg {
    pub near_c: f64,
    pub hysteresis_c: f64,
    pub replace_after_secs: u64,
    pub heartbeat_secs: u64,
    pub trend_len: usize,
    /// A drive's class default (`monitor.temp_warn_c`/`temp_crit_c`).
    pub drive_default: (f64, f64),
}

/// Everything between passes: the components, their trend, what was last
/// written to the history.
#[derive(Debug, Default)]
pub struct Tracker {
    pub components: BTreeMap<String, Component>,
    pub fans: Vec<Fan>,
    pub cooling: Option<Cooling>,
    pub at_unix: u64,
    trend: HashMap<String, VecDeque<(u64, f64)>>,
    /// Per id: the (temp, level, unix) last written to the history.
    written: HashMap<String, (Option<f64>, Level, u64)>,
    fans_written: HashMap<String, (Option<u64>, u64)>,
    pub passes: u64,
}

/// The base (class or `[thermal.classes]`/setting) limits for a class.
fn class_base(class: Class, settings: &Settings, cfg: &PassCfg) -> (Option<LimitSetting>, (f64, f64)) {
    let base = if class == Class::Drive { cfg.drive_default } else { class.default_limits() };
    (settings.classes.get(class.as_str()).copied(), base)
}

/// The setting for one component: its own over its class's.
fn setting_for(id: &str, class: Class, settings: &Settings, cfg: &PassCfg) -> (Option<LimitSetting>, (f64, f64)) {
    let (class_set, base) = class_base(class, settings, cfg);
    let own = settings.components.get(id).copied();
    let merged = match (own, class_set) {
        (Some(o), Some(c)) => Some(LimitSetting { warn_c: o.warn_c.or(c.warn_c), crit_c: o.crit_c.or(c.crit_c) }),
        (o, c) => o.or(c),
    };
    (merged, base)
}

impl Tracker {
    /// One pass: limits, levels, cooling, the alerts to tell, and the
    /// history line due (None when nothing changed and no heartbeat is due).
    pub fn pass(
        &mut self,
        readings: Vec<Reading>,
        mut fans: Vec<Fan>,
        settings: &Settings,
        cfg: &PassCfg,
        now: u64,
        boot_id: &str,
    ) -> (Vec<Alert>, Option<HistoryLine>) {
        let mut alerts = Vec::new();
        let mut next = BTreeMap::new();
        for r in readings {
            let (set, base) = setting_for(&r.id, r.class, settings, cfg);
            let limits = resolve(r.device, set, base);
            let prev = self.components.get(&r.id);
            let prev_level = prev.map_or(Level::Unknown, |c| c.level);
            let lvl = level(prev_level, r.temp_c, &limits, cfg.near_c, cfg.hysteresis_c);
            let since_unix = match prev {
                Some(p) if p.level == lvl => p.since_unix,
                _ => now,
            };
            let max_c = match (prev.and_then(|p| p.max_c), r.temp_c) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            if Alert::worth_telling(prev_level, lvl) {
                alerts.push(Alert { id: r.id.clone(), label: r.label.clone(), drive_id: r.drive_id, from: prev_level, to: lvl, temp_c: r.temp_c, limits });
            }
            if let Some(t) = r.temp_c {
                let tr = self.trend.entry(r.id.clone()).or_default();
                tr.push_back((now, t));
                while tr.len() > cfg.trend_len.max(1) {
                    tr.pop_front();
                }
            }
            next.insert(
                r.id.clone(),
                Component {
                    id: r.id,
                    class: r.class,
                    label: r.label,
                    source: r.source,
                    drive_id: r.drive_id,
                    shelf: r.shelf,
                    temp_c: r.temp_c,
                    limits,
                    device_limits: r.device.sane(),
                    level: lvl,
                    since_unix,
                    max_c,
                },
            );
        }
        // A component that is gone takes its trend with it.
        self.trend.retain(|id, _| next.contains_key(id));
        self.components = next;
        for f in &mut fans {
            f.problem = fan_problem(f);
        }
        let comps: Vec<Component> = self.components.values().cloned().collect();
        let (request, why) = cooling(&comps, &fans);
        let since_unix = match &self.cooling {
            Some(c) if c.request == request => c.since_unix,
            _ => now,
        };
        self.cooling = Some(Cooling { request, why, since_unix });
        self.fans = fans;
        self.at_unix = now;
        self.passes += 1;
        let line = self.history_due(now, boot_id, cfg.heartbeat_secs);
        (alerts, line)
    }

    /// What is due in the history: a component whose reading moved ≥ 1 °C,
    /// whose level changed, or that has not been written for a heartbeat;
    /// a fan whose RPM moved ≥ 10 %, or the same heartbeat.
    fn history_due(&mut self, now: u64, boot_id: &str, heartbeat: u64) -> Option<HistoryLine> {
        let mut components = Vec::new();
        for c in self.components.values() {
            let due = self.written.get(&c.id).map_or(true, |(t, l, at)| {
                *l != c.level
                    || now.saturating_sub(*at) >= heartbeat
                    || match (t, c.temp_c) {
                        (Some(a), Some(b)) => (a - b).abs() >= 1.0,
                        (a, b) => a.is_some() != b.is_some(),
                    }
            });
            if due {
                self.written.insert(c.id.clone(), (c.temp_c, c.level, now));
                components.push(HistoryPoint { id: c.id.clone(), class: c.class, temp_c: c.temp_c, level: c.level, warn_c: c.limits.warn_c, crit_c: c.limits.crit_c });
            }
        }
        let mut fans = Vec::new();
        for f in &self.fans {
            let due = self.fans_written.get(&f.id).map_or(true, |(r, at)| {
                now.saturating_sub(*at) >= heartbeat
                    || match (r, f.rpm) {
                        (Some(a), Some(b)) => a.abs_diff(b) * 10 >= (*a).max(1),
                        (a, b) => a.is_some() != b.is_some(),
                    }
            });
            if due {
                self.fans_written.insert(f.id.clone(), (f.rpm, now));
                fans.push(FanPoint { id: f.id.clone(), rpm: f.rpm });
            }
        }
        self.written.retain(|id, _| self.components.contains_key(id));
        let ids: std::collections::HashSet<&str> = self.fans.iter().map(|f| f.id.as_str()).collect();
        self.fans_written.retain(|id, _| ids.contains(id.as_str()));
        (!components.is_empty() || !fans.is_empty()).then(|| HistoryLine { unix: now, boot_id: boot_id.to_string(), components, fans })
    }

    /// A component's recent readings, oldest first.
    pub fn trend(&self, id: &str) -> Vec<(u64, f64)> {
        self.trend.get(id).map(|t| t.iter().copied().collect()).unwrap_or_default()
    }

    /// The whole picture, as `GET /api/v1/thermal` serves it.
    pub fn snapshot(&self) -> serde_json::Value {
        let comps: Vec<&Component> = self.components.values().collect();
        let mut by_level: BTreeMap<&str, usize> = BTreeMap::new();
        for c in &comps {
            *by_level.entry(c.level.as_str()).or_default() += 1;
        }
        let hottest = comps
            .iter()
            .filter(|c| c.temp_c.is_some())
            .min_by(|a, b| margin(a).total_cmp(&margin(b)))
            .map(|c| serde_json::json!({"id": c.id, "label": c.label, "temp_c": c.temp_c, "warn_c": c.limits.warn_c, "under_warning_c": margin(c)}));
        serde_json::json!({
            "at_unix": self.at_unix,
            "passes": self.passes,
            "cooling": self.cooling,
            "levels": by_level,
            "closest_to_warning": hottest,
            "components": comps,
            "fans": self.fans,
        })
    }
}

/// The class a hwmon chip's temperature belongs to, from the chip's name,
/// the sensor's label and what the chip hangs off. None: a drive's own chip
/// (read with its drive).
pub fn hwmon_class(chip: &str, label: &str, on_net_device: bool, on_scsi_host: bool) -> Option<Class> {
    let l = label.to_ascii_lowercase();
    if l.contains("inlet") || l.contains("ambient") || l.contains("intake") {
        return Some(Class::Inlet);
    }
    if l.contains("exhaust") || l.contains("outlet") {
        return Some(Class::Exhaust);
    }
    let c = chip.to_ascii_lowercase();
    Some(match c.as_str() {
        "nvme" | "drivetemp" => return None,
        "coretemp" | "k10temp" | "k8temp" | "zenpower" | "via_cputemp" | "cpu_thermal" | "fam15h_power" => Class::Cpu,
        "jc42" | "spd5118" | "i5k_amb" => Class::Dimm,
        "amdgpu" | "radeon" | "nouveau" | "i915" | "xe" => Class::Gpu,
        _ if c.starts_with("pch_") || c == "i5500_temp" || c == "intel_pch_thermal" => Class::Chipset,
        _ if on_net_device || matches!(c.as_str(), "ixgbe" | "igb" | "i40e" | "ice" | "bnxt_en" | "mlx5" | "tg3" | "qede" | "atlantic" | "sfc" | "cxgb4") => Class::Nic,
        _ if on_scsi_host || matches!(c.as_str(), "mpt3sas" | "megaraid_sas" | "smartpqi" | "aacraid") => Class::Hba,
        _ if c.contains("psu") || c.starts_with("pmbus") || c.starts_with("dps") => Class::Psu,
        _ if c == "acpitz" || c.starts_with("nct") || c.starts_with("it87") || c.starts_with("w83") || c.starts_with("f71") || c == "dell_smm" || c.starts_with("asus") => Class::Board,
        _ if l.contains("cpu") || l.contains("package") || l.starts_with("core ") || l.starts_with("tctl") || l.starts_with("tdie") => Class::Cpu,
        _ => Class::Other,
    })
}

/// A hwmon value in millidegrees (or RPM) as read from sysfs.
pub fn milli(s: &str) -> Option<f64> {
    s.trim().parse::<i64>().ok().map(|v| v as f64 / 1000.0)
}

/// One hwmon chip's sensors, read from its directory.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Chip {
    /// `hwmon3`.
    pub hwmon: String,
    /// `name`.
    pub name: String,
    /// A stable handle: the chip name and the device it hangs off
    /// (`coretemp.0`, `0000:3b:00.0`), so ids survive hwmonN renumbering.
    pub device: Option<String>,
    pub on_net_device: bool,
    pub on_scsi_host: bool,
    pub temps: Vec<TempSensor>,
    pub fans: Vec<FanSensor>,
}

/// `temp<N>_*` of a hwmon chip, in °C.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TempSensor {
    pub index: u32,
    pub label: Option<String>,
    pub input: Option<f64>,
    pub max: Option<f64>,
    pub crit: Option<f64>,
}

/// `fan<N>_*` of a hwmon chip.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FanSensor {
    pub index: u32,
    pub label: Option<String>,
    pub rpm: Option<u64>,
    pub min: Option<u64>,
    /// `_alarm` or `_fault` set.
    pub flagged: bool,
}

impl Chip {
    pub fn handle(&self) -> String {
        match &self.device {
            Some(d) => format!("{}.{}", self.name, d),
            None => format!("{}.{}", self.name, self.hwmon),
        }
    }
}

/// Read every chip under `<sys>/class/hwmon`.
pub fn read_hwmon(sys: &std::path::Path) -> Vec<Chip> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(sys.join("class/hwmon")) else { return out };
    let mut dirs: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    dirs.sort();
    for dir in dirs {
        if let Some(c) = read_chip(&dir) {
            out.push(c);
        }
    }
    out
}

fn read_trim(p: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// One chip's sensors from its hwmon directory.
pub fn read_chip(dir: &std::path::Path) -> Option<Chip> {
    let name = read_trim(&dir.join("name"))?;
    let hwmon = dir.file_name()?.to_string_lossy().to_string();
    let dev = std::fs::canonicalize(dir.join("device")).ok();
    let device = dev.as_ref().and_then(|d| d.file_name()).map(|f| f.to_string_lossy().to_string());
    let on_net_device = dev.as_ref().is_some_and(|d| d.join("net").is_dir());
    let on_scsi_host = dev.as_ref().is_some_and(|d| {
        std::fs::read_dir(d).map(|r| r.flatten().any(|e| e.file_name().to_string_lossy().starts_with("host"))).unwrap_or(false)
    });
    let mut chip = Chip { hwmon, name, device, on_net_device, on_scsi_host, ..Default::default() };
    let mut files: Vec<String> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
    files.sort();
    let get = |f: String| read_trim(&dir.join(f));
    for f in &files {
        if let Some(n) = f.strip_prefix("temp").and_then(|r| r.strip_suffix("_input")).and_then(|n| n.parse::<u32>().ok()) {
            let t = get(format!("temp{n}_input")).and_then(|v| milli(&v));
            let max = get(format!("temp{n}_max")).and_then(|v| milli(&v));
            let crit = get(format!("temp{n}_crit")).and_then(|v| milli(&v));
            chip.temps.push(TempSensor { index: n, label: get(format!("temp{n}_label")), input: t, max, crit });
        }
        if let Some(n) = f.strip_prefix("fan").and_then(|r| r.strip_suffix("_input")).and_then(|n| n.parse::<u32>().ok()) {
            let rpm = get(format!("fan{n}_input")).and_then(|v| v.parse::<u64>().ok());
            let min = get(format!("fan{n}_min")).and_then(|v| v.parse::<u64>().ok());
            let flag = |k: &str| get(format!("fan{n}_{k}")).is_some_and(|v| v != "0");
            chip.fans.push(FanSensor { index: n, label: get(format!("fan{n}_label")), rpm, min, flagged: flag("alarm") || flag("fault") });
        }
    }
    chip.temps.sort_by_key(|t| t.index);
    chip.fans.sort_by_key(|f| f.index);
    Some(chip)
}

/// The readings and fans of the non-drive hwmon chips.
pub fn hwmon_readings(chips: &[Chip]) -> (Vec<Reading>, Vec<Fan>) {
    let (mut temps, mut fans) = (Vec::new(), Vec::new());
    for chip in chips {
        let handle = chip.handle();
        for s in &chip.temps {
            let n = s.index;
            let l = s.label.clone().unwrap_or_else(|| format!("temp{n}"));
            let Some(class) = hwmon_class(&chip.name, &l, chip.on_net_device, chip.on_scsi_host) else { continue };
            let where_ = chip.device.as_deref().map(|d| format!(" ({d})")).unwrap_or_default();
            temps.push(Reading {
                id: format!("hwmon:{handle}:temp{n}"),
                class,
                label: format!("{} {l}{where_}", chip.name),
                source: format!("hwmon:{}", chip.name),
                drive_id: None,
                shelf: None,
                temp_c: s.input,
                device: DeviceLimits { warn_c: s.max, crit_c: s.crit },
            });
        }
        if hwmon_class(&chip.name, "", false, false).is_none() {
            continue;
        }
        for f in &chip.fans {
            let n = f.index;
            let l = f.label.clone().unwrap_or_else(|| format!("fan{n}"));
            fans.push(Fan {
                id: format!("hwmon:{handle}:fan{n}"),
                label: format!("{} {l}", chip.name),
                source: format!("hwmon:{}", chip.name),
                shelf: None,
                rpm: f.rpm,
                // Many chips report 0 as "no minimum set".
                min_rpm: f.min.filter(|m| *m > 0),
                failed: f.flagged,
                problem: None,
            });
        }
    }
    (temps, fans)
}

/// A drive's own hwmon chip (NVMe: the controller's, with WCTEMP/CCTEMP as
/// `temp1_max`/`temp1_crit`; drivetemp: the drive's SCT limits), looked for
/// under the block device, its controller and one level of nesting.
pub fn drive_hwmon(sys: &std::path::Path, name: &str) -> Option<(Option<f64>, DeviceLimits)> {
    let base = sys.join("block").join(name).join("device");
    let mut dirs = vec![base.clone(), base.join("device")];
    let mut chips = Vec::new();
    while let Some(d) = dirs.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n == "hwmon" {
                dirs.push(e.path());
            } else if n.starts_with("hwmon") {
                chips.push(e.path());
            }
        }
    }
    chips.sort();
    for c in chips {
        let Some(chip) = read_chip(&c) else { continue };
        // temp1 is the composite / the drive's own sensor.
        if let Some(s) = chip.temps.iter().find(|t| t.index == 1).or(chip.temps.first()) {
            if s.input.is_some() {
                return Some((s.input, DeviceLimits { warn_c: s.max, crit_c: s.crit }.sane()));
            }
        }
    }
    None
}

/// A shelf's SES temperature sensors and fans.
pub fn shelf_readings(r: &crate::ses::ShelfReport) -> (Vec<Reading>, Vec<Fan>) {
    use crate::ses::ElementStatus;
    let (mut temps, mut fans) = (Vec::new(), Vec::new());
    if r.through.is_some() {
        return (temps, fans);
    }
    let shelf_name = r.shelf.model.clone().unwrap_or_else(|| "shelf".into());
    for e in r.elements.iter().filter(|e| !e.overall) {
        let named = e.name.as_deref().map(|n| format!(" ({n})")).unwrap_or_default();
        match e.element_type {
            0x04 => {
                if matches!(e.status, ElementStatus::NotInstalled | ElementStatus::Unsupported) {
                    continue;
                }
                let th = e.thresholds.as_ref();
                temps.push(Reading {
                    id: format!("shelf:{}:temp{}", r.key, e.index),
                    class: Class::Shelf,
                    label: format!("{shelf_name} {} temperature {}{named}", r.key, e.index),
                    source: "ses".into(),
                    drive_id: None,
                    shelf: Some(r.key.clone()),
                    temp_c: e.temperature_c.map(f64::from),
                    device: DeviceLimits { warn_c: th.and_then(|t| t.high_warning).map(f64::from), crit_c: th.and_then(|t| t.high_critical).map(f64::from) },
                });
            }
            0x03 => {
                if matches!(e.status, ElementStatus::NotInstalled | ElementStatus::Unsupported) {
                    continue;
                }
                fans.push(Fan {
                    id: format!("shelf:{}:fan{}", r.key, e.index),
                    label: format!("{shelf_name} {} fan {}{named}", r.key, e.index),
                    source: "ses".into(),
                    shelf: Some(r.key.clone()),
                    rpm: e.rpm.map(u64::from),
                    min_rpm: None,
                    failed: e.fault || matches!(e.status, ElementStatus::Critical | ElementStatus::Unrecoverable),
                    problem: None,
                });
            }
            _ => {}
        }
    }
    (temps, fans)
}

/// A drive's temperature reading for the pass: its own hwmon when read
/// fresh, else the health sampler's latest with the limits it found.
pub fn drive_reading(d: &crate::drive::Drive, fresh: Option<(Option<f64>, DeviceLimits)>) -> Reading {
    let lim = d.health.temp_limits.clone().unwrap_or_default();
    let sampled = DeviceLimits { warn_c: lim.warn_c.map(f64::from), crit_c: lim.crit_c.map(f64::from) };
    let (temp_c, device, source) = match fresh {
        Some((Some(t), dl)) => {
            let dl = if dl.warn_c.is_none() && dl.crit_c.is_none() { sampled } else { dl };
            (Some(t), dl, "hwmon")
        }
        _ => (d.health.temperature_c.map(f64::from), sampled, "smart"),
    };
    Reading {
        id: format!("drive:{}", d.id.0),
        class: Class::Drive,
        label: format!("{} ({} {})", d.name, d.model, d.serial),
        source: source.into(),
        drive_id: Some(d.id),
        shelf: d.location.shelf.as_ref().and_then(|s| s.key()),
        temp_c,
        device,
    }
}

/// The thermal state of the running daemon: the tracker and the live
/// settings, kept in `<data_dir>/thermal-limits.json`.
#[derive(Debug, Default)]
pub struct Runtime {
    pub tracker: Tracker,
    pub settings: Settings,
    pub settings_path: Option<std::path::PathBuf>,
}

impl Runtime {
    /// The settings saved by a previous run; a file that cannot be read is
    /// set aside and said (#102), never fatal.
    pub fn load(data_dir: Option<&str>) -> (Self, Option<String>) {
        let path = data_dir.map(|d| std::path::PathBuf::from(d).join("thermal-limits.json"));
        let (saved, note) = path.as_deref().map_or((None, None), crate::inventory::load_state::<Settings>);
        let settings = saved.filter(|s| s.validate().is_ok()).unwrap_or_default();
        (Self { tracker: Tracker::default(), settings, settings_path: path }, note)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        if let Some(p) = &self.settings_path {
            crate::inventory::write_atomic(p, &serde_json::to_vec_pretty(&self.settings)?)?;
        }
        Ok(())
    }
}

/// `[thermal.classes]` under the live settings: a live class setting
/// replaces the configured one.
pub fn effective_settings(config: &BTreeMap<String, LimitSetting>, live: &Settings) -> Settings {
    let mut s = Settings { classes: config.clone(), components: live.components.clone() };
    for (k, v) in &live.classes {
        s.classes.insert(k.clone(), *v);
    }
    s
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// The thermal loop: one pass every `thermal.interval_secs`.
pub async fn run(state: std::sync::Arc<crate::api::AppState>) {
    let cfg = state.config.thermal.clone();
    if !cfg.enabled {
        tracing::info!("thermal: off ([thermal] enabled = false)");
        return;
    }
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(cfg.interval_secs.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(e) = pass(&state).await {
            tracing::warn!("thermal pass failed: {e:#}");
        }
    }
}

/// How long one pass's sysfs reads may take.
const READ_LIMIT: std::time::Duration = std::time::Duration::from_secs(20);

/// One pass: read, judge, tell, act, keep.
pub async fn pass(state: &std::sync::Arc<crate::api::AppState>) -> anyhow::Result<()> {
    use crate::events::Severity;
    let tcfg = &state.config.thermal;
    let pcfg = PassCfg {
        near_c: tcfg.near_c,
        hysteresis_c: tcfg.hysteresis_c,
        replace_after_secs: tcfg.replace_after_mins * 60,
        heartbeat_secs: tcfg.heartbeat_secs,
        trend_len: tcfg.trend_samples,
        drive_default: (f64::from(state.config.monitor.temp_warn_c), f64::from(state.config.monitor.temp_crit_c)),
    };
    let drives: Vec<crate::drive::Drive> = {
        let inv = state.inventory.read().await;
        inv.drives.values().filter(|d| d.activity != crate::drive::Activity::Missing).cloned().collect()
    };
    let names: Vec<String> = drives.iter().map(|d| d.name.clone()).collect();
    let read = tokio::task::spawn_blocking(move || {
        let sys = std::path::Path::new("/sys");
        let chips = read_hwmon(sys);
        let fresh: HashMap<String, (Option<f64>, DeviceLimits)> = names.iter().filter_map(|n| drive_hwmon(sys, n).map(|h| (n.clone(), h))).collect();
        (chips, fresh)
    });
    let (chips, fresh) = tokio::time::timeout(READ_LIMIT, read).await.map_err(|_| anyhow::anyhow!("sysfs reads took over {} s", READ_LIMIT.as_secs()))??;
    let (mut readings, mut fans) = hwmon_readings(&chips);
    for d in &drives {
        readings.push(drive_reading(d, fresh.get(&d.name).copied()));
    }
    for r in state.shelves.read().await.values() {
        let (t, f) = shelf_readings(r);
        readings.extend(t);
        fans.extend(f);
    }
    let now = unix_now();
    let boot = state.history.boot.boot_id.clone();
    let (alerts, line, cooling_change, drive_states) = {
        let mut rt = state.thermal.write().await;
        let settings = effective_settings(&tcfg.classes, &rt.settings);
        let was = rt.tracker.cooling.clone();
        let (alerts, line) = rt.tracker.pass(readings, fans, &settings, &pcfg, now, &boot);
        let is = rt.tracker.cooling.clone();
        let change = match (&was, &is) {
            (Some(a), Some(b)) if a.request == b.request => None,
            (None, Some(b)) if b.request == CoolingLevel::Auto => None,
            (_, b) => b.clone(),
        };
        let drive_states: Vec<(DriveId, DriveThermal)> = rt
            .tracker
            .components
            .values()
            .filter_map(|c| c.drive_id.map(|id| (id, DriveThermal::from_component(c, now, pcfg.replace_after_secs))))
            .collect();
        (alerts, line, change, drive_states)
    };
    let mut proposals = Vec::new();
    {
        let mut inv = state.inventory.write().await;
        for (id, t) in drive_states {
            if let Some(d) = inv.drives.get_mut(&id) {
                if t.replace_proposed && !d.thermal.as_ref().is_some_and(|o| o.replace_proposed) {
                    proposals.push((id, format!(
                        "{} ({} {}): over its critical limit {:.0} °C for {} min ({}) — replace it, or find why it runs hot",
                        d.name, d.model, d.serial, t.limits.crit_c, now.saturating_sub(t.since_unix) / 60,
                        t.temp_c.map_or("no reading".into(), |v| format!("{v:.0} °C now")),
                    )));
                }
                d.thermal = Some(t);
            }
        }
    }
    {
        let mut log = state.events.write().await;
        for a in &alerts {
            let msg = a.message();
            match a.severity() {
                Severity::Error => tracing::error!(component = %a.id, "thermal: {msg}"),
                Severity::Warning => tracing::warn!(component = %a.id, "thermal: {msg}"),
                Severity::Info => tracing::info!(component = %a.id, "thermal: {msg}"),
            }
            log.push(a.drive_id, a.severity(), "thermal", msg);
        }
        for (id, msg) in proposals {
            tracing::error!("thermal: {msg}");
            log.push(Some(id), Severity::Error, "thermal", msg);
        }
        if let Some(c) = cooling_change {
            let sev = match c.request {
                CoolingLevel::Auto => Severity::Info,
                CoolingLevel::Raise => Severity::Warning,
                CoolingLevel::Max => Severity::Error,
            };
            let why = if c.why.is_empty() { "nothing is near a limit".to_string() } else { c.why.join("; ") };
            let word = serde_json::to_value(c.request).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
            log.push(None, sev, "cooling", format!("cooling request {word}: {why}"));
        }
    }
    if let Some(line) = line {
        let h = state.history.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || h.append_thermal(&line)).await? {
            tracing::warn!("thermal history: {e:#}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PassCfg {
        PassCfg { near_c: 5.0, hysteresis_c: 2.0, replace_after_secs: 1800, heartbeat_secs: 600, trend_len: 4, drive_default: (55.0, 70.0) }
    }

    fn reading(id: &str, class: Class, t: f64, device: DeviceLimits) -> Reading {
        Reading { id: id.into(), class, label: id.into(), source: "test".into(), drive_id: None, shelf: None, temp_c: Some(t), device }
    }

    #[test]
    fn the_devices_own_limits_come_first_and_a_setting_only_lowers_them() {
        let none = DeviceLimits::default();
        // Nothing from the device or a setting: the class default.
        let l = resolve(none, None, (85.0, 95.0));
        assert_eq!((l.warn_c, l.crit_c, l.warn_source, l.crit_source), (85.0, 95.0, LimitSource::Class, LimitSource::Class));
        // NVMe WCTEMP/CCTEMP.
        let nvme = DeviceLimits { warn_c: Some(75.0), crit_c: Some(80.0) };
        let l = resolve(nvme, None, (55.0, 70.0));
        assert_eq!((l.warn_c, l.crit_c, l.warn_source), (75.0, 80.0, LimitSource::Device));
        // A setting above the device's own is ignored; below it, it applies.
        let up = LimitSetting { warn_c: Some(78.0), crit_c: Some(90.0) };
        let l = resolve(nvme, Some(up), (55.0, 70.0));
        assert_eq!((l.warn_c, l.crit_c), (75.0, 80.0));
        let down = LimitSetting { warn_c: Some(65.0), crit_c: None };
        let l = resolve(nvme, Some(down), (55.0, 70.0));
        assert_eq!((l.warn_c, l.warn_source, l.crit_c), (65.0, LimitSource::Setting, 80.0));
        // A setting fills what the device does not give.
        let l = resolve(none, Some(LimitSetting { warn_c: Some(40.0), crit_c: None }), (55.0, 70.0));
        assert_eq!((l.warn_c, l.warn_source, l.crit_c, l.crit_source), (40.0, LimitSource::Setting, 70.0, LimitSource::Class));
        // Only a critical from the device (a SAS reference temperature, a
        // CPU's TjMax): warn 5 °C under it when the default is above that.
        let l = resolve(DeviceLimits { warn_c: None, crit_c: Some(60.0) }, None, (55.0, 70.0));
        assert_eq!((l.warn_c, l.crit_c), (55.0, 60.0));
        let l = resolve(DeviceLimits { warn_c: None, crit_c: Some(60.0) }, None, (58.0, 70.0));
        assert_eq!((l.warn_c, l.warn_source), (55.0, LimitSource::Device));
        let l = resolve(DeviceLimits { warn_c: None, crit_c: Some(100.0) }, None, (85.0, 95.0));
        assert_eq!((l.warn_c, l.warn_source, l.crit_c), (85.0, LimitSource::Class, 100.0));
        // Placeholder limits are no limits.
        let junk = DeviceLimits { warn_c: Some(0.0), crit_c: Some(255.0) };
        assert_eq!(resolve(junk, None, (85.0, 95.0)).crit_source, LimitSource::Class);
        // A warning at or over the critical: put under it.
        let l = resolve(DeviceLimits { warn_c: Some(90.0), crit_c: Some(80.0) }, None, (85.0, 95.0));
        assert!(l.warn_c < l.crit_c && l.crit_c == 80.0);
    }

    #[test]
    fn levels_enter_at_the_limit_and_leave_below_the_hysteresis() {
        let l = resolve(DeviceLimits::default(), None, (55.0, 70.0));
        let lv = |p, t| level(p, Some(t), &l, 5.0, 2.0);
        assert_eq!(lv(Level::Unknown, 40.0), Level::Ok);
        assert_eq!(lv(Level::Ok, 50.0), Level::Near);
        assert_eq!(lv(Level::Near, 55.0), Level::Warning);
        assert_eq!(lv(Level::Warning, 54.0), Level::Warning, "held 2 °C under");
        assert_eq!(lv(Level::Warning, 53.0), Level::Near);
        assert_eq!(lv(Level::Warning, 70.0), Level::Critical);
        assert_eq!(lv(Level::Critical, 68.5), Level::Critical);
        assert_eq!(lv(Level::Critical, 68.0), Level::Warning);
        assert_eq!(lv(Level::Near, 48.5), Level::Near);
        assert_eq!(lv(Level::Near, 48.0), Level::Ok);
        assert_eq!(level(Level::Warning, None, &l, 5.0, 2.0), Level::Unknown);
    }

    #[test]
    fn a_pass_alerts_on_warning_and_critical_and_when_it_is_over() {
        let mut tr = Tracker::default();
        let set = Settings::default();
        let pass = |tr: &mut Tracker, t: f64, now: u64| {
            tr.pass(vec![reading("drive:x", Class::Drive, t, DeviceLimits::default())], vec![], &set, &cfg(), now, "b").0
        };
        assert!(pass(&mut tr, 40.0, 0).is_empty());
        assert!(pass(&mut tr, 51.0, 30).is_empty(), "near is no alert");
        assert_eq!(tr.cooling.as_ref().unwrap().request, CoolingLevel::Raise);
        let a = pass(&mut tr, 56.0, 60);
        assert_eq!((a.len(), a[0].to, a[0].severity()), (1, Level::Warning, crate::events::Severity::Warning));
        assert!(a[0].message().contains("over its warning limit 55 °C"), "{}", a[0].message());
        let a = pass(&mut tr, 71.0, 90);
        assert_eq!((a[0].to, a[0].severity()), (Level::Critical, crate::events::Severity::Error));
        assert_eq!(tr.cooling.as_ref().unwrap().request, CoolingLevel::Max);
        assert!(pass(&mut tr, 69.0, 120).is_empty(), "held by hysteresis");
        let a = pass(&mut tr, 60.0, 150);
        assert_eq!((a[0].from, a[0].to), (Level::Critical, Level::Warning));
        assert!(a[0].message().contains("back under critical"));
        let a = pass(&mut tr, 45.0, 180);
        assert_eq!((a[0].to, a[0].severity()), (Level::Ok, crate::events::Severity::Info));
        assert_eq!(tr.cooling.as_ref().unwrap().request, CoolingLevel::Auto);
        assert_eq!(tr.trend("drive:x").len(), 4, "trend capped");
    }

    #[test]
    fn a_hot_drive_slows_then_leaves_placement_then_is_proposed_for_replacement() {
        let mut tr = Tracker::default();
        let set = Settings::default();
        let mut at = |t: f64, now: u64| {
            tr.pass(vec![reading("drive:x", Class::Drive, t, DeviceLimits::default())], vec![], &set, &cfg(), now, "b");
            DriveThermal::from_component(&tr.components["drive:x"], now, cfg().replace_after_secs)
        };
        let d = at(50.0, 0);
        assert!(!d.throttle_background && !d.exclude_new_placement);
        let d = at(60.0, 30);
        assert!(d.throttle_background && !d.exclude_new_placement && !d.replace_proposed);
        let d = at(72.0, 60);
        assert!(d.throttle_background && d.exclude_new_placement && !d.replace_proposed);
        let d = at(72.0, 60 + 1799);
        assert!(!d.replace_proposed);
        let d = at(73.0, 60 + 1800);
        assert!(d.replace_proposed, "critical for 30 min");
        assert_eq!(d.since_unix, 60);
        let d = at(40.0, 4000);
        assert!(!d.throttle_background && !d.replace_proposed);
    }

    #[test]
    fn a_stopped_fan_raises_the_cooling_request() {
        let mut tr = Tracker::default();
        let fan = |rpm, failed| Fan { id: "f".into(), label: "fan 1".into(), source: "ses".into(), shelf: None, rpm: Some(rpm), min_rpm: Some(600), failed, problem: None };
        tr.pass(vec![], vec![fan(4000, false)], &Settings::default(), &cfg(), 0, "b");
        assert_eq!(tr.cooling.as_ref().unwrap().request, CoolingLevel::Auto);
        tr.pass(vec![], vec![fan(0, false)], &Settings::default(), &cfg(), 30, "b");
        let c = tr.cooling.clone().unwrap();
        assert_eq!((c.request, c.why[0].as_str()), (CoolingLevel::Raise, "fan 1 stopped (minimum 600 RPM)"));
        tr.pass(vec![], vec![fan(4000, true)], &Settings::default(), &cfg(), 60, "b");
        assert_eq!(tr.fans[0].problem.as_deref(), Some("fan 1 reports a fault"));
    }

    #[test]
    fn settings_by_class_and_component() {
        let mut set = Settings::default();
        set.classes.insert("cpu".into(), LimitSetting { warn_c: Some(70.0), crit_c: None });
        set.components.insert("hwmon:k10temp.0000:00:18.3:temp1".into(), LimitSetting { warn_c: None, crit_c: Some(80.0) });
        set.validate().unwrap();
        let mut tr = Tracker::default();
        tr.pass(
            vec![reading("hwmon:k10temp.0000:00:18.3:temp1", Class::Cpu, 40.0, DeviceLimits::default()), reading("hwmon:k10temp.x:temp1", Class::Cpu, 40.0, DeviceLimits::default())],
            vec![],
            &set,
            &cfg(),
            0,
            "b",
        );
        let a = &tr.components["hwmon:k10temp.0000:00:18.3:temp1"].limits;
        assert_eq!((a.warn_c, a.crit_c), (70.0, 80.0), "own critical, class warning");
        let b = &tr.components["hwmon:k10temp.x:temp1"].limits;
        assert_eq!((b.warn_c, b.crit_c, b.crit_source), (70.0, 95.0, LimitSource::Class));
        let bad: Settings = serde_json::from_str(r#"{"classes": {"toaster": {"warn_c": 1}}}"#).unwrap();
        assert!(bad.validate().unwrap_err().contains("toaster"));
        let bad: Settings = serde_json::from_str(r#"{"classes": {"cpu": {"warn_c": 90, "crit_c": 80}}}"#).unwrap();
        assert!(bad.validate().is_err());
        assert!(serde_json::from_str::<Settings>(r#"{"class": {}}"#).is_err(), "a typo is refused");
    }

    #[test]
    fn history_lines_on_change_or_heartbeat() {
        let mut tr = Tracker::default();
        let set = Settings::default();
        let mut p = |t: f64, now: u64| tr.pass(vec![reading("a", Class::Cpu, t, DeviceLimits::default())], vec![], &set, &cfg(), now, "b").1;
        assert_eq!(p(40.0, 0).unwrap().components.len(), 1, "first is written");
        assert!(p(40.5, 30).is_none(), "under 1 °C");
        assert_eq!(p(41.0, 60).unwrap().components[0].temp_c, Some(41.0));
        assert!(p(41.0, 600).is_none());
        assert!(p(41.0, 660).is_some(), "heartbeat");
    }

    #[test]
    fn hwmon_chips_are_classed() {
        assert_eq!(hwmon_class("coretemp", "Package id 0", false, false), Some(Class::Cpu));
        assert_eq!(hwmon_class("k10temp", "Tctl", false, false), Some(Class::Cpu));
        assert_eq!(hwmon_class("jc42", "temp1", false, false), Some(Class::Dimm));
        assert_eq!(hwmon_class("nvme", "Composite", false, false), None, "with its drive");
        assert_eq!(hwmon_class("drivetemp", "temp1", false, false), None);
        assert_eq!(hwmon_class("mlx5", "temp1", true, false), Some(Class::Nic));
        assert_eq!(hwmon_class("whatever", "temp1", true, false), Some(Class::Nic));
        assert_eq!(hwmon_class("whatever", "temp1", false, true), Some(Class::Hba));
        assert_eq!(hwmon_class("pch_cannonlake", "temp1", false, false), Some(Class::Chipset));
        assert_eq!(hwmon_class("nct6779", "SYSTIN", false, false), Some(Class::Board));
        assert_eq!(hwmon_class("nct6779", "Inlet Temp", false, false), Some(Class::Inlet));
        assert_eq!(hwmon_class("acpitz", "temp1", false, false), Some(Class::Board));
        assert_eq!(hwmon_class("mystery", "temp1", false, false), Some(Class::Other));
    }

    /// A sysfs tree with a CPU, a DIMM, a NIC, a fan chip and an NVMe drive.
    #[test]
    fn hwmon_is_read_from_sysfs() {
        let root = std::env::temp_dir().join(format!("stormdrive-thermal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let w = |p: &str, v: &str| {
            let p = root.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, v).unwrap();
        };
        let link = |target: &str, at: &str| {
            let at = root.join(at);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(root.join(target), at).unwrap();
        };
        // CPU
        w("devices/platform/coretemp.0/x", "");
        w("class/hwmon/hwmon0/name", "coretemp\n");
        w("class/hwmon/hwmon0/temp1_input", "45000\n");
        w("class/hwmon/hwmon0/temp1_label", "Package id 0\n");
        w("class/hwmon/hwmon0/temp1_max", "84000\n");
        w("class/hwmon/hwmon0/temp1_crit", "100000\n");
        link("devices/platform/coretemp.0", "class/hwmon/hwmon0/device");
        // NIC
        w("devices/pci0000:00/0000:3b:00.0/net/eth0/x", "");
        w("class/hwmon/hwmon1/name", "mlx5\n");
        w("class/hwmon/hwmon1/temp1_input", "61000\n");
        link("devices/pci0000:00/0000:3b:00.0", "class/hwmon/hwmon1/device");
        // Board fans
        w("class/hwmon/hwmon2/name", "nct6779\n");
        w("class/hwmon/hwmon2/fan1_input", "1200\n");
        w("class/hwmon/hwmon2/fan1_min", "300\n");
        w("class/hwmon/hwmon2/fan2_input", "0\n");
        w("class/hwmon/hwmon2/fan2_min", "300\n");
        w("class/hwmon/hwmon2/fan2_alarm", "1\n");
        // NVMe: controller nvme0 with its hwmon, namespace nvme0n1 → nvme0
        w("devices/pci0000:00/0000:5e:00.0/nvme/nvme0/hwmon3/name", "nvme\n");
        w("devices/pci0000:00/0000:5e:00.0/nvme/nvme0/hwmon3/temp1_input", "38850\n");
        w("devices/pci0000:00/0000:5e:00.0/nvme/nvme0/hwmon3/temp1_max", "75850\n");
        w("devices/pci0000:00/0000:5e:00.0/nvme/nvme0/hwmon3/temp1_crit", "79850\n");
        link("devices/pci0000:00/0000:5e:00.0/nvme/nvme0", "block/nvme0n1/device");
        link("devices/pci0000:00/0000:5e:00.0/nvme/nvme0/hwmon3", "class/hwmon/hwmon3");

        let chips = read_hwmon(&root);
        assert_eq!(chips.len(), 4);
        let (temps, fans) = hwmon_readings(&chips);
        assert_eq!(temps.len(), 2, "the NVMe chip goes with its drive: {temps:?}");
        let cpu = temps.iter().find(|r| r.class == Class::Cpu).unwrap();
        assert_eq!(cpu.id, "hwmon:coretemp.coretemp.0:temp1");
        assert_eq!((cpu.temp_c, cpu.device.warn_c, cpu.device.crit_c), (Some(45.0), Some(84.0), Some(100.0)));
        assert!(temps.iter().any(|r| r.class == Class::Nic && r.temp_c == Some(61.0)));
        assert_eq!(fans.len(), 2);
        assert!(fans[1].failed);
        let (t, l) = drive_hwmon(&root, "nvme0n1").unwrap();
        assert_eq!((t, l.warn_c, l.crit_c), (Some(38.85), Some(75.85), Some(79.85)));
        assert!(drive_hwmon(&root, "sda").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
