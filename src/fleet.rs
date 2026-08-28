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
use crate::drive::{Activity, Designation, DrainRecord, DriveId, HealthStatus, Membership};
use crate::events::Severity;
use crate::stormblock::DrainStatus;

/// How long a failed auto-add is left alone before it is tried again.
const AUTO_ADD_RETRY: Duration = Duration::from_secs(600);

/// A drive as the label push sees it: id, name, path, labels, identity.
type LabelJob = (DriveId, String, String, Vec<(String, String)>, uuid::Uuid);
/// A drive as auto-add sees it: id, name, path, labels, kind.
type AddJob = (DriveId, String, String, Vec<(String, String)>, crate::drive::DriveKind);

/// Per-tick state the policy keeps between runs.
#[derive(Default)]
pub struct FleetState {
    auto_add_attempted: std::collections::HashMap<DriveId, Instant>,
}

/// Everything, in the order that matters: labels first (a drain places by
/// them), then health, then drains, then auto-add.
pub async fn tick(state: &Arc<AppState>, fs: &mut FleetState) {
    if !state.stormblock.enabled() {
        return;
    }
    sync_labels(state).await;
    if state.config.stormblock.push_health {
        push_health(state).await;
    }
    poll_drains(state).await;
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
            .map(|d| (d.id, d.name.clone(), d.path.clone(), d.stormblock_labels(), d.id.0))
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
                (d.id, d.name.clone(), d.path.clone(), word, d.health.messages.join("; "))
            })
            .filter(|(id, _, _, word, _)| {
                // Only on change.
                let inv_word = inv.drives.get(id).and_then(|d| d.pushed_health.clone());
                inv_word.as_deref() != Some(*word)
            })
            .collect()
    };
    for (id, name, path, word, why) in due {
        // A drain is our decision (below), not the engine's: never let a
        // `failed` report start one behind our back.
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
            Err(e) => tracing::debug!(drive = %name, "health not reported: {e:#}"),
        }
        // A fleet drive that is failing gets drained without anyone asking.
        if matches!(word, "failed" | "failing") && state.config.stormblock.drain_on_failing {
            if let Err(e) = start_drain(state, id, "health", true).await {
                tracing::warn!(drive = %name, "drain not started: {e:#}");
            }
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
        (d.name.clone(), d.path.clone(), d.membership, d.drain.clone())
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

/// Stop a drain. What moved stays moved; the drive takes allocations again.
pub async fn cancel_drain(state: &Arc<AppState>, id: DriveId) -> anyhow::Result<()> {
    let (name, path) = {
        let inv = state.inventory.read().await;
        let d = inv.drives.get(&id).ok_or_else(|| anyhow::anyhow!("drive {id} not in inventory"))?;
        (d.name.clone(), d.path.clone())
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
            .map(|d| (d.id, d.name.clone(), d.path.clone(), d.drain.as_ref().map(|r| r.then_leave).unwrap_or(false)))
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
            // stormblock forgot it (a restart): start again.
            let _ = start_drain(state, id, "resumed", then_leave).await;
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
            Some(d) => (d.name.clone(), d.path.clone()),
            None => return,
        }
    };
    match state.stormblock.delete_drive(&path, false).await {
        Ok(()) => {
            {
                let mut inv = state.inventory.write().await;
                if let Some(d) = inv.drives.get_mut(&id) {
                    d.membership = Membership::Out;
                    d.activity = Activity::Idle;
                    d.pushed_labels.clear();
                    d.pushed_health = None;
                }
            }
            if let Err(e) = crate::topology::set_locate(&name, true) {
                tracing::debug!(drive = %name, "locate LED not set: {e}");
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
            state.stormblock.format_slab(path, &tier).await?;
            slab_tier = Some(tier);
        }
    }
    let mut inv = state.inventory.write().await;
    if let Some(d) = inv.drives.get_mut(&id) {
        d.membership = Membership::Fleet;
        d.pushed_labels = labels.to_vec();
        d.pushed_health = None;
        d.drain = None;
    }
    let _ = name;
    Ok(slab_tier)
}
