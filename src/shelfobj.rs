//! The `Shelf` object (#83): one per SES enclosure, `storage.storm.io/v1`,
//! cluster-scoped, named by the shelf's key (its logical id).
//!
//! The node that sees the shelf writes `status`: identity, IOMs and paths,
//! power supplies, fans, temperatures, bays → drives, problems, the LEDs as
//! the enclosure reports them, and `conditions` (Healthy, SinglePath,
//! PSUDegraded, FanDegraded, OverTemp). `spec` is the user's and the node
//! never writes it:
//!
//! - `identify: bool` — the shelf's locate LED (enclosure IDENT);
//! - `faultLed: bool` — the shelf's fault LED (enclosure REQUEST FAILURE);
//! - `defaultFormat {blockSize, protection}` — the format a drive in this
//!   shelf is planned to when its own Drive has no `spec.format`. The
//!   format belongs to a RAID set or a drive (owner, 2026-10-10); the shelf
//!   gives only the default for drives not yet assigned.
//!
//! Fan control (`cooling`) is not here: #90.
//!
//! Pure: the controller feeds it a [`ShelfReport`], the shelf summary and the
//! drives in the shelf, and acts on what [`ShelfSpec`] and [`transitions`]
//! return.

use crate::pi::Protection;
use crate::ses::{ElementStatus, ShelfReport, ET_COOLING, ET_ENCLOSURE, ET_POWER_SUPPLY, ET_TEMPERATURE};
use crate::shelfview::Summary;
use serde_json::{json, Value};

/// The condition types, in the order they appear in `status.conditions`.
pub const CONDITIONS: [&str; 5] = ["Healthy", "SinglePath", "PSUDegraded", "FanDegraded", "OverTemp"];

/// `spec` as the user wrote it. `None` = not asked: the node leaves that
/// LED as it is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShelfSpec {
    pub identify: Option<bool>,
    pub fault_led: Option<bool>,
    pub default_format: Option<(u32, Protection)>,
}

impl ShelfSpec {
    pub fn parse(spec: &Value) -> ShelfSpec {
        ShelfSpec {
            identify: spec["identify"].as_bool(),
            fault_led: spec["faultLed"].as_bool(),
            default_format: spec
                .get("defaultFormat")
                .and_then(|f| crate::api::kube::wanted_format(&json!({ "format": f }))),
        }
    }
}

/// A drive in the shelf, as the Shelf status lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct BayDrive {
    pub bay: Option<u32>,
    pub drive: String,
    pub name: String,
    pub model: Option<String>,
    pub health: String,
    pub block_size: u32,
    pub needs_reformat: bool,
}

/// One condition: `status` "True"/"False", a CamelCase reason and a message.
#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    pub kind: &'static str,
    pub status: bool,
    pub reason: String,
    pub message: String,
}

fn enclosure(rep: &ShelfReport) -> Option<&crate::ses::Element> {
    let i = crate::ses::find_enclosure_element(&rep.elements)?;
    rep.elements.get(i).filter(|e| e.element_type == ET_ENCLOSURE)
}

fn bad(s: ElementStatus) -> bool {
    s.is_bad()
}

