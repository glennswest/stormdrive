# StormDrive Development Guide

## Project Overview

StormDrive is the **physical drive management plane** of the Storm ecosystem —
the layer *below* StormBlock. StormBlock turns drives into volumes; StormDrive
knows what the drives *are*: where they sit, how healthy they are, how worn
they are, how hot they are, what firmware they run, and when one is about to
die. It hands qualified drives to StormBlock and tells it when to get data off
one.

Pure Rust. Single daemon (`stormdrive`) with a REST API, a stormd UI
extension, and a monitor loop. Runs on every storage node alongside
stormblock.

**Version: 0.8.0** — version locations: `Cargo.toml`, `Cargo.lock`, this file.

## Why it exists (from the stormblock review, 2026-08-26)

The full review is in [docs/stormblock-review.md](docs/stormblock-review.md).
The short form — stormblock today has:

- **No drive discovery.** Drives enter only via config/CLI at startup or
  `POST /api/v1/drives {path}`. Nothing scans `/dev`, no hotplug.
- **No health polling.** `smart_status()` is called only on demand from
  `GET /api/v1/drives/{id}/smart`; the `stormblock_drive_*` Prometheus gauges
  are declared and never set.
- **No failure detection.** An I/O error propagates to the caller and is not
  recorded. Nothing ever marks a RAID member `Failed`; degraded-read is dead
  code in production.
- **No physical location.** No enclosure, bay, SAS address, PCIe slot —
  nothing below node-level `topology` labels. stormblock's own CLAUDE.md:
  *"stormblock has to know its own drives first, then where those drives
  are."*
- **No stable drive identity.** `DeviceId.uuid` is `Uuid::new_v4()` on every
  open; the slab header's `device_uuid` is written and never checked.
- **Working NVMe SMART decode exists** but only feeds `must-gather`
  (stormblock `src/main.rs:2342`) — good reference code for our collector.

StormDrive owns everything in that list. StormBlock stays the *consumer* of
drives; StormDrive is the *curator* of them.

## Build on dev, never on this Mac

Same rule as every project here: **every `cargo build/test/check` runs on
`root@dev.g8.lo`** (`/root/stormdrive`). The drive path is Linux-only
(sysfs, ioctls, SG_IO, netlink uevents) behind `cfg(target_os = "linux")`;
a macOS build skips exactly the code most likely to be wrong.

```
commit  →  push  →  ssh root@dev.g8.lo 'cd /root/stormdrive && git pull && \
    CARGO_TARGET_DIR=/build/cargo/stormdrive cargo test'
```

Target dirs live on dev's 2 TB spinning drive (`/build/cargo/<project>`),
never on the 198 GB SSD root — see ~/CLAUDE.md "Nothing lives on the SSD
between builds".

The repo is public (matching stormblock); `/root/stormdrive` on dev pulls
`origin` from GitHub over https.

After building on dev, clean up: `rm -rf /root/stormdrive/target/debug` when
done; check `df -h /` before and after.

```bash
# Release build (what ships)
cargo build --release --target x86_64-unknown-linux-musl
```

## Architecture

See [docs/architecture.md](docs/architecture.md) for the full design.

```
src/
  main.rs         CLI entry, config load, daemon startup
  lib.rs          module tree
  config.rs       stormdrive.toml parsing + validation
  drive.rs        Drive model: stable identity, kind, location, state, health
  inventory.rs    persistent drive registry (<data_dir>/inventory.json)
  discovery/      sysfs enumeration + hotplug (udev netlink) [Linux]
  smart/          health collectors: NVMe admin ioctl, SCSI/ATA via sysfs+SG_IO [Linux]
  monitor.rs      poll loop: rescan, collect, evaluate thresholds, transition states
  events.rs       event ring + severity model
  topology.rs     location resolution: enclosure/bay (SES sysfs), PCIe path, SAS address
  firmware.rs     firmware inventory (update engine: Phase 5)
  thermal.rs      thermal policy (report/alert now; actuation later)
  sequence.rs     maintenance sequencer: one disruptive op at a time, health-gated
  scsi.rs         raw SCSI over SG_IO: INQUIRY/VPD, READ CAPACITY(16), MODE SENSE/
                  SELECT, FORMAT UNIT, TEST UNIT READY progress, RECEIVE/SEND
                  DIAGNOSTIC; sense decoding is portable + unit-tested
  ses.rs          SES-2 enclosure pages (config 0x01, status 0x02, descriptors
                  0x07, additional status 0x0A): shelf identity, PSU/fan/temp/
                  voltage/current/slot elements, shelf + bay IDENT control
  format.rs       sector-size reformat jobs: MODE SELECT block length + FORMAT
                  UNIT (IMMED), progress via TUR sense, kernel rescan after
  stormblock.rs   client for stormblock :9090 (add drive w/ labels+uuid, slabs, health, drain)
  fleet.rs        the loop: labels, health push, drains → retire, auto-add
  api/kube.rs     /apis/storage.storm.io/v1/{drives,enclosures} — Kubernetes-shaped (stormblock#80)
  api/            axum REST :9092  (/api/v1/*, /api/v1/summary for stormd)
```

