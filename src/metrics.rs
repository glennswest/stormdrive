//! `GET /metrics` (#18, stormcos#64): what stormdrive knows about every
//! drive and shelf, in the Prometheus text format.
//!
//! Upstream `smartctl_exporter` names where one fits, so a dashboard built
//! for it reads these too; `stormdrive_*` for the rest. Every drive series
//! carries `device`, `serial`, `model`, `enclosure` and `bay` (empty when
//! unknown, which Prometheus treats as absent).
//!
//! Rendered from the cached inventory and the last SES scan: a scrape never
//! touches a drive. A drive the node cannot see (`missing`) keeps its info
//! and last-poll series and drops its readings, which would be stale.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::drive::{Activity, Drive, DriveKind, HealthStatus};
use crate::poller::PollStats;
use crate::ses::ElementStatus;
use crate::topology::Shelves;

pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// One metric family: its samples, in the order they were added.
struct Family {
    help: &'static str,
    kind: &'static str,
    samples: Vec<(String, f64)>,
}

#[derive(Default)]
struct Out {
    order: Vec<&'static str>,
    families: BTreeMap<&'static str, Family>,
}

impl Out {
    fn add(&mut self, name: &'static str, kind: &'static str, help: &'static str, labels: &[(&str, String)], v: f64) {
        let f = self.families.entry(name).or_insert_with(|| {
            self.order.push(name);
            Family { help, kind, samples: vec![] }
        });
        f.samples.push((render_labels(labels), v));
    }

    fn gauge(&mut self, name: &'static str, help: &'static str, labels: &[(&str, String)], v: f64) {
        self.add(name, "gauge", help, labels, v);
    }

    fn counter(&mut self, name: &'static str, help: &'static str, labels: &[(&str, String)], v: f64) {
        self.add(name, "counter", help, labels, v);
    }

    fn finish(self) -> String {
        let mut s = String::new();
        for name in &self.order {
            let f = &self.families[name];
            let _ = writeln!(s, "# HELP {name} {}", f.help);
            let _ = writeln!(s, "# TYPE {name} {}", f.kind);
            for (l, v) in &f.samples {
                let _ = writeln!(s, "{name}{l} {}", fmt_value(*v));
            }
        }
        s
    }
}

fn fmt_value(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Label value escaping per the text format: backslash, quote, newline.
pub fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn render_labels(labels: &[(&str, String)]) -> String {
    if labels.is_empty() {
        return String::new();
    }
    let body: Vec<String> = labels.iter().map(|(k, v)| format!("{k}=\"{}\"", escape(v))).collect();
    format!("{{{}}}", body.join(","))
}

fn word<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

fn unix(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// The labels every drive series carries.
fn drive_labels(d: &Drive) -> Vec<(&'static str, String)> {
    vec![
        ("device", d.name.clone()),
        ("serial", d.serial.clone()),
        ("model", d.model.clone()),
        ("enclosure", d.location.shelf.as_ref().and_then(|s| s.key()).unwrap_or_default()),
        ("bay", d.location.bay.map(|b| b.to_string()).unwrap_or_default()),
    ]
}

fn with(base: &[(&'static str, String)], extra: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
    base.iter().chain(extra).cloned().collect()
}

fn interface(k: DriveKind) -> &'static str {
    match k {
        DriveKind::NvmeSsd => "nvme",
        DriveKind::SasSsd | DriveKind::SasHdd => "scsi",
        DriveKind::SataSsd | DriveKind::SataHdd => "sat",
        DriveKind::Unknown => "unknown",
    }
}

const STATUSES: [HealthStatus; 5] =
    [HealthStatus::Unknown, HealthStatus::Good, HealthStatus::Warning, HealthStatus::Failing, HealthStatus::Failed];

/// The whole page.
pub fn render<'a>(
    version: &str,
    node: &str,
    drives: impl IntoIterator<Item = &'a Drive>,
    shelves: &Shelves,
    poll: &PollStats,
) -> String {
    let mut o = Out::default();
    o.gauge("stormdrive_build_info", "stormdrive's version and node; always 1.", &[("version", version.into()), ("node", node.into())], 1.0);

    let mut drives: Vec<&Drive> = drives.into_iter().collect();
    drives.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.0.cmp(&b.id.0)));
    for d in drives {
        drive(&mut o, d);
    }
    for (key, s) in shelves {
        shelf(&mut o, key, s);
    }
    poller(&mut o, poll);
    o.finish()
}

