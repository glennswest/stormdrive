//! Who may change a drive (#45, stormcos#250).
//!
//! Owner, 2026-10-03: "we need to make sure we have a security model that non
//! admins cant format drives etc." Every write on :9092 — format, sanitize,
//! partition, enroll, firmware, tests, fleet join/leave, designation, locate,
//! … — needs one of:
//!
//! - a **Kubernetes bearer** the apiserver vouches for: TokenReview (who is
//!   it?), then SubjectAccessReview for that user on `storage.storm.io`, the
//!   resource and verb [`classify`] names. The release's `storage-admin`
//!   ClusterRole allows them; `storage-viewer` does not;
//! - the node-local **admin token** (`[api] admin_token`), for a node with
//!   no apiserver. It is root's, never mounted into another service.
//!
//! Reads stay open (#19 is TLS and read access). Outcomes: a valid bearer that
//! is not allowed → 403 with the apiserver's reason; anything else that is not
//! a credential → 401; the apiserver unreachable → 503. Answers are cached for
//! a minute per bearer, resource, verb and name; [`Gate::recheck`], which the
//! worker runs before each destroying step, never uses the cache.
//!
//! `admin_gate = "audit"` lets a refused write through and logs it as one
//! `enforce` would refuse. Every decision on a write is audited (who, what,
//! which drive, the outcome): the event ring, the log, and
//! `<data_dir>/audit.log`.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::ApiConfig;
use crate::kubeapi::{KubeApi, KubeError, KubeUser};

const CACHE_TTL: Duration = Duration::from_secs(60);

/// Who asked for a write, as the gate established it. Carried by a worker
/// job, so the worker can ask again before it destroys anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requester {
    /// `kubernetes:<user>`, `admin-token`, or `none` (let through by
    /// `admin_gate = "audit"`).
    pub who: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<KubeUser>,
}

impl Requester {
    pub fn kube(user: KubeUser) -> Self {
        Requester { who: format!("kubernetes:{}", user.username), user: Some(user) }
    }
    pub fn admin_token() -> Self {
        Requester { who: "admin-token".into(), user: None }
    }
}

/// What a write is, in RBAC terms: `storage.storm.io` resource, verb, name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Access {
    pub resource: &'static str,
    pub verb: &'static str,
    pub name: Option<String>,
}

impl Access {
    fn new(resource: &'static str, verb: &'static str, name: Option<&str>) -> Self {
        Access { resource, verb, name: name.map(str::to_string) }
    }
    /// Writes that act on a drive's contents or firmware: a drive operation.
    pub fn destructive(&self) -> bool {
        self.resource == "driveoperations"
    }
}

/// The access a request needs, or `None` for a read.
///
/// - format, firmware update, worker jobs, fleet join/leave, tests (not their
///   cancel) → `create driveoperations`; resuming or cancelling a job →
///   `update driveoperations/<job>`;
/// - forgetting a drive → `delete drives/<id>`; every other drive write
///   (designation, overcommit, locate, drain, test cancel) and the
///   resource PATCH → `update`/`patch drives/<id>`;
/// - shelf locate → `update enclosures/<key>`;
/// - firmware images → `create`/`delete firmwareimages/<name>`;
/// - any other write → `update drives` (nothing is open by omission).
pub fn classify(method: &str, path: &str) -> Option<Access> {
    if matches!(method, "GET" | "HEAD" | "OPTIONS") {
        return None;
    }
    let segs: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let op = |name: Option<&str>| Access::new("driveoperations", "create", name);
    match segs.as_slice() {
        ["api", "v1", "worker", "jobs"] => Some(op(None)),
        ["api", "v1", "worker", "jobs", id, ..] => Some(Access::new("driveoperations", "update", Some(id))),
        ["api", "v1", "format"] | ["api", "v1", "firmware"] => Some(op(None)),
        ["api", "v1", "firmware", "images", name, ..] => {
            Some(Access::new("firmwareimages", if method == "DELETE" { "delete" } else { "create" }, Some(name)))
        }
        ["api", "v1", "shelves", _, "format", ..] => Some(op(None)),
        ["api", "v1", "shelves", key, ..] => Some(Access::new("enclosures", "update", Some(key))),
        ["api", "v1", "drives", id] if method == "DELETE" => Some(Access::new("drives", "delete", Some(id))),
        ["api", "v1", "drives", _, "test", "cancel"] => Some(Access::new("drives", "update", segs.get(3).copied())),
        ["api", "v1", "drives", _, "format" | "firmware" | "fleet" | "test", ..] => Some(op(None)),
        ["api", "v1", "drives", id, ..] => Some(Access::new("drives", "update", Some(id))),
        ["apis", _, _, "drives", name] => Some(Access::new("drives", "patch", Some(name))),
        _ => Some(Access::new("drives", "update", None)),
    }
}

