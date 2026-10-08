//! `long` (the night window): waves of stormdrive's main workload at the
//! machine's own scale, until the window ends.
//!
//! A wave is (1) API readers — as many as the node has drives / 8, 4 to 64,
//! each reading the drive list, placement, feed, topology, card, shelves
//! and kube Drives — and (2) a smoke test (sampled reads, nothing written)
//! on every idle, usable drive at once (`STORM_WAVE_MAX` caps it). Sized
//! from what the node reports, never assumed.
//!
//! Measured per wave (a `metric` line): API p50/p95 latency and errors,
//! smoke tests passed/failed and their wall time, health-read timeouts and
//! stuck drives, drives still busy after the wave, event growth. A wave
//! fails on any error, a drive left busy, a stuck or timed-out health read,
//! a smoke test that failed, or a p95 slower than twice the first wave's
//! (+250 ms): slowdown and residue are failures even when every call passed.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::api::Api;
use crate::env::Env;
use crate::pick::{idle, present, s, usable};
use crate::report::{Report, Why};

const ENDPOINTS: [&str; 7] = [
    "api/v1/drives",
    "api/v1/placement",
    "api/v1/components",
    "api/v1/topology",
    "api/v1/summary",
    "api/v1/shelves",
    "apis/storage.storm.io/v1/drives",
];
const REQUESTS_PER_READER: usize = 14;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Wave {
    pub n: usize,
    pub drives: usize,
    pub readers: usize,
    pub requests: usize,
    pub errors: Vec<String>,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub smoke_started: usize,
    pub smoke_busy: usize,
    pub smoke_failed: Vec<String>,
    pub smoke_ms: u64,
    pub busy_after: Vec<String>,
    pub stuck: usize,
    pub timeouts: u64,
    pub events: u64,
}

impl Wave {
    fn metric(&self) -> Value {
        json!({
            "wave": self.n, "drives": self.drives, "readers": self.readers, "requests": self.requests,
            "errors": self.errors.len(), "p50_ms": self.p50_ms, "p95_ms": self.p95_ms,
            "smoke_started": self.smoke_started, "smoke_busy": self.smoke_busy,
            "smoke_failed": self.smoke_failed.len(), "smoke_ms": self.smoke_ms,
            "busy_after": self.busy_after.len(), "stuck": self.stuck,
            "health_timeouts": self.timeouts, "events": self.events,
        })
    }
}

/// How many API readers a node with `drives` drives gets.
pub fn readers_for(drives: usize) -> usize {
    (4 + drives / 8).clamp(4, 64)
}

pub fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

/// Did this wave regress, against the first? `Ok(detail)` or the reasons.
pub fn judge(first: Option<&Wave>, w: &Wave) -> Result<String, String> {
    let mut why = vec![];
    if !w.errors.is_empty() {
        why.push(format!("{} API errors (first: {})", w.errors.len(), w.errors[0]));
    }
    if !w.smoke_failed.is_empty() {
        why.push(format!("smoke failed on {}", w.smoke_failed.join(", ")));
    }
    if !w.busy_after.is_empty() {
        why.push(format!("still busy after the wave: {}", w.busy_after.join(", ")));
    }
    if w.stuck > 0 {
        why.push(format!("{} drives with a stuck health read", w.stuck));
    }
    if w.timeouts > 0 {
        why.push(format!("{} health reads timed out during the wave", w.timeouts));
    }
    if let Some(f) = first {
        let limit = (f.p95_ms * 2).max(f.p95_ms + 250);
        if w.p95_ms > limit {
            why.push(format!("p95 {} ms vs {} ms in wave {} (limit {limit})", w.p95_ms, f.p95_ms, f.n));
        }
    }
    if why.is_empty() {
        Ok(format!(
            "{} requests p95 {} ms; smoke {} drives in {} s",
            w.requests,
            w.p95_ms,
            w.smoke_started,
            w.smoke_ms / 1000
        ))
    } else {
        Err(why.join("; "))
    }
}

