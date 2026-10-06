//! Fleet policy — the loop between what this daemon knows about a drive and
//! what stormblock does with it (stormblock#70, closed in stormblock v11).
//!
//! Four things, each a small function the monitor tick calls:
//!
//! - **labels**: a fleet drive's location (shelf, bay, hba, pcie slot) and
//!   stable identity are pushed to stormblock once, and again when they
//!   change — they are the failure domain every slab on the drive is placed
//!   by, so a mirror's legs stay out of one enclosure.
//! - **health**: our Failing/Failed conclusion goes to the engine, which
//!   quarantines the drive's slabs and makes every redundant volume stop
//!   reading that leg *before* an I/O fails on it. `healthy` lifts it.
//! - **overcommit**: each drive's overcommit setting (#13) goes to the
//!   engine for every drive that carries slabs, on change; stormblock
//!   enforces it when a claim binds (stormblock#152). An engine without the
//!   route yet is asked again every ten minutes, quietly.
//! - **auto-add**: a qualified out-of-fleet drive is registered, labelled
//!   and given a slab. Off by default (`stormblock.auto_add`).
//! - **drains**: a fleet drive that goes Failing/Failed, or that an operator
//!   asked to leave, is drained over HTTP; when stormblock reports the drive
//!   empty it leaves the fleet, the locate LED comes on, and the drive is
//!   retired — ready for the swap.
//!
//! None of it is on the request path: everything here is best-effort,
//! logged, and retried on the next tick.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::api::AppState;
use crate::drive::{Activity, Designation, DrainRecord, Drive, DriveId, HealthStatus, Membership};
use crate::events::Severity;
use crate::stormblock::DrainStatus;

/// How long a failed auto-add is left alone before it is tried again.
const AUTO_ADD_RETRY: Duration = Duration::from_secs(600);
/// How long an engine without the overcommit route is left alone.
const OVERCOMMIT_RETRY: Duration = Duration::from_secs(600);

/// A drive as the label push sees it: id, name, path, labels, identity.
type LabelJob = (DriveId, String, String, Vec<(String, String)>, uuid::Uuid);
/// A drive as auto-add sees it: id, name, path, labels, kind.
type AddJob = (DriveId, String, String, Vec<(String, String)>, crate::drive::DriveKind);

/// Per-tick state the policy keeps between runs.
#[derive(Default)]
pub struct FleetState {
    auto_add_attempted: std::collections::HashMap<DriveId, Instant>,
    /// The engine answered 404/405 to the overcommit push: not before then.
    overcommit_unsupported_until: Option<Instant>,
}

/// Everything, in the order that matters: labels first (a drain places by
/// them), then health, then drains, then auto-add.
pub async fn tick(state: &Arc<AppState>, fs: &mut FleetState) {
    if !state.stormblock.enabled() {
        return;
    }
    sync_labels(state).await;
    sync_overcommit(state, fs).await;
    if state.config.stormblock.push_health {
        push_health(state).await;
    }
    poll_drains(state).await;
    retry_pending_drains(state).await;
    if state.config.stormblock.auto_add {
        auto_add(state, fs).await;
    }
}

/// Push location labels + identity for every fleet drive whose labels
/// changed since stormblock last heard them.
async fn sync_labels(state: &Arc<AppState>) {
    let due: Vec<LabelJob> = {
        let inv = state.inventory.read().await;
        inv.drives
            .values()
            .filter(|d| d.membership == Membership::Fleet && d.activity != Activity::Missing)
            .filter(|d| d.pushed_labels != d.stormblock_labels())
            .map(|d| (d.id, d.name.clone(), d.stormblock_path(), d.stormblock_labels(), d.id.0))
            .collect()
    };
    for (id, name, path, labels, uuid) in due {
        match state.stormblock.set_labels(&path, &labels, Some(uuid)).await {
            Ok(()) => {
                let mut inv = state.inventory.write().await;
                if let Some(d) = inv.drives.get_mut(&id) {
                    d.pushed_labels = labels.clone();
                }
                tracing::info!(drive = %name, labels = ?labels, "location labels pushed to stormblock");
            }
            Err(e) => tracing::debug!(drive = %name, "labels not pushed: {e:#}"),
        }
    }
}

