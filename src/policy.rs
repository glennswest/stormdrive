//! DrivePolicy (#50, stormcos#251): which of a node's drives become stormblock
//! slabs, of which tier, without anyone running a job by hand.
//!
//! `[stormblock] auto_add` cannot do it for stormcos#251's two storage nodes:
//! its tier comes from `tier_map` (per drive kind) in a config every node
//! shares, so stormblock1's shelf and stormblock2's drives — SAS HDDs both —
//! cannot get different tiers; and it skips a drive that needs a reformat,
//! which every NetApp drive does (520-byte sectors). A `DrivePolicy` object
//! (`storage.storm.io/v1`, storage-admin only) says it instead:
//!
//! ```yaml
//! spec:
//!   nodes: [stormblock1]            # and/or nodeSelector: {matchLabels: …}
//!   drives: {kinds: [sas_hdd], blockSizes: [520, 512], shelf: 5000a098…}
//!   reformat: 4096                  # only on a drive that needs it
//!   enroll: {role: data, tier: warm}
//! ```
//!
//! This module is the decision, pure and tested; `controller.rs` runs it each
//! pass and hands the drives to the drive worker, whose guards and lanes
//! apply as to any job (never a drive holding data, enroll one per failure
//! domain, the requester re-checked before every step).

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::drive::{Activity, Designation, Drive, DriveKind, HealthStatus, Membership};
use crate::worker::{Role, Step};