/// The five conditions from the report and its summary.
pub fn conditions(rep: &ShelfReport, sm: &Summary) -> Vec<Condition> {
    let mut out = Vec::new();

    let psu_bad: Vec<String> = sm
        .power_supplies
        .iter()
        .filter(|p| p.installed && (bad(p.status) || !p.flags.is_empty() && p.status != ElementStatus::Ok))
        .map(|p| format!("power supply {} {:?}{}", p.index, p.status, if p.flags.is_empty() { String::new() } else { format!(" ({})", p.flags.join(", ")) }))
        .collect();
    let (psu_ok, psu_n) = rep.count(ET_POWER_SUPPLY);
    let fans_bad: Vec<String> = rep
        .elements
        .iter()
        .filter(|e| e.element_type == ET_COOLING && !e.overall && bad(e.status))
        .map(|e| format!("fan {} {:?}{}", e.index, e.status, e.rpm.map(|r| format!(" at {r} rpm")).unwrap_or_default()))
        .collect();
    let (fan_ok, fan_n) = rep.count(ET_COOLING);
    let hot: Vec<String> = rep
        .elements
        .iter()
        .filter(|e| e.element_type == ET_TEMPERATURE && !e.overall)
        .filter(|e| {
            let over_flag = e.flags.iter().any(|f| f.starts_with("overtemp"));
            let over_limit = match (e.temperature_c, e.thresholds.as_ref().and_then(|t| t.high_warning.or(t.high_critical))) {
                (Some(t), Some(lim)) => t as f32 >= lim,
                _ => false,
            };
            over_flag || over_limit || matches!(e.status, ElementStatus::Critical | ElementStatus::Unrecoverable)
        })
        .map(|e| {
            let name = e.name.clone().unwrap_or_else(|| format!("sensor {}", e.index));
            format!("{name} {}", e.temperature_c.map(|t| format!("{t} °C")).unwrap_or_else(|| format!("{:?}", e.status)))
        })
        .collect();
    let single = sm.multipath.paths <= 1 || (sm.multipath.ioms_installed >= 2 && sm.multipath.ioms_with_path < sm.multipath.ioms_installed && sm.multipath.ioms_with_path > 0);

    let worst = rep.worst();
    let healthy = !bad(worst) && psu_bad.is_empty() && fans_bad.is_empty() && hot.is_empty();
    out.push(Condition {
        kind: "Healthy",
        status: healthy,
        reason: if healthy { "AllOk".into() } else { format!("{worst:?}") },
        message: if healthy {
            "every element reports OK".into()
        } else if sm.problems.is_empty() {
            format!("enclosure reports {worst:?}")
        } else {
            sm.problems.iter().map(|p| format!("{} {:?}", p.element, p.status)).collect::<Vec<_>>().join("; ")
        },
    });
    out.push(Condition {
        kind: "SinglePath",
        status: single,
        reason: if single { "OnePath".into() } else { "Multipath".into() },
        message: sm.multipath.note.clone(),
    });
    out.push(Condition {
        kind: "PSUDegraded",
        status: !psu_bad.is_empty(),
        reason: if psu_bad.is_empty() { "PowerOk".into() } else { "PowerSupplyFault".into() },
        message: if psu_bad.is_empty() { format!("{psu_ok} of {psu_n} installed power supplies OK") } else { psu_bad.join("; ") },
    });
    out.push(Condition {
        kind: "FanDegraded",
        status: !fans_bad.is_empty(),
        reason: if fans_bad.is_empty() { "FansOk".into() } else { "FanFault".into() },
        message: if fans_bad.is_empty() { format!("{fan_ok} of {fan_n} fans OK") } else { fans_bad.join("; ") },
    });
    out.push(Condition {
        kind: "OverTemp",
        status: !hot.is_empty(),
        reason: if hot.is_empty() { "TemperatureOk".into() } else { "OverTemperature".into() },
        message: if hot.is_empty() {
            match rep.max_temperature_c() {
                Some(t) => format!("hottest sensor {t} °C"),
                None => "no temperature reported".into(),
            }
        } else {
            hot.join("; ")
        },
    });
    out
}

/// `status.conditions`: `lastTransitionTime` kept from `previous` (the
/// object's current status) when the condition's status did not change.
pub fn conditions_json(now: &[Condition], previous: &Value, at: &str) -> Value {
    Value::Array(
        now.iter()
            .map(|c| {
                let st = if c.status { "True" } else { "False" };
                let prev = previous.as_array().into_iter().flatten().find(|p| p["type"] == c.kind);
                let since = match prev {
                    Some(p) if p["status"] == st => p["lastTransitionTime"].clone(),
                    _ => json!(at),
                };
                json!({ "type": c.kind, "status": st, "reason": c.reason, "message": c.message, "lastTransitionTime": since })
            })
            .collect(),
    )
}

/// Conditions whose status changed since `previous` (a condition never
/// reported before counts only when it is a problem: a new Shelf object
/// starts quiet unless something is wrong). Each is an Event: (type,
/// reason, message), Warning when the change is to a bad state.
pub fn transitions(now: &[Condition], previous: &Value) -> Vec<(&'static str, String, String)> {
    let mut out = Vec::new();
    for c in now {
        let st = if c.status { "True" } else { "False" };
        let prev = previous.as_array().into_iter().flatten().find(|p| p["type"] == c.kind);
        let bad_now = if c.kind == "Healthy" { !c.status } else { c.status };
        match prev {
            Some(p) if p["status"] == st => {}
            None if !bad_now => {}
            _ => {
                let kind = if bad_now { "Warning" } else { "Normal" };
                out.push((kind, format!("{}{}", c.kind, if c.status { "True" } else { "False" }), c.message.clone()));
            }
        }
    }
    out
}

