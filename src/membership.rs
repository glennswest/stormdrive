//! Why a drive is in the fleet or out of it (#156): never a bare `out`.
//!
//! The DrivePolicy pass (`controller.rs`) decides for every drive out of the
//! fleet on its node and records it on the drive (`Drive.membership_reason`):
//! the policy's skip, its job's state, or no policy selecting the drive and
//! why not. Where no pass has run (no apiserver) the drive's own record says
//! what it can. The Drive object carries it as `status.membershipReason` and
//! the condition `Member`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::drive::{Activity, Drive, Membership};

/// The reason, as a condition has it: a CamelCase word and a sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Why {
    pub reason: String,
    pub message: String,
}

impl Why {
    pub fn new(reason: &str, message: impl Into<String>) -> Why {
        Why { reason: reason.into(), message: message.into() }
    }
}

/// The condition reason for a policy's skip (`policy::verdict`,
/// `policy::blocker`, the worker's guard).
pub fn code_for(skip: &str) -> &'static str {
    let s = skip.to_ascii_lowercase();
    if s.starts_with("designated") {
        "Designated"
    } else if s.starts_with("holds") || s.starts_with("in use") || s.contains("mounted") {
        "InUse"
    } else if s.starts_with("busy") {
        "Busy"
    } else if s.starts_with("health is") {
        "Failed"
    } else if s.contains("warning temperature") {
        "Hot"
    } else if s.contains("host-managed smr") {
        "HostManagedSMR"
    } else if s.contains("-byte sectors") {
        "NeedsReformat"
    } else if s.starts_with("no health verdict") {
        "NoHealthYet"
    } else if s.contains("missing") {
        "Missing"
    } else {
        "Skipped"
    }
}

/// A policy's record of a drive (its `status.nodes.<node>.drives.<id>`) as
/// the drive's reason. None for a record that has the drive in the fleet.
pub fn from_record(policy: &str, r: &Value) -> Option<Why> {
    let state = r["state"].as_str().unwrap_or("");
    let reason = r["reason"].as_str().filter(|s| !s.is_empty());
    Some(match state {
        "skipped" => {
            let why = reason.unwrap_or("skipped");
            Why::new(code_for(why), format!("DrivePolicy {policy}: {why}"))
        }
        "queued" | "running" | "interrupted" => Why::new("Enrolling", format!("DrivePolicy {policy}: being enrolled ({state})")),
        "planned" => Why::new("Planned", format!("DrivePolicy {policy} is dryRun: it would run {}", steps(r))),
        "failed" | "refused" | "cancelled" => Why::new(
            "EnrollFailed",
            format!("DrivePolicy {policy}: {state}: {}; edit the policy to try again", reason.unwrap_or(state)),
        ),
        "waiting" => Why::new("Waiting", format!("DrivePolicy {policy}: {}", reason.unwrap_or("waiting"))),
        "suspended" => Why::new("Suspended", format!("DrivePolicy {policy} is suspended")),
        "enrolled" => return None,
        _ => Why::new("Skipped", format!("DrivePolicy {policy}: {state}")),
    })
}

fn steps(r: &Value) -> String {
    let s = r["steps"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>();
    if s.is_empty() {
        "its steps".into()
    } else {
        s.join(" → ")
    }
}

/// No policy took the drive: what each policy on the node said of it.
/// `policies`: (name, the selector's mismatch); empty = no policy selects
/// this node.
pub fn not_selected(policies: &[(String, String)]) -> Why {
    if policies.is_empty() {
        return Why::new("NoPolicy", "no DrivePolicy selects this node: a drive is enrolled by a DrivePolicy or by hand (POST /api/v1/drives/{id}/enroll, a DriveOperation)");
    }
    let each = policies.iter().map(|(p, m)| format!("{p}: {m}")).collect::<Vec<_>>().join("; ");
    Why::new("NotSelected", format!("no DrivePolicy on this node selects it ({each})"))
}

/// The drive's reason: None in the fleet; the pass's word; else what its own
/// record says.
pub fn why(d: &Drive) -> Option<Why> {
    if d.membership == Membership::Fleet {
        return None;
    }
    if d.activity == Activity::Missing {
        return Some(Why::new("Missing", "not seen on the node"));
    }
    if let Some(w) = &d.membership_reason {
        return Some(w.clone());
    }
    if let Some(b) = crate::policy::blocker(d) {
        return Some(Why::new(code_for(&b), b));
    }
    Some(Why::new(
        "NotDecided",
        "no DrivePolicy pass has run on this node (no apiserver): enrol it by hand (POST /api/v1/drives/{id}/enroll, a DriveOperation)",
    ))
}

/// The tier of the drive's slabs as the engine lists them: its data slab's,
/// else any slab's (#157).
pub fn tier(d: &Drive) -> Option<String> {
    let slabs = &d.usage.as_ref()?.slabs;
    slabs.iter().find(|s| s.role == "data").or_else(|| slabs.first()).map(|s| s.tier.clone()).filter(|t| !t.is_empty())
}

/// What a member says of itself beyond "in the fleet" (#157): a `fast`
/// slab holds only what asks for `fast` — never a default placement.
pub fn member_note(d: &Drive) -> Option<&'static str> {
    (d.membership == Membership::Fleet && tier(d).as_deref() == Some("fast")).then_some("the fast tier: only what asks for fast uses it")
}

