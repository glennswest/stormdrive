//! (From stormstorage's test crate: one reporting shape across the fleet.)
//! What a run reports: one JSON object per test on stdout (and in
//! `/results/results.jsonl`), a summary line last, and the exit code —
//! 0 all passed, 1 a test failed, 2 the run could not happen (also: a test
//! could not run because the node's stormdrive, or what it calls, was not
//! there through a whole retry policy — infrastructure, #71).

use std::io::Write;
use std::time::Instant;

use serde_json::json;

/// Why a test did not pass.
pub enum Why {
    Fail(String),
    Skip(String),
    /// Infrastructure, not the feature: the call timed out, was refused, or
    /// kept failing 5xx through the whole retry policy (#71). Reported as a
    /// skip with `"infrastructure": true`, and the run exits 2, never 1.
    Infra(String),
}

impl Why {
    pub fn message(&self) -> &str {
        match self {
            Why::Fail(m) | Why::Skip(m) | Why::Infra(m) => m,
        }
    }
}

impl From<String> for Why {
    fn from(s: String) -> Self {
        Why::Fail(s)
    }
}

impl From<&str> for Why {
    fn from(s: &str) -> Self {
        Why::Fail(s.to_string())
    }
}

/// A test's result: `Ok(detail)` passes.
pub type Outcome = Result<String, Why>;

/// Fail unless `cond`.
pub fn ensure(cond: bool, why: impl Into<String>) -> Result<(), Why> {
    if cond {
        Ok(())
    } else {
        Err(Why::Fail(why.into()))
    }
}

#[derive(Default)]
pub struct Report {
    pub pass: u32,
    pub fail: u32,
    pub skip: u32,
    /// Of `skip`: the tests infrastructure stopped.
    pub infra: u32,
    file: Option<std::fs::File>,
}

impl Report {
    pub fn new(results: &std::path::Path) -> Self {
        let _ = std::fs::create_dir_all(results);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(results.join("results.jsonl"))
            .ok();
        Report { file, ..Default::default() }
    }

    fn line(&mut self, v: serde_json::Value) {
        let s = v.to_string();
        println!("{s}");
        let _ = std::io::stdout().flush();
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "{s}");
        }
    }

    pub fn record(&mut self, test: &str, outcome: &Outcome, ms: u128) {
        let (status, detail) = match outcome {
            Ok(d) => {
                self.pass += 1;
                ("pass", d.as_str())
            }
            Err(Why::Fail(d)) => {
                self.fail += 1;
                ("fail", d.as_str())
            }
            Err(Why::Skip(d)) => {
                self.skip += 1;
                ("skip", d.as_str())
            }
            Err(Why::Infra(d)) => {
                self.skip += 1;
                self.infra += 1;
                let v = json!({ "test": test, "status": "skip", "infrastructure": true, "ms": ms as u64, "detail": d });
                return self.line(v);
            }
        };
        self.line(json!({ "test": test, "status": status, "ms": ms as u64, "detail": detail }));
    }

    /// Run one test, time it, and record it. `STORM_ONLY=<substring>` runs
    /// only the matching tests, for chasing one failure.
    pub async fn run<F>(&mut self, test: &str, f: F) -> bool
    where
        F: std::future::Future<Output = Outcome>,
    {
        if let Ok(only) = std::env::var("STORM_ONLY") {
            if !only.is_empty() && !test.contains(&only) && test != "api-up" {
                return true;
            }
        }
        let t = Instant::now();
        let outcome = f.await;
        let ok = outcome.is_ok();
        self.record(test, &outcome, t.elapsed().as_millis());
        ok
    }

    /// A metric line (the long suite's per-wave numbers). Not a test.
    pub fn metric(&mut self, v: serde_json::Value) {
        self.line(json!({ "metric": v }));
    }

    pub fn summary(&mut self) {
        let (p, f, s, i) = (self.pass, self.fail, self.skip, self.infra);
        self.line(json!({ "summary": { "pass": p, "fail": f, "skip": s, "infrastructure": i } }));
    }

    /// 1 when a test failed; else 2 when infrastructure stopped one (the
    /// run did not fully happen); else 0.
    pub fn exit_code(&self) -> i32 {
        if self.fail > 0 {
            1
        } else if self.infra > 0 {
            2
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infrastructure_is_a_skip_and_exits_2_unless_something_failed() {
        let mut r = Report::default();
        r.record("a", &Ok("ok".into()), 1);
        assert_eq!(r.exit_code(), 0);
        r.record("b", &Err(Why::Infra("gave up after 4 attempts".into())), 1);
        assert_eq!((r.skip, r.infra, r.fail, r.exit_code()), (1, 1, 0, 2));
        r.record("c", &Err(Why::Fail("wrong".into())), 1);
        assert_eq!(r.exit_code(), 1);
    }
}
