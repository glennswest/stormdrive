//! What the node's engine says about its own slabs, held against the slabs
//! on each drive (#58).
//!
//! The Dell (C2NR0Q2, stormcos 11.95) ran diskless from forge — the
//! engine's health said `slabs.system = remote`, `slabs.data = remote` —
//! while its only drive carried both slab partitions, and stormdrive called
//! that drive fine. Owner (2026-10-08): the flow-over onto the local disk is
//! mandatory; a node that can't take its own disk fails loudly as a
//! hardware fault, and stormdrive marks the drive suspect or failed.
//!
//! The engine's open `GET /api/v1/health` carries (stormblock#322/#344):
//!
//! ```json
//! "slabs": {"diskless": true, "system": "remote", "data": "remote",
//!           "items": [{"role": "system", "source": "remote", "transport": "nvme-tcp"}],
//!           "local_disk": {"state": "refused", "drive": "/dev/sda", "reason": "…", "from": "initramfs"}}
//! ```
//!
//! A half is `local`, `remote`, `mixed` (a flow-over under way) or `none`.
//! `local_disk` is the boot's verdict on the machine's own disk: `taken`,
//! `refused` (a drive with slabs not taken, and why), `failed` (taken and
//! unusable) or `none`.

use crate::contents::SlabPart;
use crate::drive::HealthStatus;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineSlabs {
    #[serde(default)]
    pub diskless: bool,
    #[serde(default)]
    pub system: String,
    #[serde(default)]
    pub data: String,
    #[serde(default)]
    pub items: Vec<EngineSlabItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_disk: Option<LocalDisk>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineSlabItem {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub source: String,
    /// A local slab's device: `/dev/sda@<offset>` for a partition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalDisk {
    #[serde(default)]
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// The `slabs` object of the engine's health; None from an engine that
/// does not report one (before stormblock#322).
pub fn parse(health: &Value) -> Option<EngineSlabs> {
    serde_json::from_value(health.get("slabs")?.clone()).ok()
}

/// The engine left a drive's slabs unused (#58), as kept on the drive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineFinding {
    /// `warning` (suspect) or `failing` (the engine took the disk and
    /// could not use it).
    pub severity: HealthStatus,
    /// The halves whose slabs are on this drive and run elsewhere.
    pub roles: Vec<String>,
    pub message: String,
}

/// Does the engine's `/dev/…` name this drive?
fn names_drive(dev: &str, name: &str, paths: &[String]) -> bool {
    let dev = dev.split('@').next().unwrap_or(dev);
    let base = dev.strip_prefix("/dev/").unwrap_or(dev);
    base == name || paths.iter().any(|p| p == dev)
}

/// The finding for one drive: its slab partitions held against where the
/// engine runs each half. None when the drive has no slabs, when the
/// engine uses them, or when a half is mid flow-over (`mixed`).
pub fn finding(name: &str, paths: &[String], slabs: &[SlabPart], r: &EngineSlabs) -> Option<EngineFinding> {
    if slabs.is_empty() {
        return None;
    }
    let verdict = r.local_disk.as_ref().filter(|l| match &l.drive {
        Some(d) => names_drive(d, name, paths),
        None => true,
    });
    let half = |role: &str| if role == "data" { r.data.as_str() } else { r.system.as_str() };
    let mut roles: Vec<String> = vec![];
    for s in slabs {
        // A whole-drive slab's role is not on the disk: it stands for
        // either half.
        let unused = match s.role.as_str() {
            "system" | "data" => half(&s.role) == "remote" || r.diskless,
            _ => r.diskless || r.system == "remote" || r.data == "remote",
        };
        if unused && !roles.contains(&s.role) {
            roles.push(s.role.clone());
        }
    }
    let failed = verdict.filter(|l| l.state == "failed");
    let refused = verdict.filter(|l| l.state == "refused");
    if roles.is_empty() && failed.is_none() && refused.is_none() {
        return None;
    }
    let parts: Vec<String> = slabs
        .iter()
        .map(|s| match s.partition {
            Some(n) if s.name.is_empty() => format!("partition {n} ({})", s.role),
            Some(n) => format!("partition {n} '{}' ({})", s.name, s.role),
            None => "the whole drive".into(),
        })
        .collect();
    let why = verdict
        .and_then(|l| l.reason.as_deref().map(|r| format!("; the engine's boot says: {} — {r}", l.state)))
        .unwrap_or_default();
    let (severity, lead) = if failed.is_some() {
        (HealthStatus::Failing, "the engine took this disk and could not use it")
    } else {
        (HealthStatus::Warning, "suspect: the engine is not using this disk's slabs")
    };
    Some(EngineFinding {
        severity,
        roles,
        message: format!(
            "{lead}: stormblock slabs in {} while the engine runs system={} data={}{}{why}",
            parts.join(", "),
            or_unknown(&r.system),
            or_unknown(&r.data),
            if r.diskless { " (diskless)" } else { "" },
        ),
    })
}

fn or_unknown(s: &str) -> &str {
    if s.is_empty() {
        "?"
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn part(n: usize, name: &str, role: &str) -> SlabPart {
        SlabPart { partition: Some(n), name: name.into(), role: role.into(), offset_bytes: 1 << 20 }
    }

    /// The Dell's sda: EFI, a system slab and a data slab.
    fn dell() -> Vec<SlabPart> {
        vec![part(2, "stormblock", "system"), part(3, "stormblock-data", "data")]
    }

    fn report(v: Value) -> EngineSlabs {
        parse(&json!({ "status": "ok", "slabs": v })).expect("slabs")
    }

    #[test]
    fn the_dell_running_diskless_from_forge_is_suspect() {
        let r = report(json!({"diskless": true, "system": "remote", "data": "remote",
            "items": [{"role": "system", "source": "remote", "transport": "nvme-tcp", "volumes": 61}]}));
        let f = finding("sda", &["/dev/sda".into()], &dell(), &r).expect("a finding");
        assert_eq!(f.severity, HealthStatus::Warning);
        assert_eq!(f.roles, vec!["system", "data"]);
        assert!(f.message.contains("partition 2 'stormblock' (system)"), "{}", f.message);
        assert!(f.message.contains("system=remote data=remote (diskless)"), "{}", f.message);
    }

    #[test]
    fn the_boots_verdict_names_the_reason_and_a_failed_take_is_failing() {
        let mut r = report(json!({"diskless": true, "system": "remote", "data": "remote",
            "local_disk": {"state": "refused", "drive": "/dev/sda", "reason": "behind a SAS expander", "from": "initramfs"}}));
        let f = finding("sda", &[], &dell(), &r).unwrap();
        assert_eq!(f.severity, HealthStatus::Warning);
        assert!(f.message.contains("refused — behind a SAS expander"), "{}", f.message);
        r.local_disk = Some(LocalDisk { state: "failed".into(), drive: Some("/dev/sda".into()), reason: Some("Input/output error".into()), from: None });
        let f = finding("sda", &[], &dell(), &r).unwrap();
        assert_eq!(f.severity, HealthStatus::Failing);
        assert!(f.message.contains("Input/output error"));
        // A verdict about another drive says nothing about this one's
        // reason, but its unused slabs are still a finding.
        let f = finding("sdb", &[], &dell(), &r).unwrap();
        assert_eq!(f.severity, HealthStatus::Warning);
        assert!(!f.message.contains("Input/output error"));
    }

    #[test]
    fn used_slabs_a_flow_over_and_a_blank_drive_are_fine() {
        let local = report(json!({"diskless": false, "system": "local", "data": "local",
            "items": [{"role": "system", "source": "local", "device": "/dev/sda@1048576"}],
            "local_disk": {"state": "taken", "drive": "/dev/sda"}}));
        assert_eq!(finding("sda", &[], &dell(), &local), None);
        let mid = report(json!({"diskless": false, "system": "mixed", "data": "remote"}));
        let f = finding("sda", &[], &dell(), &mid).expect("data half still remote");
        assert_eq!(f.roles, vec!["data"]);
        let mid = report(json!({"diskless": false, "system": "mixed", "data": "mixed"}));
        assert_eq!(finding("sda", &[], &dell(), &mid), None, "mid flow-over");
        let remote = report(json!({"diskless": true, "system": "remote", "data": "remote"}));
        assert_eq!(finding("sdb", &[], &[], &remote), None, "a drive without slabs");
    }

    #[test]
    fn an_engine_without_a_slab_report_parses_to_none() {
        assert_eq!(parse(&json!({"status": "ok"})), None);
        let whole = SlabPart { partition: None, name: String::new(), role: "unknown".into(), offset_bytes: 0 };
        let r = report(json!({"diskless": false, "system": "remote", "data": "local"}));
        let f = finding("sdc", &[], &[whole], &r).unwrap();
        assert!(f.message.contains("the whole drive"));
    }
}
