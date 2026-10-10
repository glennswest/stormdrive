//! `short` (< 2 min): stormdrive is up on the node and does its main job —
//! it has found the node's drives, named each by a stable identity, judged
//! its health, and says where each one is. Read-only: nothing on the node
//! changes.

use std::collections::HashSet;
use std::time::Duration;

use serde_json::Value;

use crate::api::Api;
use crate::env::Env;
use crate::pick::{present, s};
use crate::report::{ensure, Outcome, Report, Why};

const KINDS: [&str; 6] = ["nvme_ssd", "sas_ssd", "sas_hdd", "sata_ssd", "sata_hdd", "unknown"];
const VERDICTS: [&str; 5] = ["good", "warning", "failing", "failed", "unknown"];

pub async fn run(_env: &Env, api: &Api, r: &mut Report) -> Result<(), String> {
    crate::api_up(api, r).await?;
    r.run("drives-listed", drives_listed(api)).await;
    r.run("drive-identity", drive_identity(api)).await;
    r.run("health-verdicts", health_verdicts(api)).await;
    r.run("drive-slabs", drive_slabs(api)).await;
    r.run("system-data", system_data(api)).await;
    r.run("system-drives", system_drives(api)).await;
    r.run("summary-card", summary_card(api)).await;
    r.run("placement", placement(api)).await;
    r.run("components-feed", components_feed(api)).await;
    r.run("kube-drives", kube_drives(api)).await;
    r.run("events", events(api)).await;
    r.run("hbas", hbas(api)).await;
    r.run("metrics", metrics(api)).await;
    Ok(())
}

/// Every drive is well-formed, and no two share an id.
async fn drives_listed(api: &Api) -> Outcome {
    let ds = crate::drives(api).await?;
    let mut ids = HashSet::new();
    for d in &ds {
        let id = s(d, "id");
        ensure(id.len() == 36, format!("{}: id {id:?} is not a uuid", s(d, "name")))?;
        ensure(ids.insert(id.to_string()), format!("id {id} listed twice"))?;
        ensure(KINDS.contains(&s(d, "kind")), format!("{}: kind {:?}", s(d, "name"), d["kind"]))?;
        ensure(["out", "fleet"].contains(&s(d, "membership")), format!("{}: membership {:?}", s(d, "name"), d["membership"]))?;
        ensure(!s(d, "path").is_empty(), format!("{id}: no path"))?;
    }
    let missing = ds.iter().filter(|d| !present(d)).count();
    Ok(format!("{} drives ({} present, {missing} missing)", ds.len(), ds.len() - missing))
}

/// A drive is found again by its id and by its WWID, whatever the case.
async fn drive_identity(api: &Api) -> Outcome {
    let ds = crate::drives(api).await?;
    if ds.is_empty() {
        return Err(Why::Skip("no drives on this node".into()));
    }
    let mut n = 0;
    for d in ds.iter().filter(|d| present(d)) {
        let id = s(d, "id");
        let by_id = crate::drive(api, id).await?;
        ensure(s(&by_id, "id") == id, format!("GET {id} answered {}", s(&by_id, "id")))?;
        if let Some(w) = d["wwid"].as_str().filter(|w| !w.is_empty()) {
            let by_wwid = crate::drive(api, &w.to_uppercase()).await?;
            ensure(s(&by_wwid, "id") == id, format!("WWID {w} resolved to {}", s(&by_wwid, "id")))?;
            n += 1;
        }
    }
    Ok(format!("every present drive resolves by id; {n} by WWID"))
}

