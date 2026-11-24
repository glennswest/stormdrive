//! The node's SAS cards at run time (#166): the table each discovery pass
//! refreshes ([`scan`]), the guards a `CardOperation` flash passes
//! ([`plan`]), and the flash run itself ([`start`]), whose state the
//! controller writes to the operation's status.
//!
//! One card flash at a time on a node. The run reads the card again
//! (regions too) when it ends, so the HostBusAdapter shows the new versions.

use crate::api::AppState;
use crate::card::{self, CardReport, FlashRecord, FlashSpec, GoldenRef};
use crate::mpt::Region;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long the reset waits for a moment with no I/O in flight.
pub const QUIESCE: Duration = Duration::from_secs(30);

/// The cards and the flash runs of this node.
#[derive(Default)]
pub struct Cards {
    pub reports: tokio::sync::RwLock<card::Cards>,
    /// Flash runs by CardOperation name.
    pub runs: Mutex<HashMap<String, Arc<Run>>>,
}

/// One flash run's state.
#[derive(Debug, Clone, Default)]
pub struct RunState {
    pub phase: String,
    pub step: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub message: String,
    pub image_version: Option<String>,
    /// The firmware manifest's entry the image is (#184).
    pub manifest: Option<crate::fwmanifest::Available>,
    pub before: Option<String>,
    pub after: Option<String>,
    pub started: u64,
    pub finished: Option<u64>,
}

#[derive(Debug, Default)]
pub struct Run {
    pub state: Mutex<RunState>,
}

impl card::Progress for Run {
    fn step(&self, step: &str, done: u64, total: u64) {
        let mut s = self.state.lock().unwrap();
        s.step = step.to_string();
        s.bytes_done = done;
        s.bytes_total = total;
    }
}

impl Run {
    pub fn running(&self) -> bool {
        self.state.lock().unwrap().finished.is_none()
    }
    /// The operation's status from the run.
    pub fn status(&self, requester: &str, generation: Option<i64>) -> Value {
        let s = self.state.lock().unwrap().clone();
        json!({
            "phase": s.phase, "message": s.message, "requester": requester, "step": s.step,
            "bytesDone": s.bytes_done, "bytesTotal": s.bytes_total, "imageVersion": s.image_version, "manifest": s.manifest,
            "before": s.before, "after": s.after,
            "started": crate::controller::rfc3339(s.started), "finished": s.finished.map(crate::controller::rfc3339),
            "observedGeneration": generation,
        })
    }
}

/// Refresh the card table: pages every pass, the regions once per run (and
/// after a flash, which reads its own card again).
pub async fn scan(state: &Arc<AppState>) {
    let prev = state.cards.reports.read().await.clone();
    let Ok((fresh, errs)) = tokio::task::spawn_blocking(move || card::scan(&prev, &[])).await else { return };
    let old: Vec<String> = state.cards.reports.read().await.keys().cloned().collect();
    {
        let mut log = state.events.write().await;
        for k in fresh.keys().filter(|k| !old.contains(k)) {
            let r = &fresh[k];
            log.push(None, crate::events::Severity::Info, "hba", format!("card {k} ({}): firmware {} read through {}", r.man0.as_ref().and_then(|m| m.board_name.clone()).unwrap_or_else(|| r.info.adapter().into()), crate::mpt::version(r.info.firmware_version), r.ctl));
        }
        for e in errs {
            tracing::debug!("card: {e}");
        }
    }
    *state.cards.reports.write().await = fresh;
}

/// The card a CardOperation names: its HostBusAdapter's name or its PCI
/// address.
pub fn find<'a>(cards: &'a card::Cards, node: &str, name: &str) -> Option<&'a CardReport> {
    cards.values().find(|r| r.pci == name || card::object_name(node, &r.pci) == name)
}

/// The block devices of this node's drives on the card, and what holds the
/// node's system slab among them.
pub async fn drives_on(state: &AppState, pci: &str) -> (Vec<String>, Vec<String>) {
    let inv = state.inventory.read().await;
    let mut devs = Vec::new();
    let mut system = Vec::new();
    for d in inv.drives.values() {
        if d.location.controller.as_ref().and_then(|c| c.pcie_addr.as_deref()) != Some(pci) {
            continue;
        }
        if crate::inuse::of(d).use_ == crate::inuse::Use::System {
            system.push(d.name.clone());
        }
        devs.push(d.name.clone());
    }
    (devs, system)
}

/// A flash, checked and ready: the card, the golden it writes and its
/// bytes, the version the image carries, and the manifest entry when it
/// came from one (#184).
#[derive(Debug, Clone)]
pub struct Planned {
    pub card: CardReport,
    pub golden: GoldenRef,
    pub image: Vec<u8>,
    pub version: Option<String>,
    pub manifest: Option<crate::fwmanifest::Available>,
}