/// Push the overcommit setting of every drive with slabs on it — a fleet
/// drive, or the node's own system disk — that the engine has not
/// accepted yet.
async fn sync_overcommit(state: &Arc<AppState>, fs: &mut FleetState) {
    if fs.overcommit_unsupported_until.is_some_and(|t| Instant::now() < t) {
        return;
    }
    type Job = (DriveId, String, String, crate::drive::Overcommit, Option<String>, String);
    let due: Vec<Job> = {
        let inv = state.inventory.read().await;
        inv.drives
            .values()
            .filter(|d| d.activity != Activity::Missing)
            .filter(|d| d.membership == Membership::Fleet || d.usage.as_ref().is_some_and(|u| !u.slabs.is_empty()))
            .filter(|d| d.pushed_overcommit != Some(d.overcommit))
            .map(|d| (d.id, d.name.clone(), d.stormblock_path(), d.overcommit, d.wwid.clone(), d.serial.clone()))
            .collect()
    };
    for (id, name, path, oc, wwn, serial) in due {
        match state.stormblock.set_overcommit(&path, oc, id.0, wwn.as_deref(), &serial).await {
            Ok(true) => {
                if let Some(d) = state.inventory.write().await.drives.get_mut(&id) {
                    d.pushed_overcommit = Some(oc);
                }
                tracing::info!(drive = %name, overcommit = %oc.word(), "overcommit pushed to stormblock");
            }
            Ok(false) => {
                tracing::debug!("stormblock has no overcommit route yet (stormblock#152)");
                fs.overcommit_unsupported_until = Some(Instant::now() + OVERCOMMIT_RETRY);
                return;
            }
            Err(e) => tracing::debug!(drive = %name, "overcommit not pushed: {e:#}"),
        }
    }
}

/// Report a changed health conclusion for every fleet drive.
async fn push_health(state: &Arc<AppState>) {
    let due: Vec<(DriveId, String, String, &'static str, String)> = {
        let inv = state.inventory.read().await;
        inv.drives
            .values()
            .filter(|d| d.membership == Membership::Fleet && d.activity != Activity::Missing)
            .filter(|d| d.health.status() != HealthStatus::Unknown)
            .map(|d| {
                // An operator's Failed designation counts as failed too.
                let word = if d.designation == Designation::Failed { "failed" } else { d.stormblock_health() };
                (d.id, d.name.clone(), d.stormblock_path(), word, d.health.messages.join("; "))
            })
            .filter(|(id, _, _, word, _)| {
                // Only on change.
                let inv_word = inv.drives.get(id).and_then(|d| d.pushed_health.clone());
                inv_word.as_deref() != Some(*word)
            })
            .collect()
    };
    for (id, name, path, word, why) in due {
        // We never ask for a drain in the report (`drain: false`), but the
        // engine drains a `failed` drive whatever we say, and rebuilds a
        // `failing`/`failed` one's redundant volumes when its `[rebuild]
        // automatic` is on (the default). Our drain below adopts the
        // engine's, or waits as `pending` while a rebuild runs (#43).
        let reason = if why.is_empty() { None } else { Some(why.as_str()) };
        match state.stormblock.report_health(&path, word, reason, false).await {
            Ok(_) => {
                {
                    let mut inv = state.inventory.write().await;
                    if let Some(d) = inv.drives.get_mut(&id) {
                        d.pushed_health = Some(word.to_string());
                    }
                }
                let sev = match word {
                    "failed" | "failing" => Severity::Warning,
                    _ => Severity::Info,
                };
                state.events.write().await.push(
                    Some(id),
                    sev,
                    "stormblock",
                    format!("{name}: reported {word} to stormblock{}", if word == "healthy" { " — quarantine lifted" } else { " — slabs quarantined, legs distrusted" }),
                );
            }
            Err(e) => {
                tracing::debug!(drive = %name, "health not reported: {e:#}");
                continue;
            }
        }
        // A fleet drive that is failing gets drained without anyone asking.
        if matches!(word, "failed" | "failing") && state.config.stormblock.drain_on_failing {
            if let Err(e) = request_drain(state, id, "health", true).await {
                tracing::info!(drive = %name, "drain pending: {e:#}");
            }
        } else if word == "healthy" {
            drop_pending_drain(state, id, &name).await;
        }
    }
}

