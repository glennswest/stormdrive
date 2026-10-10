//! `medium` (< 30 min): stormdrive's features and failure paths, end to end,
//! through its API — without harming a drive (`pick` says which drive each
//! check may touch). What it changes it puts back (`crate::UNDO`).

use std::time::Duration;

use serde_json::{json, Value};

use crate::api::Api;
use crate::env::Env;
use crate::pick::{self, in_fleet, in_use, present, s};
use crate::report::{ensure, Outcome, Report, Why};

pub async fn run(env: &Env, api: &Api, r: &mut Report) -> Result<(), String> {
    crate::api_up(api, r).await?;
    r.run("writes-need-storage-admin", writes_need_admin(api, env)).await;
    r.run("reads-need-a-credential", reads_need_credential(api)).await;
    r.run("not-found-envelope", not_found(api)).await;
    r.run("bad-requests", bad_requests(api, env)).await;
    r.run("refusals-on-guarded-drive", refusals(api)).await;
    r.run("forget-refused-while-present", forget_refused(api)).await;
    r.run("resolve-by-every-handle", resolve(api)).await;
    r.run("designation-round-trip", designation(api)).await;
    r.run("overcommit-round-trip", overcommit(api)).await;
    r.run("smoke-test", smoke(api)).await;
    r.run("read-scan-cancel", read_scan_cancel(api)).await;
    r.run("topology-covers-every-drive", topology(api)).await;
    r.run("kube-watch", kube_watch(api)).await;
    r.run("placement-by-wwn", placement_one(api)).await;
    r.run("usage-from-stormblock", usage(api)).await;
    r.run("shelves", shelves(api)).await;
    r.run("nvme-health", nvme(api)).await;
    r.run("monitor-cost", monitor(api)).await;
    r.run("worker-refusals", worker_refusals(api)).await;
    Ok(())
}

fn status_is(reply: &crate::api::Reply, want: &[u16], what: &str) -> Result<(), Why> {
    ensure(want.contains(&reply.status), format!("{what}: HTTP {} (wanted {want:?}): {}", reply.status, reply.text.chars().take(160).collect::<String>()))
}

/// Every write needs a storage-admin bearer (#45, stormcos#250): with none,
/// or a made-up one, a write is refused before it reaches a drive — even one
/// that names no drive at all. With the run's bearer it gets past the gate
/// (and is then a plain 404). Reads with the run's credential answer.
async fn writes_need_admin(api: &Api, env: &Env) -> Outcome {
    api.need((0, 18, 0), "the write gate")?;
    let h = api.get("api/v1/health").await?.json("GET health")?;
    let gate = h["writes"]["gate"].as_str().unwrap_or_default().to_string();
    ensure(matches!(gate.as_str(), "enforce" | "audit"), format!("health.writes.gate {:?}", h["writes"]["gate"]))?;
    if gate != "enforce" {
        return Err(Why::Skip(format!("the node runs admin_gate = {gate}")));
    }
    let path = "api/v1/drives/no-such-drive-stormdrive-test/designation/spare";
    let none = api.post_as(path, None).await?;
    status_is(&none, &[401], "a write with no bearer")?;
    ensure(none.body["code"] == "unauthorized", format!("envelope {}", none.text))?;
    let fake = api.post_as(path, Some("not-a-real-bearer")).await?;
    status_is(&fake, &[401], "a write with a made-up bearer")?;
    for (p, what) in [("api/v1/format", "a batch format"), ("api/v1/worker/jobs", "a worker job (not a dry run)")] {
        status_is(&api.post_as(p, None).await?, &[401], what)?;
    }
    status_is(&api.get("api/v1/drives").await?, &[200], "a read with the run's credential")?;
    let mut detail = "no bearer / made-up bearer → 401 on designation, format, worker job; the run reads".to_string();
    if let Some(t) = &env.token {
        status_is(&api.post_as(path, Some(t)).await?, &[404], "the run's bearer, unknown drive")?;
        detail.push_str("; the run's bearer gets through to a 404");
    }
    Ok(detail)
}