**Ports:** stormdrive listens on **:9092** (stormblock has :9090, stormd
:9080).

## The testbed (Glenn, 2026-08-26)

Three clusters across three machines — three performance levels:

| Cluster | Hardware | Role |
|---|---|---|
| 2.5" shelf | NetApp SAS shelf, 2.5" drives | High performance |
| 3.5" shelf | NetApp SAS shelf, 3.5" drives | Medium performance |
| PVE (spinning up) | Proxmox VE cluster | Backup |

So tiering operates at **cluster** granularity here, not only per-drive
within a node — a volume's hot copy lives on the 2.5" cluster, its
protection copy on the 3.5" one, its backup on PVE. That is the concrete
case behind stormblock#72 (cross-cluster placement/RAID) and it composes
with per-drive tiers: stormdrive's kind→tier derivation still applies
*within* each cluster.

## Integration contracts

**The hierarchy (Glenn, 2026-08-26):** stormblock is fully distributed —
moves between clusters, RAID between clusters, and the **full site
hierarchy** physically: site ⊃ building ⊃ floor/room ⊃ row ⊃ rack ⊃ node
⊃ hba ⊃ shelf ⊃ bay, with the logical overlay itself hierarchical:
**multicluster-of-multiclusters** — clusters group recursively into a
federation, and **tiering runs across clusters** (a tier can be a whole
cluster; tier migration is movement between clusters).

**Layering (Glenn, 2026-08-26): stormblock is an execution engine.** The
cross-node/cross-cluster brain is a separate planned service,
**stormstorage** — topology registry (node-and-above), placement at every
rung, cross-cluster tiering + orchestration, driving stormblock /v1 on
many nodes. Three layers, no overlap: stormdrive = hardware truth (below
the node), stormblock = per-node execution, stormstorage = fleet policy.
stormblock#72 was re-scoped accordingly: stormblock keeps the primitives
(label chains, rung-aware local allocation, remotely-drivable /v1
fencing/prestage); orchestration goes to stormstorage. stormdrive is authoritative **below the node only**; everything
node-and-above stays with stormblock. Shared label vocabulary: `site`,
`building`, `room`, `row`, `rack`, `node`, `cluster` (theirs); `hba`,
`shelf`, `bay`, `pcie_slot` (ours). See docs/architecture.md "Position in
the distributed hierarchy"; stormblock#72 carries the placement ask.

- **stormblock** (`http://127.0.0.1:9090`): `POST /api/v1/drives {path}`,
  `POST /api/v1/slabs {device_path,tier}`, `GET /api/v1/drives/{id}/smart`,
  `DELETE /api/v1/drives/{id}`. Drain/evacuate over HTTP does not exist yet —
  filed as a stormblock issue; until then stormdrive reports and recommends
  but cannot trigger evacuation remotely.
- **stormd UI**: `[process.ui]` block (label/proxy/summary) in the node's
  stormd config. Phase 1 ships `GET /api/v1/summary` in stormd's
  `RemoteSummary` shape (`health`/`detail`/`metrics`) for the dashboard card;
  the full iframe UI comes later and must be relocatable under
  `/ui/proxy/stormdrive/` (relative links or `<base>` injection — see mkube's
  `layout.go` for the pattern).