/// Start (or resume tracking) a drain of a fleet drive. `then_leave` retires
/// the drive once the drain is empty. Idempotent: a drain already running is
/// adopted, not restarted.
pub async fn start_drain(
    state: &Arc<AppState>,
    id: DriveId,
    reason: &str,
    then_leave: bool,
) -> anyhow::Result<DrainRecord> {
    let (name, path, membership, existing) = {
        let inv = state.inventory.read().await;
        let d = inv.drives.get(&id).ok_or_else(|| anyhow::anyhow!("drive {id} not in inventory"))?;
        (d.name.clone(), d.stormblock_path(), d.membership, d.drain.clone())
    };
    if membership != Membership::Fleet {
        anyhow::bail!("{name}: not in the fleet, nothing to drain");
    }
    if let Some(r) = existing.filter(|r| r.state == "running") {
        return Ok(r);
    }
    let status = match state.stormblock.drain_status(&path).await? {
        Some(s) if s.is_running() => s,
        _ => state.stormblock.start_drain(&path).await?,
    };
    let rec = DrainRecord {
        state: status.state.clone(),
        moved: status.moved,
        failed: status.failed,
        remaining: status.remaining,
        errors: status.errors.clone(),
        reason: reason.to_string(),
        then_leave,
    };
    {
        let mut inv = state.inventory.write().await;
        if let Some(d) = inv.drives.get_mut(&id) {
            d.drain = Some(rec.clone());
            if status.is_running() {
                d.activity = Activity::Draining;
            }
        }
    }
    state.events.write().await.push(
        Some(id),
        Severity::Warning,
        "drain",
        format!("{name}: drain started ({reason}); {} leg(s) to move", status.remaining),
    );
    state.persist().await;
    Ok(rec)
}

/// A drain we want and the engine has not started yet: its `POST …/drain`
/// was refused (409 while a rebuild of the drive's volumes runs — the
/// engine drains after it) or did not answer, or the engine forgot a drain
/// across its restart. Retried every fleet tick (#43).
pub const PENDING: &str = "pending";

/// Whether this tick should try the drive's drain again.
pub fn drain_due(d: &Drive) -> bool {
    d.membership == Membership::Fleet && d.activity != Activity::Missing && d.drain.as_ref().is_some_and(|r| r.state == PENDING)
}

/// Mark a drive's drain as wanted-but-not-started, keeping why and whether
/// it retires. Returns whether it was not pending before.
fn mark_pending(d: &mut Drive, reason: &str, then_leave: bool, error: String) -> bool {
    let was = d.drain.as_ref().is_some_and(|r| r.state == PENDING);
    let rec = d.drain.get_or_insert_with(DrainRecord::default);
    if !was {
        rec.reason = reason.to_string();
        rec.then_leave = then_leave;
    }
    rec.state = PENDING.into();
    rec.errors = vec![error];
    if d.activity == Activity::Draining {
        d.activity = Activity::Idle;
    }
    !was
}

/// `start_drain`, and when the engine will not start it now, keep it
/// `pending` so the fleet tick tries again — the automatic drains (health,
/// a Failed designation) have no operator to retry them.
pub async fn request_drain(state: &Arc<AppState>, id: DriveId, reason: &str, then_leave: bool) -> anyhow::Result<DrainRecord> {
    match start_drain(state, id, reason, then_leave).await {
        Ok(r) => Ok(r),
        Err(e) => {
            let first = {
                let mut inv = state.inventory.write().await;
                match inv.drives.get_mut(&id) {
                    Some(d) if d.membership == Membership::Fleet => mark_pending(d, reason, then_leave, format!("{e:#}")),
                    _ => return Err(e),
                }
            };
            if first {
                let name = state.inventory.read().await.drives.get(&id).map(|d| d.name.clone()).unwrap_or_default();
                state.events.write().await.push(
                    Some(id),
                    Severity::Warning,
                    "drain",
                    format!("{name}: drain ({reason}) pending — the engine did not start it: {e:#}; trying again each tick"),
                );
            }
            state.persist().await;
            Err(e)
        }
    }
}

