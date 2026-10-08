//! The node's stormdrive, over HTTPS (#19), the way any client on the fleet
//! sees it: the node CA checks the server, a client pair or a bearer says
//! who the run is.

use std::time::Duration;

use retry::{Idempotent, Policy};
use serde_json::Value;

use crate::env::Tls;
use crate::report::Why;

pub struct Api {
    http: reqwest::Client,
    base: String,
    tls: Tls,
    token: Option<String>,
    /// The bearer is a storage-admin one (`STORM_STORMDRIVE_TOKEN`): a
    /// refused write is then a failure, not a skip.
    admin: bool,
    /// The node's stormdrive version, from `/api/v1/health` (api-up).
    pub version: std::sync::Mutex<Option<(u64, u64, u64)>>,
}

/// One answer: status, headers we care about, and the body (JSON when it
/// parses, the text otherwise).
pub struct Reply {
    pub status: u16,
    pub etag: Option<String>,
    pub content_type: String,
    pub body: Value,
    pub text: String,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body when the status is 2xx, else a failure naming the call —
    /// infrastructure for 503/504 (stormdrive, or what it calls, not there
    /// through its own retries: the engine, the apiserver; #71).
    pub fn json(self, what: &str) -> Result<Value, Why> {
        if self.ok() {
            return Ok(self.body);
        }
        let m = format!("{what}: HTTP {} {}", self.status, self.text.chars().take(200).collect::<String>());
        Err(if matches!(self.status, 503 | 504) { Why::Infra(m) } else { Why::Fail(m) })
    }
}

/// One request's own timeout; `retry::Policy::TEST` bounds the whole call.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim().trim_start_matches('v').split(['.', '-', '+']).map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next()??, it.next().flatten().unwrap_or(0)))
}

impl Api {
    /// `token`: the run's storage-admin bearer; `read_token`: a bearer for
    /// reads when there is none (the pod's service account).
    pub fn new(base: &str, tls: &Tls, token: Option<String>, read_token: Option<String>) -> Self {
        let mut b = reqwest::Client::builder().timeout(REQUEST_TIMEOUT).connect_timeout(Duration::from_secs(10));
        if let Some(ca) = &tls.ca {
            match reqwest::Certificate::from_pem_bundle(ca) {
                Ok(cs) => {
                    for c in cs {
                        b = b.add_root_certificate(c);
                    }
                }
                Err(e) => eprintln!("STORM_STORMDRIVE_CA: {e}"),
            }
        }
        if let Some(id) = &tls.identity {
            match reqwest::Identity::from_pem(id) {
                Ok(i) => b = b.identity(i),
                Err(e) => eprintln!("STORM_STORMDRIVE_CERT/_KEY: {e}"),
            }
        }
        let http = b.build().expect("http client");
        let admin = token.is_some();
        Api { http, base: base.trim_end_matches('/').to_string(), tls: tls.clone(), token: token.or(read_token), admin, version: Default::default() }
    }

    pub fn tls(&self) -> &Tls {
        &self.tls
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }

    /// Every call to the node (#71): `retry::Policy::TEST`. A read retries
    /// timeouts, refused connections and 5xx; a write (`Idempotent::No`)
    /// only what never reached the node, since the suites' writes start
    /// things (a test, a designation) that a repeat would find busy. A call
    /// that gives up, or a transport error not retried, is infrastructure
    /// (`Why::Infra`), not a failure of the feature under test.
    async fn send_with(&self, req: reqwest::RequestBuilder, idem: Idempotent, usual: Duration) -> Result<reqwest::Response, Why> {
        let what = match req.try_clone().and_then(|r| r.build().ok()) {
            Some(r) => format!("{} {}", r.method(), r.url().path()),
            None => "stormdrive".to_string(),
        };
        let tried = retry::with_backoff(
            &Policy::TEST,
            &what,
            |a| {
                // The suites send JSON or no body: always cloneable.
                let mut r = req.try_clone().expect("test request body is cloneable").timeout(a.timeout(usual));
                if let Some(t) = &self.token {
                    r = r.bearer_auth(t);
                }
                r.send()
            },
            |r| retry::classify_response(r, idem),
        )
        .await;
        if let Some(infra) = tried.gave_up {
            return Err(Why::Infra(infra.to_string()));
        }
        tried.result.map_err(|e| Why::Infra(format!("{what}: {}", retry::error_chain(&e))))
    }

    async fn send(&self, req: reqwest::RequestBuilder, idem: Idempotent) -> Result<Reply, Why> {
        let r = self.send_with(req, idem, REQUEST_TIMEOUT).await?;
        let status = r.status().as_u16();
        let header = |h: reqwest::header::HeaderName| r.headers().get(h).and_then(|v| v.to_str().ok()).map(str::to_string);
        let etag = header(reqwest::header::ETAG);
        let content_type = header(reqwest::header::CONTENT_TYPE).unwrap_or_default();
        let text = r.text().await.unwrap_or_default();
        let body = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok(Reply { status, etag, content_type, body, text })
    }

    pub async fn get(&self, path: &str) -> Result<Reply, Why> {
        self.send(self.http.get(self.url(path)), Idempotent::Yes).await
    }

