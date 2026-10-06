//! A small client for the cluster's apiserver (#45): the two reviews that
//! decide who may change a drive (TokenReview, SubjectAccessReview), and the
//! object calls the controller needs (list, get, create, merge-patch, status,
//! Events). Plain JSON over reqwest — the handful of verbs used here do not
//! need kube-rs and its build.
//!
//! Where the apiserver is and who stormdrive is to it: `[kubernetes]`, then
//! `$STORMDRIVE_KUBE_API` / `_CA` / `_TOKEN_FILE`, then the in-cluster
//! service account. The token file is re-read on every call, so a rotated
//! projected token is picked up.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::KubernetesConfig;

const SA_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

/// Who a reviewed bearer is.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KubeUser {
    pub username: String,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
}

/// What the apiserver answered to a call that was made.
#[derive(Debug)]
pub enum KubeError {
    /// It could not be reached, or answered garbage.
    Unavailable(String),
    /// It answered with an error status.
    Status(u16, String),
}

impl std::fmt::Display for KubeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KubeError::Unavailable(e) => write!(f, "apiserver unreachable: {e}"),
            KubeError::Status(c, m) => write!(f, "apiserver: {c}: {m}"),
        }
    }
}

impl KubeError {
    pub fn code(&self) -> Option<u16> {
        match self {
            KubeError::Status(c, _) => Some(*c),
            _ => None,
        }
    }
}

pub struct KubeApi {
    base: String,
    token_file: Option<PathBuf>,
    http: reqwest::Client,
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// The apiserver URL, CA and token file from config, environment and the
/// in-cluster service account, in that order; `None` without a URL.
pub fn resolve(cfg: &KubernetesConfig) -> Option<(String, Option<String>, Option<String>)> {
    let env = |k: &str| non_empty(std::env::var(k).ok());
    let sa = |f: &str| {
        let p = format!("{SA_DIR}/{f}");
        std::path::Path::new(&p).exists().then_some(p)
    };
    let url = non_empty(Some(cfg.api_url.clone())).or_else(|| env("STORMDRIVE_KUBE_API")).or_else(|| {
        let host = env("KUBERNETES_SERVICE_HOST")?;
        sa("token")?;
        let port = env("KUBERNETES_SERVICE_PORT").unwrap_or_else(|| "443".into());
        let host = if host.contains(':') { format!("[{host}]") } else { host };
        Some(format!("https://{host}:{port}"))
    })?;
    let ca = non_empty(Some(cfg.ca_file.clone())).or_else(|| env("STORMDRIVE_KUBE_CA")).or_else(|| sa("ca.crt"));
    let token = non_empty(Some(cfg.token_file.clone())).or_else(|| env("STORMDRIVE_KUBE_TOKEN_FILE")).or_else(|| sa("token"));
    Some((url.trim_end_matches('/').to_string(), ca, token))
}

impl KubeApi {
    /// `None` when no apiserver is named; `Err` when one is named and its CA
    /// cannot be read (a misconfiguration worth failing loudly on).
    pub fn from_config(cfg: &KubernetesConfig) -> anyhow::Result<Option<KubeApi>> {
        let Some((base, ca, token_file)) = resolve(cfg) else {
            return Ok(None);
        };
        let mut b = reqwest::Client::builder().timeout(Duration::from_secs(10));
        if let Some(ca) = &ca {
            let pem = std::fs::read(ca).map_err(|e| anyhow::anyhow!("kubernetes.ca_file {ca}: {e}"))?;
            let cert = reqwest::Certificate::from_pem(&pem).map_err(|e| anyhow::anyhow!("kubernetes.ca_file {ca}: {e}"))?;
            b = b.add_root_certificate(cert);
        }
        if cfg.insecure {
            b = b.danger_accept_invalid_certs(true);
        }
        let http = b.build().map_err(|e| anyhow::anyhow!("apiserver client: {e}"))?;
        Ok(Some(KubeApi { base, token_file: token_file.map(PathBuf::from), http }))
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn token(&self) -> Option<String> {
        let f = self.token_file.as_ref()?;
        non_empty(std::fs::read_to_string(f).ok())
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<&Value>, content_type: &str) -> Result<Value, KubeError> {
        let mut req = self.http.request(method, format!("{}{path}", self.base));
        if let Some(t) = self.token() {
            req = req.bearer_auth(t);
        }
        if let Some(b) = body {
            req = req.header(reqwest::header::CONTENT_TYPE, content_type).body(b.to_string());
        }
        let resp = req.send().await.map_err(|e| KubeError::Unavailable(e.to_string()))?;
        let code = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| KubeError::Unavailable(e.to_string()))?;
        if !(200..300).contains(&code) {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| text.chars().take(300).collect());
            return Err(KubeError::Status(code, msg));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| KubeError::Unavailable(format!("{path}: {e}")))
    }

