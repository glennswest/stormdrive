//! The monitor loop: rescan, collect, evaluate, transition, persist.
//!
//! `evaluate` is a pure function of (config, sample, previous media errors)
//! so the threshold policy is unit-testable without hardware. Worsening
//! transitions are hysteresis-guarded — one bad poll never flips a drive.

use crate::config::MonitorConfig;
use crate::drive::{Activity, DriveId, DriveKind, HealthReport, HealthStatus, Membership};
use crate::events::Severity;
use crate::inventory::TrendSample;
use crate::smart::{self, crit, Sample};
use crate::{api::AppState, discovery, topology};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Turn one sample into a health verdict plus the reasons for it.
pub fn evaluate(
    cfg: &MonitorConfig,
    s: &Sample,
    prev_media_errors: Option<u64>,
) -> (HealthStatus, Vec<String>) {
    fn worsen(st: &mut HealthStatus, why: &mut Vec<String>, to: HealthStatus, msg: String) {
        if to > *st {
            *st = to;
        }
        if !why.contains(&msg) {
            why.push(msg);
        }
    }

    let mut status = HealthStatus::Good;
    let mut why: Vec<String> = s.messages.clone();

    if !s.kernel_ok {
        worsen(
            &mut status,
            &mut why,
            HealthStatus::Failed,
            "device unusable (kernel state / command failure)".into(),
        );
        return (status, why);
    }
    if s.critical_warning & crit::READ_ONLY != 0 {
        worsen(&mut status, &mut why, HealthStatus::Failed, "NVMe: media in read-only mode".into());
    }
    if s.critical_warning & crit::RELIABILITY_DEGRADED != 0 {
        worsen(&mut status, &mut why, HealthStatus::Failing, "NVMe: reliability degraded".into());
    }
    if s.critical_warning & crit::SPARE_BELOW_THRESHOLD != 0 {
        worsen(&mut status, &mut why, HealthStatus::Failing, "NVMe: spare below threshold".into());
    }
    if s.critical_warning & crit::VOLATILE_BACKUP_FAILED != 0 {
        worsen(&mut status, &mut why, HealthStatus::Warning, "NVMe: volatile backup failed".into());
    }
    if s.critical_warning & crit::TEMPERATURE != 0 {
        worsen(&mut status, &mut why, HealthStatus::Warning, "NVMe: temperature over threshold".into());
    }
    if let Some(spare) = s.available_spare_pct {
        if spare <= cfg.spare_crit_pct {
            worsen(&mut status, &mut why, HealthStatus::Failing, format!("available spare {spare}% ≤ {}%", cfg.spare_crit_pct));
        } else if spare <= cfg.spare_warn_pct {
            worsen(&mut status, &mut why, HealthStatus::Warning, format!("available spare {spare}% ≤ {}%", cfg.spare_warn_pct));
        }
    }
    if let Some(wear) = s.wear_pct {
        if wear >= cfg.wear_crit_pct {
            worsen(&mut status, &mut why, HealthStatus::Failing, format!("wear {wear}% ≥ {}%", cfg.wear_crit_pct));
        } else if wear >= cfg.wear_warn_pct {
            worsen(&mut status, &mut why, HealthStatus::Warning, format!("wear {wear}% ≥ {}%", cfg.wear_warn_pct));
        }
    }
    if let Some(t) = s.temperature_c {
        if t >= cfg.temp_crit_c {
            worsen(&mut status, &mut why, HealthStatus::Warning, format!("temperature {t}°C ≥ critical {}°C", cfg.temp_crit_c));
        } else if t >= cfg.temp_warn_c {
            worsen(&mut status, &mut why, HealthStatus::Warning, format!("temperature {t}°C ≥ warn {}°C", cfg.temp_warn_c));
        }
    }
    if let Some(prev) = prev_media_errors {
        if s.media_errors > prev {
            worsen(
                &mut status,
                &mut why,
                HealthStatus::Warning,
                format!("media errors growing: {prev} → {}", s.media_errors),
            );
        }
    }
    (status, why)
}