/// Nothing answers anonymously but health (#19, stormcos#81): with no
/// credential a read is 401 (`/` too: there is no page, #84), a
/// made-up bearer is 401 whatever the node allows, and plain HTTP answers
/// health only. A node on `allow_anonymous` (the transition) skips the
/// anonymous half.
async fn reads_need_credential(api: &Api) -> Outcome {
    api.need((0, 21, 0), "TLS and read credentials")?;
    let h = api.get_as("api/v1/health", None).await?;
    status_is(&h, &[200], "health with no credential")?;
    for path in ["api/v1/drives", "metrics", "api/v1/placement"] {
        status_is(&api.get_as(path, Some("not-a-real-bearer")).await?, &[401], &format!("{path} with a made-up bearer"))?;
    }
    if h.body["reads"]["anonymous"] == serde_json::json!(true) {
        return Err(Why::Skip("the node runs api.allow_anonymous (transition): made-up bearers refused, anonymous reads served".into()));
    }
    for path in ["api/v1/drives", "metrics", "api/v1/placement", "api/v1/components", "api/v1/worker/jobs"] {
        let r = api.get_as(path, None).await?;
        status_is(&r, &[401], &format!("{path} with no credential"))?;
        ensure(r.body["code"] == "unauthorized", format!("{path}: envelope {}", r.text))?;
    }
    status_is(&api.get_as("", None).await?, &[401], "/ with no credential")?;
    let mut detail = "no credential → 401 on drives, metrics, placement, feed, jobs, /; health open".to_string();
    if let Some(rest) = api.base().strip_prefix("https://") {
        let plain = Api::new(&format!("http://{rest}"), &Default::default(), None, None);
        status_is(&plain.get("api/v1/health").await?, &[200], "health over plain HTTP")?;
        let d = plain.get("api/v1/drives").await?;
        status_is(&d, &[403], "drives over plain HTTP")?;
        ensure(d.body["code"] == "tls_required", format!("plain HTTP envelope {}", d.text))?;
        detail.push_str("; plain HTTP: health only (403 tls_required)");
    }
    Ok(detail)
}

/// Unknown handles answer 404 in stormblock's `{error, code}` envelope.
async fn not_found(api: &Api) -> Outcome {
    for path in ["api/v1/drives/no-such-drive", "api/v1/drives/no-such-drive/health", "api/v1/shelves/no-such-shelf", "api/v1/placement/no-such-drive"] {
        let reply = api.get(path).await?;
        status_is(&reply, &[404], path)?;
        ensure(reply.body["code"] == "not_found" && reply.body["error"].is_string(), format!("{path}: envelope {}", reply.text))?;
    }
    Ok("404 + {error, code: not_found} on drives, health, shelves, placement".into())
}

/// Malformed requests are refused before anything happens. Each is refused
/// by a check that runs before the drive is looked at or anything starts.
async fn bad_requests(api: &Api, env: &Env) -> Outcome {
    let ds = crate::drives(api).await?;
    let id = ds.iter().find(|d| present(d)).map(|d| s(d, "id").to_string()).unwrap_or_else(|| "no-such-drive".into());
    let before = ds.iter().find(|d| s(d, "id") == id).map(|d| s(d, "activity").to_string());
    let cases = [
        (format!("api/v1/drives/{id}/locate/sideways"), 400),
        (format!("api/v1/drives/{id}/designation/bogus"), 400),
        (format!("api/v1/drives/{id}/overcommit/lots"), 400),
        (format!("api/v1/drives/{id}/test/bogus"), 400),
        (format!("api/v1/drives/{id}/fleet/bogus"), 400),
    ];
    for (path, want) in &cases {
        status_is(&api.post_empty(path).await?, &[*want], path)?;
    }
    // Overcommit ratio outside 1–16 (the typo guard): checked before the drive.
    status_is(&api.post(&format!("api/v1/drives/{id}/overcommit"), json!({"enabled": true, "ratio": 99.0})).await?, &[400], "overcommit 99×")?;
    if before.is_some() {
        // An unusable sector size: validated before any drive guard, so it
        // cannot start a format on any drive.
        status_is(&api.post("api/v1/format", json!({"drives": [id], "block_size": 1234})).await?, &[400], "format 1234")?;
        // A firmware image that is not in the store (or no store): refused
        // before any drive is considered.
        let img = format!("stormdrive-test-{}-absent.bin", env.run_id.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>());
        status_is(&api.post("api/v1/firmware", json!({"drives": [id], "image": img})).await?, &[400, 404], "firmware, absent image")?;
        let after = crate::drive(api, &id).await?;
        ensure(Some(s(&after, "activity")) == before.as_deref(), format!("activity moved to {}", s(&after, "activity")))?;
    }
    Ok(format!("{} malformed requests refused", cases.len() + if before.is_some() { 3 } else { 1 }))
}