/// Every present drive has a verdict; once health polling has run, at least
/// one has actually been sampled. Waits up to one poll interval for that.
async fn health_verdicts(api: &Api) -> Outcome {
    let ds = crate::drives(api).await?;
    let live: Vec<&Value> = ds.iter().filter(|d| present(d)).collect();
    if live.is_empty() {
        return Err(Why::Skip("no present drives on this node".into()));
    }
    for d in &live {
        let v = d["health"]["status"].as_str().unwrap_or("unknown");
        ensure(VERDICTS.contains(&v), format!("{}: verdict {v:?}", s(d, "name")))?;
        // #58: no sample is never silent — it says why.
        if d["health"]["collected_at"].is_null() {
            ensure(
                d["health"]["not_collected"].as_str().is_some_and(|w| !w.is_empty()),
                format!("{}: no health sample and no reason why", s(d, "name")),
            )?;
        }
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(75);
    loop {
        let ds = crate::drives(api).await?;
        let sampled: Vec<&Value> = ds.iter().filter(|d| present(d) && !d["health"]["collected_at"].is_null()).collect();
        if !sampled.is_empty() {
            let bad: Vec<String> = sampled
                .iter()
                .filter(|d| ["failing", "failed"].contains(&d["health"]["status"].as_str().unwrap_or("")))
                .map(|d| format!("{} {}", s(d, "name"), d["health"]["status"]))
                .collect();
            let mut detail = format!("{} of {} present drives sampled", sampled.len(), live.len());
            if !bad.is_empty() {
                // A failing drive is stormdrive doing its job, not a failed test.
                detail.push_str(&format!("; reported: {}", bad.join(", ")));
            }
            return Ok(detail);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Why::Fail("no drive has a health sample after 75 s".into()));
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Every present drive answers /slabs (#58), and a drive whose slab
/// partitions the engine runs from the network carries the finding.
async fn drive_slabs(api: &Api) -> Outcome {
    let ds = crate::drives(api).await?;
    let live: Vec<&Value> = ds.iter().filter(|d| present(d)).collect();
    if live.is_empty() {
        return Err(Why::Skip("no present drives on this node".into()));
    }
    let (mut with, mut findings) = (0, vec![]);
    for d in &live {
        let v = api.get(&format!("api/v1/drives/{}/slabs", s(d, "id"))).await?.json("GET slabs")?;
        let on_disk = v["on_disk"].as_array().ok_or_else(|| Why::Fail(format!("{}: on_disk is not a list", s(d, "name"))))?;
        if on_disk.is_empty() {
            continue;
        }
        with += 1;
        let remote = v["engine"]["diskless"] == true || v["engine"]["system"] == "remote" || v["engine"]["data"] == "remote";
        if remote {
            ensure(!v["finding"].is_null(), format!("{}: slabs on the disk, engine runs remote, no finding", s(d, "name")))?;
        }
        if !v["finding"].is_null() {
            findings.push(format!("{}: {}", s(d, "name"), v["finding"]["message"].as_str().unwrap_or("")));
        }
    }
    let mut out = format!("{} drives answer, {with} with slab partitions", live.len());
    if !findings.is_empty() {
        // An unused local disk is stormdrive doing its job, not a failed test.
        out.push_str(&format!("; reported: {}", findings.join("; ")));
    }
    Ok(out)
}

/// Drive history and hardware assets in system-data (#64): when the volume
/// is mounted for stormdrive, this boot's assets are recorded and every
/// sampled drive has history. Skipped while it is not mounted.
async fn system_data(api: &Api) -> Outcome {
    api.need((0, 26, 0), "drive history + assets (#64)")?;
    let st = api.get("api/v1/history").await?.json("GET /api/v1/history")?;
    ensure(st["dir"].is_string(), "history status names no dir")?;
    if st["active"] != true {
        return Err(Why::Skip(format!("system-data not mounted: {}", st["reason"].as_str().unwrap_or("?"))));
    }
    // The first discovery pass takes the assets; give it a moment.
    let mut assets = None;
    for _ in 0..30 {
        let r = api.get("api/v1/assets").await?;
        if r.ok() {
            assets = Some(r.json("GET /api/v1/assets")?);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    let a = assets.ok_or_else(|| Why::Fail("system-data is active but no assets record was taken in 30 s".into()))?;
    let items = a["items"].as_object().ok_or_else(|| Why::Fail("assets: items is not an object".into()))?;
    ensure(!items.is_empty(), "assets: no items")?;
    ensure(a["boot_id"] == st["boot"]["boot_id"], format!("assets boot {} ≠ this boot {}", a["boot_id"], st["boot"]["boot_id"]))?;
    ensure(a["changes"].is_array(), "assets: no changes list")?;
    let cpus = items.keys().filter(|k| k.starts_with("cpu/")).count();
    // Every drive health was collected for has history.
    let mut with = 0;
    for d in crate::drives(api).await?.iter().filter(|d| present(d) && !d["health"]["collected_at"].is_null()) {
        let h = api.get(&format!("api/v1/drives/{}/history?limit=1", s(d, "id"))).await?.json("GET drive history")?;
        let recs = h["records"].as_array().map(Vec::len).unwrap_or_default();
        ensure(recs == 1, format!("{}: sampled, but {recs} history records", s(d, "name")))?;
        with += 1;
    }
    Ok(format!(
        "{}: {} asset items ({cpus} cpu), {} change(s) since the previous boot; {with} drives with history",
        st["dir"].as_str().unwrap_or_default(),
        items.len(),
        a["changes"].as_array().map(Vec::len).unwrap_or_default()
    ))
}

/// The system-designated drives (#66): the route lists exactly the drives
/// designated `system`, and with system-data mounted the install's file is
/// written and names each of them. Read only.
async fn system_drives(api: &Api) -> Outcome {
    api.need((0, 32, 0), "system designation (#66)")?;
    let v = api.get("api/v1/system-drives").await?.json("GET /api/v1/system-drives")?;
    let listed: Vec<String> = v["designated"].as_array().into_iter().flatten().map(|e| s(e, "id").to_string()).collect();
    let marked: Vec<String> = crate::drives(api).await?.iter().filter(|d| s(d, "designation") == "system").map(|d| s(d, "id").to_string()).collect();
    for id in &marked {
        ensure(listed.contains(id), format!("{id} is designated system but not listed"))?;
    }
    ensure(listed.len() == marked.len(), format!("{} listed, {} designated system", listed.len(), marked.len()))?;
    let st = api.get("api/v1/history").await?.json("GET /api/v1/history")?;
    if st["active"] != true {
        return Err(Why::Skip(format!("{} system drive(s); system-data not mounted, no file for the install", marked.len())));
    }
    // The first discovery pass writes the file; give it a moment.
    let mut file = v["system_data"].clone();
    for _ in 0..30 {
        if file["written"] == true {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        file = api.get("api/v1/system-drives").await?.json("GET /api/v1/system-drives")?["system_data"].clone();
    }
    ensure(file["written"] == true, format!("system-data is active but {} was not written in 30 s: {}", s(&file, "file"), file["last_error"]))?;
    let in_file: Vec<String> = file["drives"].as_array().into_iter().flatten().map(|e| s(e, "id").to_string()).collect();
    for id in &marked {
        ensure(in_file.contains(id), format!("{id} is designated system but not in {}", s(&file, "file")))?;
    }
    Ok(format!("{} system drive(s), {} in {}", marked.len(), in_file.len(), s(&file, "file")))
}

/// The stormd card: a known health word, and its Drives count is the list's.
async fn summary_card(api: &Api) -> Outcome {
    let v = api.get("api/v1/summary").await?.json("GET /api/v1/summary")?;
    let h = v["health"].as_str().unwrap_or_default();
    ensure(["ok", "warn", "error", "idle"].contains(&h), format!("health {h:?}"))?;
    let n = crate::drives(api).await?.len();
    let shown = v["metrics"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["label"] == "Drives"))
        .and_then(|m| m["value"].as_str())
        .unwrap_or_default()
        .to_string();
    ensure(shown == n.to_string(), format!("card says {shown:?} drives, the list has {n}"))?;
    Ok(format!("{h}: {}", v["detail"].as_str().unwrap_or_default()))
}

/// Where every drive is: one record per drive, and an unchanged view
/// answers 304 to its own ETag.
async fn placement(api: &Api) -> Outcome {
    api.need((0, 12, 0), "/api/v1/placement")?;
    let n = crate::drives(api).await?.len();
    for _ in 0..3 {
        let reply = api.get("api/v1/placement").await?;
        let etag = reply.etag.clone().unwrap_or_default();
        let v = reply.json("GET /api/v1/placement")?;
        let g = v["generation"].as_u64().ok_or_else(|| Why::Fail("no generation".into()))?;
        let listed = v["drives"].as_array().map(Vec::len).unwrap_or(0);
        ensure(listed == n, format!("placement lists {listed} drives, /drives {n}"))?;
        ensure(etag == format!("\"{g}\""), format!("ETag {etag:?} for generation {g}"))?;
        let again = api.get_if_none_match("api/v1/placement", &etag).await?;
        if again.status == 304 {
            return Ok(format!("{n} drives, generation {g}, 304 on If-None-Match"));
        }
        // A drive moved or changed state between the two reads: look again.
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(Why::Fail("If-None-Match never answered 304 (generation moving on every read?)".into()))
}

/// The stormview feed: the node's `system` component and one per drive.
async fn components_feed(api: &Api) -> Outcome {
    let v = api.get("api/v1/components").await?.json("GET /api/v1/components")?;
    let feed = v.as_array().ok_or_else(|| Why::Fail("feed is not an array".into()))?;
    ensure(feed.iter().any(|c| c["id"] == "system"), "no system component")?;
    let ids: HashSet<&str> = feed.iter().filter_map(|c| c["id"].as_str()).collect();
    for d in crate::drives(api).await? {
        let want = format!("drive:{}", s(&d, "id"));
        ensure(ids.contains(want.as_str()), format!("feed has no {want}"))?;
    }
    Ok(format!("{} components", feed.len()))
}

/// The Kubernetes-shaped Drive list has one object per drive.
async fn kube_drives(api: &Api) -> Outcome {
    let v = api.get("apis/storage.storm.io/v1/drives").await?.json("GET kube drives")?;
    ensure(v["kind"] == "DriveList", format!("kind {}", v["kind"]))?;
    let n = crate::drives(api).await?.len();
    let items = v["items"].as_array().map(Vec::len).unwrap_or(0);
    ensure(items == n, format!("{items} Drive objects for {n} drives"))?;
    Ok(format!("{items} Drive objects, resourceVersion {}", v["metadata"]["resourceVersion"]))
}

async fn events(api: &Api) -> Outcome {
    let v = api.get("api/v1/events").await?.json("GET /api/v1/events")?;
    let latest = v["latest_seq"].as_u64().ok_or_else(|| Why::Fail("no latest_seq".into()))?;
    let evs = v["events"].as_array().ok_or_else(|| Why::Fail("no events array".into()))?;
    ensure(evs.iter().all(|e| e["seq"].as_u64().is_some_and(|q| q <= latest)), "an event past latest_seq")?;
    Ok(format!("{} events, latest seq {latest}", evs.len()))
}

async fn hbas(api: &Api) -> Outcome {
    api.need((0, 13, 0), "/api/v1/hbas")?;
    let v = api.get("api/v1/hbas").await?.json("GET /api/v1/hbas")?;
    let hbas = v["hbas"].as_array().ok_or_else(|| Why::Fail("no hbas array".into()))?;
    ensure(hbas.iter().all(|h| !s(h, "pcie_addr").is_empty()), "an HBA with no PCIe address")?;
    Ok(format!("{} HBAs", hbas.len()))
}

/// `/metrics` (#18) is Prometheus text, and every drive the node has is in
/// it, by serial.
async fn metrics(api: &Api) -> Outcome {
    api.need((0, 19, 0), "/metrics")?;
    let m = api.get("metrics").await?;
    ensure(m.status == 200 && m.content_type.starts_with("text/plain"), format!("GET /metrics: {} {}", m.status, m.content_type))?;
    let mut samples = 0;
    for line in m.text.lines().filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let (name, value) = line.rsplit_once(' ').ok_or_else(|| Why::Fail(format!("not a sample: {line}")))?;
        ensure(value.parse::<f64>().is_ok(), format!("not a number: {line}"))?;
        ensure(name.starts_with("smartctl_") || name.starts_with("stormdrive_"), format!("unexpected family: {line}"))?;
        samples += 1;
    }
    ensure(m.text.contains("stormdrive_build_info{"), "no stormdrive_build_info")?;
    let ds = crate::drives(api).await?;
    for d in &ds {
        let serial = s(d, "serial").replace('\\', "\\\\").replace('"', "\\\"");
        ensure(m.text.contains(&format!("serial=\"{serial}\"")), format!("{}: not in /metrics", s(d, "name")))?;
    }
    Ok(format!("{samples} samples, {} drives", ds.len()))
}
