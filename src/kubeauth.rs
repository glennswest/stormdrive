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
//! - a **client certificate** from the node CA (verified in the TLS
//!   handshake, #19): its CN is the user and each O a group, reviewed with a
//!   SubjectAccessReview as a bearer's user is.
//!
//! Reads (#19) need a credential too: the admin token, a node-CA client
//! certificate (the node CA vouches; no review), or a bearer the apiserver
//! allows `get` on `storage.storm.io` ([`read_access`]; the release's
//! `storage-viewer` role). `[api] allow_anonymous` (the transition) serves a
//! read that carries no credential; one that carries a bad one is refused.
//!
//! Outcomes: a valid caller that is not allowed → 403 with the apiserver's
//! reason; anything else that is not a credential → 401; the apiserver
//! unreachable → 503. Answers are cached for a minute per caller, resource,
//! verb and name; [`Gate::recheck`], which the worker runs before each
//! destroying step, never uses the cache.
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
use crate::tls::ClientIdentity;

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
    /// A node-CA client certificate: CN the user, each O a group.
    pub fn cert(c: &ClientIdentity) -> Self {
        Requester { who: format!("cert:{}", c.cn), user: Some(c.user()) }
    }
}

impl ClientIdentity {
    /// The certificate's subject as the apiserver reads one.
    pub fn user(&self) -> KubeUser {
        KubeUser { username: self.cn.clone(), groups: self.groups.clone(), uid: None }
    }
}

/// What a caller presented: a bearer (`Authorization: Bearer`), a verified
/// client certificate, both or neither. A bearer wins over a certificate:
/// a service acting for a person forwards the person's bearer.
#[derive(Debug, Clone, Copy, Default)]
pub struct Credential<'a> {
    pub bearer: Option<&'a str>,
    pub cert: Option<&'a ClientIdentity>,
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
        ["api", "v1", "shelves", _, "format" | "firmware" | "bays", ..] => Some(op(None)),
        ["api", "v1", "shelves", key, ..] => Some(Access::new("enclosures", "update", Some(key))),
        ["api", "v1", "drives", id] if method == "DELETE" => Some(Access::new("drives", "delete", Some(id))),
        ["api", "v1", "drives", _, "test", "cancel"] => Some(Access::new("drives", "update", segs.get(3).copied())),
        ["api", "v1", "drives", _, "format" | "firmware" | "fleet" | "test" | "enroll", ..] => Some(op(None)),
        ["api", "v1", "drives", id, ..] => Some(Access::new("drives", "update", Some(id))),
        ["apis", _, _, "drives", name] => Some(Access::new("drives", "patch", Some(name))),
        _ => Some(Access::new("drives", "update", None)),
    }
}

/// The access a read needs: `get` on the resource the path is about —
/// worker jobs are `driveoperations`, shelves `enclosures`, firmware images
/// `firmwareimages`, everything else (drives, placement, the feed, metrics,
/// the page) `drives`. `storage-viewer` allows all of them.
pub fn read_access(path: &str) -> Access {
    let segs: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let resource = match segs.as_slice() {
        ["api", "v1", "worker", ..] => "driveoperations",
        ["api", "v1", "shelves", ..] | ["apis", _, _, "enclosures", ..] => "enclosures",
        ["api", "v1", "firmware", "images", ..] => "firmwareimages",
        _ => "drives",
    };
    Access::new(resource, "get", None)
}

/// Paths anyone may ask, over either transport: health (stormd's liveness
/// probe and stormcentral's check send nothing). It names the node and the
/// version and how writes are decided — nothing about a drive.
pub fn is_health(path: &str) -> bool {
    matches!(path, "/api/v1/health" | "/healthz")
}

/// The page's code: the shell (`/`, `/ui`) is answered 401 with the page to
/// a caller with no credential — it signs in — and the assets are served to
/// anyone. Neither holds a drive's data.
pub fn is_page_asset(path: &str) -> bool {
    path.starts_with("/assets/") || path.starts_with("/ui/assets/")
}

