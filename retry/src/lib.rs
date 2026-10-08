//! The one retry helper for every call that leaves stormdrive (#71): the
//! engine client, the apiserver client, and the test container's client of
//! :9092.
//!
//! [`with_backoff`] runs an operation until it succeeds, gets a real answer,
//! or the [`Policy`] runs out:
//!
//! - **bounded**: at most `attempts` tries, and none starts once the
//!   whole-operation `deadline` would be passed; [`Attempt::timeout`] cuts
//!   each try's own timeout to what is left;
//! - **backoff with jitter**: `base · 2^(n-1)`, capped at `max_delay`, then
//!   "equal jitter" (half fixed, half random), so callers that failed together
//!   do not come back together; a `Retry-After` is a floor;
//! - **a real answer is final**: the classifier says what is transient
//!   ([`classify_response`]: timeouts, connect/reset, 5xx, 408, 429) and what
//!   is an answer (other 4xx, a validation error) — an answer is never
//!   retried;
//! - **idempotency**: a write that is not safe to repeat is classified with
//!   [`Idempotent::No`]: only a failure where the request never went out
//!   (connect refused, DNS, TLS handshake) or a 429 is retried;
//! - **logged**: "succeeded on attempt 3 after 4.1 s", or "gave up after 4
//!   attempts / 30.2 s: …", so a flaky dependency shows in the log;
//! - **classified**: giving up on a transient failure yields [`Infra`], an
//!   error type callers find in a chain ([`find_infra`], or anyhow's
//!   `downcast_ref`) to tell infrastructure from a real failure.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::time::Instant;

/// How hard to try. Every policy is a constant here, so the defaults are in
/// one place (and in the README's "Remote calls and retries").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub name: &'static str,
    /// Tries in all, the first included.
    pub attempts: u32,
    /// The delay after the first failure, before jitter.
    pub base: Duration,
    /// The longest delay between two tries, before jitter.
    pub max_delay: Duration,
    /// The whole operation: no try starts after it.
    pub deadline: Duration,
}

impl Policy {
    /// The local engine (:9090): 5 s requests, on the monitor/fleet ticks.
    pub const ENGINE: Policy = Policy {
        name: "engine",
        attempts: 4,
        base: Duration::from_millis(250),
        max_delay: Duration::from_secs(4),
        deadline: Duration::from_secs(30),
    };
    /// The engine's volume placement walk (30 s requests, the usage tick).
    pub const ENGINE_SLOW: Policy = Policy {
        name: "engine-slow",
        attempts: 3,
        base: Duration::from_secs(1),
        max_delay: Duration::from_secs(5),
        deadline: Duration::from_secs(100),
    };
    /// The apiserver: 10 s requests; the write gate waits on it, so short.
    pub const KUBE: Policy = Policy {
        name: "kube",
        attempts: 3,
        base: Duration::from_millis(200),
        max_delay: Duration::from_secs(2),
        deadline: Duration::from_secs(20),
    };
    /// The test container's calls to the node's stormdrive (60 s requests).
    pub const TEST: Policy = Policy {
        name: "test",
        attempts: 4,
        base: Duration::from_millis(500),
        max_delay: Duration::from_secs(5),
        deadline: Duration::from_secs(150),
    };

    /// The delay after failed try `n` (1-based), before jitter.
    pub fn backoff(&self, n: u32) -> Duration {
        let factor = 1u32.checked_shl(n.saturating_sub(1)).unwrap_or(u32::MAX);
        self.base.saturating_mul(factor).min(self.max_delay)
    }
}

/// What a try is told about itself.
#[derive(Debug, Clone, Copy)]
pub struct Attempt {
    /// 1-based.
    pub n: u32,
    /// What is left of the policy's deadline.
    pub remaining: Duration,
}

impl Attempt {
    /// A try's own timeout: `usual`, cut to what is left of the deadline.
    pub fn timeout(&self, usual: Duration) -> Duration {
        usual.min(self.remaining).max(Duration::from_millis(100))
    }
}

/// What the classifier makes of one try.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// It worked.
    Ok,
    /// A real answer (4xx, a validation error): final, never retried.
    Fail(String),
    /// Transient: try again, not before `after` (a Retry-After).
    Retry { why: String, after: Option<Duration> },
}

