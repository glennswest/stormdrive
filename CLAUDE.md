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

**Version: 0.46.1** — version locations: `Cargo.toml`, `Cargo.lock`, this file.

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

## Build and test with sc-build

Per the cross-project rules (`~/src/CLAUDE.md`): commit, push, then
`sc-build` from this checkout. It builds the pushed commit on a fresh build VM
(dev.g8.lo is retired) and destroys it; nothing here needs root.

```
git push && sc-build                                   # cargo build && cargo test
sc-build 'cargo clippy --workspace --all-targets -- -D warnings'
```

The drive path is Linux-only (sysfs, ioctls, SG_IO, netlink uevents) behind
`cfg(target_os = "linux")`. A non-Linux build skips exactly the code most
likely to be wrong. What ships is `cargo build --release --target
x86_64-unknown-linux-musl`, built by stormcentral into the golden
(`stormcentral component build stormdrive --url http://stormcentral.g8.lo`;
stormdrive is a *service* component).

## Architecture

See [docs/architecture.md](docs/architecture.md) for the full design.

```
src/
  main.rs         CLI (--config, --listen, --data-dir), state, spawns monitor + API
  lib.rs          module tree
  config.rs       stormdrive.toml parsing + validation (example file is tested)
  drive.rs        Drive model: stable identity, kind, location, lifecycle, health,
                  overcommit, join/format/test/firmware guards
  inventory.rs    persistent registry (<data_dir>/inventory.json) + trends
  discovery/      sysfs enumeration, classification, probe cache [Linux]; USB
                  disks apart as removable media (#106)
  hotplug.rs      NETLINK_KOBJECT_UEVENT listener → debounced discovery pass
  contents.rs     slab probe (STRMSLAB at LBA 0 / GPT partition starts) → in_use_by
  engine.rs       the engine's own slab report (health `slabs`) vs the slab
                  partitions on each drive → engine_finding (#58)
  smart/          health samples: NVMe Get Log Page 0x02; SAS/SATA sysfs + LOG
                  SENSE 0x2F/0x0D/0x11 or ATA SMART (#22)
  poller.rs       health scheduler: per-drive phase, bounded, timed out, costed
  monitor.rs      loop: discovery, SES/HBA scans, threshold engine, trends,
                  usage + reconcile, events
  events.rs       event ring (4096; newest 512 in events.json, #25)
  transport.rs    SAS or SATA from the drive itself: VPD 0x83, log page 0x18,
                  negotiated link rate (#105)
  bays.rs         `[[bay_map]]`: a platform's physical bay numbering over a
                  controller's slots (#134)
  inuse.rs        what a drive is used for (system/data/raid/mounted/free) and
                  why; destructive verbs refuse it (#123)
  pi.rs           protection-information probe (VPD 0xB4, READ CAPACITY) and
                  the format plan: 4096+PI1 first, alternatives (#82, #130)
  topology.rs     location: SAS/SES/mpt3sas bays, NVMe PCIe slots, locate LEDs,
                  shelf position by the engine's rules (#65)
  megaraid.rs     a RAID controller's enclosures as internal shelves: MFI reads
                  through the driver's char device (#65)
  raidctl.rs      the RAID controller itself: identity, mode (RAID = finding),
                  each disk's state, set good / JBOD over MFI, personality via
                  perccli (#132)
  hba.rs          PCIe SCSI HBA inventory (firmware, BIOS, NVDATA versions)
  scsi.rs         raw SCSI over SG_IO: INQUIRY/VPD, READ CAPACITY(16), MODE SENSE/
                  SELECT, FORMAT UNIT, TUR progress, RECEIVE/SEND DIAGNOSTIC,
                  WRITE BUFFER; sense decoding portable + unit-tested
  ses.rs          SES-2 pages 0x01/0x02/0x07/0x0A, shelf reports, IDENT/FAULT
                  control (bay or the enclosure)
  shelfview.rs    shelf summary: IOMs, PSUs, connectors, multipath, problems (#81)
  shelfobj.rs     the Shelf object: status, conditions, transitions, spec (#83)
  format.rs       sector-size reformat jobs (520 → 512/4096)
  formatcli.rs    `stormdrive format`: one drive from the initramfs, no daemon;
                  fast format (FFMT) first, then FORMAT UNIT (#136)
  worker.rs       the drive worker (#5): select × steps jobs, guards, lanes,
                  jobs.json, restart recovery, prep phase
  erase.rs        NVMe Format NVM / Sanitize, SCSI SANITIZE (build + parse)
  gpt.rs          one-partition GPT with stormblock's slab type GUIDs
  firmware.rs     image store + WRITE BUFFER / NVMe download+commit jobs,
                  redundancy gate before a data-serving drive resets (#24)
  iomfw.rs        shelf IOM firmware: SES Download Microcode page 0x0E (#35)
  drivetest.rs    smoke / read_scan / destructive_sample
  wear.rs         wear-out projection from the wear trend (#23)
  thermal.rs      every component's temperature (hwmon, drives, SES), limits,
                  levels, alerts, cooling request, hot drives, history (#125)
  policy.rs       DrivePolicy: match nodes + drives → worker steps (#50)
  metrics.rs      Prometheus /metrics from cached state (#18)
  tls.rs          :9092 TLS from the stormcert pair; plain = health only (#19)
  stormblock.rs   engine client (:9090, bearer token): drives, labels, slabs,
                  health, drain, overcommit
  fleet.rs        the loop: labels, health push, overcommit push, drains →
                  retire, auto-add
  usage.rs        per-drive used/free joined from stormblock's slabs (#12)
  history.rs      drive history in system-data: records, counter rules,
                  findings across installs (#64)
  assets.rs       per-boot hardware assets: DMI, cpuinfo, SMBIOS 17/38, NICs,
                  NVMe, HBAs, shelves, bays; diff vs the previous boot (#64)
  placement.rs    where every drive + shelf is, hashed generation (#10)
  components.rs   stormview feed: drives, shelves, HBAs with actions
  api/mod.rs      axum REST :9092, summary card (no page since #84: the UI is
                  stormconsole's drive plugin)
  api/kube.rs     /apis/storage.storm.io/v1/{drives,enclosures} (stormblock#80)
  kubeapi.rs      apiserver client (reqwest): TokenReview, SAR, objects, Events (#45)
  kubeauth.rs     the write gate: classify → bearer review / admin token, audit (#45)
  controller.rs   Drive objects, DriveOperations (#45) and DrivePolicies (#50)
                  in the apiserver
retry/            workspace crate: one backoff helper for every remote call (#71)
test/             test container (#11): /test short|medium|long, pick.rs = safety
tests/            suites.rs (the three suites against this daemon on every
                  cargo test), kube.rs (stand-in apiserver: gate, controller,
                  DrivePolicy), restart.rs, tls.rs, chassis160.rs (#31),
                  engine_tls.rs (engine client over https, #101)
```