/// A drive in the fleet (or holding stormblock's data) is refused a
/// destructive test, a format and a second join — and is left as it was.
async fn refusals(api: &Api) -> Outcome {
    let ds = crate::drives(api).await?;
    let Some(d) = pick::guarded(&ds) else {
        return Err(Why::Skip("requires a drive in the fleet or holding stormblock data".into()));
    };
    let (id, name) = (s(d, "id").to_string(), s(d, "name").to_string());
    let before = s(d, "activity").to_string();
    let mut checked = vec![];
    status_is(&api.post(&format!("api/v1/drives/{id}/test"), json!({"kind": "destructive_sample"})).await?, &[409], "destructive test")?;
    checked.push("destructive test");
    status_is(&api.post(&format!("api/v1/drives/{id}/format"), json!({"block_size": 4096})).await?, &[409], "format")?;
    checked.push("format");
    if in_fleet(d) {
        // "already in the fleet" is join's first guard (400 when the node has
        // stormblock off, which is refused all the same).
        status_is(&api.post(&format!("api/v1/drives/{id}/fleet"), json!({"action": "join"})).await?, &[400, 409], "join")?;
        checked.push("join");
    }
    let after = crate::drive(api, &id).await?;
    ensure(s(&after, "activity") == before, format!("{name}: activity {before} → {}", s(&after, "activity")))?;
    ensure(after["membership"] == d["membership"], format!("{name}: membership changed"))?;
    let why = if in_fleet(d) { "in the fleet" } else { "holds stormblock data" };
    Ok(format!("{name} ({why}): {} refused, unchanged", checked.join(", ")))
}

async fn forget_refused(api: &Api) -> Outcome {
    let ds = crate::drives(api).await?;
    let Some(d) = pick::forget_refused(&ds) else {
        return Err(Why::Skip("no present drive".into()));
    };
    let id = s(d, "id");
    status_is(&api.delete(&format!("api/v1/drives/{id}")).await?, &[409], "DELETE a present drive")?;
    let still = crate::drive(api, id).await?;
    ensure(s(&still, "id") == id, "the drive went")?;
    Ok(format!("{}: DELETE refused (present)", s(d, "name")))
}

/// Every handle the API takes names the same drive.
async fn resolve(api: &Api) -> Outcome {
    let ds = crate::drives(api).await?;
    let Some(d) = ds.iter().find(|d| present(d)) else {
        return Err(Why::Skip("no present drive".into()));
    };
    let id = s(d, "id");
    let mut handles = vec![s(d, "path").to_string(), s(d, "name").to_string()];
    if let Some(w) = d["wwid"].as_str().filter(|w| !w.is_empty()) {
        handles.push(w.to_uppercase());
    }
    if pick::serial_unique(&ds, s(d, "serial")) {
        handles.push(s(d, "serial").to_string());
    }
    for h in &handles {
        let got = crate::drive(api, &h.replace('/', "%2F")).await?;
        ensure(s(&got, "id") == id, format!("{h:?} resolved to {}", s(&got, "id")))?;
    }
    Ok(format!("{}: {} handles", s(d, "name"), handles.len()))
}