- **Drive exclusions**: never manage `ublkb*` (stormblock's own exports),
  `loop*`, `ram*`, `zram*`, `dm-*`, `md*`, `sr*`, `nbd*`, or the boot drive.

## Work Plan

### Phase 0: Project bootstrap — IN PROGRESS
- [x] stormblock deep review (docs/stormblock-review.md)
- [x] Architecture design (docs/architecture.md)
- [x] Repo, CLAUDE.md, README, CHANGELOG, .gitignore
- [x] Crate scaffold compiles + tests pass on dev.g8.lo (27/27, clippy
      clean, release binary smoke-tested against a real disk)
- [x] File stormblock integration/bug issues (rule 11) — stormblock#65
      (unstable DeviceId), #66 (DELETE drives guard), #67 (evacuate_slab
      break), #68 (drive gauges never set), #69 (RAID failure states
      unreachable), #70 (drive-plane integration surface: stable id on
      open, slab↔drive link, HTTP drain, failure-domain labels)
- [x] Tag v0.1.0

### Phase 1b: Fleet membership, designations, drive testing (2026-08-26) — IN PROGRESS

Glenn's direction: discovery finds drives; the UI then moves them to the
**fleet** (= handed to stormblock). Independently of fleet membership a
drive can be **tested** (only destructively when out of fleet), or marked
**reserved**, **spare**, or **failed** — both in fleet and out.

Model change: the single `DriveState` becomes three orthogonal fields —
`membership` (out|fleet), `designation` (none|reserved|spare|failed,
operator-set), `activity` (idle|testing|joining|draining|missing).

- [x] Rework drive model (breaking for the persisted inventory shape —
      old files load, lifecycle fields reset to defaults; pre-1.0 minor)