    pub async fn get(&self, path: &str) -> Result<Value, KubeError> {
        self.call(reqwest::Method::GET, path, None, "").await
    }
    pub async fn create(&self, path: &str, body: &Value) -> Result<Value, KubeError> {
        self.call(reqwest::Method::POST, path, Some(body), "application/json").await
    }
    pub async fn merge_patch(&self, path: &str, body: &Value) -> Result<Value, KubeError> {
        self.call(reqwest::Method::PATCH, path, Some(body), "application/merge-patch+json").await
    }
    pub async fn delete(&self, path: &str) -> Result<Value, KubeError> {
        self.call(reqwest::Method::DELETE, path, None, "").await
    }

    /// TokenReview: who is this bearer? `Ok(None)` = not a valid bearer.
    pub async fn token_review(&self, bearer: &str) -> Result<Option<KubeUser>, KubeError> {
        let v = self
            .create(
                "/apis/authentication.k8s.io/v1/tokenreviews",
                &json!({ "apiVersion": "authentication.k8s.io/v1", "kind": "TokenReview", "spec": { "token": bearer } }),
            )
            .await?;
        Ok(parse_token_review(&v))
    }

    /// SubjectAccessReview: may `user` do `verb` on `resource` (in
    /// `storage.storm.io`)? `Ok((allowed, reason))`.
    pub async fn access_review(&self, user: &KubeUser, resource: &str, verb: &str, name: Option<&str>) -> Result<(bool, String), KubeError> {
        let v = self.create("/apis/authorization.k8s.io/v1/subjectaccessreviews", &access_review_body(user, resource, verb, name)).await?;
        Ok((v["status"]["allowed"].as_bool().unwrap_or(false), v["status"]["reason"].as_str().unwrap_or("").to_string()))
    }
}

pub fn parse_token_review(v: &Value) -> Option<KubeUser> {
    if v["status"]["authenticated"].as_bool() != Some(true) {
        return None;
    }
    let u = &v["status"]["user"];
    let username = u["username"].as_str()?.to_string();
    let groups = u["groups"].as_array().map(|a| a.iter().filter_map(|g| g.as_str().map(str::to_string)).collect()).unwrap_or_default();
    Some(KubeUser { username, groups, uid: u["uid"].as_str().map(str::to_string) })
}

pub fn access_review_body(user: &KubeUser, resource: &str, verb: &str, name: Option<&str>) -> Value {
    let mut attrs = json!({ "group": crate::api::kube::GROUP, "resource": resource, "verb": verb });
    if let Some(n) = name {
        attrs["name"] = json!(n);
    }
    let mut spec = json!({ "user": user.username, "groups": user.groups, "resourceAttributes": attrs });
    if let Some(uid) = &user.uid {
        spec["uid"] = json!(uid);
    }
    json!({ "apiVersion": "authorization.k8s.io/v1", "kind": "SubjectAccessReview", "spec": spec })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_review_answers() {
        let ok = json!({ "status": { "authenticated": true, "user": { "username": "alice", "uid": "u1", "groups": ["storage-admins", "system:authenticated"] } } });
        let u = parse_token_review(&ok).unwrap();
        assert_eq!(u.username, "alice");
        assert_eq!(u.groups, vec!["storage-admins", "system:authenticated"]);
        assert_eq!(u.uid.as_deref(), Some("u1"));
        assert!(parse_token_review(&json!({ "status": { "authenticated": false } })).is_none());
        assert!(parse_token_review(&json!({ "status": {} })).is_none());
    }

    #[test]
    fn access_review_names_the_group_resource_and_verb() {
        let u = KubeUser { username: "alice".into(), groups: vec!["g".into()], uid: None };
        let b = access_review_body(&u, "driveoperations", "create", Some("sdb"));
        assert_eq!(b["kind"], "SubjectAccessReview");
        assert_eq!(b["spec"]["user"], "alice");
        assert_eq!(b["spec"]["groups"][0], "g");
        assert_eq!(b["spec"]["resourceAttributes"]["group"], "storage.storm.io");
        assert_eq!(b["spec"]["resourceAttributes"]["resource"], "driveoperations");
        assert_eq!(b["spec"]["resourceAttributes"]["verb"], "create");
        assert_eq!(b["spec"]["resourceAttributes"]["name"], "sdb");
        assert!(b["spec"].get("uid").is_none());
    }

    #[test]
    fn no_url_no_client() {
        // Only meaningful where the test host is not itself a pod.
        if std::env::var("KUBERNETES_SERVICE_HOST").is_ok() || std::env::var("STORMDRIVE_KUBE_API").is_ok() {
            return;
        }
        assert!(KubeApi::from_config(&KubernetesConfig::default()).unwrap().is_none());
        let c = KubernetesConfig { api_url: "https://k:6443/".into(), ..Default::default() };
        let api = KubeApi::from_config(&c).unwrap().unwrap();
        assert_eq!(api.base(), "https://k:6443");
    }
}