/// The gate's answer for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allowed(Requester),
    /// Refused under `enforce`, let through under `audit`.
    AuditOnly(Requester, String),
    /// HTTP status, who (as far as known), why.
    Refused(u16, String, String),
}

#[derive(Clone)]
enum Review {
    Allowed(KubeUser),
    Denied(KubeUser, String),
    Unauthenticated(String),
}

/// Bearer, resource, verb, name.
type CacheKey = (String, &'static str, &'static str, Option<String>);

pub struct Gate {
    kube: Option<Arc<KubeApi>>,
    admin_token: Option<String>,
    enforce: bool,
    audit_log: Option<PathBuf>,
    cache: Mutex<HashMap<CacheKey, (Instant, Review)>>,
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Equal in time independent of where the first difference is.
fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Gate {
    pub fn new(cfg: &ApiConfig, kube: Option<Arc<KubeApi>>, data_dir: Option<&str>) -> Self {
        let admin_token = non_empty(Some(cfg.admin_token.clone()))
            .or_else(|| non_empty(std::env::var("STORMDRIVE_ADMIN_TOKEN").ok()))
            .or_else(|| non_empty(Some(cfg.admin_token_file.clone())).and_then(|f| non_empty(std::fs::read_to_string(f).ok())));
        let enforce = cfg.admin_gate != "audit";
        if !enforce {
            tracing::warn!("api.admin_gate = audit: writes without an allowed bearer go through (and are logged)");
        }
        if kube.is_none() && admin_token.is_none() {
            tracing::warn!("no apiserver and no admin token: every write on the API will be refused");
        }
        Gate { kube, admin_token, enforce, audit_log: data_dir.map(|d| PathBuf::from(d).join("audit.log")), cache: Mutex::new(HashMap::new()) }
    }

    /// For `/api/v1/health`: how writes are decided here (no secrets).
    pub fn describe(&self) -> serde_json::Value {
        json!({
            "gate": if self.enforce { "enforce" } else { "audit" },
            "apiserver": self.kube.is_some(),
            "admin_token": self.admin_token.is_some(),
        })
    }

    pub fn enforcing(&self) -> bool {
        self.enforce
    }

    pub fn kube(&self) -> Option<&Arc<KubeApi>> {
        self.kube.as_ref()
    }

    /// May the holder of `bearer` make this write?
    pub async fn check(&self, bearer: Option<&str>, access: &Access) -> Decision {
        let refuse = |code: u16, who: &str, why: String| {
            if self.enforce {
                Decision::Refused(code, who.to_string(), why)
            } else {
                Decision::AuditOnly(Requester { who: who.to_string(), user: None }, why)
            }
        };
        let Some(bearer) = bearer.map(str::trim).filter(|b| !b.is_empty()) else {
            return refuse(401, "none", format!("a storage-admin bearer is required to {} {}", access.verb, access.resource));
        };
        if let Some(t) = &self.admin_token {
            if same(bearer, t) {
                return Decision::Allowed(Requester::admin_token());
            }
        }
        let Some(kube) = &self.kube else {
            return refuse(401, "unknown-bearer", "not the admin token, and no apiserver is configured to review it".into());
        };
        let key = (bearer.to_string(), access.resource, access.verb, access.name.clone());
        let cached = {
            let c = self.cache.lock().unwrap();
            c.get(&key).filter(|(at, _)| at.elapsed() < CACHE_TTL).map(|(_, r)| r.clone())
        };
        let review = match cached {
            Some(r) => r,
            None => {
                let r = match review(kube, bearer, access).await {
                    Ok(r) => r,
                    Err(e) => {
                        // Not cached: the next request asks again.
                        return refuse(503, "unknown-bearer", format!("cannot review the bearer: {e}"));
                    }
                };
                let mut c = self.cache.lock().unwrap();
                c.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
                c.insert(key, (Instant::now(), r.clone()));
                r
            }
        };
        match review {
            Review::Allowed(u) => Decision::Allowed(Requester::kube(u)),
            Review::Denied(u, why) => {
                let msg = format!(
                    "{} may not {} {}.{}{}",
                    u.username,
                    access.verb,
                    access.resource,
                    crate::api::kube::GROUP,
                    if why.is_empty() { String::new() } else { format!(": {why}") }
                );
                if self.enforce {
                    Decision::Refused(403, format!("kubernetes:{}", u.username), msg)
                } else {
                    Decision::AuditOnly(Requester::kube(u), msg)
                }
            }
            Review::Unauthenticated(why) => refuse(401, "unknown-bearer", why),
        }
    }

    /// Ask the apiserver again, uncached, whether `who` may still do
    /// `access` — the worker's check before each destroying step. `Ok` for
    /// the admin token and, under `audit`, for anyone.
    pub async fn recheck(&self, who: &Requester, access: &Access) -> Result<(), RecheckError> {
        let Some(user) = &who.user else {
            return match who.who.as_str() {
                "admin-token" => Ok(()),
                _ if !self.enforce => Ok(()),
                other => Err(RecheckError::Denied(format!("requester {other} cannot be re-checked"))),
            };
        };
        let Some(kube) = &self.kube else {
            return Err(RecheckError::Unavailable("no apiserver configured".into()));
        };
        match kube.access_review(user, access.resource, access.verb, access.name.as_deref()).await {
            Ok((true, _)) => Ok(()),
            Ok((false, why)) if self.enforce => Err(RecheckError::Denied(format!(
                "{} may no longer {} {}{}",
                user.username,
                access.verb,
                access.resource,
                if why.is_empty() { String::new() } else { format!(": {why}") }
            ))),
            Ok((false, _)) => Ok(()),
            Err(e) => Err(RecheckError::Unavailable(e.to_string())),
        }
    }

    /// One audit line: the log, `<data_dir>/audit.log`. The caller also
    /// pushes it to the event ring.
    pub fn audit(&self, line: &serde_json::Value) {
        tracing::info!(target: "stormdrive::audit", "{line}");
        let Some(p) = &self.audit_log else { return };
        let r = std::fs::OpenOptions::new().create(true).append(true).open(p).and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = r {
            tracing::warn!("audit log {}: {e}", p.display());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecheckError {
    Denied(String),
    Unavailable(String),
}

async fn review(kube: &KubeApi, bearer: &str, access: &Access) -> Result<Review, KubeError> {
    let Some(user) = kube.token_review(bearer).await? else {
        return Ok(Review::Unauthenticated("the apiserver does not recognise the bearer".into()));
    };
    let (allowed, why) = kube.access_review(&user, access.resource, access.verb, access.name.as_deref()).await?;
    Ok(if allowed { Review::Allowed(user) } else { Review::Denied(user, why) })
}

/// The JSON line for one decision.
pub fn audit_line(method: &str, path: &str, access: &Access, who: &str, decision: &str, reason: &str, status: Option<u16>) -> serde_json::Value {
    let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    json!({
        "time": secs, "who": who, "method": method, "path": path,
        "resource": access.resource, "verb": access.verb, "target": access.name,
        "decision": decision, "reason": reason, "status": status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(r: &'static str, v: &'static str, n: Option<&str>) -> Option<Access> {
        Some(Access::new(r, v, n))
    }

    #[test]
    fn reads_are_open() {
        for p in ["/api/v1/drives", "/api/v1/worker/jobs", "/apis/storage.storm.io/v1/drives/x"] {
            assert_eq!(classify("GET", p), None);
        }
    }

    #[test]
    fn writes_map_to_storage_access() {
        let op = a("driveoperations", "create", None);
        assert_eq!(classify("POST", "/api/v1/worker/jobs"), op);
        assert_eq!(classify("POST", "/api/v1/format"), op);
        assert_eq!(classify("POST", "/api/v1/firmware"), op);
        assert_eq!(classify("POST", "/api/v1/drives/sdb/format/4096"), op);
        assert_eq!(classify("POST", "/api/v1/drives/sdb/firmware"), op);
        assert_eq!(classify("POST", "/api/v1/drives/sdb/fleet/join"), op);
        assert_eq!(classify("POST", "/api/v1/drives/sdb/test/smoke"), op);
        assert_eq!(classify("POST", "/api/v1/shelves/5000abc/format"), op);
        assert_eq!(classify("POST", "/api/v1/worker/jobs/j1/resume"), a("driveoperations", "update", Some("j1")));
        assert_eq!(classify("POST", "/api/v1/drives/sdb/test/cancel"), a("drives", "update", Some("sdb")));
        assert_eq!(classify("POST", "/api/v1/drives/sdb/designation/spare"), a("drives", "update", Some("sdb")));
        assert_eq!(classify("PUT", "/api/v1/drives/sdb/overcommit"), a("drives", "update", Some("sdb")));
        assert_eq!(classify("DELETE", "/api/v1/drives/sdb/drain"), a("drives", "update", Some("sdb")));
        assert_eq!(classify("DELETE", "/api/v1/drives/sdb"), a("drives", "delete", Some("sdb")));
        assert_eq!(classify("POST", "/api/v1/shelves/5000abc/locate/on"), a("enclosures", "update", Some("5000abc")));
        assert_eq!(classify("PUT", "/api/v1/firmware/images/x.bin"), a("firmwareimages", "create", Some("x.bin")));
        assert_eq!(classify("DELETE", "/api/v1/firmware/images/x.bin"), a("firmwareimages", "delete", Some("x.bin")));
        assert_eq!(classify("PATCH", "/apis/storage.storm.io/v1/drives/u1"), a("drives", "patch", Some("u1")));
        assert_eq!(classify("POST", "/api/v1/something/new"), a("drives", "update", None), "nothing is open by omission");
        assert!(classify("POST", "/api/v1/worker/jobs").unwrap().destructive());
        assert!(!classify("POST", "/api/v1/drives/sdb/locate").unwrap().destructive());
    }

    fn gate(token: Option<&str>, enforce: bool) -> Gate {
        let cfg = ApiConfig { admin_token: token.unwrap_or("").into(), admin_token_file: String::new(), admin_gate: if enforce { "enforce" } else { "audit" }.into() };
        Gate::new(&cfg, None, None)
    }

    #[tokio::test]
    async fn without_an_apiserver_only_the_admin_token_writes() {
        if std::env::var("STORMDRIVE_ADMIN_TOKEN").is_ok() {
            return;
        }
        let g = gate(Some("s3cret"), true);
        let op = classify("POST", "/api/v1/worker/jobs").unwrap();
        assert_eq!(g.check(Some("s3cret"), &op).await, Decision::Allowed(Requester::admin_token()));
        assert!(matches!(g.check(None, &op).await, Decision::Refused(401, w, _) if w == "none"));
        assert!(matches!(g.check(Some("s3cre"), &op).await, Decision::Refused(401, w, _) if w == "unknown-bearer"));
        assert!(matches!(g.check(Some("   "), &op).await, Decision::Refused(401, _, _)));
        // No token configured at all: nothing writes.
        let closed = gate(None, true);
        assert!(matches!(closed.check(Some(""), &op).await, Decision::Refused(401, _, _)));
    }

    #[tokio::test]
    async fn audit_lets_through_and_says_so() {
        if std::env::var("STORMDRIVE_ADMIN_TOKEN").is_ok() {
            return;
        }
        let g = gate(None, false);
        let op = classify("POST", "/api/v1/format").unwrap();
        assert!(matches!(g.check(None, &op).await, Decision::AuditOnly(r, _) if r.who == "none"));
        assert_eq!(g.recheck(&Requester { who: "none".into(), user: None }, &op).await, Ok(()));
    }

    #[tokio::test]
    async fn recheck_needs_someone_to_ask() {
        let g = gate(Some("t"), true);
        let op = classify("POST", "/api/v1/format").unwrap();
        assert_eq!(g.recheck(&Requester::admin_token(), &op).await, Ok(()));
        assert!(matches!(g.recheck(&Requester { who: "none".into(), user: None }, &op).await, Err(RecheckError::Denied(_))));
        let alice = Requester::kube(KubeUser { username: "alice".into(), groups: vec![], uid: None });
        assert!(matches!(g.recheck(&alice, &op).await, Err(RecheckError::Unavailable(_))));
    }

    #[test]
    fn constant_time_compare() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "ab"));
    }
}