/// Hysteresis state per drive: a candidate worse status must repeat
/// `cfg.hysteresis` consecutive samples before it sticks. Improvement is
/// immediate.
#[derive(Default)]
pub struct Damper {
    pending: HashMap<DriveId, (HealthStatus, u32)>,
}

impl Damper {
    pub fn apply(
        &mut self,
        cfg: &MonitorConfig,
        id: DriveId,
        current: HealthStatus,
        candidate: HealthStatus,
    ) -> HealthStatus {
        if candidate <= current {
            self.pending.remove(&id);
            return candidate;
        }
        let entry = self.pending.entry(id).or_insert((candidate, 0));
        if entry.0 != candidate {
            *entry = (candidate, 0);
        }
        entry.1 += 1;
        if entry.1 >= cfg.hysteresis {
            self.pending.remove(&id);
            candidate
        } else {
            current
        }
    }
}

pub async fn run(state: Arc<AppState>) {
    let mut damper = Damper::default();
    let mut fleet = crate::fleet::FleetState::default();
    let disc_int = state.config.discovery.interval_secs;
    let mon_int = state.config.monitor.interval_secs;
    let mut last_disc: Option<std::time::Instant> = None;
    let mut last_mon: Option<std::time::Instant> = None;
    loop {
        let disc_due = last_disc.map_or(true, |t| t.elapsed().as_secs() >= disc_int);
        let mon_due = last_mon.map_or(true, |t| t.elapsed().as_secs() >= mon_int);
        if disc_due {
            last_disc = Some(std::time::Instant::now());
        }
        if mon_due {
            last_mon = Some(std::time::Instant::now());
        }
        if let Err(e) = tick(&state, &mut damper, disc_due, mon_due).await {
            tracing::error!("monitor tick failed: {e:#}");
        }
        // The stormblock loop: labels, health, drains, auto-add. After the
        // tick so it acts on this round's conclusions.
        if disc_due || mon_due {
            crate::fleet::tick(&state, &mut fleet).await;
            state.persist().await;
        }
        tokio::time::sleep(Duration::from_secs(disc_int.min(mon_int).max(1))).await;
    }
}

async fn tick(
    state: &Arc<AppState>,
    damper: &mut Damper,
    discover: bool,
    collect: bool,
) -> anyhow::Result<()> {
    if discover {
        let cfg = state.config.discovery.clone();
        let observed = tokio::task::spawn_blocking(move || discovery::scan(&cfg)).await?;
        merge_observed(state, observed).await;
    }

    if !collect {
        state.persist().await;
        return Ok(());
    }

    // Collect health for every present drive.
    let targets: Vec<(DriveId, crate::drive::Drive)> = {
        let inv = state.inventory.read().await;
        inv.drives
            .iter()
            .filter(|(_, d)| d.activity != Activity::Missing)
            .map(|(id, d)| (*id, d.clone()))
            .collect()
    };
    for (id, drive) in targets {
        let d2 = drive.clone();
        let sample = tokio::task::spawn_blocking(move || smart::collect(&d2)).await?;
        // Baseline for growth detection: only a real prior sample counts —
        // a fresh drive's default 0 would turn its first reading into a
        // false "growing" warning.
        let prev = {
            let inv = state.inventory.read().await;
            inv.drives
                .get(&id)
                .filter(|d| d.health.collected_at.is_some())
                .map(|d| d.health.media_errors)
        };
        let (candidate, why) = evaluate(&state.config.monitor, &sample, prev);
        let current = drive.health.status();
        let effective = damper.apply(&state.config.monitor, id, current, candidate);

        let mut inv = state.inventory.write().await;
        if let Some(d) = inv.drives.get_mut(&id) {
            d.health = HealthReport {
                status: Some(effective),
                temperature_c: sample.temperature_c,
                power_on_hours: sample.power_on_hours,
                media_errors: sample.media_errors,
                available_spare_pct: sample.available_spare_pct,
                wear_pct: sample.wear_pct,
                critical_warning: sample.critical_warning,
                messages: why.clone(),
                collected_at: Some(SystemTime::now()),
            };
            // Health and designation stay separate: a health-Failed drive
            // keeps its operator designation; the summary card and the UI
            // treat health-Failed as bad regardless.
        }
        if d_is_ssd(&drive.kind) || sample.wear_pct.is_some() {
            inv.record_trend(
                id,
                TrendSample {
                    unix_secs: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    wear_pct: sample.wear_pct,
                    media_errors: sample.media_errors,
                },
            );
        }
        drop(inv);

        if effective != current {
            let sev = match effective {
                HealthStatus::Failed | HealthStatus::Failing => Severity::Error,
                HealthStatus::Warning => Severity::Warning,
                _ => Severity::Info,
            };
            state.events.write().await.push(
                Some(id),
                sev,
                "health",
                format!(
                    "{} ({}): {:?} → {:?}: {}",
                    drive.name,
                    drive.model,
                    current,
                    effective,
                    why.join("; ")
                ),
            );
        }
    }

    // Reconcile Active state against stormblock's drive list.
    if state.stormblock.enabled() {
        if let Err(e) = reconcile_stormblock(state).await {
            tracing::debug!("stormblock reconcile skipped: {e:#}");
        }
    }

    state.persist().await;
    Ok(())
}