async fn monitor_totals(api: &Api) -> (u64, usize) {
    match api.get("api/v1/monitor").await {
        Ok(r) if r.ok() => (r.body["timeouts"].as_u64().unwrap_or(0), r.body["stuck"].as_array().map(Vec::len).unwrap_or(0)),
        _ => (0, 0),
    }
}

async fn wave(env: &Env, api: &Api, n: usize, budget: Duration) -> Result<Wave, Why> {
    let drives = crate::drives(api).await?;
    let seq0 = crate::latest_seq(api).await?;
    let (timeouts0, _) = monitor_totals(api).await;
    let mut w = Wave { n, drives: drives.len(), readers: readers_for(drives.len()), ..Default::default() };

    // (2) smoke tests first, so the readers run against a node under load.
    let mut targets: Vec<(String, String)> = drives
        .iter()
        .filter(|d| present(d) && idle(d) && usable(d))
        .map(|d| (s(d, "id").to_string(), s(d, "name").to_string()))
        .collect();
    if let Some(max) = env.wave_max {
        targets.truncate(max);
    }
    let smoke_t = Instant::now();
    let mut running = vec![];
    for (id, name) in &targets {
        match api.post(&format!("api/v1/drives/{id}/test"), json!({"kind": "smoke"})).await {
            Ok(r) if r.ok() => {
                crate::UNDO.lock().unwrap().tests.push(id.clone());
                running.push((id.clone(), name.clone()));
            }
            Ok(r) if r.status == 409 => w.smoke_busy += 1,
            Ok(r) => w.errors.push(format!("smoke {name}: HTTP {}", r.status)),
            Err(e) => w.errors.push(format!("smoke {name}: {}", e.message())),
        }
    }
    w.smoke_started = running.len();

    // (1) readers.
    let mut tasks = tokio::task::JoinSet::new();
    let base = api.url("");
    let (tls, token, read_token) = (api.tls().clone(), env.token.clone(), env.read_token.clone());
    for i in 0..w.readers {
        let (base, tls, token, read_token) = (base.clone(), tls.clone(), token.clone(), read_token.clone());
        tasks.spawn(async move {
            let api = Api::new(&base, &tls, token, read_token);
            let mut lat = vec![];
            let mut errs = vec![];
            for k in 0..REQUESTS_PER_READER {
                let path = ENDPOINTS[(i + k) % ENDPOINTS.len()];
                let t = Instant::now();
                match api.get(path).await {
                    Ok(r) if r.ok() => lat.push(t.elapsed().as_millis() as u64),
                    Ok(r) => errs.push(format!("{path}: HTTP {}", r.status)),
                    Err(e) => errs.push(format!("{path}: {}", e.message())),
                }
            }
            (lat, errs)
        });
    }
    let mut lat = vec![];
    while let Some(res) = tasks.join_next().await {
        let (l, e) = res.map_err(|e| Why::Fail(format!("reader: {e}")))?;
        lat.extend(l);
        w.errors.extend(e);
    }
    lat.sort_unstable();
    w.requests = lat.len() + w.errors.len();
    w.p50_ms = percentile(&lat, 0.5);
    w.p95_ms = percentile(&lat, 0.95);

    // Wait for the smoke tests, within the wave's budget.
    let deadline = tokio::time::Instant::now() + budget.min(Duration::from_secs(1200));
    for (id, name) in &running {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match crate::wait_test(api, id, left).await {
            Ok(t) if t["state"] == "passed" => {}
            Ok(t) => w.smoke_failed.push(format!("{name} ({})", t["state"])),
            // Out of window: stop it; the window ending is not the drive's fault.
            Err(_) => {
                let _ = api.post_empty(&format!("api/v1/drives/{id}/test/cancel")).await;
                let _ = crate::wait_test(api, id, Duration::from_secs(30)).await;
            }
        }
        crate::UNDO.lock().unwrap().tests.retain(|i| i != id);
    }
    w.smoke_ms = smoke_t.elapsed().as_millis() as u64;

    // Residue: nothing we started is still busy.
    let after = crate::drives(api).await?;
    w.busy_after = after
        .iter()
        .filter(|d| s(d, "activity") == "testing" && targets.iter().any(|(id, _)| id == s(d, "id")))
        .map(|d| s(d, "name").to_string())
        .collect();
    let (timeouts1, stuck) = monitor_totals(api).await;
    w.timeouts = timeouts1.saturating_sub(timeouts0);
    w.stuck = stuck;
    w.events = crate::latest_seq(api).await?.saturating_sub(seq0);
    Ok(w)
}