/// Try every pending drain again. `start_drain` adopts a drain the engine
/// started by itself (after a rebuild), so the drive still retires.
async fn retry_pending_drains(state: &Arc<AppState>) {
    let due: Vec<(DriveId, String, String, bool)> = {
        let inv = state.inventory.read().await;
        inv.drives
            .values()
            .filter(|d| drain_due(d))
            .map(|d| {
                let r = d.drain.as_ref().unwrap();
                (d.id, d.name.clone(), r.reason.clone(), r.then_leave)
            })
            .collect()
    };
    for (id, name, reason, then_leave) in due {
        if let Err(e) = request_drain(state, id, &reason, then_leave).await {
            tracing::debug!(drive = %name, "drain still pending: {e:#}");
        }
    }
}

/// The drive is healthy again: a drain health asked for and the engine
/// never started is no longer wanted.
async fn drop_pending_drain(state: &Arc<AppState>, id: DriveId, name: &str) {
    let dropped = {
        let mut inv = state.inventory.write().await;
        match inv.drives.get_mut(&id) {
            Some(d) if d.drain.as_ref().is_some_and(|r| r.state == PENDING && r.reason == "health") => {
                d.drain = None;
                true
            }
            _ => false,
        }
    };
    if dropped {
        state.events.write().await.push(Some(id), Severity::Info, "drain", format!("{name}: healthy again; the pending drain is dropped"));
        state.persist().await;
    }
}

/// Stop a drain. What moved stays moved; the drive takes allocations again.
pub async fn cancel_drain(state: &Arc<AppState>, id: DriveId) -> anyhow::Result<()> {
    let (name, path) = {
        let inv = state.inventory.read().await;
        let d = inv.drives.get(&id).ok_or_else(|| anyhow::anyhow!("drive {id} not in inventory"))?;
        (d.name.clone(), d.stormblock_path())
    };
    state.stormblock.cancel_drain(&path).await?;
    let mut inv = state.inventory.write().await;
    if let Some(d) = inv.drives.get_mut(&id) {
        if let Some(r) = d.drain.as_mut() {
            r.state = "cancelled".into();
        }
        if d.activity == Activity::Draining {
            d.activity = Activity::Idle;
        }
    }
    drop(inv);
    state.events.write().await.push(Some(id), Severity::Info, "drain", format!("{name}: drain cancelled"));
    state.persist().await;
    Ok(())
}

/// Follow every running drain; retire the drive when it is empty.
async fn poll_drains(state: &Arc<AppState>) {
    let running: Vec<(DriveId, String, String, bool)> = {
        let inv = state.inventory.read().await;
        inv.drives
            .values()
            .filter(|d| d.drain.as_ref().is_some_and(|r| r.state == "running"))
            .map(|d| (d.id, d.name.clone(), d.stormblock_path(), d.drain.as_ref().map(|r| r.then_leave).unwrap_or(false)))
            .collect()
    };
    for (id, name, path, then_leave) in running {
        let status: Option<DrainStatus> = match state.stormblock.drain_status(&path).await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(drive = %name, "drain status unavailable: {e:#}");
                continue;
            }
        };
        let Some(status) = status else {
            // stormblock forgot it (a restart): pending, so this tick's
            // retry starts it again (start_drain adopts only a running
            // record, so calling it here would do nothing).
            let mut inv = state.inventory.write().await;
            if let Some(d) = inv.drives.get_mut(&id) {
                let reason = d.drain.as_ref().map(|r| r.reason.clone()).unwrap_or_else(|| "resumed".into());
                mark_pending(d, &reason, then_leave, "the engine has no record of this drain (restarted?)".into());
            }
            continue;
        };
        {
            let mut inv = state.inventory.write().await;
            if let Some(d) = inv.drives.get_mut(&id) {
                if let Some(r) = d.drain.as_mut() {
                    r.state = status.state.clone();
                    r.moved = status.moved;
                    r.failed = status.failed;
                    r.remaining = status.remaining;
                    r.errors = status.errors.clone();
                }
            }
        }
        match status.state.as_str() {
            "running" => {}
            "empty" => {
                state.events.write().await.push(
                    Some(id),
                    Severity::Info,
                    "drain",
                    format!("{name}: drain complete — {} leg(s) moved, nothing left on the drive", status.moved),
                );
                if then_leave {
                    retire(state, id).await;
                } else {
                    let mut inv = state.inventory.write().await;
                    if let Some(d) = inv.drives.get_mut(&id) {
                        d.activity = Activity::Idle;
                    }
                }
                state.persist().await;
            }
            "stuck" => {
                state.events.write().await.push(
                    Some(id),
                    Severity::Error,
                    "drain",
                    format!(
                        "{name}: drain stuck — {} leg(s) could not be moved: {}",
                        status.remaining,
                        status.errors.first().cloned().unwrap_or_default()
                    ),
                );
                state.persist().await;
            }
            _ => {
                let mut inv = state.inventory.write().await;
                if let Some(d) = inv.drives.get_mut(&id) {
                    if d.activity == Activity::Draining {
                        d.activity = Activity::Idle;
                    }
                }
            }
        }
    }
}