Not in the tree (design only, see docs/architecture.md): a sequencer. The
thermal actuator is the BMC's (stormipmi#98), acting on our cooling request.

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

- **stormblock** (`http://127.0.0.1:9090`, or `https://` verified against
  `stormblock.ca_file`, #101; `Authorization: Bearer`, v17+):
  `GET/POST /api/v1/drives {path,labels,uuid}`, `DELETE /api/v1/drives/{id}`,
  `PUT …/{id}/labels`, `GET …/{id}/slabs`, `POST …/{id}/health`,
  `GET/POST/DELETE …/{id}/drain`, `GET/POST /api/v1/slabs`, and
  `PUT …/{id}/overcommit` (stormblock#152 — not on stormblock main as of
  2026-09-28; 404 → retried every 10 min). Checked against stormblock
  `src/mgmt/api/drives.rs` for #7.
- **stormd UI**: `[process.ui]` block (label/proxy/summary) in the node's
  stormd config. Phase 1 ships `GET /api/v1/summary` in stormd's
  `RemoteSummary` shape (`health`/`detail`/`metrics`) for the dashboard card;
  the full iframe UI comes later and must be relocatable under
  `/ui/proxy/stormdrive/` (relative links or `<base>` injection — see mkube's
  `layout.go` for the pattern).
- **Drive exclusions**: never manage `ublkb*` (stormblock's own exports),
  `loop*`, `ram*`, `zram*`, `dm-*`, `md*`, `sr*`, `nbd*`, or the boot drive.

## Work Plan

The work plan is docs/work-plan.md (not loaded by default: grep -n "^### " docs/work-plan.md, then read only your section).

## Rules recap
- Conventional commits, changelog on every change, docs ship with code.
- Commit early/often, push after every logical unit. No claude attribution.
- Check `gh issue list --state open` at session start.
- Bugs found in stormblock/stormd → file issues there, don't fix here.