fn drive(o: &mut Out, d: &Drive) {
    let l = drive_labels(d);
    let h = &d.health;
    o.gauge(
        "smartctl_device",
        "Device info (smartctl_exporter's name); always 1.",
        &with(&l, &[("interface", interface(d.kind).into()), ("firmware_version", d.firmware.clone())]),
        1.0,
    );
    o.gauge(
        "stormdrive_drive_info",
        "What stormdrive knows about the drive; always 1.",
        &with(
            &l,
            &[
                ("id", d.id.0.to_string()),
                ("wwn", d.wwid.clone().unwrap_or_default()),
                ("kind", word(&d.kind)),
                ("firmware", d.firmware.clone()),
                ("membership", word(&d.membership)),
                ("designation", word(&d.designation)),
                ("activity", word(&d.activity)),
                ("owner", d.owner().into()),
            ],
        ),
        1.0,
    );
    if let Some(t) = h.collected_at {
        o.gauge(
            "stormdrive_drive_last_poll_timestamp_seconds",
            "Unix time of the last health poll the drive answered.",
            &l,
            unix(t),
        );
    }
    if d.activity == Activity::Missing {
        return;
    }
    o.gauge("smartctl_device_capacity_bytes", "Capacity in bytes.", &l, d.capacity_bytes as f64);
    o.gauge("smartctl_device_block_size", "Logical block size as the drive reports it.", &with(&l, &[("blocks_type", "logical".into())]), d.block_size as f64);
    if d.physical_block_size > 0 {
        o.gauge("smartctl_device_block_size", "Logical block size as the drive reports it.", &with(&l, &[("blocks_type", "physical".into())]), d.physical_block_size as f64);
    }
    let status = h.status();
    for s in STATUSES {
        o.gauge(
            "stormdrive_drive_health_status",
            "Health verdict (threshold engine, with hysteresis): 1 for the current status.",
            &with(&l, &[("status", word(&s))]),
            (s == status) as u8 as f64,
        );
    }
    match status {
        HealthStatus::Good | HealthStatus::Warning => {
            o.gauge("smartctl_device_smart_status", "1 when health passes, 0 when failing or failed.", &l, 1.0)
        }
        HealthStatus::Failing | HealthStatus::Failed => {
            o.gauge("smartctl_device_smart_status", "1 when health passes, 0 when failing or failed.", &l, 0.0)
        }
        HealthStatus::Unknown => {}
    }
    if h.collected_at.is_none() {
        return;
    }
    if let Some(t) = h.temperature_c {
        o.gauge("smartctl_device_temperature", "Temperature in °C.", &with(&l, &[("temperature_type", "current".into())]), t as f64);
    }
    if let Some(p) = h.power_on_hours {
        o.counter("smartctl_device_power_on_seconds", "Power-on time in seconds.", &l, (p * 3600) as f64);
    }
    if let Some(w) = h.wear_pct {
        o.gauge("smartctl_device_percentage_used", "Endurance used, percent (may exceed 100).", &l, w as f64);
    }
    if let Some(s) = h.available_spare_pct {
        o.gauge("smartctl_device_available_spare", "Available spare, percent.", &l, s as f64);
    }
    if d.kind == DriveKind::NvmeSsd {
        o.gauge("smartctl_device_critical_warning", "NVMe critical warning bits.", &l, h.critical_warning as f64);
        o.counter("smartctl_device_media_errors", "NVMe media and data integrity errors.", &l, h.media_errors as f64);
        if let Some(n) = &h.nvme {
            o.gauge("smartctl_device_available_spare_threshold", "Available spare threshold, percent.", &l, n.available_spare_threshold_pct as f64);
            o.counter("smartctl_device_num_err_log_entries", "NVMe error information log entries.", &l, n.error_log_entries as f64);
            o.counter("smartctl_device_power_cycle_count", "Power cycles.", &l, n.power_cycles as f64);
            o.counter("smartctl_device_bytes_read", "Bytes read (NVMe data units × 512000).", &l, n.bytes_read as f64);
            o.counter("smartctl_device_bytes_written", "Bytes written (NVMe data units × 512000).", &l, n.bytes_written as f64);
            o.counter("stormdrive_drive_unsafe_shutdowns_total", "NVMe unsafe shutdowns.", &l, n.unsafe_shutdowns as f64);
        }
    } else {
        // sysfs ioerr_cnt: commands the kernel saw fail, since boot. Not
        // media errors; SCSI log sense / ATA SMART (grown defects,
        // reallocated / pending sectors) is #22.
        o.counter("stormdrive_drive_io_errors_total", "Commands that failed on the drive since boot (sysfs ioerr_cnt).", &l, h.media_errors as f64);
    }
    if let Some(u) = &d.usage {
        o.gauge("stormdrive_drive_used_bytes", "Bytes allocated in stormblock slabs on the drive.", &l, u.used_bytes as f64);
        o.gauge("stormdrive_drive_free_bytes", "Capacity minus used.", &l, u.free_bytes as f64);
    }
}

