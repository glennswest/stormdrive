//! The stormview components feed: every drive and shelf as a
//! `ComponentSummary` with real actions, so stormd, stormsh, and
//! stormconsole render this daemon's world — grids by relations, buttons
//! that make things happen — with no per-UI code.
//!
//! Action paths are parameter-less POST routes (a stormview renderer
//! invokes method+path with no body) — see the `/locate/{state}`,
//! `/fleet/{action}`, `/designation/{value}`, `/test/{kind}` routes.

use crate::api::AppState;
use crate::drive::{Activity, Designation, Drive, DriveKind, HealthStatus, Membership};
use std::collections::BTreeMap;
use std::sync::Arc;
use stormview::{Action, ComponentSummary, Health, Metric, Relation};

fn health_of(d: &Drive) -> Health {
    if d.activity == Activity::Missing || d.designation == Designation::Failed {
        return Health::Error;
    }
    match d.health.status() {
        HealthStatus::Failed | HealthStatus::Failing => Health::Error,
        HealthStatus::Warning => Health::Warn,
        HealthStatus::Good => Health::Ok,
        HealthStatus::Unknown => Health::Unknown,
    }
}

fn act(id: &str, label: &str, method: &str, path: String, enabled: bool, danger: bool) -> Action {
    Action {
        id: id.into(),
        label: label.into(),
        method: method.into(),
        path,
        enabled,
        danger,
    }
}

fn kind_str(k: DriveKind) -> &'static str {
    match k {
        DriveKind::NvmeSsd => "nvme ssd",
        DriveKind::SasSsd => "sas ssd",
        DriveKind::SasHdd => "sas hdd",
        DriveKind::SataSsd => "sata ssd",
        DriveKind::SataHdd => "sata hdd",
        DriveKind::Unknown => "unknown",
    }
}

fn drive_component(d: &Drive) -> ComponentSummary {
    let base = format!("/api/v1/drives/{}", d.id);
    let idle = d.activity == Activity::Idle;
    let present = d.activity != Activity::Missing;

    let mut detail = vec![
        kind_str(d.kind).to_string(),
        stormview::format_bytes(d.capacity_bytes),
        match d.membership {
            Membership::Fleet => "fleet".into(),
            Membership::Out => "out of fleet".into(),
        },
    ];
    if d.designation != Designation::None {
        detail.push(format!("{:?}", d.designation).to_lowercase());
    }
    if let Some(sh) = &d.location.shelf {
        if let Some(b) = d.location.bay {
            detail.push(format!("{} bay {b}", sh.display()));
        }
    }
    if d.activity != Activity::Idle {
        detail.push(format!("{:?}", d.activity).to_lowercase());
    }

    let mut metrics = Vec::new();
    if let Some(t) = d.health.temperature_c {
        metrics.push(Metric::new("temp", t.to_string()).unit("°C"));
    }
    if let Some(w) = d.health.wear_pct {
        let m = Metric::new("wear", w.to_string()).unit("%");
        metrics.push(if w >= 80 { m.tone("warn") } else { m });
    }
    if let Some(sp) = d.health.available_spare_pct {
        metrics.push(Metric::new("spare", sp.to_string()).unit("%"));
    }
    if d.health.media_errors > 0 {
        metrics.push(Metric::new("media errs", d.health.media_errors.to_string()).tone("warn"));
    }
    if d.paths.len() > 1 {
        metrics.push(Metric::new("paths", d.paths.len().to_string()).tone("accent"));
    }

    let mut actions = Vec::new();
    if d.location.bay.is_some() {
        actions.push(act("locate-on", "Locate", "POST", format!("{base}/locate/on"), present, false));
        actions.push(act("locate-off", "LED off", "POST", format!("{base}/locate/off"), present, false));
    }
    match d.membership {
        Membership::Out => actions.push(act(
            "fleet-join",
            "Join fleet",
            "POST",
            format!("{base}/fleet/join"),
            idle && d.fleet_join_blocker().is_none(),
            false,
        )),
        Membership::Fleet => actions.push(act(
            "fleet-leave",
            "Leave fleet",
            "POST",
            format!("{base}/fleet/leave"),
            idle,
            true,
        )),
    }
    if d.activity == Activity::Testing {
        actions.push(act("test-cancel", "Cancel test", "POST", format!("{base}/test/cancel"), true, false));
    } else {
        actions.push(act("test-smoke", "Smoke test", "POST", format!("{base}/test/smoke"), idle && present, false));
        actions.push(act("test-scan", "Read scan", "POST", format!("{base}/test/read_scan"), idle && present, false));
        actions.push(act(
            "test-destructive",
            "Destructive test",
            "POST",
            format!("{base}/test/destructive_sample"),
            idle && present && d.membership == Membership::Out,
            true,
        ));
    }
    actions.push(act(
        "mark-spare",
        "Mark spare",
        "POST",
        format!("{base}/designation/spare"),
        d.designation != Designation::Spare,
        false,
    ));
    actions.push(act(
        "mark-failed",
        "Mark failed",
        "POST",
        format!("{base}/designation/failed"),
        d.designation != Designation::Failed,
        true,
    ));
    actions.push(act(
        "clear-designation",
        "Clear mark",
        "POST",
        format!("{base}/designation/none"),
        d.designation != Designation::None,
        false,
    ));

    let mut relations = Vec::new();
    if let Some(sh) = &d.location.shelf {
        if let Some(key) = sh.key() {
            relations.push(Relation::belongs_to("shelf", format!("shelf:{key}")));
        }
    }
    relations.push(Relation::belongs_to("system", "system"));

    ComponentSummary {
        id: format!("drive:{}", d.id),
        kind: "drive".into(),
        label: format!("{} · {}", d.name, d.model),
        health: health_of(d),
        detail: detail.join(" · "),
        metrics,
        actions,
        relations,
        link: None,
    }
}

