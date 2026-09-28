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

**Version: 0.16.0** — version locations: `Cargo.toml`, `Cargo.lock`, `web/package.json` (+ its lock), this file.

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
`sc-build` from this checkout. It builds the pushed commit on dev.g8.lo in a
scratch volume and deletes it; there is no checkout on dev, and nothing here
needs root.

```
git push && sc-build                                   # cargo build && cargo test
sc-build 'cargo clippy --all-targets -- -D warnings'
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
  discovery/      sysfs enumeration, classification, probe cache [Linux]
  hotplug.rs      NETLINK_KOBJECT_UEVENT listener → debounced discovery pass
  contents.rs     slab probe (STRMSLAB at LBA 0 / GPT partition starts) → in_use_by
  smart/          health samples: NVMe Get Log Page 0x02; SAS/SATA sysfs only
  poller.rs       health scheduler: per-drive phase, bounded, timed out, costed
  monitor.rs      loop: discovery, SES/HBA scans, threshold engine, trends,
                  usage + reconcile, events
  events.rs       in-memory event ring (4096, not persisted)
  topology.rs     location: SAS/SES/mpt3sas bays, NVMe PCIe slots, locate LEDs
  hba.rs          PCIe SCSI HBA inventory (firmware, BIOS, NVDATA versions)
  scsi.rs         raw SCSI over SG_IO: INQUIRY/VPD, READ CAPACITY(16), MODE SENSE/
                  SELECT, FORMAT UNIT, TUR progress, RECEIVE/SEND DIAGNOSTIC,
                  WRITE BUFFER; sense decoding portable + unit-tested
  ses.rs          SES-2 pages 0x01/0x02/0x07/0x0A, shelf reports, IDENT control
  format.rs       sector-size reformat jobs (520 → 512/4096)
  firmware.rs     image store + WRITE BUFFER / NVMe download+commit jobs
  drivetest.rs    smoke / read_scan / destructive_sample
  stormblock.rs   engine client (:9090, bearer token): drives, labels, slabs,
                  health, drain, overcommit
  fleet.rs        the loop: labels, health push, overcommit push, drains →
                  retire, auto-add
  usage.rs        per-drive used/free joined from stormblock's slabs (#12)
  placement.rs    where every drive + shelf is, hashed generation (#10)
  components.rs   stormview feed: drives, shelves, HBAs with actions
  api/mod.rs      axum REST :9092, summary card, embeds web/dist (the page)
  api/kube.rs     /apis/storage.storm.io/v1/{drives,enclosures} (stormblock#80)
web/              the page: Svelte 5 + stormview DataGrid (#6); web/dist is
                  committed and embedded; rebuild on dev with web/rebuild.sh
```

Not in the tree (design only, see docs/architecture.md): a sequencer, a
thermal actuator, SCSI log sense.

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

- **stormblock** (`http://127.0.0.1:9090`, `Authorization: Bearer`, v17+):
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

### Phase 0: Project bootstrap — DONE
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

### Phase 1b: Fleet membership, designations, drive testing (2026-08-26) — DONE (v0.2.0)

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