fn shelf(o: &mut Out, key: &str, s: &crate::ses::ShelfReport) {
    let l = vec![("enclosure", key.to_string())];
    o.gauge(
        "stormdrive_enclosure_info",
        "A SAS shelf the node talks to over SES; always 1.",
        &with(
            &l,
            &[
                ("vendor", s.shelf.vendor.clone().unwrap_or_default()),
                ("model", s.shelf.model.clone().unwrap_or_default()),
                ("serial", s.shelf.serial.clone().unwrap_or_default()),
            ],
        ),
        1.0,
    );
    o.gauge("stormdrive_enclosure_ok", "1 when no shelf element reports critical, noncritical or unrecoverable.", &l, (s.worst() == ElementStatus::Ok) as u8 as f64);
    o.gauge("stormdrive_enclosure_last_scan_timestamp_seconds", "Unix time of the last SES scan of the shelf.", &l, unix(s.collected_at));
    for e in s.elements.iter().filter(|e| !e.overall) {
        if matches!(e.status, ElementStatus::NotInstalled | ElementStatus::Unsupported) {
            continue;
        }
        let el = with(&l, &[("type", e.type_name.clone()), ("index", e.index.to_string())]);
        o.gauge("stormdrive_enclosure_element_ok", "1 when the element's SES status is OK.", &el, (e.status == ElementStatus::Ok) as u8 as f64);
        if let Some(t) = e.temperature_c {
            o.gauge("stormdrive_enclosure_temperature_celsius", "Temperature sensor reading, °C.", &el, t as f64);
        }
        if let Some(r) = e.rpm {
            o.gauge("stormdrive_enclosure_fan_rpm", "Cooling element speed.", &el, r as f64);
        }
        if let Some(v) = e.volts {
            o.gauge("stormdrive_enclosure_volts", "Voltage sensor reading.", &el, v as f64);
        }
        if let Some(a) = e.amps {
            o.gauge("stormdrive_enclosure_amps", "Current sensor reading.", &el, a as f64);
        }
    }
}