/// spare, then back: the setting sticks, an event records it, and it is
/// restored (also by cleanup if the suite stops here).
async fn designation(api: &Api) -> Outcome {
    let ds = crate::drives(api).await?;
    let Some(d) = pick::settings_candidate(&ds) else {
        return Err(Why::Skip("requires an idle, out-of-fleet drive with no designation, data or slabs".into()));
    };
    let (id, name) = (s(d, "id").to_string(), s(d, "name").to_string());
    let seq = crate::latest_seq(api).await?;
    crate::UNDO.lock().unwrap().designation.push((id.clone(), "none".into()));
    api.post(&format!("api/v1/drives/{id}/designation"), json!({"designation": "spare"})).await?.json("set spare")?;
    let now = crate::drive(api, &id).await?;
    let set = s(&now, "designation") == "spare";
    api.post_empty(&format!("api/v1/drives/{id}/designation/none")).await.ok();
    let back = crate::drive(api, &id).await?;
    if s(&back, "designation") == "none" {
        crate::UNDO.lock().unwrap().designation.retain(|(i, _)| i != &id);
    }
    ensure(set, format!("{name}: designation reads {} after setting spare", s(&now, "designation")))?;
    ensure(s(&back, "designation") == "none", format!("{name}: not restored ({})", s(&back, "designation")))?;
    let evs = api.get(&format!("api/v1/events?since={seq}")).await?.json("events")?;
    let n = evs["events"].as_array().map(|e| e.iter().filter(|e| e["kind"] == "designation" && e["drive_id"] == id.as_str()).count()).unwrap_or(0);
    ensure(n >= 2, format!("{name}: {n} designation events for two changes"))?;
    Ok(format!("{name}: none → spare → none, {n} events"))
}

/// Overcommit on and back, on a drive with no slabs (nothing reaches the
/// engine): the setting sticks and the ratio guard holds.
async fn overcommit(api: &Api) -> Outcome {
    api.need((0, 14, 0), "per-drive overcommit")?;
    let ds = crate::drives(api).await?;
    let Some(d) = pick::settings_candidate(&ds) else {
        return Err(Why::Skip("requires an idle, out-of-fleet drive with no designation, data or slabs".into()));
    };
    let (id, name) = (s(d, "id").to_string(), s(d, "name").to_string());
    let orig = api.get(&format!("api/v1/drives/{id}/overcommit")).await?.json("GET overcommit")?["overcommit"].clone();
    crate::UNDO.lock().unwrap().overcommit.push((id.clone(), orig.clone()));
    let set = api.post_empty(&format!("api/v1/drives/{id}/overcommit/2")).await?.json("overcommit 2")?;
    let ok = set["overcommit"]["enabled"] == true && set["overcommit"]["ratio"].as_f64() == Some(2.0);
    let body = json!({"enabled": orig["enabled"].as_bool().unwrap_or(false), "ratio": orig["ratio"]});
    let back = api.post(&format!("api/v1/drives/{id}/overcommit"), body).await?.json("restore overcommit")?;
    if back["overcommit"] == orig {
        crate::UNDO.lock().unwrap().overcommit.retain(|(i, _)| i != &id);
    }
    ensure(ok, format!("{name}: after 2×: {}", set["overcommit"]))?;
    ensure(back["overcommit"] == orig, format!("{name}: not restored: {} (was {orig})", back["overcommit"]))?;
    Ok(format!("{name}: {orig} → 2× → restored"))
}

/// The smoke test (sampled reads only) runs to a verdict.
async fn smoke(api: &Api) -> Outcome {
    let ds = crate::drives(api).await?;
    let Some(d) = pick::read_candidate(&ds) else {
        return Err(Why::Skip("requires an idle, usable drive".into()));
    };
    let (id, name) = (s(d, "id").to_string(), s(d, "name").to_string());
    let started = api.post(&format!("api/v1/drives/{id}/test"), json!({"kind": "smoke"})).await?;
    if started.status == 409 {
        return Err(Why::Skip(format!("{name} became busy: {}", started.text)));
    }
    started.json("start smoke")?;
    crate::UNDO.lock().unwrap().tests.push(id.clone());
    let t = crate::wait_test(api, &id, Duration::from_secs(300)).await?;
    crate::UNDO.lock().unwrap().tests.retain(|i| i != &id);
    let after = crate::drive(api, &id).await?;
    ensure(s(&after, "activity") == "idle", format!("{name}: activity {} after the test", s(&after, "activity")))?;
    ensure(t["state"] == "passed", format!("{name}: smoke {}: {}", t["state"], t["errors"]))?;
    Ok(format!("{name}: smoke passed, {} MiB read", t["bytes_done"].as_u64().unwrap_or(0) >> 20))
}