/// The enclosure's own LEDs as it reports them: (identify, fault requested,
/// fault indicated).
pub fn leds(rep: &ShelfReport) -> (Option<bool>, Option<bool>, Option<bool>) {
    match enclosure(rep) {
        Some(e) => (Some(e.ident), Some(e.raw[3] & 0x02 != 0), Some(e.raw[2] & 0x02 != 0)),
        None => (None, None, None),
    }
}

/// The LED changes `spec` asks for that the enclosure does not show yet:
/// (identify, faultLed), each Some(wanted) when it must be sent.
pub fn led_actions(spec: &ShelfSpec, rep: &ShelfReport) -> (Option<bool>, Option<bool>) {
    let (ident, fault, _) = leds(rep);
    let id = spec.identify.filter(|want| ident.is_some_and(|now| now != *want));
    let fl = spec.fault_led.filter(|want| fault.is_some_and(|now| now != *want));
    (id, fl)
}

/// The Shelf object this node writes (status, labels; `spec` empty — the
/// user's). `previous` is the object's current status (for transition
/// times); `at` the RFC 3339 time now.
pub fn shelf_object(rep: &ShelfReport, sm: &Summary, drives: &[BayDrive], node: &str, previous: &Value, at: &str) -> Value {
    let (ident, fault_req, fault_ind) = leds(rep);
    let fans: Vec<Value> = rep
        .elements
        .iter()
        .filter(|e| e.element_type == ET_COOLING && !e.overall)
        .map(|e| json!({
            "index": e.index, "name": e.name, "status": e.status, "rpm": e.rpm,
            // SES cooling status byte 3 bits 2:0: 0 stopped … 7 highest.
            "speedCode": e.raw[3] & 0x07, "flags": e.flags,
        }))
        .collect();
    let temps: Vec<Value> = rep
        .elements
        .iter()
        .filter(|e| e.element_type == ET_TEMPERATURE && !e.overall)
        .map(|e| json!({
            "index": e.index, "name": e.name, "status": e.status, "celsius": e.temperature_c,
            "thresholds": e.thresholds, "flags": e.flags,
        }))
        .collect();
    let bays: Vec<Value> = drives
        .iter()
        .map(|d| json!({
            "bay": d.bay, "drive": d.drive, "name": d.name, "model": d.model, "health": d.health,
            "blockSize": d.block_size, "needsReformat": d.needs_reformat,
        }))
        .collect();
    let conds = conditions(rep, sm);
    let mut labels = serde_json::Map::new();
    labels.insert("storm.io/component".into(), json!("stormdrive"));
    if let Some(n) = crate::controller::label_value(node) {
        labels.insert("storm.io/node".into(), json!(n));
    }
    json!({
        "apiVersion": format!("{}/{}", crate::api::kube::GROUP, crate::api::kube::VERSION),
        "kind": "Shelf",
        "metadata": { "name": rep.key, "labels": labels },
        "status": {
            "node": node,
            "present": true,
            "vendor": rep.shelf.vendor,
            "model": rep.shelf.model,
            "shelfId": sm.shelf_id,
            "serial": sm.serial.clone().or_else(|| rep.shelf.serial.clone()),
            "partNumber": sm.part_number,
            "logicalId": rep.shelf.logical_id,
            "state": rep.worst(),
            "paths": rep.esps.iter().map(|p| json!({ "scsiId": p.scsi_id, "sasAddress": p.sas_address, "firmware": p.revision })).collect::<Vec<_>>(),
            "ioms": sm.ioms,
            "powerSupplies": sm.power_supplies,
            "connectors": sm.connectors,
            "multipath": sm.multipath,
            "fans": fans,
            "temperatures": temps,
            "bays": bays,
            "problems": sm.problems,
            "identify": ident,
            "faultLed": fault_req,
            "faultIndicated": fault_ind,
            "conditions": conditions_json(&conds, &previous["conditions"], at),
        },
    })
}

