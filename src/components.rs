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

/// How an HBA is named in the feed: its PCIe address when we resolved one
/// (that is what an operator reads off `lspci`), else the SCSI host.
fn controller_name(c: &crate::drive::Controller) -> Option<String> {
    c.pcie_addr.clone().or_else(|| c.scsi_host.clone())
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
    if d.needs_reformat() {
        detail.push(format!("{} B sectors · unusable · reformat", d.block_size));
    } else if d.block_size != 512 {
        detail.push(format!("{} B sectors", d.block_size));
    }
    match d.activity {
        Activity::Idle => {}
        Activity::UpdatingFirmware => detail.push("updating firmware".into()),
        a => detail.push(format!("{a:?}").to_lowercase()),
    }
    if let Some(f) = &d.firmware_update {
        if f.reset_required && f.state == "done" {
            detail.push("firmware pending reset".into());
        }
    }

    // Placement first, and as data: a renderer orders a shelf's drives by
    // bay and names the card that fails as a unit, without a regex over
    // `detail` (issue #3).
    let mut metrics = Vec::new();
    if let Some(b) = d.location.bay {
        metrics.push(Metric::new("bay", b.to_string()));
    }
    if let Some(h) = d.location.controller.as_ref().and_then(controller_name) {
        metrics.push(Metric::new("hba", h).tone("muted"));
    }
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
    if d.needs_reformat() {
        metrics.push(Metric::new("sector", d.block_size.to_string()).unit("B").tone("warn"));
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
            idle && present && d.destructive_test_blocker().is_none(),
            true,
        ));
    }
    actions.push(act(
        "format-4k",
        if d.needs_reformat() { "Reformat 4K" } else { "Format 4K" },
        "POST",
        format!("{base}/format/4096"),
        d.format_blocker().is_none() && present,
        true,
    ));
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

fn ses_health(st: crate::ses::ElementStatus) -> Health {
    use crate::ses::ElementStatus as E;
    match st {
        E::Ok => Health::Ok,
        E::Noncritical => Health::Warn,
        E::Critical | E::Unrecoverable => Health::Error,
        _ => Health::Unknown,
    }
}

fn push_unique(out: &mut Vec<String>, name: String) {
    if !out.contains(&name) {
        out.push(name);
    }
}

/// The HBAs a shelf is reached through. One per SES processor path — the
/// enclosure device's SCSI host names the card that path lands on — plus
/// the controllers its drives hang off, since a single-IOM shelf whose ses
/// module is not bound still has drives with a controller. A shelf is what
/// an operator pulls a card for, so the card is named (issue #3).
fn shelf_hbas(
    members: &[&Drive],
    rep: Option<&crate::ses::ShelfReport>,
    by_host: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut out = Vec::new();
    for esp in rep.map(|r| r.esps.as_slice()).unwrap_or_default() {
        if let Some(n) = esp.scsi_id.split(':').next().filter(|n| !n.is_empty()) {
            let host = format!("host{n}");
            push_unique(&mut out, by_host.get(&host).cloned().unwrap_or(host));
        }
    }
    for d in members {
        if let Some(name) = d.location.controller.as_ref().and_then(controller_name) {
            push_unique(&mut out, name);
        }
    }
    out
}