- [x] `POST /api/v1/drives/{id}/fleet` — join (stormblock add + optional
      slab format with tier) / leave (guarded: refuse when the drive still
      carries a slab, `force` override until stormblock#70 drain lands)
- [x] `POST /api/v1/drives/{id}/designation` — none|reserved|spare|failed;
      failed-in-fleet raises a drain-needed warning event
- [x] Test engine (`drivetest.rs`): smoke (sampled reads), read_scan (full
      sequential read, progress, cancel, maps past bad regions),
      destructive_sample (write/verify, O_DIRECT read-back, out-of-fleet +
      unmounted only); one test per drive
- [x] Embedded UI page (vanilla JS, stormd style tokens, proxy-prefix
      aware): drive table with join/leave, designation, test, locate,
      event feed
- [x] Update summary card + two-way fleet reconcile; docs; v0.2.0
      (34/34 tests + live smoke test on dev: designation, smoke test
      passed on real disk, UI served. Not yet exercised: destructive test
      on real hardware, join/leave against a live stormblock)

### Phase 1c: NetApp shelf topology (2026-08-26) — IN PROGRESS

Glenn's direction: testing happens on **NetApp SAS shelves** behind
stormblock, more shelves over time — so the hierarchy is
**controller / shelf / drive, multiple of each**.

Consequences:
- `Location` becomes hierarchical: `controller {scsi_host, pcie_addr,
  driver}`, `shelf {id, vendor, model, serial, sas_address}` (from the SES
  processor's SCSI device; serial from VPD page 0x80), `bay`.
- **Dual-IOM shelves present one physical drive on two /dev paths with one
  WWID.** Discovery must group observations by DriveId: one Drive, a
  `paths` list, a stable primary — not a path that flaps every scan.
- `GET /api/v1/topology` — the controller → shelf → drive tree.
- UI: location column shows shelf model + bay; multipath badge.

- [x] Location restructure + shelf enrichment (vendor/model/serial via SES
      device + VPD 0x80)
- [x] Multipath grouping in discovery merge (stable sorted-first primary)
- [x] Topology API (`GET /api/v1/topology`) + UI shelf/bay + paths badge
- [x] stormblock issue: shelf/controller failure-domain-aware slab
      placement — stormblock#71
- [x] v0.3.0 (37/37 tests, clippy clean, live topology tree verified on
      dev; shelf/multipath paths await the NetApp rig)

### Phase 1d: stormview feed — DONE (v0.4.0)
- [x] `GET /api/v1/components` + `/ws/components` via the stormview crate
      (now public): drives + shelves with relations (shelf has_many
      drives → grids) and real actions (locate, fleet join/leave,
      designation, tests) through parameter-less action routes
- [x] Renders in stormd's dashboard/SPA, stormsh tiles, and stormconsole's
      stormdrive plugin (stormconsole consumes the feed per its
      architecture doc — no bespoke mapping needed)

### Phase 1e: NetApp shelf management + 520→4096 reformat (2026-09-05) — IN PROGRESS

Glenn fired up the first NetApp shelf on **stormblock1**: LSI SAS3008
(mpt3sas) → NETAPP DS22412IOM12A (DS224C, IOM12), single path today.
Drives: SEAGATE ST1200MM0098 at **520-byte sectors** (kernel: "Unsupported
sector size 520" → sd attaches with 0 blocks) and NETAPP X425_HCBEP1T2A10
already at 512. Discovery skipped size-0 devices, so the 520s were
invisible. Three asks: shelf info, manage the shelves, reformat 1..n drives
to 4096.

- [x] `scsi.rs`: SG_IO plumbing + sense decoding (portable parsers, tests)
- [x] Discovery sees unusable-sector drives: READ CAPACITY(16) is the
      truth for `block_size`/capacity; `Drive.usable` (kernel exposes
      capacity), `physical_block_size`, `needs_reformat()`
- [x] `ses.rs`: enclosure enumeration (`/sys/class/enclosure` when the
      ses module is bound; otherwise SCSI type-13 devices via sg), page
      parsers, `ShelfReport`; bay via mpt3sas `bay_identifier` /
      `enclosure_identifier` when sysfs enclosure slots are absent
- [x] `format.rs`: batch reformat job — MODE SELECT(10) block descriptor
      (fallback MODE SELECT(6)), FORMAT UNIT FMTDATA+IMMED, poll TUR for
      progress (sense 02/04/04 + SKSV progress), rescan the sd device,
      verify the new geometry; out-of-fleet + unmounted only; one per drive
- [x] API: `GET /api/v1/shelves`, `GET /api/v1/shelves/{key}`,
      `POST /api/v1/shelves/{key}/locate`, `POST /api/v1/shelves/{key}/format`
      (every drive in the shelf that needs it), `POST /api/v1/drives/{id}/format`,
      `POST /api/v1/format {drives:[…], block_size}`, `GET /api/v1/format`
- [x] UI: sector column with "520 · reformat" badge, Format button,
      select-many + "Format selected", shelves panel (PSU/fan/temp)
- [x] components feed + kube Enclosure status carry shelf elements
- [x] docs, changelog, v0.7.0 (73 tests, clippy clean, smoke-tested on
      dev: READ CAPACITY over sg, format validation, UI)
- [ ] Live pass on stormblock1 (needs the node's address from Glenn):
      SES pages from the DS22412 IOM12, bay map via page 0x0A vs mpt3sas
      bay_identifier, a real 520→4096 format on one Seagate ST1200MM0098,
      then the shelf-wide batch
- [x] Phase 1e-fw: firmware update (Glenn 2026-09-05: "can we also update
      firmware?") — Phase 5 pulled forward: image store, WRITE BUFFER
      0x0E/0x0F (0x07 fallback), NVMe download+commit, one/many/by-model,
      fleet drives serialised; UI upload + update. v0.8.0 (80 tests,
      clippy clean, image store smoke-tested on dev). Not yet run against
      a real drive: needs a vendor image for the ST1200MM0098 / X425 on
      stormblock1

### Phase 1: Discovery + inventory
- [ ] sysfs enumeration: /sys/block scan, classify NVMe/SAS/SATA, SSD/HDD
- [ ] Stable identity: WWID → uuid5, fallback model+serial
- [ ] Inventory persistence with atomic writes; Missing-state detection
- [ ] Exclusion policy (config + built-in list above)
- [ ] Hotplug: netlink kobject uevent listener (add/remove without polling)
- [ ] `GET /api/v1/drives`, `GET /api/v1/drives/{id}`

### Phase 2: Monitoring (health, wear, thermal)
- [ ] NVMe: Get Log Page 0x02 via NVME_IOCTL_ADMIN_CMD (temp, spare,
      percentage_used, media errors, critical warnings, POH)
- [ ] SCSI/SATA: sysfs (state, ioerr_cnt, hwmon) first; SG_IO log pages
      (Informational Exceptions 0x2F, Temperature 0x0D, SSD wear 0x11) next
- [ ] Threshold engine → health state machine (Good/Warning/Failing/Failed)
- [ ] Wear trending: persisted samples, projected life
- [ ] Event ring + `GET /api/v1/events`
- [ ] Prometheus `/metrics` (stormdrive_drive_healthy, _temperature_celsius,
      _wear_pct, _media_errors, _available_spare_pct)
- [ ] `GET /api/v1/summary` for the stormd card

### Phase 3: Location awareness
- [ ] SES enclosure mapping via /sys/class/enclosure (enclosure id, bay)
- [ ] Locate/fault LED: `POST /api/v1/drives/{id}/locate` (sysfs slot attrs)
- [ ] NVMe: PCIe BDF chain + physical slot (/sys/bus/pci/slots)
- [ ] SAS address/expander topology
- [ ] Location → failure-domain labels for stormblock placement

### Phase 4: StormBlock integration — DONE (0.5.0, against stormblock v11)
- [x] Client: list/add (with labels + uuid)/relabel/remove drives, slabs by
      drive, format slab, health report, drain start/status/cancel
- [x] Auto-add policy (off by default): qualified drive → POST drives with
      labels + uuid → POST slabs with tier derived from kind; 10-minute
      backoff on failure
- [x] Failure flow: Failing/Failed (health or operator) → health pushed →
      drain → poll → empty → leave fleet + locate LED → event "safe to pull"
- [x] Reconcile loop: our inventory vs stormblock's drive list; labels
      re-pushed on location change
- [x] `POST/GET/DELETE /api/v1/drives/{id}/drain`; `leave` with `drain: true`
- [ ] Not yet exercised against real shelves: the label chain a dual-IOM
      NetApp shelf produces, and a drain under I/O load

### Phase 5: Firmware management — mostly DONE (v0.8.0, pulled into 1e)
- [x] Firmware inventory per drive (already collected in Phase 1)
- [x] Image store (<data_dir>/firmware); policy file (model → desired
      version, never automatic) still to do
- [x] NVMe: Firmware Image Download (0x11) + Commit (0x10)
- [x] SCSI: WRITE BUFFER mode 0x0E/0x0F, 0x07 fallback
- [x] Fleet drives one at a time, health-gated
- [ ] Redundancy check via stormblock before a fleet drive resets (no
      rebuild in flight, volume not already degraded)
- [ ] Shelf (IOM) firmware via SES download microcode page 0x0E

### Phase 6: Thermal management
- [ ] Per-drive + per-enclosure thermal view
- [ ] Threshold alerts (warn/critical from config)
- [ ] SES fan/cooling element control (SG_IO SES-2 control page) — actuation,
      gated behind explicit config

### Phase 8: Drive crypto (pending — scope to be confirmed with Glenn)

Glenn (2026-08-26): "some crypto work is also pending." Assumed scope for a
drive-management plane — confirm before building:
- [ ] SED / TCG OPAL: discover locking capability + state, take ownership,
      unlock on boot, key management (where keys live is the design
      question — stormcert? local TPM? stormblock?)
- [ ] Crypto erase for retirement: NVMe Format with SES=2 / Sanitize
      (crypto erase), ATA SECURITY ERASE / OPAL revert, SCSI SANITIZE —
      wired into the retire flow as the step before a drive leaves the bay
- [ ] Erase certificates in the event log (what was erased, how, verified
      when)

### Phase 7: UI extension (stormd newer UI)
- [ ] Embedded SPA (Svelte, stormd style tokens), relocatable base path
- [ ] Drive grid with location, health, wear; enclosure/bay view
- [ ] Locate-LED buttons, event stream, firmware inventory
- [ ] `[process.ui]` deploy snippet with proxy + summary

### Open questions (to resolve with Glenn)
- **Migrations:** the drain flow needs a stormblock-side API
  (evacuate-by-drive). Filed as a stormblock issue; the exact contract (who
  drives retries, what "empty" means for a drive with multiple slabs) is to
  be worked out.
- **Thermal actuation** scope: alert-only vs fan control vs workload
  throttling.
- **Qualification/burn-in** for new drives before handing to stormblock —
  wanted or straight-to-service?
- SAS2/SAS3 controller specifics (which HBAs are in the fleet — affects
  whether we need mpt3sas-specific sysfs paths).

## Rules recap
- Conventional commits, changelog on every change, docs ship with code.
- Commit early/often, push after every logical unit. No claude attribution.
- Check `gh issue list --state open` at session start.
- Bugs found in stormblock/stormd → file issues there, don't fix here.