/// An empty drive leaves the fleet and lights its locate LED: the swap can
/// happen whenever the tech gets there.
async fn retire(state: &Arc<AppState>, id: DriveId) {
    let (name, path) = {
        let inv = state.inventory.read().await;
        match inv.drives.get(&id) {
            Some(d) => (d.name.clone(), d.stormblock_path()),
            None => return,
        }
    };
    match state.stormblock.delete_drive(&path, false).await {
        Ok(()) => {
            {
                let mut inv = state.inventory.write().await;
                if let Some(d) = inv.drives.get_mut(&id) {
                    d.membership = Membership::Out;
                    d.fleet_partition = None;
                    d.activity = Activity::Idle;
                    d.pushed_labels.clear();
                    d.pushed_health = None;
                    d.pushed_overcommit = None;
                }
            }
            {
                let shelves = state.shelves.read().await.clone();
                let loc = state.inventory.read().await.drives.get(&id).map(|d| d.location.clone()).unwrap_or_default();
                if let Err(e) = crate::topology::set_locate(&name, &loc, &shelves, true) {
                    tracing::debug!(drive = %name, "locate LED not set: {e}");
                }
            }
            state.events.write().await.push(
                Some(id),
                Severity::Warning,
                "fleet",
                format!("{name}: retired — out of the fleet, locate LED on; safe to pull"),
            );
        }
        Err(e) => {
            state.events.write().await.push(
                Some(id),
                Severity::Error,
                "fleet",
                format!("{name}: drained but could not leave the fleet: {e:#}"),
            );
        }
    }
}

/// Register every qualified out-of-fleet drive: open it with its labels and
/// identity, format a slab on it. A drive that failed is left alone for a
/// while rather than hammered every tick.
async fn auto_add(state: &Arc<AppState>, fs: &mut FleetState) {
    let candidates: Vec<AddJob> = {
        let inv = state.inventory.read().await;
        inv.drives
            .values()
            .filter(|d| d.fleet_join_blocker().is_none())
            .filter(|d| d.designation == Designation::None)
            .filter(|d| d.health.status() != HealthStatus::Unknown)
            .map(|d| (d.id, d.name.clone(), d.path.clone(), d.stormblock_labels(), d.kind))
            .collect()
    };
    for (id, name, path, labels, kind) in candidates {
        if fs.auto_add_attempted.get(&id).is_some_and(|t| t.elapsed() < AUTO_ADD_RETRY) {
            continue;
        }
        fs.auto_add_attempted.insert(id, Instant::now());
        match join(state, id, &name, &path, &labels, kind, state.config.stormblock.auto_format_slab, None).await {
            Ok(tier) => {
                state.events.write().await.push(
                    Some(id),
                    Severity::Info,
                    "fleet",
                    match tier {
                        Some(t) => format!("{name}: auto-added to the fleet, slab formatted ({t}), labels {labels:?}"),
                        None => format!("{name}: auto-added to the fleet, labels {labels:?}"),
                    },
                );
                state.persist().await;
            }
            Err(e) => tracing::warn!(drive = %name, "auto-add failed: {e:#}"),
        }
    }
}