pub fn is_page_shell(path: &str) -> bool {
    matches!(path, "/" | "/ui" | "/ui/")
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

/// Caller (`b:<bearer>` or `c:<cn>|<groups>`), resource, verb, name.
type CacheKey = (String, &'static str, &'static str, Option<String>);

pub struct Gate {
    kube: Option<Arc<KubeApi>>,
    admin_token: Option<String>,
    enforce: bool,
    anonymous: bool,
    said_anonymous: std::sync::atomic::AtomicBool,
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
        if cfg.allow_anonymous {
            tracing::warn!(
                "api.allow_anonymous is on: :9092 serves plain HTTP and reads with no credential, as before #19 — \
                 a transition setting, to be turned off once every caller presents one"
            );
        }
        Gate {
            kube,
            admin_token,
            enforce,
            anonymous: cfg.allow_anonymous,
            said_anonymous: Default::default(),
            audit_log: data_dir.map(|d| PathBuf::from(d).join("audit.log")),
            cache: Mutex::new(HashMap::new()),
        }
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

    /// `[api] allow_anonymous`: plain HTTP and credential-less reads served.
    pub fn anonymous(&self) -> bool {
        self.anonymous
    }

    /// May this caller read `access`? Never `AuditOnly`: `admin_gate` is
    /// about writes; reads have `allow_anonymous`.
    pub async fn check_read(&self, cred: Credential<'_>, access: &Access) -> Decision {
        if let Some(bearer) = cred.bearer.map(str::trim).filter(|b| !b.is_empty()) {
            if self.admin_token.as_deref().is_some_and(|t| same(bearer, t)) {
                return Decision::Allowed(Requester::admin_token());
            }
            return match self.reviewed(format!("b:{bearer}"), Some(bearer), None, access).await {
                Ok(Review::Allowed(u)) => Decision::Allowed(Requester::kube(u)),
                Ok(Review::Denied(u, why)) => Decision::Refused(403, format!("kubernetes:{}", u.username), denied(&u, access, &why)),
                Ok(Review::Unauthenticated(why)) => Decision::Refused(401, "unknown-bearer".into(), why),
                Err(e) => Decision::Refused(503, "unknown-bearer".into(), e),
            };
        }
        if let Some(c) = cred.cert {
            return Decision::Allowed(Requester::cert(c));
        }
        if self.anonymous {
            if !self.said_anonymous.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::info!("serving a read with no credential (api.allow_anonymous)");
            }
            return Decision::Allowed(Requester { who: "anonymous".into(), user: None });
        }
        Decision::Refused(
            401,
            "none".into(),
            "a credential is required: Authorization: Bearer <token>, or a client certificate from the node CA".into(),
        )
    }

    /// TokenReview (for a bearer) and SubjectAccessReview, cached a minute.
    /// `Err`: the apiserver could not be asked (not cached).
    async fn reviewed(&self, key: String, bearer: Option<&str>, cert: Option<&ClientIdentity>, access: &Access) -> Result<Review, String> {
        let Some(kube) = &self.kube else {
            return Ok(Review::Unauthenticated(match cert {
                Some(_) => "no apiserver is configured to review the client certificate's user".into(),
                None => "not the admin token, and no apiserver is configured to review it".into(),
            }));
        };
        let key = (key, access.resource, access.verb, access.name.clone());
        let cached = {
            let c = self.cache.lock().unwrap();
            c.get(&key).filter(|(at, _)| at.elapsed() < CACHE_TTL).map(|(_, r)| r.clone())
        };
        if let Some(r) = cached {
            return Ok(r);
        }
        let r = match (bearer, cert) {
            (Some(b), _) => review(kube, b, access).await,
            (None, Some(c)) => {
                let u = c.user();
                kube.access_review(&u, access.resource, access.verb, access.name.as_deref())
                    .await
                    .map(|(ok, why)| if ok { Review::Allowed(u) } else { Review::Denied(u, why) })
            }
            (None, None) => Ok(Review::Unauthenticated("no credential".into())),
        }
        .map_err(|e| format!("cannot review the caller: {e}"))?;
        let mut c = self.cache.lock().unwrap();
        c.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
        c.insert(key, (Instant::now(), r.clone()));
        Ok(r)
    }

    pub fn kube(&self) -> Option<&Arc<KubeApi>> {
        self.kube.as_ref()
    }

    /// May this caller make this write? A bearer is reviewed (or is the
    /// admin token); with none, a client certificate's user is.
    pub async fn check(&self, cred: Credential<'_>, access: &Access) -> Decision {
        let refuse = |code: u16, who: &str, why: String| {
            if self.enforce {
                Decision::Refused(code, who.to_string(), why)
            } else {
                Decision::AuditOnly(Requester { who: who.to_string(), user: None }, why)
            }
        };
        let bearer = cred.bearer.map(str::trim).filter(|b| !b.is_empty());
        let review = match (bearer, cred.cert) {
            (Some(b), _) => {
                if self.admin_token.as_deref().is_some_and(|t| same(b, t)) {
                    return Decision::Allowed(Requester::admin_token());
                }
                self.reviewed(format!("b:{b}"), Some(b), None, access).await
            }
            (None, Some(c)) => self.reviewed(format!("c:{}|{}", c.cn, c.groups.join(",")), None, Some(c), access).await,
            (None, None) => {
                return refuse(401, "none", format!("a storage-admin bearer is required to {} {}", access.verb, access.resource));
            }
        };
        let who = match (bearer, cred.cert) {
            (None, Some(c)) => format!("cert:{}", c.cn),
            _ => "unknown-bearer".to_string(),
        };
        let review = match review {
            Ok(r) => r,
            Err(e) => return refuse(503, &who, e),
        };
        let as_requester = |u: KubeUser| match (bearer, cred.cert) {
            (None, Some(c)) => Requester::cert(c),
            _ => Requester::kube(u),
        };
        match review {
            Review::Allowed(u) => Decision::Allowed(as_requester(u)),
            Review::Denied(u, why) => {
                let msg = denied(&u, access, &why);
                let r = as_requester(u);
                if self.enforce {
                    Decision::Refused(403, r.who, msg)
                } else {
                    Decision::AuditOnly(r, msg)
                }
            }
            Review::Unauthenticated(why) => refuse(401, &who, why),
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

fn denied(u: &KubeUser, access: &Access, why: &str) -> String {
    format!(
        "{} may not {} {}.{}{}",
        u.username,
        access.verb,
        access.resource,
        crate::api::kube::GROUP,
        if why.is_empty() { String::new() } else { format!(": {why}") }
    )
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
    fn reads_are_not_writes() {
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
        assert_eq!(classify("POST", "/api/v1/drives/sdb/enroll"), op, "#42: enrol is a drive operation");
        assert_eq!(classify("POST", "/api/v1/shelves/5000a098aaaa0001/firmware"), op, "#35: IOM firmware is a drive operation");
        assert_eq!(classify("POST", "/api/v1/shelves/5000a098aaaa0001/bays/5/power"), op, "#81: a bay's power is a drive operation");
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

    fn bearer(t: &str) -> Credential<'_> {
        Credential { bearer: Some(t), cert: None }
    }

    #[test]
    fn reads_need_get_on_what_they_are_about() {
        assert_eq!(read_access("/api/v1/drives"), Access::new("drives", "get", None));
        assert_eq!(read_access("/metrics"), Access::new("drives", "get", None));
        assert_eq!(read_access("/api/v1/worker/jobs/j1"), Access::new("driveoperations", "get", None));
        assert_eq!(read_access("/api/v1/shelves/5000abc"), Access::new("enclosures", "get", None));
        assert_eq!(read_access("/apis/storage.storm.io/v1/enclosures"), Access::new("enclosures", "get", None));
        assert_eq!(read_access("/api/v1/firmware/images"), Access::new("firmwareimages", "get", None));
        assert!(is_health("/api/v1/health") && is_health("/healthz"));
        assert!(!is_health("/api/v1/health/x") && !is_health("/api/v1/drives") && !is_health("/metrics"));
        assert!(is_page_shell("/") && is_page_shell("/ui/") && !is_page_shell("/api"));
        assert!(is_page_asset("/assets/app.js") && is_page_asset("/ui/assets/app.css") && !is_page_asset("/api/v1/assets"));
    }

    fn gate_with(token: Option<&str>, anonymous: bool) -> Gate {
        let cfg = ApiConfig { admin_token: token.unwrap_or("").into(), allow_anonymous: anonymous, ..Default::default() };
        Gate::new(&cfg, None, None)
    }

    #[tokio::test]
    async fn a_read_needs_a_credential_unless_anonymous_is_on() {
        if std::env::var("STORMDRIVE_ADMIN_TOKEN").is_ok() {
            return;
        }
        let read = read_access("/api/v1/drives");
        let carol = ClientIdentity { cn: "stormconsole".into(), groups: vec!["storm:services".into()] };
        let cert = Credential { bearer: None, cert: Some(&carol) };
        for anonymous in [false, true] {
            let g = gate_with(Some("s3cret"), anonymous);
            assert_eq!(g.check_read(bearer("s3cret"), &read).await, Decision::Allowed(Requester::admin_token()));
            assert!(matches!(g.check_read(cert, &read).await, Decision::Allowed(r) if r.who == "cert:stormconsole"));
            // A credential that is sent is checked, anonymous or not.
            assert!(matches!(g.check_read(bearer("nope"), &read).await, Decision::Refused(401, _, _)));
            let none = g.check_read(Credential::default(), &read).await;
            if anonymous {
                assert!(matches!(none, Decision::Allowed(r) if r.who == "anonymous"));
            } else {
                assert!(matches!(none, Decision::Refused(401, w, _) if w == "none"));
            }
        }
    }

    #[tokio::test]
    async fn a_certificate_writes_only_when_reviewed() {
        // No apiserver to ask: the certificate's user cannot be authorised.
        let g = gate(Some("t"), true);
        let carol = ClientIdentity { cn: "carol".into(), groups: vec!["storage-admins".into()] };
        let op = classify("POST", "/api/v1/format").unwrap();
        let d = g.check(Credential { bearer: None, cert: Some(&carol) }, &op).await;
        assert!(matches!(d, Decision::Refused(401, ref w, _) if w == "cert:carol"), "{d:?}");
        // A bearer wins over the certificate it rides with.
        let d = g.check(Credential { bearer: Some("t"), cert: Some(&carol) }, &op).await;
        assert_eq!(d, Decision::Allowed(Requester::admin_token()));
        assert_eq!(Requester::cert(&carol).user.unwrap().groups, ["storage-admins"]);
    }

    fn gate(token: Option<&str>, enforce: bool) -> Gate {
        let cfg = ApiConfig { admin_token: token.unwrap_or("").into(), admin_gate: if enforce { "enforce" } else { "audit" }.into(), ..Default::default() };
        Gate::new(&cfg, None, None)
    }

    #[tokio::test]
    async fn without_an_apiserver_only_the_admin_token_writes() {
        if std::env::var("STORMDRIVE_ADMIN_TOKEN").is_ok() {
            return;
        }
        let g = gate(Some("s3cret"), true);
        let op = classify("POST", "/api/v1/worker/jobs").unwrap();
        assert_eq!(g.check(bearer("s3cret"), &op).await, Decision::Allowed(Requester::admin_token()));
        assert!(matches!(g.check(Credential::default(), &op).await, Decision::Refused(401, w, _) if w == "none"));
        assert!(matches!(g.check(bearer("s3cre"), &op).await, Decision::Refused(401, w, _) if w == "unknown-bearer"));
        assert!(matches!(g.check(bearer("   "), &op).await, Decision::Refused(401, _, _)));
        // No token configured at all: nothing writes.
        let closed = gate(None, true);
        assert!(matches!(closed.check(bearer(""), &op).await, Decision::Refused(401, _, _)));
    }

    #[tokio::test]
    async fn audit_lets_through_and_says_so() {
        if std::env::var("STORMDRIVE_ADMIN_TOKEN").is_ok() {
            return;
        }
        let g = gate(None, false);
        let op = classify("POST", "/api/v1/format").unwrap();
        assert!(matches!(g.check(Credential::default(), &op).await, Decision::AuditOnly(r, _) if r.who == "none"));
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