/// Assemble the full feed: system rollup, shelves, drives.
pub async fn collect(state: &Arc<AppState>) -> Vec<ComponentSummary> {
    let ses = state.shelves.read().await.clone();
    let inv = state.inventory.read().await;
    let mut drives: Vec<&Drive> = inv.drives.values().collect();
    drives.sort_by(|a, b| a.name.cmp(&b.name));

    // scsi_host → the HBA's display name, so an enclosure device's SCSI
    // host resolves to the same PCIe address the drives report.
    let mut by_host: BTreeMap<String, String> = BTreeMap::new();
    for d in &drives {
        if let Some(c) = &d.location.controller {
            if let (Some(h), Some(name)) = (&c.scsi_host, controller_name(c)) {
                by_host.entry(h.clone()).or_insert(name);
            }
        }
    }

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

    // Shelves the SES scan knows but no drive points at yet still show.
    for (key, r) in &ses {
        shelves
            .entry(key.clone())
            .or_insert_with(|| (r.shelf.display(), Vec::new()));
    }

    for (key, (display, members)) in &shelves {
        let drive_worst = members
            .iter()
            .map(|d| health_of(d))
            .fold(Health::Ok, |acc, h| match (acc, h) {
                (Health::Error, _) | (_, Health::Error) => Health::Error,
                (Health::Warn, _) | (_, Health::Warn) => Health::Warn,
                (a, _) => a,
            });
        let rep = ses.get(key);
        let health = match rep.map(|r| ses_health(r.worst())) {
            Some(Health::Error) => Health::Error,
            Some(Health::Warn) => {
                if drive_worst == Health::Error { Health::Error } else { Health::Warn }
            }
            _ => drive_worst,
        };
        let needs = members.iter().filter(|d| d.needs_reformat()).count();
        let mut detail = vec![format!("{} drives", members.len())];
        let mut metrics = vec![Metric::new("drives", members.len().to_string())];
        for hba in shelf_hbas(members, rep, &by_host) {
            metrics.push(Metric::new("hba", hba).tone("muted"));
        }
        let mut actions = Vec::new();
        if let Some(r) = rep {
            use crate::ses::{ET_COOLING, ET_POWER_SUPPLY};
            let (psu_ok, psu_n) = r.count(ET_POWER_SUPPLY);
            let (fan_ok, fan_n) = r.count(ET_COOLING);
            detail.push(format!("{:?}", r.worst()).to_lowercase());
            if r.esps.len() > 1 {
                detail.push(format!("{} paths", r.esps.len()));
            }
            if psu_n > 0 {
                let m = Metric::new("psu", format!("{psu_ok}/{psu_n}"));
                metrics.push(if psu_ok < psu_n { m.tone("error") } else { m });
            }
            if fan_n > 0 {
                let m = Metric::new("fans", format!("{fan_ok}/{fan_n}"));
                metrics.push(if fan_ok < fan_n { m.tone("error") } else { m });
            }
            if let Some(t) = r.max_temperature_c() {
                let m = Metric::new("temp", t.to_string()).unit("°C");
                metrics.push(if t >= 45 { m.tone("warn") } else { m });
            }
            let base = format!("/api/v1/shelves/{key}");
            actions.push(act("locate-on", "Locate shelf", "POST", format!("{base}/locate/on"), true, false));
            actions.push(act("locate-off", "Shelf LED off", "POST", format!("{base}/locate/off"), true, false));
            actions.push(act(
                "format-4k",
                "Reformat 520s → 4K",
                "POST",
                format!("{base}/format/4096"),
                needs > 0,
                true,
            ));
        }
        if needs > 0 {
            detail.push(format!("{needs} need reformat"));
            metrics.push(Metric::new("reformat", needs.to_string()).tone("warn"));
        }
        out.push(ComponentSummary {
            id: format!("shelf:{key}"),
            kind: "shelf".into(),
            label: display.clone(),
            health,
            detail: detail.join(" · "),
            metrics,
            actions,
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
            physical_block_size: 512,
            usable: true,
            format: None,
            firmware_update: None,
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
    fn unusable_drive_offers_reformat_and_nothing_destructive_else() {
        let mut d = drive();
        d.block_size = 520;
        d.usable = false;
        let c = drive_component(&d);
        assert!(c.detail.contains("520 B sectors"));
        let f = c.actions.iter().find(|a| a.id == "format-4k").unwrap();
        assert!(f.enabled && f.danger);
        assert!(f.path.ends_with("/format/4096"));
        assert!(!c.actions.iter().find(|a| a.id == "fleet-join").unwrap().enabled);
        assert!(!c.actions.iter().find(|a| a.id == "test-destructive").unwrap().enabled);
        assert!(c.metrics.iter().any(|m| m.label == "sector"));
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

    #[test]
    fn placement_is_data_not_prose() {
        let mut d = drive();
        d.location.bay = Some(4);
        d.location.controller = Some(Controller {
            scsi_host: Some("host7".into()),
            pcie_addr: Some("0000:03:00.0".into()),
            driver: Some("mpt3sas".into()),
        });
        let c = drive_component(&d);
        let bay = c.metrics.iter().find(|m| m.label == "bay").unwrap();
        assert_eq!(bay.value, "4", "a renderer orders by this, not by a regex");
        let hba = c.metrics.iter().find(|m| m.label == "hba").unwrap();
        assert_eq!(hba.value, "0000:03:00.0");
    }

    #[test]
    fn hba_falls_back_to_the_scsi_host_when_there_is_no_bdf() {
        let mut d = drive();
        d.location.controller = Some(Controller {
            scsi_host: Some("host7".into()),
            ..Default::default()
        });
        let c = drive_component(&d);
        assert_eq!(c.metrics.iter().find(|m| m.label == "hba").unwrap().value, "host7");
        assert!(c.metrics.iter().all(|m| m.label != "bay"), "no bay, no metric");
    }

    fn esp(scsi_id: &str) -> crate::ses::EspPath {
        crate::ses::EspPath {
            scsi_id: scsi_id.into(),
            sg_path: None,
            sas_address: None,
            serial: None,
        }
    }

    #[test]
    fn shelf_hbas_name_every_path() {
        let mut d = drive();
        d.location.controller = Some(Controller {
            scsi_host: Some("host7".into()),
            pcie_addr: Some("0000:03:00.0".into()),
            driver: Some("mpt3sas".into()),
        });
        let members = vec![&d];
        let by_host = BTreeMap::from([
            ("host7".to_string(), "0000:03:00.0".to_string()),
            ("host8".to_string(), "0000:81:00.0".to_string()),
        ]);
        let rep = crate::ses::ShelfReport {
            key: "k".into(),
            shelf: Shelf::default(),
            esps: vec![
                esp("7:0:8:0"),
                esp("8:0:8:0"),
            ],
            generation: 0,
            critical: false,
            noncritical: false,
            unrecoverable: false,
            info: false,
            elements: Vec::new(),
            slots: BTreeMap::new(),
            collected_at: SystemTime::now(),
            status_raw: Vec::new(),
        };
        // Both IOM paths, named once each — the drive's own controller is
        // already covered by host7.
        assert_eq!(
            shelf_hbas(&members, Some(&rep), &by_host),
            vec!["0000:03:00.0".to_string(), "0000:81:00.0".to_string()]
        );
        // No SES report: the members still name the card they hang off.
        assert_eq!(shelf_hbas(&members, None, &by_host), vec!["0000:03:00.0".to_string()]);
    }
}