/// What decides whether status must be rewritten: everything but the
/// transition times (which only change with a status anyway).
pub fn fingerprint(obj: &Value) -> String {
    let mut st = obj["status"].clone();
    if let Some(cs) = st["conditions"].as_array_mut() {
        for c in cs {
            if let Some(m) = c.as_object_mut() {
                m.remove("lastTransitionTime");
            }
        }
    }
    // Fan speeds and temperatures move all the time; the controller
    // rewrites at least every REFRESH anyway.
    if let Some(fs) = st["fans"].as_array_mut() {
        for f in fs {
            if let Some(m) = f.as_object_mut() {
                m.remove("rpm");
            }
        }
    }
    if let Some(ts) = st["temperatures"].as_array_mut() {
        for t in ts {
            if let Some(m) = t.as_object_mut() {
                m.remove("celsius");
            }
        }
    }
    st.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ses::{Element, EspPath, ShelfReport};
    use std::collections::BTreeMap;

    fn el(t: u8, index: u32, status: ElementStatus, raw: [u8; 4]) -> Element {
        Element {
            element_type: t,
            type_name: crate::ses::element_type_name(t).into(),
            index,
            overall: false,
            name: None,
            status,
            predicted_failure: false,
            disabled: false,
            swapped: false,
            ident: raw[1] & 0x80 != 0,
            fault: false,
            temperature_c: None,
            rpm: None,
            volts: None,
            amps: None,
            bay: None,
            sas_address: None,
            flags: vec![],
            attributes: BTreeMap::new(),
            thresholds: None,
            raw,
        }
    }

    fn report(esps: usize) -> ShelfReport {
        let mut fan = el(ET_COOLING, 0, ElementStatus::Ok, [1, 0, 0x30, 0x03]);
        fan.rpm = Some(480);
        let mut t = el(ET_TEMPERATURE, 0, ElementStatus::Ok, [1, 0, 50, 0]);
        t.temperature_c = Some(30);
        ShelfReport {
            key: "500a09800e359135".into(),
            shelf: crate::drive::Shelf { logical_id: Some("500a09800e359135".into()), vendor: Some("NETAPP".into()), model: Some("DS22412IOM12A".into()), ..Default::default() },
            esps: (0..esps)
                .map(|i| EspPath { scsi_id: format!("0:0:{i}:0"), sg_path: None, sas_address: Some(format!("500a09800853bf4{}", 12 + i)), serial: None, revision: Some("0401".into()) })
                .collect(),
            generation: 7,
            critical: false,
            noncritical: false,
            unrecoverable: false,
            info: false,
            elements: vec![
                el(ET_POWER_SUPPLY, 0, ElementStatus::Ok, [1, 0, 0, 0]),
                fan,
                t,
                el(ET_ENCLOSURE, 0, ElementStatus::Ok, [1, 0, 0, 0]),
            ],
            slots: BTreeMap::new(),
            help_text: None,
            collected_at: std::time::SystemTime::UNIX_EPOCH,
            status_raw: vec![],
        }
    }

    fn summary(rep: &ShelfReport) -> Summary {
        crate::shelfview::summarize(rep, &BTreeMap::new())
    }

    fn cond<'a>(cs: &'a [Condition], k: &str) -> &'a Condition {
        cs.iter().find(|c| c.kind == k).unwrap()
    }

    #[test]
    fn a_healthy_two_path_shelf() {
        let r = report(2);
        let cs = conditions(&r, &summary(&r));
        assert_eq!(cs.iter().map(|c| c.kind).collect::<Vec<_>>(), CONDITIONS);
        assert!(cond(&cs, "Healthy").status);
        assert!(!cond(&cs, "SinglePath").status);
        assert!(!cond(&cs, "PSUDegraded").status && !cond(&cs, "FanDegraded").status && !cond(&cs, "OverTemp").status);
        assert_eq!(cond(&cs, "OverTemp").message, "hottest sensor 30 °C");
    }

    #[test]
    fn faults_raise_their_conditions() {
        let mut r = report(1);
        r.elements[0] = el(ET_POWER_SUPPLY, 0, ElementStatus::Critical, [2, 0, 0, 0x02]);
        r.elements[0].flags = vec!["ac fail".into()];
        r.elements[1].status = ElementStatus::Noncritical;
        r.elements[2].flags = vec!["overtemp warning".into()];
        r.critical = true;
        let cs = conditions(&r, &summary(&r));
        assert!(!cond(&cs, "Healthy").status);
        assert!(cond(&cs, "SinglePath").status, "one ESP");
        assert_eq!(cond(&cs, "PSUDegraded").message, "power supply 0 Critical (ac fail)");
        assert!(cond(&cs, "FanDegraded").message.starts_with("fan 0 Noncritical at 480 rpm"));
        assert!(cond(&cs, "OverTemp").status);
    }

    #[test]
    fn over_the_page_05_limit_is_over_temp() {
        let mut r = report(2);
        r.elements[2].thresholds = Some(crate::ses::Thresholds { unit: "°C".into(), high_critical: Some(45.0), high_warning: Some(30.0), low_warning: None, low_critical: None });
        let cs = conditions(&r, &summary(&r));
        assert!(cond(&cs, "OverTemp").status);
    }

    #[test]
    fn transition_times_are_kept_and_events_only_on_change() {
        let r = report(2);
        let cs = conditions(&r, &summary(&r));
        // A new object: nothing wrong → no events.
        assert!(transitions(&cs, &Value::Null).is_empty());
        let first = conditions_json(&cs, &Value::Null, "t0");
        let again = conditions_json(&cs, &first, "t1");
        assert_eq!(again, first, "unchanged → t0 kept");
        // A fan fails.
        let mut r2 = report(2);
        r2.elements[1].status = ElementStatus::Critical;
        let cs2 = conditions(&r2, &summary(&r2));
        let ev = transitions(&cs2, &first);
        assert!(ev.contains(&("Warning", "HealthyFalse".into(), cond(&cs2, "Healthy").message.clone())));
        assert!(ev.iter().any(|(k, r, _)| *k == "Warning" && r == "FanDegradedTrue"));
        let after = conditions_json(&cs2, &first, "t2");
        let fan = after.as_array().unwrap().iter().find(|c| c["type"] == "FanDegraded").unwrap();
        assert_eq!(fan["lastTransitionTime"], "t2");
        // And recovers: Normal events.
        let back = transitions(&cs, &after);
        assert!(back.iter().any(|(k, r, _)| *k == "Normal" && r == "FanDegradedFalse"));
        // A new object that is already bad says so.
        assert!(!transitions(&cs2, &Value::Null).is_empty());
    }

    #[test]
    fn spec_and_leds() {
        let spec = ShelfSpec::parse(&json!({ "identify": true, "faultLed": false, "defaultFormat": { "blockSize": 4096, "protection": "type1" } }));
        assert_eq!(spec.identify, Some(true));
        assert_eq!(spec.fault_led, Some(false));
        assert_eq!(spec.default_format, Some((4096, Protection::Type1)));
        assert_eq!(ShelfSpec::parse(&json!({})), ShelfSpec::default());
        let mut r = report(2);
        assert_eq!(led_actions(&spec, &r), (Some(true), None), "ident off → on; fault already off");
        r.elements[3] = el(ET_ENCLOSURE, 0, ElementStatus::Ok, [1, 0x80, 0, 0x02]);
        assert_eq!(leds(&r), (Some(true), Some(true), Some(false)));
        assert_eq!(led_actions(&spec, &r), (None, Some(false)));
        // No enclosure element: nothing to send.
        r.elements.pop();
        assert_eq!(led_actions(&spec, &r), (None, None));
    }

    #[test]
    fn the_object() {
        let r = report(2);
        let d = BayDrive { bay: Some(0), drive: "ca0aa25f".into(), name: "sdb".into(), model: Some("ST1200MM0098".into()), health: "healthy".into(), block_size: 520, needs_reformat: true };
        let o = shelf_object(&r, &summary(&r), &[d], "c2nr0q2", &Value::Null, "t0");
        assert_eq!(o["kind"], "Shelf");
        assert_eq!(o["metadata"]["name"], "500a09800e359135");
        assert_eq!(o["metadata"]["labels"]["storm.io/node"], "c2nr0q2");
        assert!(o.get("spec").is_none(), "spec is the user's");
        let st = &o["status"];
        assert_eq!(st["fans"][0]["rpm"], 480);
        assert_eq!(st["fans"][0]["speedCode"], 3);
        assert_eq!(st["temperatures"][0]["celsius"], 30);
        assert_eq!(st["bays"][0]["needsReformat"], true);
        assert_eq!(st["identify"], false);
        assert_eq!(st["conditions"].as_array().unwrap().len(), 5);
        // Fan speed alone does not make a rewrite due.
        let mut r2 = r.clone();
        r2.elements[1].rpm = Some(520);
        let o2 = shelf_object(&r2, &summary(&r2), &[], "c2nr0q2", &Value::Null, "t9");
        let o1 = shelf_object(&r, &summary(&r), &[], "c2nr0q2", &Value::Null, "t0");
        assert_eq!(fingerprint(&o1), fingerprint(&o2));
    }
}