/// Open a drive in stormblock with its labels and identity, optionally
/// format a slab, mark it Fleet. Shared by auto-add and the join API.
#[allow(clippy::too_many_arguments)]
pub async fn join(
    state: &Arc<AppState>,
    id: DriveId,
    name: &str,
    path: &str,
    labels: &[(String, String)],
    kind: crate::drive::DriveKind,
    format_slab: bool,
    tier: Option<String>,
) -> anyhow::Result<Option<String>> {
    let listed = state.stormblock.list_drives().await?;
    let already = listed
        .iter()
        .any(|sd| sd.get("path").and_then(|v| v.as_str()) == Some(path));
    if already {
        state.stormblock.set_labels(path, labels, Some(id.0)).await?;
    } else {
        state.stormblock.add_drive(path, labels, Some(id.0)).await?;
    }
    let mut slab_tier = None;
    if format_slab {
        let has_slab = !state.stormblock.drive_slabs(path).await.unwrap_or_default().is_empty();
        if !has_slab {
            let tier = tier.unwrap_or_else(|| state.stormblock.tier_for(kind));
            state.stormblock.format_slab(path, &tier, None).await?;
            slab_tier = Some(tier);
        }
    }
    let mut inv = state.inventory.write().await;
    if let Some(d) = inv.drives.get_mut(&id) {
        d.membership = Membership::Fleet;
        // A whole-disk join; the worker's partition enroll sets it after.
        d.fleet_partition = None;
        d.pushed_labels = labels.to_vec();
        d.pushed_health = None;
        d.pushed_overcommit = None;
        d.drain = None;
    }
    let _ = name;
    Ok(slab_tier)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(over: impl FnOnce(&mut Drive)) -> Drive {
        let mut d: Drive = serde_json::from_value(serde_json::json!({
            "id": DriveId::derive(Some("naa.5000c500aaaa0001"), "M", "S1"), "path": "/dev/sdb", "name": "sdb", "paths": ["/dev/sdb"],
            "kind": "sas_hdd", "model": "M", "serial": "S1", "firmware": "N003", "wwid": "naa.5000c500aaaa0001",
            "capacity_bytes": 1_000_000_000u64, "block_size": 512, "membership": "fleet",
            "first_seen": {"secs_since_epoch": 0, "nanos_since_epoch": 0}, "last_seen": {"secs_since_epoch": 0, "nanos_since_epoch": 0},
        }))
        .unwrap();
        over(&mut d);
        d
    }

    /// A refused start (409: the engine is rebuilding the drive's volumes
    /// first) leaves a pending drain the tick tries again; nothing else does.
    #[test]
    fn a_refused_drain_stays_wanted() {
        let mut d = drive(|_| {});
        assert!(!drain_due(&d), "no drain asked for");
        assert!(mark_pending(&mut d, "health", true, "409 rebuild running".into()), "first time: an event");
        assert!(drain_due(&d));
        let r = d.drain.clone().unwrap();
        assert_eq!((r.state.as_str(), r.reason.as_str(), r.then_leave), ("pending", "health", true));

        // Again: still pending, reason and retire kept, error refreshed.
        assert!(!mark_pending(&mut d, "resumed", false, "502".into()), "no second event");
        let r = d.drain.clone().unwrap();
        assert_eq!((r.reason.as_str(), r.then_leave, r.errors.as_slice()), ("health", true, &["502".to_string()][..]));

        // The engine forgot a running drain: pending, activity back to idle,
        // the original reason kept.
        let mut forgot = drive(|d| {
            d.activity = Activity::Draining;
            d.drain = Some(DrainRecord { state: "running".into(), reason: "operator".into(), then_leave: true, ..Default::default() });
        });
        mark_pending(&mut forgot, "operator", true, "gone".into());
        assert!(drain_due(&forgot));
        assert_eq!(forgot.activity, Activity::Idle);

        for (state, due) in [("running", false), ("empty", false), ("stuck", false), ("cancelled", false), ("pending", true)] {
            let d = drive(|d| d.drain = Some(DrainRecord { state: state.into(), ..Default::default() }));
            assert_eq!(drain_due(&d), due, "{state}");
        }
        // Out of the fleet or out of sight: nothing to drain from here.
        let out = drive(|d| {
            d.membership = Membership::Out;
            d.drain = Some(DrainRecord { state: PENDING.into(), ..Default::default() });
        });
        assert!(!drain_due(&out));
        let gone = drive(|d| {
            d.activity = Activity::Missing;
            d.drain = Some(DrainRecord { state: PENDING.into(), ..Default::default() });
        });
        assert!(!drain_due(&gone));
    }
}