/// A full read scan starts, reports progress, and cancels cleanly.
async fn read_scan_cancel(api: &Api) -> Outcome {
    let ds = crate::drives(api).await?;
    let Some(d) = pick::read_candidate(&ds) else {
        return Err(Why::Skip("requires an idle, usable drive".into()));
    };
    let (id, name) = (s(d, "id").to_string(), s(d, "name").to_string());
    let started = api.post_empty(&format!("api/v1/drives/{id}/test/read_scan")).await?;
    if started.status == 409 {
        return Err(Why::Skip(format!("{name} became busy: {}", started.text)));
    }
    started.json("start read scan")?;
    crate::UNDO.lock().unwrap().tests.push(id.clone());
    tokio::time::sleep(Duration::from_secs(2)).await;
    let c = api.post_empty(&format!("api/v1/drives/{id}/test/cancel")).await?;
    // 409 "not running": a tiny drive finished its scan within 2 s.
    status_is(&c, &[200, 409], "cancel")?;
    let t = crate::wait_test(api, &id, Duration::from_secs(60)).await?;
    crate::UNDO.lock().unwrap().tests.retain(|i| i != &id);
    let state = t["state"].as_str().unwrap_or_default().to_string();
    ensure(state == "cancelled" || (c.status == 409 && state == "passed"), format!("{name}: read scan ended {state}: {}", t["errors"]))?;
    let after = crate::drive(api, &id).await?;
    ensure(s(&after, "activity") == "idle", format!("{name}: activity {} after cancel", s(&after, "activity")))?;
    Ok(format!("{name}: read scan {state} after {} MiB", t["bytes_done"].as_u64().unwrap_or(0) >> 20))
}

/// The controller → shelf → drive tree holds every drive exactly once.
async fn topology(api: &Api) -> Outcome {
    let v = api.get("api/v1/topology").await?.json("GET /api/v1/topology")?;
    let mut seen: Vec<String> = vec![];
    let ids = |arr: &Value, seen: &mut Vec<String>| {
        for d in arr.as_array().into_iter().flatten() {
            seen.push(s(d, "id").to_string());
        }
    };
    for c in v["controllers"].as_array().into_iter().flatten() {
        ids(&c["direct"], &mut seen);
        for sh in c["shelves"].as_array().into_iter().flatten() {
            ids(&sh["drives"], &mut seen);
        }
    }
    ids(&v["unlocated"], &mut seen);
    let mut want: Vec<String> = crate::drives(api).await?.iter().map(|d| s(d, "id").to_string()).collect();
    seen.sort();
    want.sort();
    ensure(seen == want, format!("tree has {} drive leaves for {} drives", seen.len(), want.len()))?;
    Ok(format!("{} drives under {} controllers", want.len(), v["controllers"].as_array().map(Vec::len).unwrap_or(0)))
}

/// `?watch=1` streams an ADDED event per Drive first.
async fn kube_watch(api: &Api) -> Outcome {
    let n = crate::drives(api).await?.len();
    if n == 0 {
        return Err(Why::Skip("no drives to watch".into()));
    }
    let lines = api.stream_lines("apis/storage.storm.io/v1/drives?watch=1", n, Duration::from_secs(10)).await?;
    ensure(lines.len() == n, format!("{} watch events for {n} drives", lines.len()))?;
    for l in &lines {
        let v: Value = serde_json::from_str(l).map_err(|e| Why::Fail(format!("watch line {l:?}: {e}")))?;
        ensure(v["type"] == "ADDED" && v["object"]["kind"] == "Drive", format!("watch line {l}"))?;
    }
    Ok(format!("{n} ADDED events"))
}