fn poller(o: &mut Out, p: &PollStats) {
    o.gauge("stormdrive_poll_drives", "Drives the health poller samples.", &[], p.drives as f64);
    o.gauge("stormdrive_poll_in_flight", "Health reads running now.", &[], p.in_flight as f64);
    o.gauge("stormdrive_poll_stuck", "Drives whose last health read has not returned.", &[], p.stuck.len() as f64);
    o.counter("stormdrive_poll_samples_total", "Health reads that returned.", &[], p.samples as f64);
    o.counter("stormdrive_poll_timeouts_total", "Health reads that timed out.", &[], p.timeouts as f64);
    if let Some(a) = p.avg_sample_ms {
        o.gauge("stormdrive_poll_sample_seconds", "Moving average of one health read.", &[], a / 1000.0);
    }
    if let Some(l) = p.load_pct {
        o.gauge("stormdrive_poll_load_ratio", "Share of the poller's capacity one cycle uses (over 1 = behind).", &[], l / 100.0);
    }
    if let Some(ms) = p.discovery_ms {
        o.gauge("stormdrive_discovery_seconds", "Wall time of the last discovery pass.", &[], ms as f64 / 1000.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::{HealthReport, Location, Shelf};
    use crate::smart::NvmeCounters;

    fn drive(name: &str, kind: &str, over: impl FnOnce(&mut Drive)) -> Drive {
        let mut d: Drive = serde_json::from_value(serde_json::json!({
            "id": crate::drive::DriveId::derive(Some(&format!("naa.{name}")), "M", name), "path": format!("/dev/{name}"), "name": name,
            "paths": [format!("/dev/{name}")], "kind": kind, "model": "ST1200MM0098", "serial": format!("S-{name}"), "firmware": "N003",
            "wwid": format!("naa.{name}"), "capacity_bytes": 1_200_243_695_616u64, "block_size": 512,
            "first_seen": {"secs_since_epoch": 0, "nanos_since_epoch": 0}, "last_seen": {"secs_since_epoch": 0, "nanos_since_epoch": 0},
        }))
        .unwrap();
        over(&mut d);
        d
    }

    /// Every line is a comment or `name{labels} value`; every family has
    /// one HELP and one TYPE before its samples.
    fn assert_valid(page: &str) {
        let mut typed = std::collections::HashSet::new();
        for line in page.lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                assert!(typed.insert(rest.split(' ').next().unwrap().to_string()), "TYPE twice: {line}");
                continue;
            }
            if line.starts_with('#') {
                continue;
            }
            let (name, value) = line.rsplit_once(' ').unwrap();
            let base = name.split('{').next().unwrap();
            assert!(typed.contains(base), "sample before its TYPE: {line}");
            assert!(base.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':'), "{line}");
            value.parse::<f64>().unwrap_or_else(|_| panic!("value: {line}"));
            if name.contains('{') {
                assert!(name.ends_with('}'), "{line}");
            }
        }
    }

    fn sample<'a>(page: &'a str, prefix: &str) -> Option<&'a str> {
        page.lines().find(|l| l.starts_with(prefix)).and_then(|l| l.rsplit_once(' ')).map(|(_, v)| v)
    }

    #[test]
    fn a_shelf_hdd_and_an_nvme_ssd() {
        let hdd = drive("sdb", "sas_hdd", |d| {
            d.location = Location {
                shelf: Some(Shelf { logical_id: Some("5000a098aaaa0001".into()), model: Some("DS224C".into()), ..Default::default() }),
                bay: Some(4),
                ..Default::default()
            };
            d.health = HealthReport {
                status: Some(HealthStatus::Good),
                temperature_c: Some(31),
                media_errors: 2,
                collected_at: Some(UNIX_EPOCH + std::time::Duration::from_secs(1_791_300_000)),
                ..Default::default()
            };
        });
        let nvme = drive("nvme0n1", "nvme_ssd", |d| {
            d.model = "Samsung \"PM9A3\"".into();
            d.health = HealthReport {
                status: Some(HealthStatus::Failing),
                temperature_c: Some(42),
                power_on_hours: Some(10),
                wear_pct: Some(13),
                available_spare_pct: Some(87),
                critical_warning: 4,
                media_errors: 7,
                collected_at: Some(UNIX_EPOCH + std::time::Duration::from_secs(1_791_300_001)),
                nvme: Some(NvmeCounters { available_spare_threshold_pct: 10, bytes_read: 512_000, bytes_written: 1_024_000, power_cycles: 41, unsafe_shutdowns: 5, error_log_entries: 9 }),
                ..Default::default()
            };
        });
        let gone = drive("sdz", "sata_hdd", |d| {
            d.activity = Activity::Missing;
            d.health.temperature_c = Some(99);
        });
        let page = render("0.19.0", "n1", [&hdd, &nvme, &gone], &Shelves::new(), &PollStats { drives: 3, samples: 12, ..Default::default() });
        assert_valid(&page);

        let hl = r#"{device="sdb",serial="S-sdb",model="ST1200MM0098",enclosure="5000a098aaaa0001",bay="4""#;
        assert_eq!(sample(&page, &format!("smartctl_device_temperature{hl},temperature_type=\"current\"}}")), Some("31"));
        assert_eq!(sample(&page, &format!("smartctl_device_smart_status{hl}}}")), Some("1"));
        assert_eq!(sample(&page, &format!("stormdrive_drive_io_errors_total{hl}}}")), Some("2"));
        assert_eq!(sample(&page, &format!("stormdrive_drive_last_poll_timestamp_seconds{hl}}}")), Some("1791300000"));
        assert_eq!(sample(&page, &format!("stormdrive_drive_health_status{hl},status=\"good\"}}")), Some("1"));
        assert_eq!(sample(&page, &format!("stormdrive_drive_health_status{hl},status=\"failed\"}}")), Some("0"));
        assert!(!page.contains("smartctl_device_media_errors{device=\"sdb\""), "ioerr_cnt is not media errors");

        // Quotes in a label value are escaped; NVMe gets its counters.
        let nl = r#"{device="nvme0n1",serial="S-nvme0n1",model="Samsung \"PM9A3\"",enclosure="",bay="""#;
        assert_eq!(sample(&page, &format!("smartctl_device_smart_status{nl}}}")), Some("0"));
        assert_eq!(sample(&page, &format!("smartctl_device_power_on_seconds{nl}}}")), Some("36000"));
        assert_eq!(sample(&page, &format!("smartctl_device_percentage_used{nl}}}")), Some("13"));
        assert_eq!(sample(&page, &format!("smartctl_device_media_errors{nl}}}")), Some("7"));
        assert_eq!(sample(&page, &format!("smartctl_device_num_err_log_entries{nl}}}")), Some("9"));
        assert_eq!(sample(&page, &format!("smartctl_device_bytes_written{nl}}}")), Some("1024000"));
        assert_eq!(sample(&page, &format!("smartctl_device_critical_warning{nl}}}")), Some("4"));

        // A missing drive keeps its info, drops stale readings.
        assert!(page.contains("stormdrive_drive_info{device=\"sdz\""));
        assert!(page.contains("activity=\"missing\""));
        assert!(!page.contains("smartctl_device_temperature{device=\"sdz\""));

        assert_eq!(sample(&page, "stormdrive_poll_samples_total"), Some("12"));
        assert_eq!(sample(&page, "stormdrive_build_info{version=\"0.19.0\",node=\"n1\"}"), Some("1"));
    }

    #[test]
    fn a_drive_never_polled_has_no_readings() {
        let d = drive("sdc", "sas_hdd", |d| d.health.temperature_c = Some(30));
        let page = render("v", "n", [&d], &Shelves::new(), &PollStats::default());
        assert_valid(&page);
        assert!(page.contains("stormdrive_drive_info{device=\"sdc\""));
        assert!(!page.contains("smartctl_device_temperature"));
        assert!(!page.contains("smartctl_device_smart_status"), "unknown health is not a pass");
        assert!(!page.contains("stormdrive_drive_last_poll_timestamp_seconds"));
    }

    #[test]
    fn a_shelf_with_a_hot_sensor_and_a_missing_psu() {
        let el = |t: &str, i: u32, status: &str, extra: serde_json::Value| -> crate::ses::Element {
            let mut v = serde_json::json!({"element_type": 0, "type_name": t, "index": i, "overall": false, "status": status,
                "predicted_failure": false, "disabled": false, "swapped": false, "ident": false, "fault": false, "raw": [0, 0, 0, 0]});
            for (k, x) in extra.as_object().unwrap() {
                v[k] = x.clone();
            }
            serde_json::from_value(v).unwrap()
        };
        let rep = crate::ses::ShelfReport {
            key: "5000a098aaaa0001".into(),
            shelf: Shelf { vendor: Some("NETAPP".into()), model: Some("DS224C".into()), ..Default::default() },
            esps: vec![],
            generation: 0,
            critical: false,
            noncritical: true,
            unrecoverable: false,
            info: false,
            elements: vec![
                el("temperature", 0, "noncritical", serde_json::json!({"temperature_c": 51})),
                el("cooling", 1, "ok", serde_json::json!({"rpm": 9000})),
                el("power_supply", 1, "not_installed", serde_json::json!({})),
            ],
            slots: BTreeMap::new(),
            collected_at: UNIX_EPOCH,
            status_raw: vec![],
        };
        let shelves = Shelves::from([(rep.key.clone(), rep)]);
        let page = render("v", "n", [], &shelves, &PollStats::default());
        assert_valid(&page);
        let l = r#"{enclosure="5000a098aaaa0001",type="temperature",index="0"}"#;
        assert_eq!(sample(&page, &format!("stormdrive_enclosure_temperature_celsius{l}")), Some("51"));
        assert_eq!(sample(&page, &format!("stormdrive_enclosure_element_ok{l}")), Some("0"));
        assert_eq!(sample(&page, r#"stormdrive_enclosure_fan_rpm{enclosure="5000a098aaaa0001",type="cooling",index="1"}"#), Some("9000"));
        assert_eq!(sample(&page, r#"stormdrive_enclosure_ok{enclosure="5000a098aaaa0001"}"#), Some("0"));
        assert!(!page.contains("power_supply"), "an empty PSU bay is not a failed one");
    }

    #[test]
    fn escapes_backslash_quote_newline() {
        assert_eq!(escape("a\\b\"c\nd"), "a\\\\b\\\"c\\nd");
    }
}