/// `status.membershipReason`: why it is out, or a member's note.
pub fn reason_message(d: &Drive) -> Option<String> {
    why(d).map(|w| w.message).or_else(|| member_note(d).map(str::to_string))
}

/// The `Member` condition (no `lastTransitionTime`: the controller keeps it
/// from the object it replaces).
pub fn condition(d: &Drive) -> Value {
    match why(d) {
        None => match member_note(d) {
            Some(n) => json!({ "type": "Member", "status": "True", "reason": "Enrolled", "message": format!("in the fleet, {n}") }),
            None => json!({ "type": "Member", "status": "True", "reason": "Enrolled", "message": "in the fleet: the engine places data on it" }),
        },
        Some(w) => json!({ "type": "Member", "status": "False", "reason": w.reason, "message": w.message }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::{Designation, HealthStatus};

    #[test]
    fn every_case_has_its_own_word() {
        assert_eq!(code_for("designated system"), "Designated");
        assert_eq!(code_for("holds data for stormblock (partition 2)"), "InUse");
        assert_eq!(code_for("in use: mounted at /var — …"), "InUse");
        assert_eq!(code_for("busy: testing"), "Busy");
        assert_eq!(code_for("health is Failing"), "Failed");
        assert_eq!(code_for("running over its warning temperature"), "Hot");
        assert_eq!(code_for("host-managed SMR: its zones take only sequential writes, and a stormblock slab writes at random"), "HostManagedSMR");
        assert_eq!(code_for("520-byte sectors and the policy has no reformat"), "NeedsReformat");
        assert_eq!(code_for("no health verdict yet"), "NoHealthYet");
    }

    #[test]
    fn a_record_becomes_a_reason() {
        let r = json!({ "state": "skipped", "reason": "designated spare" });
        assert_eq!(from_record("blades", &r), Some(Why::new("Designated", "DrivePolicy blades: designated spare")));
        let r = json!({ "state": "running", "job": "j1" });
        assert_eq!(from_record("blades", &r).unwrap().reason, "Enrolling");
        let r = json!({ "state": "failed", "reason": "partition: EIO" });
        assert!(from_record("blades", &r).unwrap().message.contains("failed: partition: EIO"));
        let r = json!({ "state": "planned", "steps": ["partition (data)", "enroll (data slab, tier cool)"] });
        assert!(from_record("blades", &r).unwrap().message.contains("tier cool"));
        assert_eq!(from_record("blades", &json!({ "state": "enrolled" })), None);
    }

    #[test]
    fn never_a_bare_out() {
        let mut d = Drive::test_fixture("sdb");
        let c = condition(&d);
        assert_eq!((c["status"].as_str(), c["reason"].as_str()), (Some("False"), Some("NotDecided")));
        d.designation = Designation::Spare;
        assert_eq!(why(&d).unwrap().reason, "Designated");
        d.designation = Designation::None;
        d.membership_reason = Some(not_selected(&[("sas".into(), "kind sata_hdd is not one of sas_hdd".into())]));
        let w = why(&d).unwrap();
        assert_eq!(w.reason, "NotSelected");
        assert!(w.message.contains("sas: kind sata_hdd is not one of sas_hdd"), "{}", w.message);
        assert_eq!(not_selected(&[]).reason, "NoPolicy");
        d.membership = Membership::Fleet;
        d.health.status = Some(HealthStatus::Good);
        assert_eq!(condition(&d)["status"], "True");
        assert_eq!(why(&d), None);
        assert_eq!(reason_message(&d), None);
    }

    /// #157: server8's Patriot SSD, enrolled as the blades' `fast` slab.
    #[test]
    fn a_fast_member_says_what_its_tier_is_for() {
        let mut d = Drive::test_fixture("sda");
        d.membership = Membership::Fleet;
        d.health.status = Some(HealthStatus::Good);
        let slab = |role: &str, tier: &str| crate::usage::SlabUsage {
            id: "s".into(), role: role.into(), tier: tier.into(), slot_size: 1, total_bytes: 1,
            allocated_bytes: 0, free_bytes: 1, committed_bytes: None,
        };
        let mut u = crate::usage::compute(&d, &[], std::time::SystemTime::UNIX_EPOCH);
        u.slabs = vec![slab("data", "fast")];
        d.usage = Some(u.clone());
        assert_eq!(tier(&d).as_deref(), Some("fast"));
        assert_eq!(reason_message(&d).as_deref(), Some("the fast tier: only what asks for fast uses it"));
        let c = condition(&d);
        assert_eq!(c["status"], "True");
        assert!(c["message"].as_str().unwrap().contains("only what asks for fast"));
        // The HDD beside it: system + data, warm; no note.
        u.slabs = vec![slab("system", "warm"), slab("data", "cool")];
        d.usage = Some(u);
        assert_eq!(tier(&d).as_deref(), Some("cool"));
        assert_eq!(reason_message(&d), None);
    }
}