### Phase 1c: NetApp shelf topology (2026-08-26) — DONE (v0.3.0)

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
- [x] Placement as data, not prose (#3, v0.9.0): drive `bay` + `hba`
      metrics, shelf `hba` per path — so a renderer orders a shelf grid by
      bay and names the card without a regex over `detail`

### Phase 1e: NetApp shelf management + 520→4096 reformat (2026-09-05) — code DONE (v0.7.0/0.8.0); live pass open

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

### #7: docs rewritten from the code (2026-09-28) — DONE

Owner: "every component needs to update its doc from code." Pattern:
stormbootx b1347d9 / stormuefi b15dcba.
- [x] README from the source: what it does today, sc-build, every flag and
      config key with defaults, ports, health/metrics, how it ships (#4)
- [x] docs/architecture.md: design-only parts marked, stale parts fixed
- [x] CLAUDE.md: build section (sc-build), module map, phases vs the code
- [x] Example config matches config.rs (now a test)
- [x] Cross-refs checked: stormblock routes (drives.rs), stormd
      `[process.ui]` + 400 ms summary timeout, stormcos service_golden +
      40-services (found /sys ro → stormcos#166)
- [x] Issues for promises the code does not keep: #21 (SIGTERM), #22 (SAS
      log sense), #23 (wear projection), #24 (firmware redundancy gate),
      #25 (events not persisted); existing #18 (/metrics), #19 (auth)
- [x] sc-build (132/132) + clippy -D warnings pass on 024dc54; #7 and #4
      closed; golden `golden-stormdrive-9d1fa8491d44` (850d6f9), release
      request stormcos#131

### #6: ready for many more drives (2026-09-28) — IN PROGRESS

Survey against the issue's list:
- [x] Discovery across HBAs/expanders, SES bays, multipath as one drive
      (Phases 1c/1e, #10, #15)
- [x] Stable identity (WWID uuid5), hotplug without restart (#15)
- [x] Health polling that scales: phased, bounded, timed out (#15)
- [ ] Bulk operations through the drive worker — that is #5 (bulk format
      and firmware exist; bulk test/join and per-failure-domain
      sequencing do not)
- [ ] UI usable with hundreds of rows: the embedded page is a flat
      vanilla-JS table rebuilt by innerHTML every 4 s; the feed's `system`
      component has one flat has_many of every drive, shelves are roots
      with no HBA edge. Direction (Svelte + stormview DataGrid vs vanilla
      grouping) asked of the owner 2026-09-28 → **Svelte 5 + stormview
      DataGrid** in `web/`, `web/dist` committed and embedded, dist built on
      dev by sc-build (tarball over stdout), never on this VM
- [x] Plan (the page, `web/`) — 55688e8 + page test:
  - [x] groups are the top rows: each shelf, then each HBA's direct drives,
        then unlocated; each group's drives in a nested DataGrid
        (collapsed groups render nothing; keyed rows diff, no rebuild)
  - [x] compact drive columns (DataGrid text/health/metrics/actions only);
        click a drive → detail pane with designation, overcommit, tests,
        format, firmware, locate, usage, progress, health messages
  - [x] filter box + quick filters (needs reformat, attention, out of
        fleet, busy); ticking a group = all its drives
  - [x] bulk bar: format 4096/512, firmware, smoke/scan test, designation,
        locate on/off — server batch where it exists (format, firmware),
        per-drive calls otherwise (bulk test/join is #5)
  - [x] shelf pane (elements), firmware image store, events
  - [x] `web/src/lib/model.js` pure grouping/filter, `node --test`; page
        smoke test in jsdom with 212 drives (`npm run test:page`)
  - [x] dist built on dev (`web/rebuild.sh`), committed; Rust serves `web/dist` (`/`, `/ui`,
        `/ui/`, `/assets/*`, `/ui/assets/*`); `src/ui/index.html` goes
  - [x] docs, changelog; stormview#12/#13/#14 filed (nested select-all,
        leaf sections, exports condition)
  - [ ] v0.16.0, sc-build + clippy + `web/rebuild.sh --check`, close #6,
        golden
- [ ] Metrics per drive — #18

### #12: per-drive usage (2026-09-27) — DONE

"Look at a drive and know how much storage is left." stormblock v17.1
`/api/v1/slabs` names each slab's drive (`drive {serial, wwn, model,
path}`, stormblock#136) with total/free/allocated slots.
- [x] `usage` on each drive: capacity; slabs (id, role, tier, total,
      allocated, free); `outside_slabs` = capacity − Σ slab total (partition
      table, slab metadata, unpartitioned); `used` = Σ allocated; `free` =
      capacity − used. Joined by wwn, else serial, else path. Fetched each
      monitor tick; unknown (null) until stormblock answers once
- [x] `/api/v1/drives`, kube Drive status, components feed metrics, UI column
- [x] Tests (usage::tests: R230 system disk adds up, wwn→serial→path
      join, no-slab drive), docs (architecture "Per-drive usage"), changelog
- [x] Released in v0.13.0 (46ecff9); sc-build 114/114
- [x] Golden `golden-stormdrive-01a422df544f` (0.15.0 @ 9bb1abb, contains
      46ecff9), release request stormcos#131; #12 closed 2026-09-28. Live
      reading rides the release (R230 runs 0.11.0)

### #15: 160+ drives per node (2026-09-27) — DONE

Owner: 160 NVMe per 4U node (ASG-4116S-NU160R class), 1,600 per rack.
Found in the code: health polling is sequential (one hung NVMe ioctl stalls
the round for every drive); trend samples every poll (160 × 512 samples ≈
5 MB of inventory JSON rewritten every tick, under the lock); discovery
re-reads LBA 0 + GPT (and READ CAPACITY) of every drive every 30 s; no
hotplug; an NVMe namespace under native multipath
(`/sys/devices/virtual/nvme-subsystem/…`) has no PCIe location; no NVMe
locate LED; nothing links a replacement drive to the one it replaces.
- [x] Health scheduler (`poller.rs`): phase per drive, `max_concurrent`
      8, `sample_timeout_secs` 10, stuck drives skipped; `GET
      /api/v1/monitor` cost stats (3ef40c6)
- [x] Trend on change or daily; persist compact, outside the lock,
      skipped when unchanged (3ef40c6)
- [x] Discovery probe cache, one /proc/mounts read, NVMe path nodes and
      hidden disks skipped (2b536df)
- [x] Hotplug (`hotplug.rs`): kernel uevents → debounced pass (07f8ab4)
- [x] NVMe: multipath head → controller, slot on the chain, VMD BDFs,
      bay from numeric slot, locate via attention / NPEM (ede48da)
- [x] Replace: `replaces` by bay_key + event; `DELETE /api/v1/drives/{id}`
      forget; feed/UI Forget (d1d4fcf, 202580b)
- [x] Docs (architecture: cost per poll cycle table), changelog (fa9e8b9)
- [x] sc-build + clippy pass on 4b595b8 (131/131; #20 was a test window
      edge, closed); v0.15.0
- [x] sc-build passes on 8a86c76 (v0.15.0), 131/131; status on #15
- [x] Golden `golden-stormdrive-01a422df544f` (0.15.0 @ 9bb1abb, built
      after stormcentral#111 closed), release request stormcos#131; #15
      closed. Not seen on a 160-bay chassis (none here); /metrics is #18,
      UI grouping for hundreds of rows is #6

### #13: per-drive overcommit setting (2026-09-27) — DONE

Owner: "an attribute to drives to allow overcommit or not." Split per
stormblock `docs/multi-drive.md` §5: stormdrive holds the per-drive
setting; stormblock enforces it when a claim binds (stormblock#152);
rustkube-node#62 publishes the headroom; stormconsole#29 shows it.
- [x] `overcommit {enabled, ratio}` on Drive, persisted, default off
      (ratio 1.0); ratio finite, 1.0..=16.0 (typo guard)
- [x] `GET/PUT/POST /api/v1/drives/{id}/overcommit` (+ `/{off|ratio}`),
      event on change
- [x] usage: `promisable_bytes`; `committed_bytes` + `headroom_bytes` from
      the slab listing's `committed_bytes` (null until stormblock#152)
- [x] Push `PUT /api/v1/drives/{path}/overcommit` for drives with slabs;
      contract posted on stormblock#152
- [x] kube status, feed metric + action, UI selector; tests (118), docs,
      changelog; sc-build + clippy pass on 9b9d357; v0.14.0
- [x] sc-build passes on f34c3aa (v0.14.0), 118/118; status on #13,
      contract on stormblock#152
- [x] Golden `golden-stormdrive-01a422df544f` (0.15.0 @ 9bb1abb, contains
      f34c3aa), release request stormcos#131; #13 closed 2026-09-28.
      Enforcement itself is stormblock#152 (open; contract posted, no
      reply yet — match any renames it asks for)

### #10: drive placement for the PV mirror (rustkube-node#60, 2026-09-27) — DONE

rustkube-node mirrors each PV's placement: stormblock v17.1 names the
drives by `wwn` (raw sysfs wwid) + `serial` with a `generation` feed
(stormblock#136); stormdrive supplies where each drive physically is.
Existing: `/api/v1/drives` + kube Drive carry location, but the kube
fingerprint churns on every health sample, a re-bay that keeps the /dev
name is never re-located, no move event, no SAS phy.
- [x] `GET /api/v1/placement` (+ `/{wwn|serial|uuid|path}`): per drive
      wwn, serial, shelf (key/logical id/model/serial), bay, SAS address,
      phy, expander, HBA, PCIe, membership/designation/activity/health;
      shelves list; `generation` = FNV over placement fields only (53-bit,
      stable across restarts), ETag, `?since=` / If-None-Match → 304
- [x] Resolve order uuid → wwn (case-insensitive) → path/name → serial
- [x] Re-locate every discovery pass; `location` event on a move
- [x] `sas_phy` + `expander` in Location (sysfs port → phy)
- [x] Tests (104; 6 new), docs, changelog, v0.12.0; sc-build + clippy
      pass
- [x] Golden `golden-stormdrive-df998ec9c92d` (0.12.0 @ f48f7a4),
      stormcos#131; #10 closed, contract posted on rustkube-node#60.
      Not yet seen on a shelf rig (sas_phy/expander synthetic-tested)

### #14: present the stormblock engine token (P0, stormcos#104, 2026-09-27) — DONE

stormblock v17 (stormblock#107) requires `Authorization: Bearer` on all of
`/api/v1`; every stormdrive → engine call is a 401 today. #2 is parked.
- [x] Token lookup: `stormblock.api_token` / `$STORMBLOCK_API_TOKEN`, then
      `stormblock.token_file` / `$STORMBLOCK_TOKEN_FILE`, then
      `/run/stormblock/engine/api_token`, `/etc/stormblock/api_token`,
      `/var/lib/stormblock/api_token`; re-read while absent and on a 401
      (retry once); optional admin token for DELETE
- [x] Tests (98, incl. a stand-in engine: token absent → minted → rotated),
      docs, changelog, v0.11.0; sc-build + clippy -D warnings pass on ff7ade0
- [x] Golden `golden-stormdrive-fa8dbebdd485` (0.11.0 @ 98bd1d2), release
      request stormcos#123, posted on stormcos#104; #14 closed. #2 resumes

### #2: first look at a real node (R230, 2026-09-24) — DONE

stormdrive 0.9.0 on the Dell R230 (`192.168.30.2:9092`, stormcos 11.3x).
Verified over HTTP: health, summary, drives, topology, events, components,
UI all answer; one drive (WDC WD20EFAX-68F, fw 0A82, mpt3sas host0, bay 4)
with stable id. Found:
- [x] **Safety:** sda is the node's system disk (stormcos GPT, stormblock
      system + data slabs, root on ublk) but reads "out of fleet, not
      mounted" — Format 4K, destructive test and Join were all offered.
      stormblock's `/api/v1/drives` is empty (slabs are `drive=file+…`),
      so reconcile cannot tell. Fix: probe the disk for stormblock slabs
      (`STRMSLAB` at LBA 0 or at any GPT partition start) → `in_use_by`;
      block join/format/destructive test; serialise firmware like fleet
- [x] SATA behind a SAS HBA classified `sas_hdd` (sas_address exists for
      SATA end devices) — use the SCSI vendor `ATA`
- [x] `ioerr_cnt` published as "media errs" — it counts failed commands
- [x] File stormblock issue: system-disk slabs not attributed to a drive
      — stormblock#133
- [x] stormconsole on the node (:9094) aggregates the feed: `plugin:drive`
      ok, drive + shelf components present
- [x] Report on the issue (comment 5820044394): no firmware images
      anywhere, no source for them; BIOS/HBA firmware not modelled
- [x] v0.10.0 tagged (f5aa63f). sc-build passed on 244ee47 and on
      cd6600e (v0.10.0 + docs, 94/94, 2026-09-24)
- [x] Golden `golden-stormdrive-b0941b2857a0` (0.10.0 @ 205bf9a, sc-build
      passed on that commit); release request stormcos#70
- [x] Decided (Glenn, 2026-09-24): HBA firmware *version collection* is
      stormdrive's; BIOS goes to stormipmi. Flashing HBAs is not in scope
- [x] Live on the R230 (0.11.0, 2026-09-27): sda `in_use_by` stormblock
      (partitions 2, 3), kind `sata_hdd`, Join/Destructive/Format 4K
      disabled in the feed
- [x] `hba.rs`: every PCIe SCSI HBA from /sys/class/scsi_host (driver,
      version_fw, version_bios, nvdata, board name/assembly/tracer, host
      SAS address), grouped by PCIe function; refreshed each discovery
      pass; `GET /api/v1/hbas`, `firmware` on topology controllers, `hba:`
      components in the feed, UI panel; tests, docs, v0.13.0, golden
      - [x] Code + docs pushed (8f9eb43, 64c29f1 borrow fix, 5b44e1c docs)
      - [x] sc-build passes on 4a69a06 (114/114); #17 closed
      - [x] v0.13.0 tagged (46ecff9)
      - [x] sc-build passes on 46ecff9 (114/114)
      - [x] Golden `golden-stormdrive-01a422df544f` (0.15.0 @ 9bb1abb,
            contains 46ecff9), release request stormcos#131; #2 closed
            2026-09-28. `/api/v1/hbas` on the R230 rides the release

### Phase 1: Discovery + inventory — DONE (checked against the code, #7)
- [x] sysfs enumeration: /sys/block scan, classify NVMe/SAS/SATA, SSD/HDD
- [x] Stable identity: WWID → uuid5, fallback model+serial
- [x] Inventory persistence with atomic writes; Missing-state detection
- [x] Exclusion policy (config + built-in list above)
- [x] Hotplug: netlink kobject uevent listener (#15)
- [x] `GET /api/v1/drives`, `GET /api/v1/drives/{id}`

### Phase 2: Monitoring (health, wear, thermal) — mostly DONE
- [x] NVMe: Get Log Page 0x02 via NVME_IOCTL_ADMIN_CMD
- [x] SCSI/SATA: sysfs (state, ioerr_cnt, hwmon)
- [ ] SCSI/SATA: SG_IO log pages (0x2F, 0x0D, 0x11) + ATA SMART — #22
- [x] Threshold engine → health state machine, hysteresis
- [x] Wear trending: persisted samples (on change or daily)
- [ ] Wear-out projection — #23
- [x] Event ring + `GET /api/v1/events` (in memory; persistence is #25)
- [ ] Prometheus `/metrics` — #18
- [x] `GET /api/v1/summary` for the stormd card

### Phase 3: Location awareness — DONE
- [x] SES enclosure mapping (/sys/class/enclosure, or SES pages + mpt3sas)
- [x] Locate LED: sysfs slot, SES IDENT, PCIe attention, NPEM
- [x] NVMe: PCIe BDF chain + physical slot, VMD, multipath heads (#15)
- [x] SAS address, phy, expander (#10)
- [x] Location → failure-domain labels pushed to stormblock

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
      rebuild in flight, volume not already degraded) — #24
- [ ] Shelf (IOM) firmware via SES download microcode page 0x0E

### Phase 6: Thermal management
- [x] Per-drive + per-enclosure thermal view (health temps, SES shelf panel)
- [x] Threshold alerts (warn/critical from config → warning + event)
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

### Phase 7: UI extension (stormd newer UI) — DONE as a vanilla-JS page
- [x] Embedded page, relocatable base path: vanilla JS until v0.15.0,
      Svelte 5 + stormview DataGrid in `web/` from #6
- [x] Drive table with location, health, wear; shelves + HBA panels
- [x] Locate-LED buttons, event feed, firmware upload/update
- [x] `[process.ui]` deploy snippet (`deploy/stormd-ui.toml`); the stormcos
      golden does not use it (reached via HTTPRoute + stormconsole)

### Open questions (to resolve with Glenn)
- ~~Migrations~~: resolved — stormblock v11 drain API (stormblock#70),
  wired in Phase 4.
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