fn d_is_ssd(k: &DriveKind) -> bool {
    k.is_ssd()
}

/// Group raw observations by derived DriveId, each group's observations
/// sorted by path. A dual-IOM shelf presents one physical drive as two
/// /dev nodes with one WWID — that is ONE drive with two paths, and the
/// primary path is the sorted-first one so it does not flap between scans.
pub fn group_observed(
    observed: Vec<discovery::Observed>,
) -> Vec<(DriveId, Vec<discovery::Observed>)> {
    let mut groups: HashMap<DriveId, Vec<discovery::Observed>> = HashMap::new();
    for o in observed {
        let id = DriveId::derive(o.wwid.as_deref(), &o.model, &o.serial);
        groups.entry(id).or_default().push(o);
    }
    let mut out: Vec<_> = groups.into_iter().collect();
    for (_, g) in out.iter_mut() {
        g.sort_by(|a, b| a.path.cmp(&b.path));
    }
    out.sort_by(|a, b| a.1[0].path.cmp(&b.1[0].path));
    out
}

async fn merge_observed(state: &Arc<AppState>, observed: Vec<discovery::Observed>) {
    let now = SystemTime::now();
    let mut inv = state.inventory.write().await;
    let mut seen: Vec<DriveId> = Vec::new();
    let mut events = Vec::new();
    for (id, group) in group_observed(observed) {
        seen.push(id);
        let paths: Vec<String> = group.iter().map(|o| o.path.clone()).collect();
        let primary = &group[0];
        match inv.drives.get_mut(&id) {
            Some(d) => {
                if d.paths != paths {
                    events.push((
                        Some(id),
                        Severity::Info,
                        "discovered",
                        format!("{}: paths now {}", d.name, paths.join(", ")),
                    ));
                    // A path change usually means recabling or a re-bay —
                    // the old location is not to be trusted.
                    d.location = topology::locate(&primary.name);
                }
                d.path = primary.path.clone();
                d.name = primary.name.clone();
                d.paths = paths;
                d.firmware = primary.firmware.clone();
                d.capacity_bytes = primary.capacity_bytes;
                d.block_size = primary.block_size;
                d.last_seen = now;
                if d.activity == Activity::Missing {
                    d.activity = Activity::Idle;
                    events.push((
                        Some(id),
                        Severity::Info,
                        "discovered",
                        format!("{}: reappeared", d.name),
                    ));
                }
            }
            None => {
                let name = primary.name.clone();
                let location = topology::locate(&name);
                let multipath = if paths.len() > 1 {
                    format!(" ({} paths)", paths.len())
                } else {
                    String::new()
                };
                events.push((
                    Some(id),
                    Severity::Info,
                    "discovered",
                    format!(
                        "{name}: new drive {} {} ({} bytes){multipath}",
                        primary.model, primary.serial, primary.capacity_bytes
                    ),
                ));
                inv.drives.insert(
                    id,
                    crate::drive::Drive {
                        id,
                        path: primary.path.clone(),
                        name,
                        paths,
                        kind: primary.kind,
                        model: primary.model.clone(),
                        serial: primary.serial.clone(),
                        firmware: primary.firmware.clone(),
                        wwid: primary.wwid.clone(),
                        capacity_bytes: primary.capacity_bytes,
                        block_size: primary.block_size,
                        location,
                        membership: Membership::Out,
                        designation: Default::default(),
                        activity: Activity::Idle,
                        health: HealthReport::default(),
                        first_seen: now,
                        last_seen: now,
                        pushed_labels: Vec::new(),
                        pushed_health: None,
                        drain: None,
                    },
                );
            }
        }
    }
    for (id, d) in inv.drives.iter_mut() {
        if !seen.contains(id) && d.activity != Activity::Missing {
            d.activity = Activity::Missing;
            events.push((
                Some(*id),
                Severity::Error,
                "missing",
                format!("{}: device node disappeared", d.name),
            ));
        }
    }
    drop(inv);
    let mut log = state.events.write().await;
    for (id, sev, kind, msg) in events {
        log.push(id, sev, kind, msg);
    }
}

