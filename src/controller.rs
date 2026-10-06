//! Drives and drive operations as Kubernetes objects (#45).
//!
//! Two kinds in `storage.storm.io/v1` (deploy/crds.yaml), both cluster-scoped:
//!
//! - **`Drive`** — one per physical drive, written by the stormdrive of the
//!   node that has it (`metadata.name` = the stable drive id, label
//!   `storm.io/node`). Its `spec` and `status` are the node's report (the
//!   same projection `/apis/storage.storm.io/v1/drives` serves on :9092):
//!   model, size, sector size, enclosure and bay, SAS address, health,
//!   owner (`free`/`stormblock`/`stormraid`). Anyone with `storage-viewer`
//!   reads it; a write to it changes nothing on the drive and is put back.
//! - **`DriveOperation`** — format, sanitize, partition, enroll on a
//!   selection of one node's drives: `spec` is a drive worker job
//!   (`select`, `steps`, `destroy`, `dryRun`) plus `node`. Creating one is an
//!   object write the apiserver authorises (`storage-admin`). This controller
//!   then **re-checks the requester** — the user the apiserver stamped on the
//!   object (`storage.storm.io/requester`, rustkube#210) — with a
//!   SubjectAccessReview, hands the job to the worker (which re-checks again
//!   before every step and keeps the job in jobs.json across restarts), and
//!   keeps `status` (phase, per-drive progress). An operation with no stamped
//!   requester is Refused: the object alone is never trusted. Each decision
//!   is a Kubernetes Event on the operation and an audit line.
//!
//! Deleting an operation cancels its job's queued steps (running ones
//! finish). Raising `spec.resume` resumes its interrupted drives.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};

use crate::api::kube::{drive_object, GROUP, VERSION};
use crate::api::AppState;
use crate::kubeapi::{KubeApi, KubeUser};
use crate::kubeauth::{Access, Requester};
use crate::worker::{DjState, Job};

/// The annotation the apiserver stamps with the creating user.
pub const REQUESTER: &str = "storage.storm.io/requester";
/// …and that user's groups, comma-separated.
pub const REQUESTER_GROUPS: &str = "storage.storm.io/requester-groups";

/// A Drive object is rewritten at least this often even when nothing else
/// changed (so `lastSeen` and the temperatures do not go stale for good).
const REFRESH: Duration = Duration::from_secs(300);

fn path(resource: &str, name: Option<&str>) -> String {
    match name {
        Some(n) => format!("/apis/{GROUP}/{VERSION}/{resource}/{n}"),
        None => format!("/apis/{GROUP}/{VERSION}/{resource}"),
    }
}

/// A Kubernetes label value: at most 63 of `[A-Za-z0-9._-]`, starting and
/// ending alphanumeric; `None` when nothing is left.
pub fn label_value(v: &str) -> Option<String> {
    let s: String = v.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).take(63).collect();
    let s = s.trim_matches(|c: char| !c.is_ascii_alphanumeric()).to_string();
    (!s.is_empty()).then_some(s)
}

/// The Drive object as the apiserver gets it: the served projection with
/// label values made valid (`/dev/sda` is not one; the path is in status).
pub fn apiserver_drive(d: &crate::drive::Drive, node: &str) -> Value {
    let mut v = drive_object(d, node);
    let labels: BTreeMap<String, String> = v["metadata"]["labels"]
        .as_object()
        .map(|m| m.iter().filter(|(k, _)| *k != "storm.io/path").filter_map(|(k, x)| Some((k.clone(), label_value(x.as_str()?)?))).collect())
        .unwrap_or_default();
    v["metadata"] = json!({ "name": v["metadata"]["name"], "labels": labels });
    v
}

/// What decides a rewrite: everything but `lastSeen`.
fn drive_fingerprint(v: &Value) -> String {
    let mut v = v.clone();
    if let Some(st) = v["status"].as_object_mut() {
        st.remove("lastSeen");
    }
    v.to_string()
}