async fn placement_one(api: &Api) -> Outcome {
    api.need((0, 12, 0), "/api/v1/placement/{id}")?;
    let ds = crate::drives(api).await?;
    let Some(d) = ds.iter().find(|d| d["wwid"].as_str().is_some_and(|w| !w.is_empty())) else {
        return Err(Why::Skip("no drive with a WWN".into()));
    };
    let w = s(d, "wwid");
    let v = api.get(&format!("api/v1/placement/{w}")).await?.json("GET placement/{wwn}")?;
    ensure(s(&v, "id") == s(d, "id") && s(&v, "wwn") == w, format!("placement/{w}: {}", v))?;
    ensure(!s(&v, "node").is_empty(), "no node in the record")?;
    Ok(format!("{w} → {}", s(d, "name")))
}

/// A drive holding stormblock slabs shows its usage, read from the engine
/// with the engine's token (#12, #14). Waits up to two poll intervals.
async fn usage(api: &Api) -> Outcome {
    api.need((0, 13, 0), "per-drive usage")?;
    let ds = crate::drives(api).await?;
    let Some(d) = ds.iter().find(|d| present(d) && (in_fleet(d) || in_use(d))) else {
        return Err(Why::Skip("requires a drive in the fleet or holding stormblock data".into()));
    };
    let (id, name) = (s(d, "id").to_string(), s(d, "name").to_string());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(130);
    loop {
        let now = crate::drive(api, &id).await?;
        let u = &now["usage"];
        if !u.is_null() {
            let slabs = u["slabs"].as_array().map(Vec::len).unwrap_or(0);
            ensure(slabs > 0, format!("{name}: usage lists no slabs on a drive stormblock uses"))?;
            ensure(u["used_bytes"].as_u64() <= u["capacity_bytes"].as_u64(), format!("{name}: used > capacity"))?;
            return Ok(format!("{name}: {slabs} slabs, {} GiB used, {} GiB free", u["used_bytes"].as_u64().unwrap_or(0) >> 30, u["free_bytes"].as_u64().unwrap_or(0) >> 30));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Why::Fail(format!("{name}: no usage after 130 s — stormblock unreachable, or the engine token refused (#14)")));
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// requires: [sas-shelf]. Each shelf answers by its key, with power, fans
/// and slots, and its drives carry bays.
async fn shelves(api: &Api) -> Outcome {
    let v = api.get("api/v1/shelves").await?.json("GET /api/v1/shelves")?;
    let shelves = v["shelves"].as_array().cloned().unwrap_or_default();
    if shelves.is_empty() {
        return Err(Why::Skip("requires: [sas-shelf] — no SES enclosure on this machine".into()));
    }
    for sh in &shelves {
        let key = s(sh, "key");
        let one = api.get(&format!("api/v1/shelves/{key}")).await?.json("GET shelf")?;
        ensure(s(&one, "key") == key, format!("shelf {key} answered {}", s(&one, "key")))?;
        ensure(one["power_supplies"]["total"].is_u64() && one["fans"]["total"].is_u64(), format!("shelf {key}: no PSU/fan counts"))?;
        for d in one["drives"].as_array().into_iter().flatten() {
            ensure(d["bay"].is_u64(), format!("shelf {key}: {} has no bay", s(d, "name")))?;
        }
    }
    Ok(format!("{} shelves", shelves.len()))
}

/// requires: [nvme]. A sampled NVMe drive reports wear and spare.
async fn nvme(api: &Api) -> Outcome {
    let ds = crate::drives(api).await?;
    let nv: Vec<&Value> = ds.iter().filter(|d| present(d) && s(d, "kind") == "nvme_ssd").collect();
    if nv.is_empty() {
        return Err(Why::Skip("requires: [nvme] — no NVMe drive on this machine".into()));
    }
    let sampled: Vec<&&Value> = nv.iter().filter(|d| !d["health"]["collected_at"].is_null()).collect();
    ensure(!sampled.is_empty(), format!("none of {} NVMe drives has a health sample", nv.len()))?;
    for d in &sampled {
        ensure(d["health"]["wear_pct"].is_u64() && d["health"]["available_spare_pct"].is_u64(), format!("{}: no wear/spare in the SMART sample", s(d, "name")))?;
    }
    Ok(format!("{} of {} NVMe drives sampled with wear + spare", sampled.len(), nv.len()))
}

/// Health polling's cost is reported, and nothing is stuck.
async fn monitor(api: &Api) -> Outcome {
    api.need((0, 15, 0), "/api/v1/monitor")?;
    let v = api.get("api/v1/monitor").await?.json("GET /api/v1/monitor")?;
    let stuck = v["stuck"].as_array().map(Vec::len).unwrap_or(0);
    ensure(stuck == 0, format!("stuck drives: {}", v["stuck"]))?;
    Ok(format!("{} samples, {} timeouts, avg {} ms", v["samples"], v["timeouts"], v["avg_sample_ms"]))
}

/// The drive worker (#5), without running anything: malformed jobs are
/// refused, a dry run reports its plan and changes nothing, and a guarded
/// drive (in the fleet, or holding stormblock data) shows up refused.
/// Only `dry_run: true` requests carry real steps.
async fn worker_refusals(api: &Api) -> Outcome {
    api.need((0, 17, 0), "the drive worker")?;
    let before = api.get("api/v1/worker/jobs").await?.json("GET jobs")?["jobs"].as_array().map(Vec::len).unwrap_or(0);
    let bad = [
        (json!({"select": {"model": "x"}, "steps": []}), 400, "no steps"),
        (json!({"select": {}, "steps": [{"op": "partition"}], "dry_run": true}), 400, "empty selection"),
        (json!({"select": {"model": "x"}, "steps": [{"op": "partition"}, {"op": "format", "block_size": 4096}], "dry_run": true}), 400, "steps out of order"),
        (json!({"select": {"model": "x"}, "steps": [{"op": "format", "block_size": 520}], "dry_run": true}), 400, "520 is not a target"),
        (json!({"select": {"drives": ["no-such-drive"]}, "steps": [{"op": "partition"}], "dry_run": true}), 404, "unknown drive"),
        (json!({"select": {"model": "no-such-model-stormdrive-test"}, "steps": [{"op": "partition"}], "dry_run": true}), 400, "matches no drive"),
    ];
    for (body, want, what) in &bad {
        status_is(&api.post("api/v1/worker/jobs", body.clone()).await?, &[*want], what)?;
    }
    status_is(&api.get("api/v1/worker/jobs/no-such-job").await?, &[404], "unknown job")?;
    status_is(&api.post_empty("api/v1/worker/jobs/no-such-job/cancel").await?, &[404], "cancel unknown job")?;
    let mut detail = format!("{} malformed jobs refused", bad.len());
    let ds = crate::drives(api).await?;
    if let Some(d) = pick::guarded(&ds) {
        let id = s(d, "id");
        let plan = api
            .post("api/v1/worker/jobs", json!({"select": {"drives": [id]}, "steps": [{"op": "format", "block_size": 4096}], "dry_run": true}))
            .await?
            .json("dry run")?;
        ensure(plan["dry_run"] == true && plan["runnable"] == 0, format!("dry run on a guarded drive: {plan}"))?;
        let reason = plan["refused"][0]["reason"].as_str().unwrap_or_default().to_string();
        ensure(!reason.is_empty(), "no refusal reason")?;
        let after = crate::drive(api, id).await?;
        ensure(s(&after, "activity") == s(d, "activity"), "the dry run changed the drive's activity")?;
        detail.push_str(&format!("; dry run on {}: refused ({reason})", s(d, "name")));
    }
    let after = api.get("api/v1/worker/jobs").await?.json("GET jobs")?["jobs"].as_array().map(Vec::len).unwrap_or(0);
    ensure(after == before, format!("jobs went from {before} to {after}: a refused or dry-run request created a job"))?;
    Ok(detail)
}