pub async fn run(env: &Env, api: &Api, r: &mut Report) -> Result<(), String> {
    crate::api_up(api, r).await?;
    let start = Instant::now();
    let margin = (env.timeout / 10).min(Duration::from_secs(300));
    let mut first: Option<Wave> = None;
    let mut regressed: Option<usize> = None;
    let mut n = 0;
    loop {
        let left = env.timeout.saturating_sub(start.elapsed()).saturating_sub(margin);
        if n > 0 && left < Duration::from_secs(5) {
            break;
        }
        n += 1;
        let t = Instant::now();
        let outcome = match wave(env, api, n, left.max(Duration::from_secs(30))).await {
            Ok(w) => {
                r.metric(w.metric());
                let v = judge(first.as_ref(), &w).map_err(Why::Fail);
                if v.is_err() && regressed.is_none() {
                    regressed = Some(n);
                }
                if first.is_none() {
                    first = Some(w);
                }
                v
            }
            Err(e) => {
                regressed.get_or_insert(n);
                Err(e)
            }
        };
        r.record(&format!("wave-{n}"), &outcome, t.elapsed().as_millis());
        let left = env.timeout.saturating_sub(start.elapsed()).saturating_sub(margin);
        tokio::time::sleep(left.min(Duration::from_secs(30))).await;
    }
    let trend = match regressed {
        None => Ok(format!("{n} waves, none regressed")),
        Some(k) => Err(Why::Fail(format!("{n} waves; first regression at wave {k}"))),
    };
    r.record("trend", &trend, start.elapsed().as_millis());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wave(n: usize, p95: u64) -> Wave {
        Wave { n, p95_ms: p95, requests: 100, ..Default::default() }
    }

    #[test]
    fn readers_scale_with_drives() {
        assert_eq!(readers_for(0), 4);
        assert_eq!(readers_for(24), 7);
        assert_eq!(readers_for(160), 24);
        assert_eq!(readers_for(5000), 64);
    }

    #[test]
    fn slowdown_and_residue_fail_a_wave() {
        let first = wave(1, 40);
        assert!(judge(None, &first).is_ok());
        assert!(judge(Some(&first), &wave(2, 280)).is_ok(), "within first + 250 ms");
        assert!(judge(Some(&first), &wave(3, 300)).unwrap_err().contains("p95 300"));
        let big = wave(1, 400);
        assert!(judge(Some(&big), &wave(2, 790)).is_ok(), "within 2×");
        let mut left = wave(4, 40);
        left.busy_after = vec!["sdb".into()];
        assert!(judge(Some(&first), &left).unwrap_err().contains("still busy"));
        let mut hung = wave(5, 40);
        hung.stuck = 1;
        assert!(judge(Some(&first), &hung).is_err());
        let mut bad = wave(6, 40);
        bad.smoke_failed = vec!["sdc (failed)".into()];
        assert!(judge(Some(&first), &bad).unwrap_err().contains("sdc"));
    }

    #[test]
    fn percentiles() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&v, 0.5), 51);
        assert_eq!(percentile(&v, 0.95), 95);
        assert_eq!(percentile(&[], 0.95), 0);
    }
}