/// The tiers a stormblock slab can carry (stormblock `placement/topology.rs`).
pub const TIERS: [&str; 4] = ["hot", "warm", "cool", "cold"];

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelSelector {
    #[serde(default)]
    pub match_labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveSelector {
    /// Drive kinds (`sas_hdd`, `nvme_ssd`, …); empty = any.
    #[serde(default)]
    pub kinds: Vec<DriveKind>,
    #[serde(default)]
    pub min_bytes: Option<u64>,
    #[serde(default)]
    pub max_bytes: Option<u64>,
    /// Logical sector sizes as the drive reports them (520 included).
    #[serde(default)]
    pub block_sizes: Vec<u32>,
    #[serde(default)]
    pub model: Option<String>,
    /// An enclosure: its logical id, serial or sysfs id.
    #[serde(default)]
    pub shelf: Option<String>,
    /// Bays on that shelf: `"0-11,14"`.
    #[serde(default)]
    pub bays: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Enroll {
    #[serde(default)]
    pub role: Role,
    pub tier: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicySpec {
    #[serde(default)]
    pub nodes: Vec<String>,
    #[serde(default)]
    pub node_selector: Option<LabelSelector>,
    #[serde(default)]
    pub drives: DriveSelector,
    /// Reformat a drive the kernel cannot use (520/528-byte sectors) to this
    /// size first. A drive already at 512 or 4096 is left at its size.
    #[serde(default)]
    pub reformat: Option<u32>,
    pub enroll: Enroll,
    /// Act only on a node that already has a stormblock data slab (#42:
    /// "on a node with a data slab") — grow a storage node, never make one.
    #[serde(default)]
    pub require_data_slab: bool,
    #[serde(default)]
    pub suspend: bool,
    #[serde(default)]
    pub dry_run: bool,
}

impl PolicySpec {
    /// The spec out of a DrivePolicy object, checked.
    pub fn parse(spec: &Value) -> Result<PolicySpec, String> {
        let s: PolicySpec = serde_json::from_value(spec.clone()).map_err(|e| format!("spec: {e}"))?;
        s.validate()?;
        Ok(s)
    }

    pub fn validate(&self) -> Result<(), String> {
        let selects_nodes = self.nodes.iter().any(|n| !n.trim().is_empty())
            || self.node_selector.as_ref().is_some_and(|s| !s.match_labels.is_empty());
        if !selects_nodes {
            return Err("name the nodes: spec.nodes or spec.nodeSelector.matchLabels (a policy for every node is not taken)".into());
        }
        if !TIERS.contains(&self.enroll.tier.as_str()) {
            return Err(format!("enroll.tier {:?}: one of {}", self.enroll.tier, TIERS.join(", ")));
        }
        if let Some(bs) = self.reformat {
            if !crate::format::valid_target(bs) {
                return Err(format!("reformat {bs}: use 512 or 4096"));
            }
        }
        if let (Some(a), Some(b)) = (self.drives.min_bytes, self.drives.max_bytes) {
            if a > b {
                return Err(format!("drives.minBytes {a} > maxBytes {b}"));
            }
        }
        if let Some(b) = &self.drives.bays {
            if self.drives.shelf.is_none() {
                return Err("drives.bays needs drives.shelf".into());
            }
            crate::worker::parse_bays(b)?;
        }
        crate::worker::validate_steps(&self.steps_for(true))
    }

    /// Does the policy apply to this node? `labels` = the Node object's,
    /// when a selector needs them (None: not known, so no).
    pub fn selects_node(&self, node: &str, labels: Option<&BTreeMap<String, String>>) -> bool {
        let named = self.nodes.iter().map(|n| n.trim()).filter(|n| !n.is_empty()).collect::<Vec<_>>();
        if !named.is_empty() && !named.iter().any(|n| n.eq_ignore_ascii_case(node.trim())) {
            return false;
        }
        match self.node_selector.as_ref().filter(|s| !s.match_labels.is_empty()) {
            None => !named.is_empty(),
            Some(s) => labels.is_some_and(|l| s.match_labels.iter().all(|(k, v)| l.get(k) == Some(v))),
        }
    }

    /// Does the drive selector pick this drive?
    pub fn selects_drive(&self, d: &Drive) -> bool {
        let s = &self.drives;
        if !s.kinds.is_empty() && !s.kinds.contains(&d.kind) {
            return false;
        }
        if s.min_bytes.is_some_and(|m| d.capacity_bytes < m) || s.max_bytes.is_some_and(|m| d.capacity_bytes > m) {
            return false;
        }
        if !s.block_sizes.is_empty() && !s.block_sizes.contains(&d.block_size) {
            return false;
        }
        if let Some(m) = &s.model {
            if !d.model.trim().eq_ignore_ascii_case(m.trim()) {
                return false;
            }
        }
        if let Some(want) = &s.shelf {
            let Some(shelf) = &d.location.shelf else { return false };
            let want = crate::ses::normalize_sas(want);
            let names = [shelf.key(), shelf.logical_id.clone(), shelf.serial.clone(), shelf.id.clone()];
            if !names.into_iter().flatten().any(|n| crate::ses::normalize_sas(&n) == want) {
                return false;
            }
        }
        if let Some(b) = &s.bays {
            let Ok(bays) = crate::worker::parse_bays(b) else { return false };
            if !d.location.bay.is_some_and(|x| bays.contains(&x)) {
                return false;
            }
        }
        true
    }

    /// The worker steps for a drive: the reformat when it needs one, then a
    /// partition and the slab in it.
    pub fn steps_for(&self, needs_reformat: bool) -> Vec<Step> {
        let mut steps = vec![];
        if let (true, Some(bs)) = (needs_reformat, self.reformat) {
            steps.push(Step::Format { block_size: bs, protection: crate::pi::Protection::None });
        }
        steps.push(Step::Partition { role: self.enroll.role });
        steps.push(Step::Enroll { tier: Some(self.enroll.tier.clone()), role: self.enroll.role });
        steps
    }
}

/// Does this node already hold a stormblock data slab (on any drive, as the
/// engine's slab listing says)?
pub fn node_has_data_slab(drives: &[Drive]) -> bool {
    drives.iter().any(|d| d.usage.as_ref().is_some_and(|u| u.slabs.iter().any(|s| s.role == "data")))
}

/// What the policy does with one drive it selects.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Not this policy's business now (in the fleet, missing): not reported.
    Pass,
    /// Selected but left alone, and why.
    Skip(String),
    /// Hand it to the worker with these steps.
    Run(Vec<Step>),
}

/// The decision for a drive the selector picked, from its record alone (the
/// worker's guard then checks its contents: a slab or a filesystem on it).
/// `in_job`: a worker job already has it queued or running.
pub fn verdict(p: &PolicySpec, d: &Drive, in_job: bool) -> Verdict {
    if d.membership == Membership::Fleet || d.activity == Activity::Missing || in_job {
        return Verdict::Pass;
    }
    if d.designation != Designation::None {
        let word = serde_json::to_value(d.designation).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        return Verdict::Skip(format!("designated {word}"));
    }
    if let Some(who) = &d.in_use_by {
        return Verdict::Skip(format!("holds data for {who}"));
    }
    if d.activity != Activity::Idle {
        let word = serde_json::to_value(d.activity).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        return Verdict::Skip(format!("busy: {word}"));
    }
    let health = d.health.status();
    if health >= HealthStatus::Failing {
        return Verdict::Skip(format!("health is {health:?}"));
    }
    let reformat = d.needs_reformat();
    if reformat && p.reformat.is_none() {
        return Verdict::Skip(format!("{}-byte sectors and the policy has no reformat", d.block_size));
    }
    // A drive the kernel cannot read yet may never have had a health verdict;
    // the worker's guard looks again before the enroll.
    if health == HealthStatus::Unknown && !reformat {
        return Verdict::Skip("no health verdict yet".into());
    }
    Verdict::Run(p.steps_for(reformat))
}

/// A drive's last record in the policy's status, when it says not to try
/// again: its job failed (or was refused or cancelled) under this generation
/// of the policy. Editing the policy (a new generation) tries it again.
pub fn retry_blocked(record: Option<&Value>, generation: Option<i64>) -> Option<String> {
    let r = record?;
    let state = r["state"].as_str().unwrap_or("");
    if !matches!(state, "failed" | "refused" | "cancelled") {
        return None;
    }
    if r["generation"].as_i64() != generation {
        return None;
    }
    let why = r["reason"].as_str().filter(|s| !s.is_empty()).unwrap_or(state);
    Some(format!("{state} under generation {} ({why}); edit the policy to try again", generation.unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::{Location, Shelf};
    use serde_json::json;

    fn spec(v: Value) -> PolicySpec {
        PolicySpec::parse(&v).unwrap()
    }

    fn stormblock1() -> PolicySpec {
        spec(json!({
            "nodes": ["stormblock1"],
            "drives": { "kinds": ["sas_hdd"], "shelf": "0x5000A098AAAA0001" },
            "reformat": 4096,
            "enroll": { "role": "data", "tier": "warm" },
        }))
    }

    /// A fresh NetApp shelf drive: SAS HDD, 520-byte sectors, no capacity
    /// the kernel will expose, never polled.
    fn netapp(bay: u32) -> Drive {
        let mut d = Drive::test_fixture(&format!("sd{bay}"));
        d.kind = DriveKind::SasHdd;
        d.model = "ST1200MM0098".into();
        d.block_size = 520;
        d.usable = false;
        d.capacity_bytes = 1_200_000_000_000;
        d.location = Location {
            shelf: Some(Shelf { logical_id: Some("5000a098aaaa0001".into()), model: Some("DS224C".into()), ..Default::default() }),
            bay: Some(bay),
            ..Default::default()
        };
        d
    }

    #[test]
    fn a_fresh_520_byte_shelf_drive_is_reformatted_and_enrolled_at_the_policys_tier() {
        let p = stormblock1();
        assert!(p.selects_node("stormblock1", None));
        assert!(!p.selects_node("stormblock2", None));
        let d = netapp(3);
        assert!(p.selects_drive(&d));
        let Verdict::Run(steps) = verdict(&p, &d, false) else { panic!("{:?}", verdict(&p, &d, false)) };
        assert_eq!(
            steps,
            vec![
                Step::Format { block_size: 4096, protection: crate::pi::Protection::None },
                Step::Partition { role: Role::Data },
                Step::Enroll { tier: Some("warm".into()), role: Role::Data },
            ]
        );
        // The worker's own guard agrees, for a blank drive.
        let cx = crate::worker::Context { stormblock: true, ..Default::default() };
        assert_eq!(crate::worker::guard(&d, &steps, false, &cx), Ok(()));
    }

    /// #72: a usable drive of any size — 1 PiB here — is enrolled by
    /// metadata steps only: a partition table and the engine's slab. No
    /// test, no erase, no format.
    #[test]
    fn a_pib_drive_is_enrolled_by_metadata_steps_only() {
        let p = stormblock1();
        let mut d = netapp(5);
        d.block_size = 4096;
        d.usable = true;
        d.capacity_bytes = 1 << 50;
        d.health.status = Some(HealthStatus::Good);
        let Verdict::Run(steps) = verdict(&p, &d, false) else { panic!() };
        assert!(steps.iter().all(|s| matches!(s, Step::Partition { .. } | Step::Enroll { .. })), "{steps:?}");
    }

    #[test]
    fn a_drive_holding_data_is_left_alone() {
        let p = stormblock1();
        let mut d = netapp(4);
        d.block_size = 4096;
        d.usable = true;
        d.health.status = Some(HealthStatus::Good);
        let Verdict::Run(steps) = verdict(&p, &d, false) else { panic!() };
        assert_eq!(steps.len(), 2, "already 4096: no reformat");
        // A filesystem the contents probe found: the guard refuses, and a
        // policy never names a drive in `destroy`.
        let cx = crate::worker::Context { stormblock: true, holds: Some("xfs".into()), ..Default::default() };
        let e = crate::worker::guard(&d, &steps, false, &cx).unwrap_err();
        assert!(e.contains("holds xfs"), "{e}");
        // A stormblock slab found on it: never even planned.
        d.in_use_by = Some("stormblock (partition 2)".into());
        assert_eq!(verdict(&p, &d, false), Verdict::Skip("holds data for stormblock (partition 2)".into()));
    }

    #[test]
    fn reserved_spare_failed_failing_busy_fleet_and_queued_drives_are_not_taken() {
        let p = stormblock1();
        let base = netapp(5);
        let mut d = base.clone();
        d.designation = Designation::Reserved;
        assert_eq!(verdict(&p, &d, false), Verdict::Skip("designated reserved".into()));
        d.designation = Designation::Spare;
        assert_eq!(verdict(&p, &d, false), Verdict::Skip("designated spare".into()));
        d.designation = Designation::Failed;
        assert_eq!(verdict(&p, &d, false), Verdict::Skip("designated failed".into()));
        // The node's system drive (#66) never gets a data slab by policy.
        d.designation = Designation::System;
        assert_eq!(verdict(&p, &d, false), Verdict::Skip("designated system".into()));
        let mut d = base.clone();
        d.health.status = Some(HealthStatus::Failing);
        assert!(matches!(verdict(&p, &d, false), Verdict::Skip(r) if r.contains("Failing")));
        let mut d = base.clone();
        d.activity = Activity::Testing;
        assert_eq!(verdict(&p, &d, false), Verdict::Skip("busy: testing".into()));
        let mut d = base.clone();
        d.membership = Membership::Fleet;
        assert_eq!(verdict(&p, &d, false), Verdict::Pass);
        assert_eq!(verdict(&p, &base, true), Verdict::Pass, "already in a job");
        let mut d = base;
        d.activity = Activity::Missing;
        assert_eq!(verdict(&p, &d, false), Verdict::Pass);
    }

    #[test]
    fn without_reformat_a_520_drive_is_skipped_and_an_unpolled_usable_one_waits() {
        let p = spec(json!({ "nodes": ["n"], "enroll": { "tier": "cool" } }));
        assert_eq!(p.enroll.role, Role::Data);
        assert!(matches!(verdict(&p, &netapp(0), false), Verdict::Skip(r) if r.contains("no reformat")));
        let d = Drive::test_fixture("sdb");
        assert_eq!(verdict(&p, &d, false), Verdict::Skip("no health verdict yet".into()));
    }

    #[test]
    fn the_drive_selector_ands_its_fields() {
        let p = stormblock1();
        let mut d = netapp(1);
        d.kind = DriveKind::SasSsd;
        assert!(!p.selects_drive(&d), "kind");
        let mut d = netapp(1);
        d.location.shelf = Some(Shelf { logical_id: Some("5000a098bbbb0002".into()), ..Default::default() });
        assert!(!p.selects_drive(&d), "another shelf");
        d.location.shelf = None;
        assert!(!p.selects_drive(&d), "no shelf");

        let p = spec(json!({ "nodes": ["n"], "drives": { "minBytes": 1000, "maxBytes": 2000, "blockSizes": [520],
                                                       "model": " st1200mm0098 ", "shelf": "5000a098aaaa0001", "bays": "0-3" },
                             "reformat": 4096, "enroll": { "tier": "warm" } }));
        let mut d = netapp(2);
        d.capacity_bytes = 1500;
        assert!(p.selects_drive(&d));
        d.location.bay = Some(4);
        assert!(!p.selects_drive(&d), "bay");
        d.location.bay = Some(2);
        d.capacity_bytes = 2001;
        assert!(!p.selects_drive(&d), "size");
        d.capacity_bytes = 1500;
        d.block_size = 512;
        assert!(!p.selects_drive(&d), "sector size");
    }

    #[test]
    fn node_selection_by_name_and_by_labels() {
        let by_label = spec(json!({ "nodeSelector": { "matchLabels": { "storm.io/tier": "warm" } }, "enroll": { "tier": "warm" } }));
        let warm: BTreeMap<String, String> = [("storm.io/tier".to_string(), "warm".to_string())].into();
        let cool: BTreeMap<String, String> = [("storm.io/tier".to_string(), "cool".to_string())].into();
        assert!(by_label.selects_node("anything", Some(&warm)));
        assert!(!by_label.selects_node("anything", Some(&cool)));
        assert!(!by_label.selects_node("anything", None), "labels unknown: no");
        let both = spec(json!({ "nodes": ["StormBlock1"], "nodeSelector": { "matchLabels": { "storm.io/tier": "warm" } }, "enroll": { "tier": "warm" } }));
        assert!(both.selects_node("stormblock1", Some(&warm)));
        assert!(!both.selects_node("stormblock2", Some(&warm)));
    }

    #[test]
    fn a_spec_that_could_hurt_is_refused() {
        let bad = |v: Value| PolicySpec::parse(&v).unwrap_err();
        assert!(bad(json!({ "enroll": { "tier": "warm" } })).contains("name the nodes"));
        assert!(bad(json!({ "nodes": [" "], "enroll": { "tier": "warm" } })).contains("name the nodes"));
        assert!(bad(json!({ "nodes": ["n"], "enroll": { "tier": "lukewarm" } })).contains("one of hot, warm, cool, cold"));
        assert!(bad(json!({ "nodes": ["n"], "enroll": {} })).contains("tier"));
        assert!(bad(json!({ "nodes": ["n"], "reformat": 520, "enroll": { "tier": "warm" } })).contains("512 or 4096"));
        assert!(bad(json!({ "nodes": ["n"], "drives": { "bays": "0-3" }, "enroll": { "tier": "warm" } })).contains("needs drives.shelf"));
        assert!(bad(json!({ "nodes": ["n"], "drives": { "kinds": ["floppy"] }, "enroll": { "tier": "warm" } })).contains("spec:"));
        assert!(bad(json!({ "nodes": ["n"], "drives": { "minBytes": 2, "maxBytes": 1 }, "enroll": { "tier": "warm" } })).contains("minBytes"));
    }

    #[test]
    fn require_data_slab_reads_the_engines_slabs() {
        let p = spec(json!({ "nodes": ["n"], "requireDataSlab": true, "enroll": { "tier": "cool" } }));
        assert!(p.require_data_slab);
        let mut d = Drive::test_fixture("sda");
        assert!(!node_has_data_slab(std::slice::from_ref(&d)), "no usage yet");
        let slab = |role: &str| crate::usage::SlabUsage {
            id: "s".into(), role: role.into(), tier: "cool".into(), slot_size: 1, total_bytes: 1,
            allocated_bytes: 0, free_bytes: 1, committed_bytes: None,
        };
        let mut u = crate::usage::compute(&d, &[], std::time::SystemTime::UNIX_EPOCH);
        u.slabs = vec![slab("system")];
        d.usage = Some(u.clone());
        assert!(!node_has_data_slab(std::slice::from_ref(&d)), "a system slab does not count");
        u.slabs.push(slab("data"));
        d.usage = Some(u);
        assert!(node_has_data_slab(&[Drive::test_fixture("sdb"), d]));
    }

    #[test]
    fn a_failed_drive_waits_for_a_new_generation() {
        let rec = json!({ "state": "failed", "generation": 3, "reason": "format: FORMAT UNIT check condition" });
        let why = retry_blocked(Some(&rec), Some(3)).unwrap();
        assert!(why.contains("edit the policy"), "{why}");
        assert_eq!(retry_blocked(Some(&rec), Some(4)), None, "edited: try again");
        assert_eq!(retry_blocked(Some(&json!({ "state": "skipped", "generation": 3 })), Some(3)), None);
        assert_eq!(retry_blocked(None, Some(3)), None);
    }
}
