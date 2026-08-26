//! The monitor loop: rescan, collect, evaluate, transition, persist.
//!
//! `evaluate` is a pure function of (config, sample, previous media errors)
//! so the threshold policy is unit-testable without hardware. Worsening
//! transitions are hysteresis-guarded — one bad poll never flips a drive.

use crate::config::MonitorConfig;
use crate::drive::{DriveId, DriveKind, DriveState, HealthReport, HealthStatus};
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
            .filter(|(_, d)| !matches!(d.state, DriveState::Missing | DriveState::Retired))
            .map(|(id, d)| (*id, d.clone()))
            .collect()
    };
    for (id, drive) in targets {
        let d2 = drive.clone();
        let sample = tokio::task::spawn_blocking(move || smart::collect(&d2)).await?;
        let prev = {
            let inv = state.inventory.read().await;
            inv.drives.get(&id).map(|d| d.health.media_errors)
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
            if effective >= HealthStatus::Failed && d.state != DriveState::Failed {
                d.state = DriveState::Failed;
            }
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

async fn merge_observed(state: &Arc<AppState>, observed: Vec<discovery::Observed>) {
    let now = SystemTime::now();
    let mut inv = state.inventory.write().await;
    let mut seen: Vec<DriveId> = Vec::new();
    let mut events = Vec::new();
    for o in observed {
        let id = DriveId::derive(o.wwid.as_deref(), &o.model, &o.serial);
        seen.push(id);
        match inv.drives.get_mut(&id) {
            Some(d) => {
                if d.path != o.path {
                    events.push((
                        Some(id),
                        Severity::Info,
                        "discovered",
                        format!("{}: path changed {} → {}", d.name, d.path, o.path),
                    ));
                }
                d.path = o.path;
                d.name = o.name;
                d.firmware = o.firmware;
                d.capacity_bytes = o.capacity_bytes;
                d.block_size = o.block_size;
                d.last_seen = now;
                if d.state == DriveState::Missing {
                    d.state = DriveState::Discovered;
                    events.push((
                        Some(id),
                        Severity::Info,
                        "discovered",
                        format!("{}: reappeared", d.name),
                    ));
                }
            }
            None => {
                let name = o.name.clone();
                let location = topology::locate(&name);
                events.push((
                    Some(id),
                    Severity::Info,
                    "discovered",
                    format!("{name}: new drive {} {} ({} bytes)", o.model, o.serial, o.capacity_bytes),
                ));
                inv.drives.insert(
                    id,
                    crate::drive::Drive {
                        id,
                        path: o.path,
                        name,
                        kind: o.kind,
                        model: o.model,
                        serial: o.serial,
                        firmware: o.firmware,
                        wwid: o.wwid,
                        capacity_bytes: o.capacity_bytes,
                        block_size: o.block_size,
                        location,
                        state: DriveState::Discovered,
                        health: HealthReport::default(),
                        first_seen: now,
                        last_seen: now,
                    },
                );
            }
        }
    }
    for (id, d) in inv.drives.iter_mut() {
        if !seen.contains(id)
            && !matches!(d.state, DriveState::Missing | DriveState::Retired)
        {
            d.state = DriveState::Missing;
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

/// Mark drives Active when stormblock's /api/v1/drives lists them (matched
/// by path or serial).
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
        if in_sb && matches!(d.state, DriveState::Discovered | DriveState::Available) {
            d.state = DriveState::Active;
            events.push((
                Some(d.id),
                Severity::Info,
                "stormblock",
                format!("{}: registered with stormblock", d.name),
            ));
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