    pub async fn get_if_none_match(&self, path: &str, etag: &str) -> Result<Reply, Why> {
        self.send(self.http.get(self.url(path)).header(reqwest::header::IF_NONE_MATCH, etag), Idempotent::Yes).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Reply, Why> {
        self.write(self.http.post(self.url(path)).json(&body)).await
    }

    /// POST with no body (the body-free action routes).
    pub async fn post_empty(&self, path: &str) -> Result<Reply, Why> {
        self.write(self.http.post(self.url(path))).await
    }

    pub async fn delete(&self, path: &str) -> Result<Reply, Why> {
        self.write(self.http.delete(self.url(path))).await
    }

    /// A write: since 0.18.0 (#45) it needs a storage-admin bearer. Without
    /// one (`STORM_STORMDRIVE_TOKEN` unset) a 401/403 skips the check rather
    /// than failing it — the gate doing its job is not the feature's fault.
    async fn write(&self, req: reqwest::RequestBuilder) -> Result<Reply, Why> {
        let r = self.send(req, Idempotent::No).await?;
        if !self.admin && matches!(r.status, 401 | 403) {
            return Err(Why::Skip(format!("needs a storage-admin bearer (STORM_STORMDRIVE_TOKEN): HTTP {}", r.status)));
        }
        Ok(r)
    }

    /// A client with only `bearer` (or no credential at all): the node CA
    /// still checks the server, but no client certificate is presented.
    fn bare(&self, bearer: Option<&str>) -> Api {
        let tls = Tls { ca: self.tls.ca.clone(), identity: None };
        Api::new(&self.base, &tls, bearer.map(str::to_string), None)
    }

    /// The same write with no credential at all, or with `bearer`, whatever
    /// this client holds.
    pub async fn post_as(&self, path: &str, bearer: Option<&str>) -> Result<Reply, Why> {
        let api = self.bare(bearer);
        api.send(api.http.post(api.url(path)), Idempotent::No).await
    }

    /// The same read with no credential at all, or with `bearer`.
    pub async fn get_as(&self, path: &str, bearer: Option<&str>) -> Result<Reply, Why> {
        let api = self.bare(bearer);
        api.send(api.http.get(api.url(path)), Idempotent::Yes).await
    }

    /// A streaming GET (kube `?watch=1`): the first `want` lines, or what
    /// arrived within `within`.
    pub async fn stream_lines(&self, path: &str, want: usize, within: Duration) -> Result<Vec<String>, Why> {
        let mut r = self.send_with(self.http.get(self.url(path)), Idempotent::Yes, within + Duration::from_secs(5)).await?;
        if !r.status().is_success() {
            return Err(Why::Fail(format!("watch: HTTP {}", r.status())));
        }
        let mut buf = String::new();
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let done = buf.lines().count() >= want && buf.ends_with('\n');
            if done {
                break;
            }
            match tokio::time::timeout_at(deadline, r.chunk()).await {
                Ok(Ok(Some(c))) => buf.push_str(&String::from_utf8_lossy(&c)),
                _ => break,
            }
        }
        Ok(buf.lines().take(want).map(str::to_string).collect())
    }

    /// The node's stormdrive is at least `v` (a feature's first release).
    pub fn has(&self, v: (u64, u64, u64)) -> bool {
        self.version.lock().unwrap().is_some_and(|n| n >= v)
    }

    /// Skip, not fail, a check of a feature the node's release predates:
    /// the image is built at a commit, the node runs whatever release it has.
    pub fn need(&self, v: (u64, u64, u64), what: &str) -> Result<(), Why> {
        if self.has(v) {
            Ok(())
        } else {
            let n = *self.version.lock().unwrap();
            Err(Why::Skip(format!(
                "{what} is in stormdrive {}.{}.{}+; the node runs {}",
                v.0,
                v.1,
                v.2,
                n.map(|n| format!("{}.{}.{}", n.0, n.1, n.2)).unwrap_or_else(|| "an unknown version".into())
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn versions() {
        use super::parse_version;
        assert_eq!(parse_version("0.16.0"), Some((0, 16, 0)));
        assert_eq!(parse_version("v1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("0.15.0-dirty"), Some((0, 15, 0)));
        assert_eq!(parse_version("x"), None);
        assert!(Some((0, 16, 0)) >= Some((0, 12, 0)));
    }

    /// #71: a node that answers 503 twice, then 200 — a read retries
    /// through it, a write does not; a node that is not there is
    /// infrastructure, not a failure.
    #[tokio::test]
    async fn reads_retry_writes_do_not_and_absence_is_infrastructure() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let hits = Arc::new(AtomicU32::new(0));
        let h = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let n = h.fetch_add(1, Ordering::SeqCst);
                let reply = if n < 2 || n >= 3 {
                    "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                } else {
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}"
                };
                let _ = s.write_all(reply.as_bytes()).await;
            }
        });
        let tls = crate::env::Tls { ca: None, identity: None };
        let api = super::Api::new(&base, &tls, None, None);
        assert!(api.get("api/v1/health").await.ok().unwrap().ok());
        assert_eq!(hits.load(Ordering::SeqCst), 3);

        let r = api.post_empty("api/v1/x").await.ok().unwrap();
        assert_eq!(r.status, 503);
        assert_eq!(hits.load(Ordering::SeqCst), 4, "a write is sent once");
        assert!(matches!(r.json("POST x"), Err(crate::report::Why::Infra(_))));

        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let gone = format!("http://{}", l.local_addr().unwrap());
        drop(l);
        let api = super::Api::new(&gone, &tls, None, None);
        match api.get("api/v1/health").await {
            Err(crate::report::Why::Infra(m)) => assert!(m.contains("gave up after 4 attempts"), "{m}"),
            _ => panic!("not infrastructure"),
        }
    }
}