/// The requester the apiserver stamped, if it did.
pub fn stamped_requester(op: &Value) -> Option<KubeUser> {
    let ann = &op["metadata"]["annotations"];
    let username = ann[REQUESTER].as_str().map(str::trim).filter(|s| !s.is_empty())?.to_string();
    let groups = ann[REQUESTER_GROUPS]
        .as_str()
        .map(|g| g.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
        .unwrap_or_default();
    Some(KubeUser { username, groups, uid: None })
}

fn same_node(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// A job's state as a DriveOperation phase and one-line message.
pub fn phase_of(job: &Job) -> (&'static str, String) {
    let n = |s: DjState| job.drives.iter().filter(|d| d.state == s).count();
    let (done, failed, refused, cancelled, interrupted) = (n(DjState::Done), n(DjState::Failed), n(DjState::Refused), n(DjState::Cancelled), n(DjState::Interrupted));
    let active = n(DjState::Queued) + n(DjState::Running);
    let total = job.drives.len();
    let msg = format!("{done}/{total} done, {failed} failed, {refused} refused, {cancelled} cancelled, {interrupted} interrupted");
    let phase = if active > 0 {
        "Running"
    } else if interrupted > 0 {
        "Interrupted"
    } else if failed > 0 {
        "Failed"
    } else if done > 0 {
        "Succeeded"
    } else if cancelled > 0 {
        "Cancelled"
    } else {
        "Failed"
    };
    (phase, msg)
}

fn secs(t: Option<SystemTime>) -> Option<u64> {
    t.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs())
}

/// `status` for an operation whose job exists.
pub fn status_of(job: &Job, observed: Option<i64>, resumed: u64) -> Value {
    let (phase, message) = phase_of(job);
    json!({
        "phase": phase,
        "message": message,
        "job": job.id,
        "requester": job.requester.as_ref().map(|r| r.who.clone()),
        "observedGeneration": observed,
        "resumed": resumed,
        "steps": job.steps.iter().map(|s| s.describe()).collect::<Vec<_>>(),
        "drives": job.drives.iter().map(|d| json!({
            "drive": d.drive.0.to_string(), "name": d.name, "serial": d.serial, "wwn": d.wwn,
            "state": d.state, "step": d.step, "phase": d.phase, "progressPct": d.progress_pct,
            "error": d.error, "done": d.done, "started": secs(d.started), "finished": secs(d.finished),
        })).collect::<Vec<_>>(),
    })
}

pub struct Controller {
    state: Arc<AppState>,
    kube: Arc<KubeApi>,
    written: HashMap<String, (String, Instant)>,
    /// Operation status last written, so an unchanged one is not rewritten.
    op_status: HashMap<String, String>,
}

/// Run the controller until the process ends. One pass every
/// `kubernetes.interval_secs`; an apiserver that does not answer is retried.
pub async fn run(state: Arc<AppState>, kube: Arc<KubeApi>) {
    tracing::info!(apiserver = kube.base(), node = %state.node_name, "kubernetes: keeping Drive objects and running DriveOperations");
    let every = Duration::from_secs(state.config.kubernetes.interval_secs);
    let mut c = Controller { state, kube, written: HashMap::new(), op_status: HashMap::new() };
    let mut last_err = String::new();
    loop {
        let r = async {
            c.drives().await?;
            c.operations().await
        }
        .await;
        match r {
            Ok(()) => last_err.clear(),
            Err(e) if e != last_err => {
                tracing::warn!("kubernetes: {e}");
                last_err = e;
            }
            Err(_) => {}
        }
        tokio::time::sleep(every).await;
    }
}

impl Controller {
    /// Make this node's Drive objects match the inventory.
    async fn drives(&mut self) -> Result<(), String> {
        let node = self.state.node_name.clone();
        let objs: Vec<Value> = {
            let inv = self.state.inventory.read().await;
            inv.drives.values().map(|d| apiserver_drive(d, &node)).collect()
        };
        let mine: HashSet<String> = objs.iter().filter_map(|o| o["metadata"]["name"].as_str().map(str::to_string)).collect();
        for o in objs {
            let name = o["metadata"]["name"].as_str().unwrap_or_default().to_string();
            let fp = drive_fingerprint(&o);
            if let Some((prev, at)) = self.written.get(&name) {
                if *prev == fp && at.elapsed() < REFRESH {
                    continue;
                }
            }
            self.put_drive(&name, &o).await?;
            self.written.insert(name, (fp, Instant::now()));
        }
        // Objects of this node whose drive was forgotten.
        let sel = format!("storm.io/component=stormdrive,storm.io/node={}", label_value(&node).unwrap_or_default());
        let list = self.kube.get(&format!("{}?labelSelector={}", path("drives", None), urlencode(&sel))).await.map_err(|e| format!("listing Drives: {e}"))?;
        for item in list["items"].as_array().into_iter().flatten() {
            let Some(name) = item["metadata"]["name"].as_str() else { continue };
            if item["metadata"]["labels"]["storm.io/node"].as_str() != label_value(&node).as_deref() {
                continue; // a label selector the apiserver ignored
            }
            if !mine.contains(name) {
                match self.kube.delete(&path("drives", Some(name))).await {
                    Ok(_) => tracing::info!("kubernetes: Drive {name} deleted (forgotten here)"),
                    Err(e) if e.code() == Some(404) => {}
                    Err(e) => return Err(format!("deleting Drive {name}: {e}")),
                }
                self.written.remove(name);
            }
        }
        Ok(())
    }

    async fn put_drive(&self, name: &str, o: &Value) -> Result<(), String> {
        let main = json!({ "metadata": { "labels": o["metadata"]["labels"] }, "spec": o["spec"] });
        match self.kube.merge_patch(&path("drives", Some(name)), &main).await {
            Ok(_) => {}
            Err(e) if e.code() == Some(404) => {
                let body = json!({ "apiVersion": o["apiVersion"], "kind": "Drive", "metadata": o["metadata"], "spec": o["spec"] });
                match self.kube.create(&path("drives", None), &body).await {
                    Ok(_) | Err(crate::kubeapi::KubeError::Status(409, _)) => {}
                    Err(e) => return Err(format!("creating Drive {name}: {e}")),
                }
            }
            Err(e) => return Err(format!("Drive {name}: {e}")),
        }
        self.kube
            .merge_patch(&format!("{}/status", path("drives", Some(name))), &json!({ "status": o["status"] }))
            .await
            .map(|_| ())
            .map_err(|e| format!("Drive {name} status: {e}"))
    }

    /// Run this node's DriveOperations.
    async fn operations(&mut self) -> Result<(), String> {
        let list = self.kube.get(&path("driveoperations", None)).await.map_err(|e| format!("listing DriveOperations: {e}"))?;
        let node = self.state.node_name.clone();
        let mut present: HashSet<String> = HashSet::new();
        for op in list["items"].as_array().into_iter().flatten() {
            let Some(name) = op["metadata"]["name"].as_str().map(str::to_string) else { continue };
            if !same_node(op["spec"]["node"].as_str().unwrap_or(""), &node) {
                continue;
            }
            present.insert(name.clone());
            if op["metadata"]["deletionTimestamp"].is_string() {
                continue;
            }
            if let Err(e) = self.operation(&name, op).await {
                tracing::warn!("kubernetes: DriveOperation {name}: {e}");
            }
        }
        // A deleted operation stops: its queued steps are cancelled.
        for (job, op) in self.state.worker.operation_jobs() {
            if !present.contains(&op) {
                if crate::worker::cancel(&self.state, &job).await.is_ok() {
                    self.audit(&op, "cancelled", &format!("DriveOperation deleted: job {job}'s queued steps cancelled")).await;
                }
                self.op_status.remove(&op);
            }
        }
        Ok(())
    }

    async fn operation(&mut self, name: &str, op: &Value) -> Result<(), String> {
        let generation = op["metadata"]["generation"].as_i64();
        let status = &op["status"];
        let resume_wanted = op["spec"]["resume"].as_u64().unwrap_or(0);
        let resumed = status["resumed"].as_u64().unwrap_or(0);
        if let Some(job) = self.state.worker.by_operation(name) {
            let mut resumed_now = resumed;
            if resume_wanted > resumed && job.drives.iter().any(|d| d.state == DjState::Interrupted) {
                let who = match self.requester(name, op).await {
                    Ok(r) => r,
                    Err((_, why)) => {
                        self.event(op, "Warning", "ResumeRefused", &why).await;
                        return self.put_op_status(name, json!({ "resumed": resume_wanted, "message": why })).await;
                    }
                };
                match crate::worker::resume(&self.state, &job.id, Some(who.clone())).await {
                    Ok(_) => self.event(op, "Normal", "Resumed", &format!("job {} resumed for {}", job.id, who.who)).await,
                    Err(e) => self.event(op, "Warning", "ResumeFailed", &e).await,
                }
                resumed_now = resume_wanted;
            } else if resume_wanted > resumed {
                resumed_now = resume_wanted;
            }
            let job = self.state.worker.job(&job.id).unwrap_or(job);
            let st = status_of(&job, generation, resumed_now);
            let was = status["phase"].as_str().unwrap_or("").to_string();
            let now = st["phase"].as_str().unwrap_or("").to_string();
            if was != now && matches!(now.as_str(), "Succeeded" | "Failed" | "Cancelled" | "Interrupted") {
                let kind = if now == "Succeeded" { "Normal" } else { "Warning" };
                self.event(op, kind, &now, st["message"].as_str().unwrap_or("")).await;
                self.audit(name, &now.to_lowercase(), st["message"].as_str().unwrap_or("")).await;
            }
            return self.put_op_status(name, st).await;
        }
        // Already answered (refused, planned, or a job since pruned).
        if status["phase"].is_string() && status["phase"] != "Pending" && status["observedGeneration"].as_i64() == generation {
            return Ok(());
        }
        let who = match self.requester(name, op).await {
            Ok(r) => r,
            Err((phase, why)) => {
                if status["phase"].as_str() != Some(phase) || status["message"].as_str() != Some(&why) {
                    self.event(op, "Warning", if phase == "Refused" { "Refused" } else { "Pending" }, &why).await;
                    self.audit(name, &phase.to_lowercase(), &why).await;
                }
                return self.put_op_status(name, json!({ "phase": phase, "message": why, "observedGeneration": generation })).await;
            }
        };
        let req: crate::worker::Request = match serde_json::from_value(op["spec"].clone()) {
            Ok(r) => r,
            Err(e) => {
                let why = format!("spec: {e}");
                self.event(op, "Warning", "Invalid", &why).await;
                return self.put_op_status(name, json!({ "phase": "Refused", "message": why, "requester": who.who, "observedGeneration": generation })).await;
            }
        };
        let dry = req.dry_run;
        match crate::worker::submit_for(&self.state, req, Some(who.clone()), Some(name.to_string())).await {
            Ok(v) if dry => {
                let msg = format!("dry run: {} drive(s) would run, {} refused", v["runnable"], v["refused"].as_array().map(|a| a.len()).unwrap_or(0));
                self.event(op, "Normal", "Planned", &msg).await;
                self.put_op_status(name, json!({ "phase": "Planned", "message": msg, "requester": who.who, "plan": v, "observedGeneration": generation })).await
            }
            Ok(v) => {
                let job = v["id"].as_str().unwrap_or("").to_string();
                let msg = format!("job {job} for {}: {}", who.who, v["steps"].as_array().map(|s| s.iter().filter_map(|x| x["op"].as_str()).collect::<Vec<_>>().join(" → ")).unwrap_or_default());
                self.event(op, "Normal", "Accepted", &msg).await;
                self.audit(name, "accepted", &msg).await;
                match self.state.worker.job(&job) {
                    Some(j) => self.put_op_status(name, status_of(&j, generation, resume_wanted)).await,
                    None => Ok(()),
                }
            }
            Err(e) => {
                self.event(op, "Warning", "Refused", &e).await;
                self.audit(name, "refused", &e).await;
                self.put_op_status(name, json!({ "phase": "Refused", "message": e, "requester": who.who, "observedGeneration": generation })).await
            }
        }
    }

    /// The stamped requester, re-checked: may they create this operation?
    /// `Err((phase, why))`: `Refused` for good, `Pending` to ask again.
    async fn requester(&self, name: &str, op: &Value) -> Result<Requester, (&'static str, String)> {
        let Some(user) = stamped_requester(op) else {
            return Err(("Refused", format!("no requester stamped by the apiserver ({REQUESTER}, rustkube#210): the object alone is not trusted")));
        };
        let who = Requester::kube(user);
        let access = Access { resource: "driveoperations", verb: "create", name: Some(name.to_string()) };
        match self.state.gate.recheck(&who, &access).await {
            Ok(()) => Ok(who),
            Err(crate::kubeauth::RecheckError::Denied(why)) => Err(("Refused", why)),
            Err(crate::kubeauth::RecheckError::Unavailable(why)) => Err(("Pending", format!("cannot re-check {}: {why}", who.who))),
        }
    }

    async fn put_op_status(&mut self, name: &str, st: Value) -> Result<(), String> {
        let s = st.to_string();
        if self.op_status.get(name) == Some(&s) {
            return Ok(());
        }
        self.kube
            .merge_patch(&format!("{}/status", path("driveoperations", Some(name))), &json!({ "status": st }))
            .await
            .map_err(|e| format!("status: {e}"))?;
        self.op_status.insert(name.to_string(), s);
        Ok(())
    }

    /// A Kubernetes Event on the operation (best effort: the audit line and
    /// the event ring have it too).
    async fn event(&self, op: &Value, kind: &str, reason: &str, message: &str) {
        let name = op["metadata"]["name"].as_str().unwrap_or("");
        let now = chrono_now();
        let ev = json!({
            "apiVersion": "v1", "kind": "Event",
            "metadata": { "generateName": format!("{name}."), "namespace": "default" },
            "involvedObject": { "apiVersion": format!("{GROUP}/{VERSION}"), "kind": "DriveOperation", "name": name, "uid": op["metadata"]["uid"] },
            "reason": reason, "message": message, "type": kind,
            "source": { "component": "stormdrive", "host": self.state.node_name },
            "firstTimestamp": now, "lastTimestamp": now, "count": 1,
            "reportingComponent": "stormdrive", "reportingInstance": self.state.node_name,
        });
        if let Err(e) = self.kube.create("/api/v1/namespaces/default/events", &ev).await {
            tracing::debug!("kubernetes: event on {name}: {e}");
        }
    }

    async fn audit(&self, op: &str, decision: &str, message: &str) {
        let access = Access { resource: "driveoperations", verb: "create", name: Some(op.to_string()) };
        let line = crate::kubeauth::audit_line("OBJECT", &path("driveoperations", Some(op)), &access, "controller", decision, message, None);
        self.state.gate.audit(&line);
        let sev = if decision == "succeeded" || decision == "accepted" { crate::events::Severity::Info } else { crate::events::Severity::Warning };
        self.state.events.write().await.push(None, sev, "operation", format!("DriveOperation {op}: {decision} — {message}"));
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// RFC 3339 UTC, seconds.
fn chrono_now() -> String {
    let s = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    rfc3339(s)
}

pub fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil from days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::DriveId;
    use crate::worker::{DriveJob, Select, Step};

    #[test]
    fn label_values_are_valid() {
        assert_eq!(label_value("/dev/sda").as_deref(), Some("dev_sda"));
        assert_eq!(label_value("host3").as_deref(), Some("host3"));
        assert_eq!(label_value("0x5000c500a1b2c3d4").as_deref(), Some("0x5000c500a1b2c3d4"));
        assert_eq!(label_value("///"), None);
        assert_eq!(label_value(&"a".repeat(80)).unwrap().len(), 63);
    }

    #[test]
    fn the_requester_is_the_stamp_only() {
        let op = json!({ "metadata": { "annotations": { REQUESTER: "alice", REQUESTER_GROUPS: "storage-admins, system:authenticated" } }, "spec": { "requester": "mallory" } });
        let u = stamped_requester(&op).unwrap();
        assert_eq!(u.username, "alice");
        assert_eq!(u.groups, vec!["storage-admins", "system:authenticated"]);
        assert!(stamped_requester(&json!({ "metadata": {} })).is_none());
        assert!(stamped_requester(&json!({ "metadata": { "annotations": { REQUESTER: "  " } } })).is_none());
    }

    fn job(states: &[DjState]) -> Job {
        Job {
            id: "j1".into(),
            created: SystemTime::now(),
            select: Select { drives: vec!["sdb".into()], ..Default::default() },
            steps: vec![Step::Format { block_size: 4096 }],
            destroy: vec![],
            drives: states
                .iter()
                .enumerate()
                .map(|(i, s)| DriveJob {
                    drive: DriveId::derive(None, "M", &format!("s{i}")),
                    name: format!("sd{i}"),
                    serial: format!("s{i}"),
                    wwn: None,
                    kind: crate::drive::DriveKind::SasHdd,
                    state: *s,
                    step: 0,
                    phase: "x".into(),
                    progress_pct: None,
                    error: None,
                    done: vec![],
                    destroy_named: false,
                    started: None,
                    finished: None,
                })
                .collect(),
            cancel: false,
            requester: Some(Requester::kube(KubeUser { username: "alice".into(), groups: vec![], uid: None })),
            operation: Some("op1".into()),
        }
    }

    #[test]
    fn phases() {
        use DjState::*;
        assert_eq!(phase_of(&job(&[Done, Running])).0, "Running");
        assert_eq!(phase_of(&job(&[Done, Queued])).0, "Running");
        assert_eq!(phase_of(&job(&[Done, Interrupted])).0, "Interrupted");
        assert_eq!(phase_of(&job(&[Done, Failed])).0, "Failed");
        assert_eq!(phase_of(&job(&[Done, Refused])).0, "Succeeded");
        assert_eq!(phase_of(&job(&[Cancelled])).0, "Cancelled");
        let st = status_of(&job(&[Done]), Some(3), 0);
        assert_eq!(st["phase"], "Succeeded");
        assert_eq!(st["requester"], "kubernetes:alice");
        assert_eq!(st["observedGeneration"], 3);
        assert_eq!(st["drives"][0]["name"], "sd0");
    }

    #[test]
    fn an_operation_spec_is_a_worker_job() {
        let spec = json!({
            "node": "c2nr0q2",
            "select": { "shelf": "500a098000000000", "bays": "0-11", "unusable": true },
            "steps": [ { "op": "format", "blockSize": 4096 }, { "op": "sanitize", "method": "overwrite" }, { "op": "partition" } ],
            "destroy": [],
            "dryRun": true,
            "resume": 0,
        });
        let r: crate::worker::Request = serde_json::from_value(spec).unwrap();
        assert!(r.dry_run);
        assert_eq!(r.steps[0], Step::Format { block_size: 4096 });
        assert_eq!(r.select.bays.as_deref(), Some("0-11"));
    }

    #[test]
    fn apiserver_drive_has_valid_labels() {
        let d = crate::drive::Drive::test_fixture("sda");
        let v = apiserver_drive(&d, "C2NR0Q2");
        assert!(v["metadata"]["labels"].get("storm.io/path").is_none());
        assert_eq!(v["metadata"]["labels"]["storm.io/node"], "C2NR0Q2");
        assert_eq!(v["status"]["path"], "/dev/sda");
        assert!(v["metadata"].get("uid").is_none(), "the apiserver assigns the uid");
    }

    #[test]
    fn timestamps() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_791_244_800), "2026-10-06T00:00:00Z");
        assert_eq!(rfc3339(951_782_400 + 3661), "2000-02-29T01:01:01Z");
    }
}