/// Everything a flash needs, checked: the card, the image (the golden, for
/// this card: typed in the spec, or the firmware manifest's entry for the
/// card's region, #184), and the guards. `cordoned`: the Node is in
/// maintenance.
pub async fn plan(state: &AppState, spec: &FlashSpec, cordoned: Option<bool>) -> Result<Planned, String> {
    if state.cards.runs.lock().unwrap().values().any(|r| r.running()) {
        return Err("a card flash is already running on this node (one at a time)".into());
    }
    let cards = state.cards.reports.read().await;
    let card = find(&cards, &state.node_name, &spec.card).cloned().ok_or_else(|| format!("no SAS card {} on {} (its HostBusAdapter name or PCI address)", spec.card, state.node_name))?;
    drop(cards);
    let dir = crate::firmware::image_dir(state.config.data_dir.as_deref());
    let (golden, image, manifest) = if spec.image.manifest {
        let region = crate::fwmanifest::CardRegion::of_flash(spec.region).ok_or_else(|| format!("region {}: a flash writes firmware or bios", spec.region.as_str()))?;
        let (id, store) = (crate::fwmanifest::CardId::of(&card), dir.clone());
        let (entry, image, _) = tokio::task::spawn_blocking(move || crate::fwmanifest::current().card_image(region, &id, store.as_deref()))
            .await
            .map_err(|e| format!("the manifest read ended: {e}"))??;
        (spec.image.from_entry(&entry)?, image, Some(crate::fwmanifest::Available::from(&entry)))
    } else {
        let golden = spec.image.typed()?;
        if !crate::firmware::valid_image_name(&golden.name) {
            return Err(format!("image.name {:?}: not a firmware store name", golden.name));
        }
        let dir = dir.ok_or("no data_dir: this node has no firmware store")?;
        let path = dir.join(&golden.name);
        let image = std::fs::read(&path).map_err(|e| format!("image {} in the firmware store ({}): {e}", golden.name, path.display()))?;
        (golden, image, None)
    };
    let version = card::check_image(spec.region, &golden, &image, &card)?;
    let (_, system) = drives_on(state, &card.pci).await;
    if !system.is_empty() {
        match cordoned {
            Some(true) => {}
            Some(false) => return Err(format!("{} on this card hold(s) the node's system slab: cordon the Node {} (maintenance) first", system.join(", "), state.node_name)),
            None => return Err(format!("{} on this card hold(s) the node's system slab, and the Node {} could not be read to see whether it is cordoned", system.join(", "), state.node_name)),
        }
    }
    Ok(Planned { card, golden, image, version, manifest })
}

/// Start the flash. The run is in `state.cards.runs` under `op` until the
/// controller has written its end.
pub fn start(state: Arc<AppState>, op: String, spec: FlashSpec, p: Planned) -> Arc<Run> {
    let Planned { card, golden, image, version: image_version, manifest } = p;
    let run = Arc::new(Run::default());
    {
        let mut s = run.state.lock().unwrap();
        s.phase = "Running".into();
        s.step = "download".into();
        s.bytes_total = image.len() as u64;
        s.image_version = image_version;
        s.manifest = manifest;
        s.started = card::now_secs();
        s.message = format!("flashing {} of {} with {}", spec.region.label(), card.pci, golden.name);
    }
    state.cards.runs.lock().unwrap().insert(op.clone(), run.clone());
    let r2 = run.clone();
    tokio::spawn(async move {
        let (devs, _) = drives_on(&state, &card.pci).await;
        let (region, ioc, mpi25, pci, host) = (spec.region, card.ioc, card.info.mpi25(), card.pci.clone(), card.host.clone());
        let r3 = r2.clone();
        let out = tokio::task::spawn_blocking(move || run_blocking(ioc, mpi25, region, &image, &devs, &pci, &host, &r3)).await;
        let (result, fresh) = match out {
            Ok(v) => v,
            Err(e) => (Err(format!("the flash task ended: {e}")), None),
        };
        let now = card::now_secs();
        let (phase, record_result) = {
            let mut s = r2.state.lock().unwrap();
            s.finished = Some(now);
            match result {
                Ok(f) => {
                    s.before = f.before;
                    s.after = f.after;
                    s.phase = if f.reset { "Succeeded" } else { "Written" }.into();
                    s.message = f.message;
                }
                Err(e) => {
                    s.phase = "Failed".into();
                    s.message = e;
                }
            }
            (s.phase.clone(), s.message.clone())
        };
        let s = r2.state.lock().unwrap().clone();
        let record = FlashRecord {
            operation: op.clone(),
            region,
            image: golden.name.clone(),
            sha256: golden.sha256.to_lowercase(),
            before: s.before,
            after: s.after,
            started: s.started,
            finished: now,
            result: format!("{phase}: {record_result}"),
        };
        let mut cards = state.cards.reports.write().await;
        if let Some(mut f) = fresh {
            f.last_flash = Some(record);
            cards.insert(f.pci.clone(), f);
        } else if let Some(c) = cards.get_mut(&card.pci) {
            c.last_flash = Some(record);
        }
        drop(cards);
        let sev = if phase == "Succeeded" { crate::events::Severity::Info } else { crate::events::Severity::Warning };
        state.events.write().await.push(None, sev, "firmware", format!("card {}: CardOperation {op}: {phase}: {record_result}", card.pci));
    });
    run
}

/// The flash on the card's device, then the card read again.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
#[allow(clippy::too_many_arguments)]
fn run_blocking(ioc: u32, mpi25: bool, region: Region, image: &[u8], devs: &[String], pci: &str, host: &str, run: &Run) -> (Result<card::Flashed, String>, Option<CardReport>) {
    let port = match card::port_for(ioc) {
        Ok(p) => p,
        Err(e) => return (Err(e), None),
    };
    let idle = || card::wait_idle(Path::new("/sys"), devs, QUIESCE);
    let r = card::flash(&port, ioc, mpi25, region, image, &idle, run);
    let fresh = card::read(&port, ioc, pci, host, true).ok();
    (r, fresh)
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
#[allow(clippy::too_many_arguments)]
fn run_blocking(_: u32, _: bool, _: Region, _: &[u8], _: &[String], _: &str, _: &str, _: &Run) -> (Result<card::Flashed, String>, Option<CardReport>) {
    let _ = Path::new("/dev/mpt3ctl");
    (Err("this platform has no mpt control device".into()), None)
}