/// Gave up on a transient failure: infrastructure (unreachable, timing out,
/// overloaded), not a real answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Infra {
    pub what: String,
    pub attempts: u32,
    pub elapsed: Duration,
    pub last: String,
}

impl std::fmt::Display for Infra {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: infrastructure: gave up after {} attempt{} / {:.1} s: {}",
            self.what,
            self.attempts,
            if self.attempts == 1 { "" } else { "s" },
            self.elapsed.as_secs_f64(),
            self.last
        )
    }
}

impl std::error::Error for Infra {}

/// The [`Infra`] in an error's source chain, if any.
pub fn find_infra<'a>(e: &'a (dyn std::error::Error + 'static)) -> Option<&'a Infra> {
    let mut cur = Some(e);
    while let Some(e) = cur {
        if let Some(i) = e.downcast_ref::<Infra>() {
            return Some(i);
        }
        cur = e.source();
    }
    None
}

/// The last try's result, how many tries it took, and — when the policy ran
/// out on a transient failure — why.
#[derive(Debug)]
pub struct Tried<R> {
    pub result: R,
    pub attempts: u32,
    pub elapsed: Duration,
    pub gave_up: Option<Infra>,
}

static STDERR: AtomicBool = AtomicBool::new(false);

/// Also write the retry lines to stderr (the test container, which has no
/// tracing subscriber; its stdout is the JSON report).
pub fn log_to_stderr(on: bool) {
    STDERR.store(on, Ordering::Relaxed);
}

fn note(warn: bool, line: &str) {
    if warn {
        tracing::warn!("{line}");
    } else {
        tracing::info!("{line}");
    }
    if STDERR.load(Ordering::Relaxed) {
        eprintln!("{line}");
    }
}

/// A random-enough u64 for jitter: splitmix64 over a seeded counter.
fn rand_u64() -> u64 {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut s = STATE.load(Ordering::Relaxed);
    if s == 0 {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1)
            ^ (std::process::id() as u64) << 32;
        let _ = STATE.compare_exchange(0, seed | 1, Ordering::Relaxed, Ordering::Relaxed);
    }
    s = STATE.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    let mut z = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Equal jitter: somewhere in `[d/2, d]`.
fn jitter(d: Duration) -> Duration {
    let half = d / 2;
    let span = half.as_nanos() as u64;
    half + Duration::from_nanos(if span == 0 { 0 } else { rand_u64() % (span + 1) })
}

/// Run `op` under `policy`. `classify` judges each try; `what` names the
/// call in the log ("stormblock GET /api/v1/drives").
pub async fn with_backoff<R, F, Fut, C>(policy: &Policy, what: &str, mut op: F, classify: C) -> Tried<R>
where
    F: FnMut(Attempt) -> Fut,
    Fut: Future<Output = R>,
    C: Fn(&R) -> Verdict,
{
    let start = Instant::now();
    let mut n = 0;
    loop {
        n += 1;
        let remaining = policy.deadline.saturating_sub(start.elapsed());
        let result = op(Attempt { n, remaining }).await;
        let elapsed = start.elapsed();
        let secs = elapsed.as_secs_f64();
        match classify(&result) {
            Verdict::Ok => {
                if n > 1 {
                    note(false, &format!("{what}: succeeded on attempt {n} after {secs:.1} s"));
                }
                return Tried { result, attempts: n, elapsed, gave_up: None };
            }
            Verdict::Fail(why) => {
                if n > 1 {
                    note(false, &format!("{what}: a real answer on attempt {n} after {secs:.1} s, not retried: {why}"));
                }
                return Tried { result, attempts: n, elapsed, gave_up: None };
            }
            Verdict::Retry { why, after } => {
                let mut delay = jitter(policy.backoff(n));
                if let Some(a) = after {
                    delay = delay.max(a);
                }
                if n >= policy.attempts || elapsed + delay >= policy.deadline {
                    let infra = Infra { what: what.to_string(), attempts: n, elapsed, last: why };
                    note(true, &infra.to_string());
                    return Tried { result, attempts: n, elapsed, gave_up: Some(infra) };
                }
                tracing::debug!("{what}: attempt {n} failed ({why}); again in {:.1} s", delay.as_secs_f64());
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Whether a request is safe to send twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Idempotent {
    /// Reads, PUTs of a whole value, deletes, reviews: retried on any
    /// transient failure.
    Yes,
    /// A write that may not be repeated (it could do the thing twice): only
    /// retried when it never reached the server (connect) or was refused
    /// unprocessed (429).
    No,
}

/// A transport error: transient or an answer.
pub fn classify_error(e: &reqwest::Error, idem: Idempotent) -> Verdict {
    let why = error_chain(e);
    let unsent = e.is_connect();
    let maybe_sent = e.is_timeout() || e.is_request() || e.is_body();
    if unsent || (idem == Idempotent::Yes && maybe_sent) {
        Verdict::Retry { why, after: None }
    } else if maybe_sent {
        Verdict::Fail(format!("{why} (not retried: the write may have been applied)"))
    } else {
        Verdict::Fail(why)
    }
}

/// An HTTP status: 2xx/3xx ok, 408/5xx transient (idempotent only), 429
/// transient, other 4xx a real answer.
pub fn classify_status(code: u16, retry_after: Option<Duration>, idem: Idempotent) -> Verdict {
    match code {
        200..=399 => Verdict::Ok,
        429 => Verdict::Retry { why: format!("HTTP {code}"), after: retry_after },
        408 | 500..=599 if idem == Idempotent::Yes => Verdict::Retry { why: format!("HTTP {code}"), after: retry_after },
        _ => Verdict::Fail(format!("HTTP {code}")),
    }
}

/// A reqwest try: its error or its status.
pub fn classify_response(r: &Result<reqwest::Response, reqwest::Error>, idem: Idempotent) -> Verdict {
    match r {
        Ok(resp) => classify_status(resp.status().as_u16(), retry_after(resp.headers()), idem),
        Err(e) => classify_error(e, idem),
    }
}

/// `Retry-After` in seconds (the HTTP-date form is not used by our peers).
pub fn retry_after(h: &reqwest::header::HeaderMap) -> Option<Duration> {
    let v = h.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    v.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// An error and its sources on one line ("error sending request: … :
/// connection refused").
pub fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        let t = c.to_string();
        if !s.contains(&t) {
            s.push_str(": ");
            s.push_str(&t);
        }
        cur = c.source();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    const ALL: [Policy; 4] = [Policy::ENGINE, Policy::ENGINE_SLOW, Policy::KUBE, Policy::TEST];

    /// A fake that fails `fails` times, then succeeds.
    async fn flaky(policy: &Policy, fails: u32) -> Tried<Result<u32, String>> {
        let calls = AtomicU32::new(0);
        with_backoff(
            policy,
            "fake",
            |a| {
                let c = calls.fetch_add(1, Ordering::Relaxed) + 1;
                assert_eq!(a.n, c);
                async move { if c <= fails { Err(format!("down {c}")) } else { Ok(c) } }
            },
            |r| match r {
                Ok(_) => Verdict::Ok,
                Err(e) => Verdict::Retry { why: e.clone(), after: None },
            },
        )
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn every_policy_succeeds_on_its_last_attempt() {
        for p in ALL {
            let t = flaky(&p, p.attempts - 1).await;
            assert_eq!(t.result, Ok(p.attempts), "{}", p.name);
            assert_eq!(t.attempts, p.attempts, "{}", p.name);
            assert!(t.gave_up.is_none(), "{}", p.name);
            assert!(t.elapsed < p.deadline, "{}", p.name);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn every_policy_gives_up_bounded_and_says_infrastructure() {
        for p in ALL {
            let t = flaky(&p, u32::MAX).await;
            let i = t.gave_up.expect(p.name);
            assert_eq!(t.attempts, p.attempts, "{}", p.name);
            assert!(t.elapsed <= p.deadline, "{}", p.name);
            assert_eq!(i.last, format!("down {}", p.attempts));
            assert!(i.to_string().contains(&format!("infrastructure: gave up after {} attempts", p.attempts)), "{i}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_deadline_caps_the_attempts() {
        let p = Policy { name: "t", attempts: 100, base: Duration::from_secs(1), max_delay: Duration::from_secs(8), deadline: Duration::from_secs(20) };
        let t = flaky(&p, u32::MAX).await;
        assert!(t.gave_up.is_some());
        assert!(t.attempts < 10, "{}", t.attempts);
        assert!(t.elapsed <= p.deadline);
    }

    #[tokio::test(start_paused = true)]
    async fn a_real_answer_is_not_retried() {
        let calls = AtomicU32::new(0);
        let t = with_backoff(
            &Policy::ENGINE,
            "fake",
            |_| {
                calls.fetch_add(1, Ordering::Relaxed);
                async { 404u16 }
            },
            |c| classify_status(*c, None, Idempotent::Yes),
        )
        .await;
        assert_eq!((t.result, t.attempts, calls.load(Ordering::Relaxed)), (404, 1, 1));
        assert!(t.gave_up.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_is_a_floor_and_past_the_deadline_gives_up() {
        let t = with_backoff(
            &Policy::KUBE,
            "fake",
            |a| async move { if a.n == 1 { 429u16 } else { 200 } },
            |c| classify_status(*c, Some(Duration::from_secs(7)), Idempotent::No),
        )
        .await;
        assert_eq!((t.result, t.attempts), (200, 2));
        assert!(t.elapsed >= Duration::from_secs(7));

        let t = with_backoff(&Policy::KUBE, "fake", |_| async { 503u16 }, |c| classify_status(*c, Some(Duration::from_secs(60)), Idempotent::Yes)).await;
        assert_eq!(t.attempts, 1);
        assert!(t.gave_up.is_some());
    }

    #[test]
    fn statuses() {
        use Idempotent::*;
        assert_eq!(classify_status(200, None, No), Verdict::Ok);
        assert_eq!(classify_status(304, None, Yes), Verdict::Ok);
        for c in [400, 401, 403, 404, 409, 422] {
            assert!(matches!(classify_status(c, None, Yes), Verdict::Fail(_)), "{c}");
        }
        for c in [408, 500, 502, 503, 504] {
            assert!(matches!(classify_status(c, None, Yes), Verdict::Retry { .. }), "{c}");
            assert!(matches!(classify_status(c, None, No), Verdict::Fail(_)), "{c}");
        }
        assert!(matches!(classify_status(429, None, No), Verdict::Retry { .. }));
    }

    #[test]
    fn backoff_grows_and_caps_and_jitter_stays_in_range() {
        let p = Policy::ENGINE;
        assert_eq!(p.backoff(1), Duration::from_millis(250));
        assert_eq!(p.backoff(2), Duration::from_millis(500));
        assert_eq!(p.backoff(5), Duration::from_secs(4));
        assert_eq!(p.backoff(40), Duration::from_secs(4));
        for _ in 0..1000 {
            let j = jitter(Duration::from_secs(4));
            assert!(j >= Duration::from_secs(2) && j <= Duration::from_secs(4), "{j:?}");
        }
        let a = Attempt { n: 2, remaining: Duration::from_secs(3) };
        assert_eq!(a.timeout(Duration::from_secs(10)), Duration::from_secs(3));
    }

    #[derive(Debug)]
    struct Wrap(Infra);
    impl std::fmt::Display for Wrap {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "listing drives")
        }
    }
    impl std::error::Error for Wrap {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn infra_is_found_in_a_chain() {
        let i = Infra { what: "x".into(), attempts: 2, elapsed: Duration::from_secs(1), last: "timeout".into() };
        let w = Wrap(i.clone());
        assert_eq!(find_infra(&w), Some(&i));
        assert_eq!(find_infra(&std::fmt::Error), None);
    }

    /// Real sockets: a refused connection is retried even for a write that
    /// may not repeat (it never went out); a server that drops the request
    /// mid-way is retried only for an idempotent one.
    #[tokio::test]
    async fn transport_errors_by_idempotency() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        let http = reqwest::Client::new();
        let r = http.post(format!("http://{addr}/")).send().await;
        assert!(matches!(classify_response(&r, Idempotent::No), Verdict::Retry { .. }), "{r:?}");

        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((s, _)) = l.accept().await else { return };
                tokio::time::sleep(Duration::from_millis(50)).await;
                drop(s); // read nothing, answer nothing
            }
        });
        let r = http.post(format!("http://{addr}/")).body("x").send().await;
        assert!(matches!(classify_response(&r, Idempotent::Yes), Verdict::Retry { .. }), "{r:?}");
        assert!(matches!(classify_response(&r, Idempotent::No), Verdict::Fail(_)), "{r:?}");
    }
}