/// Assemble the full feed: system rollup, shelves, drives.
pub async fn collect(state: &Arc<AppState>) -> Vec<ComponentSummary> {
    let inv = state.inventory.read().await;
    let mut drives: Vec<&Drive> = inv.drives.values().collect();
    drives.sort_by(|a, b| a.name.cmp(&b.name));

    let mut out = Vec::new();
    let mut shelves: BTreeMap<String, (String, Vec<&Drive>)> = BTreeMap::new();
    for d in &drives {
        if let Some(sh) = &d.location.shelf {
            if let Some(key) = sh.key() {
                shelves
                    .entry(key)
                    .or_insert_with(|| (sh.display(), Vec::new()))
                    .1
                    .push(d);
            }
        }
    }

    // System rollup first.
    let total = drives.len();
    let fleet = drives.iter().filter(|d| d.membership == Membership::Fleet).count();
    let bad = drives.iter().filter(|d| health_of(d) == Health::Error).count();
    let warn = drives.iter().filter(|d| health_of(d) == Health::Warn).count();
    let system_health = if bad > 0 {
        Health::Error
    } else if warn > 0 {
        Health::Warn
    } else if total == 0 {
        Health::Idle
    } else {
        Health::Ok
    };
    out.push(ComponentSummary {
        id: "system".into(),
        kind: "storage".into(),
        label: format!("stormdrive · {}", state.node_name),
        health: system_health,
        detail: format!("{total} drives · {fleet} in fleet · {} shelves", shelves.len()),
        metrics: vec![
            Metric::new("drives", total.to_string()),
            Metric::new("fleet", fleet.to_string()).tone("accent"),
        ],
        actions: Vec::new(),
        relations: vec![Relation::has_many(
            "drives",
            drives.iter().map(|d| format!("drive:{}", d.id)).collect(),
        )],
        link: None,
    });

    for (key, (display, members)) in &shelves {
        let worst = members
            .iter()
            .map(|d| health_of(d))
            .fold(Health::Ok, |acc, h| match (acc, h) {
                (Health::Error, _) | (_, Health::Error) => Health::Error,
                (Health::Warn, _) | (_, Health::Warn) => Health::Warn,
                (a, _) => a,
            });
        out.push(ComponentSummary {
            id: format!("shelf:{key}"),
            kind: "shelf".into(),
            label: display.clone(),
            health: worst,
            detail: format!("{} drives", members.len()),
            metrics: vec![Metric::new("drives", members.len().to_string())],
            actions: Vec::new(),
            relations: vec![Relation::has_many(
                "drives",
                members.iter().map(|d| format!("drive:{}", d.id)).collect(),
            )],
            link: None,
        });
    }

    for d in &drives {
        out.push(drive_component(d));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::*;
    use std::time::SystemTime;

    fn drive() -> Drive {
        Drive {
            id: DriveId::derive(None, "M", "S"),
            path: "/dev/sdx".into(),
            name: "sdx".into(),
            paths: vec!["/dev/sdx".into()],
            kind: DriveKind::SasSsd,
            model: "M".into(),
            serial: "S".into(),
            firmware: "1".into(),
            wwid: None,
            capacity_bytes: 1 << 30,
            block_size: 512,
            location: Location::default(),
            membership: Membership::Out,
            designation: Designation::None,
            activity: Activity::Idle,
            health: HealthReport {
                status: Some(HealthStatus::Good),
                temperature_c: Some(40),
                ..Default::default()
            },
            first_seen: SystemTime::now(),
            last_seen: SystemTime::now(),
            pushed_labels: Vec::new(),
            pushed_health: None,
            drain: None,
        }
    }

    #[test]
    fn drive_component_carries_real_actions() {
        let c = drive_component(&drive());
        assert_eq!(c.kind, "drive");
        assert_eq!(c.health, Health::Ok);
        let join = c.actions.iter().find(|a| a.id == "fleet-join").unwrap();
        assert!(join.enabled);
        assert_eq!(join.method, "POST");
        assert!(join.path.ends_with("/fleet/join"));
        let destr = c.actions.iter().find(|a| a.id == "test-destructive").unwrap();
        assert!(destr.danger);
        assert!(destr.enabled, "out-of-fleet idle drive may run destructive");
    }

    #[test]
    fn fleet_drive_gets_leave_and_no_destructive() {
        let mut d = drive();
        d.membership = Membership::Fleet;
        let c = drive_component(&d);
        assert!(c.actions.iter().any(|a| a.id == "fleet-leave" && a.danger));
        assert!(c.actions.iter().all(|a| a.id != "fleet-join"));
        let destr = c.actions.iter().find(|a| a.id == "test-destructive").unwrap();
        assert!(!destr.enabled, "no destructive tests in the fleet");
    }

    #[test]
    fn failed_designation_is_error_health() {
        let mut d = drive();
        d.designation = Designation::Failed;
        assert_eq!(health_of(&d), Health::Error);
    }

    #[test]
    fn shelf_relation_present_when_located() {
        let mut d = drive();
        d.location.shelf = Some(Shelf {
            serial: Some("SN1".into()),
            model: Some("DS4246".into()),
            ..Default::default()
        });
        d.location.bay = Some(3);
        let c = drive_component(&d);
        assert!(c
            .relations
            .iter()
            .any(|r| r.name == "shelf" && r.targets == vec!["shelf:SN1".to_string()]));
    }
}