/// Reconcile fleet membership against stormblock's /api/v1/drives list
/// (matched by path or serial), in both directions: a drive stormblock
/// holds is Fleet regardless of who added it; a Fleet drive stormblock no
/// longer lists is Out.
async fn reconcile_stormblock(state: &Arc<AppState>) -> anyhow::Result<()> {
    let listed = state.stormblock.list_drives().await?;
    let mut inv = state.inventory.write().await;
    let mut events = Vec::new();
    for d in inv.drives.values_mut() {
        let in_sb = listed.iter().any(|sd| {
            sd.get("path").and_then(|v| v.as_str()) == Some(d.path.as_str())
                || (!d.serial.is_empty()
                    && sd.get("serial").and_then(|v| v.as_str()) == Some(d.serial.as_str()))
        });
        match (in_sb, d.membership) {
            (true, Membership::Out) => {
                d.membership = Membership::Fleet;
                events.push((
                    Some(d.id),
                    Severity::Info,
                    "stormblock",
                    format!("{}: in stormblock's drive list — marked fleet", d.name),
                ));
            }
            (false, Membership::Fleet) => {
                d.membership = Membership::Out;
                events.push((
                    Some(d.id),
                    Severity::Warning,
                    "stormblock",
                    format!("{}: no longer in stormblock's drive list — marked out of fleet", d.name),
                ));
            }
            _ => {}
        }
    }
    drop(inv);
    let mut log = state.events.write().await;
    for (id, sev, kind, msg) in events {
        log.push(id, sev, kind, msg);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> MonitorConfig {
        MonitorConfig::default()
    }

    fn good_sample() -> Sample {
        Sample {
            temperature_c: Some(35),
            available_spare_pct: Some(100),
            wear_pct: Some(1),
            kernel_ok: true,
            ..Default::default()
        }
    }

    #[test]
    fn healthy_sample_is_good() {
        let (st, _) = evaluate(&cfg(), &good_sample(), Some(0));
        assert_eq!(st, HealthStatus::Good);
    }

    #[test]
    fn kernel_dead_is_failed() {
        let s = Sample {
            kernel_ok: false,
            ..Default::default()
        };
        let (st, why) = evaluate(&cfg(), &s, None);
        assert_eq!(st, HealthStatus::Failed);
        assert!(!why.is_empty());
    }

    #[test]
    fn nvme_critical_bits() {
        let mut s = good_sample();
        s.critical_warning = crit::RELIABILITY_DEGRADED;
        assert_eq!(evaluate(&cfg(), &s, None).0, HealthStatus::Failing);
        s.critical_warning = crit::READ_ONLY;
        assert_eq!(evaluate(&cfg(), &s, None).0, HealthStatus::Failed);
        s.critical_warning = crit::TEMPERATURE;
        assert_eq!(evaluate(&cfg(), &s, None).0, HealthStatus::Warning);
    }

    #[test]
    fn spare_wear_temp_thresholds() {
        let mut s = good_sample();
        s.available_spare_pct = Some(15);
        assert_eq!(evaluate(&cfg(), &s, None).0, HealthStatus::Warning);
        s.available_spare_pct = Some(5);
        assert_eq!(evaluate(&cfg(), &s, None).0, HealthStatus::Failing);

        let mut s = good_sample();
        s.wear_pct = Some(85);
        assert_eq!(evaluate(&cfg(), &s, None).0, HealthStatus::Warning);
        s.wear_pct = Some(96);
        assert_eq!(evaluate(&cfg(), &s, None).0, HealthStatus::Failing);

        let mut s = good_sample();
        s.temperature_c = Some(60);
        assert_eq!(evaluate(&cfg(), &s, None).0, HealthStatus::Warning);
    }

    #[test]
    fn media_error_growth_warns() {
        let mut s = good_sample();
        s.media_errors = 5;
        assert_eq!(evaluate(&cfg(), &s, Some(2)).0, HealthStatus::Warning);
        assert_eq!(evaluate(&cfg(), &s, Some(5)).0, HealthStatus::Good);
        assert_eq!(evaluate(&cfg(), &s, None).0, HealthStatus::Good);
    }

    #[test]
    fn multipath_groups_by_wwid_with_stable_primary() {
        use crate::discovery::Observed;
        let ob = |name: &str, wwid: &str| Observed {
            name: name.into(),
            path: format!("/dev/{name}"),
            kind: crate::drive::DriveKind::SasHdd,
            model: "X411_HVIPC420A11".into(),
            serial: if wwid == "w1" { "S1".into() } else { "S2".into() },
            firmware: "NA02".into(),
            wwid: Some(wwid.into()),
            capacity_bytes: 420 << 30,
            block_size: 512,
        };
        // sdq and sda are the same physical drive through two IOMs.
        let groups = group_observed(vec![ob("sdq", "w1"), ob("sdb", "w2"), ob("sda", "w1")]);
        assert_eq!(groups.len(), 2, "two physical drives, not three");
        let g1 = groups
            .iter()
            .find(|(id, _)| *id == DriveId::derive(Some("w1"), "X411_HVIPC420A11", "S1"))
            .unwrap();
        assert_eq!(g1.1.len(), 2);
        assert_eq!(g1.1[0].path, "/dev/sda", "primary is sorted-first, never flaps");
    }

    #[test]
    fn damper_requires_consecutive_samples_to_worsen() {
        let cfg = cfg(); // hysteresis = 3
        let mut d = Damper::default();
        let id = DriveId::derive(None, "m", "s");
        assert_eq!(
            d.apply(&cfg, id, HealthStatus::Good, HealthStatus::Warning),
            HealthStatus::Good
        );
        assert_eq!(
            d.apply(&cfg, id, HealthStatus::Good, HealthStatus::Warning),
            HealthStatus::Good
        );
        assert_eq!(
            d.apply(&cfg, id, HealthStatus::Good, HealthStatus::Warning),
            HealthStatus::Warning,
            "third consecutive sample sticks"
        );
        // Improvement is immediate and clears the pending counter.
        assert_eq!(
            d.apply(&cfg, id, HealthStatus::Warning, HealthStatus::Good),
            HealthStatus::Good
        );
        // A different candidate resets the streak.
        d.apply(&cfg, id, HealthStatus::Good, HealthStatus::Warning);
        d.apply(&cfg, id, HealthStatus::Good, HealthStatus::Failing);
        assert_eq!(
            d.apply(&cfg, id, HealthStatus::Good, HealthStatus::Failing),
            HealthStatus::Good,
            "streak restarted on candidate change"
        );
    }
}
