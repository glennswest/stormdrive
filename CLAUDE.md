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

**Version: 0.23.0** — version locations: `Cargo.toml`, `Cargo.lock`, `web/package.json` (+ its lock), this file.

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
  discovery/      sysfs enumeration, classification, probe cache [Linux]
  hotplug.rs      NETLINK_KOBJECT_UEVENT listener → debounced discovery pass
  contents.rs     slab probe (STRMSLAB at LBA 0 / GPT partition starts) → in_use_by
  smart/          health samples: NVMe Get Log Page 0x02; SAS/SATA sysfs + LOG
                  SENSE 0x2F/0x0D/0x11 or ATA SMART (#22)
  poller.rs       health scheduler: per-drive phase, bounded, timed out, costed
  monitor.rs      loop: discovery, SES/HBA scans, threshold engine, trends,
                  usage + reconcile, events
  events.rs       event ring (4096; newest 512 in events.json, #25)
  topology.rs     location: SAS/SES/mpt3sas bays, NVMe PCIe slots, locate LEDs
  hba.rs          PCIe SCSI HBA inventory (firmware, BIOS, NVDATA versions)
  scsi.rs         raw SCSI over SG_IO: INQUIRY/VPD, READ CAPACITY(16), MODE SENSE/
                  SELECT, FORMAT UNIT, TUR progress, RECEIVE/SEND DIAGNOSTIC,
                  WRITE BUFFER; sense decoding portable + unit-tested
  ses.rs          SES-2 pages 0x01/0x02/0x07/0x0A, shelf reports, IDENT control
  format.rs       sector-size reformat jobs (520 → 512/4096)
  worker.rs       the drive worker (#5): select × steps jobs, guards, lanes,
                  jobs.json, restart recovery, prep phase
  erase.rs        NVMe Format NVM / Sanitize, SCSI SANITIZE (build + parse)
  gpt.rs          one-partition GPT with stormblock's slab type GUIDs
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
  kubeapi.rs      apiserver client (reqwest): TokenReview, SAR, objects, Events (#45)
  kubeauth.rs     the write gate: classify → bearer review / admin token, audit (#45)
  controller.rs   Drive objects + DriveOperations in the apiserver (#45)
test/             test container (#11): /test short|medium|long, pick.rs = safety
tests/suites.rs   the three suites against this daemon on every cargo test
web/              the page: Svelte 5 + stormview DataGrid (#6); web/dist is
                  committed and embedded; rebuild on dev with web/rebuild.sh
```

Not in the tree (design only, see docs/architecture.md): a sequencer, a
thermal actuator.

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

### #5: the drive worker (2026-09-28) — DONE (v0.17.0)

Owner: a worker inside stormdrive, API + console, no CLI: low-level format,
partition, stormblock format; one drive or hundreds; progress, per-drive
state, a job record; safe. Exists already: SCSI FORMAT UNIT 520→512/4096
(format.rs), join with a whole-disk slab (fleet.rs).

Design (`src/worker.rs` + helpers), one job = a selection × a list of steps:
- **Select:** `drives` (handles), `shelf` (+ `bays` "0-11,14"), `model`,
  `unusable` (needs_reformat), all ANDed; one of them required.
- **Steps**, in order, per drive:
  - `format {block_size}`: SCSI FORMAT UNIT (existing), or NVMe Format NVM
    (0x80) with the LBA format whose data size matches (Identify NS 0x06)
  - `sanitize {method: block|crypto|overwrite}`: NVMe Sanitize (0x84,
    progress Get Log 0x81; refused when the controller has other
    namespaces) or SCSI SANITIZE (0x48, IMMED, progress in TUR sense
    04/1B; SAT maps it to ATA SANITIZE). ATA SECURITY ERASE (password
    dance) is not built → issue
  - `partition {role: data|system}`: zero head/tail, one GPT partition
    1 MiB → end, type SLAB_DATA / SLAB (stormblock's GUIDs), BLKRRPART
  - `enroll {tier?, role}`: open the partition (or whole disk) in
    stormblock with labels + uuid, format a slab with role + tier;
    the drive's `stormblock_path` records which path stormblock holds
- **Safety:** never a fleet drive. A drive with a stormblock slab or a
  filesystem (ext*, xfs, btrfs, vfat, ntfs, swap, LVM, zfs — on the disk
  or any GPT partition) is refused unless `destroy` names it by stable id,
  WWN or serial (not /dev name) — data_slab_on's identity rule. Checked at
  submit and again before each destructive step. `dry_run` reports the plan.
- **Schedule:** low-level steps bounded per HBA (`worker.max_per_hba`,
  default 8), parallel otherwise (owner's first comment); `enroll` one at
  a time per failure domain (shelf, else HBA) (second comment). Whether
  low-level steps should also be one per domain → Decide issue.
- **State:** jobs persisted in `<data_dir>/jobs.json`; per-drive
  `prep` phase unusable → formatting/sanitizing n% → ready → enrolled on
  `/api/v1/drives`, kube Drive status, feed. After a restart: a SCSI
  format/sanitize still running on the drive is re-attached and polled;
  anything else in flight is `interrupted` (reported, never re-run blind);
  queued steps wait for `POST …/resume`.
- **API:** `POST /api/v1/worker/jobs`, `GET /api/v1/worker/jobs[/{id}]`,
  `POST /api/v1/worker/jobs/{id}/cancel` (queued steps only),
  `POST …/{id}/resume`.

Steps:
- [x] plan · [x] fs/slab signature probe · [x] GPT writer + crc32
      (sfdisk --verify on a file: "No errors detected")
- [x] NVMe identify/format/sanitize builders + parsers · [x] SCSI SANITIZE
- [x] selector + guards + lanes (pure, tested) · [x] worker runtime,
      jobs.json, restart · [x] enroll via partition + stormblock_path in
      fleet/reconcile/API/kube · [x] API, feed, kube `prep`
- [x] docs; medium suite `worker-refusals` (dry run only); page counts
      sanitizing as busy; issues #36 (ATA security erase), #37 (Decide:
      scheduling), #38 (page UI for jobs)
- [x] v0.17.0 (e06f9df); sc-build on da27975: 154 + 9 + harness (medium's
      worker-refusals passes: 6 malformed jobs refused, no job created),
      clippy --workspace clean, `web/rebuild.sh --check` identical; #5
      closed; golden `golden-stormdrive-e388a5764082` (c7d0463), stormcos#131.
      Not run on a real drive: the disk operations wait on
      #30 (stormblock1) / #31 (NVMe)

### #45: drives and drive operations as Kubernetes objects; destructive = storage-admin (2026-10-06) — DONE (v0.18.0)

Owner (2026-10-03): "All these need crd/kubernets objects … a security model
that non admins cant format drives etc." stormcos#250 ships `storage-admin` /
`storage-viewer` over `storage.storm.io`, all resources; stormraid#8 is the
sibling (real CRDs, controller, apiserver RBAC decides).

Design (decided from the issue + owner's comment, no open decision):
- **REST gate** (`kubeauth.rs`): every call that writes to a drive — format
  (drive/shelf/batch), worker job (not a dry run) + resume, firmware update,
  destructive test, fleet join/leave (also via kube PATCH) — needs a
  Kubernetes bearer: TokenReview → SubjectAccessReview `create
  driveoperations.storage.storm.io`. 401/403/503 like stormblock#274; answers
  cached 60 s. `[api] admin_gate = "enforce"` (default) | `"audit"`. No
  apiserver configured = refused. Every decision is an audit event (who,
  what, which drives).
- **Worker re-check**: a job carries its `requester` (user, groups); before
  each destroying step the worker SARs that user again — a role revoked
  mid-batch stops the rest.
- **Owner**: `free | stormblock | stormraid | foreign` on every drive (fleet
  or a slab → stormblock; `STORMRD1` superblock → stormraid; other fs →
  foreign). Owned drives refuse destroying steps unless released (left the
  fleet / RaidSet deleted) and named in `destroy`.
- **CRDs** (`deploy/crds.yaml`, `storage.storm.io/v1`, cluster scope):
  `Drive` (this node's stormdrive writes one per drive: spec mirror + status
  with model, size, sector size, enclosure/bay, SAS address, health, owner)
  and `DriveOperation` (spec: node, select, steps, destroy, dryRun; status:
  phase, requester, job, per-drive progress). Controller (`controller.rs`,
  plain reqwest, no kube-rs) lists this node's operations, re-checks the
  requester, submits to the worker (jobs.json = survives restarts), writes
  status, posts a Kubernetes Event per operation (audit). The requester comes
  from an annotation the **apiserver** stamps (`storage.storm.io/requester`,
  like `openshift.io/requester`): rustkube issue; until then an operation
  without it is Refused, never trusted. `deploy/rbac.yaml` = the
  controller's own narrow role.
- stormcos issue: install crds/rbac, mount an apiserver credential + CA into
  stormdrive; then `component edit stormdrive` adds `[kubernetes]`.

Steps:
- [x] kubeapi.rs (client, config `[kubernetes]`, in-cluster fallback)
- [x] kubeauth.rs (TokenReview/SAR, cache) + REST gate (every write, not
      only destructive: "never operate on them") + audit
- [x] job requester + worker re-check before every step
- [x] owner (stormraid probe) on Drive, API, kube status; guard
- [x] CRDs + rbac yaml; controller (Drive mirror, DriveOperation reconcile,
      Events)
- [x] test suites adjusted (writes skip without a bearer; medium
      writes-need-storage-admin), tests/kube.rs (stub apiserver: gate
      401/403/404, controller Refused ×3 for no stamp / denied / no match,
      other node untouched, Events, audit.log)
- [x] docs (README, architecture), changelog, v0.18.0
- [x] issues: rustkube#210 (requester stamp), stormcos#302 (install +
      credential), #47 (page bearer)
- [x] sc-build on dac3deb (v0.18.0): 170 unit + kube stub + harness (medium
      writes-need-storage-admin passes) + 9, clippy -D warnings clean,
      `web/rebuild.sh --check` identical; golden
      `golden-stormdrive-e54fd49f3502`, release request stormcos#131; #45
      closed. Live use waits on stormcos#302 (CRDs + credential) and
      rustkube#210 (requester stamp) for DriveOperations; the first real
      520→4096 is #30

### #22: SAS/SATA health — LOG SENSE and ATA SMART (2026-10-07) — IN PROGRESS

SAS/SATA health was sysfs only, so such a drive never reached `failing` on
its own. Design (the issue's, no open decision):
- `smart/scsi.rs` (parsers portable, tested): LOG SENSE 0x2F (IE ASC/ASCQ →
  predicted failure, temperature), 0x0D (temperature, when 0x2F gave none),
  0x11 (SSD Percentage Used → wear_pct); vendor `ATA`: SMART READ DATA (0xD0)
  + READ THRESHOLDS (0xD1) via ATA PASS-THROUGH(16) → reallocated (5),
  pending (197), offline uncorrectable (198), temperature (194), power-on
  hours (9), SSD wear (177/231/233: 100 − normalized), predicted failure =
  a pre-fail attribute at/below its threshold (no CK_COND needed)
- `Sample.smart` / `HealthReport.smart` (`SmartCounters`); evaluate:
  predicted failure → Failing, pending / offline uncorrectable > 0 → Warning
- metrics: `stormdrive_drive_predicted_failure`, `_reallocated_sectors`,
  `_pending_sectors`, `_offline_uncorrectable_sectors`; cost: ≤ 3 commands
  a sample (SAS), 2 (SATA), under the sampler timeout
Steps:
- [x] parsers + tests · [x] Linux collect · [x] evaluate, report, metrics
- [x] docs, changelog · [ ] build VM, release, golden; live: the R230's WD

### #23: wear-out projection from the trend (2026-10-07) — DONE (v0.23.0)

The trend (`inventory.trends`: wear_pct on change or daily, 512 samples) is
recorded and never projected. Design (the issue's, no open decision):
- `src/wear.rs` (pure): least-squares slope of wear_pct over the last year
  of samples; nothing with < 2 distinct values, < 7 days, or no wear
  growth; `WearProjection {rate_pct_per_day, days_left, wear_out_unix,
  samples, span_days}`; days capped at 100 years
- kept on the drive (`Drive.wear_projection`), recomputed when a trend
  sample is recorded; a warning event when `days_left` first drops under
  `monitor.wear_out_warn_days` (180)
- on /api/v1/drives, kube Drive status, feed metric `wear-out` (warn under
  180 d), Prometheus `stormdrive_drive_wear_out_days`, the drive pane
Steps:
- [x] wear.rs + tests · [x] monitor + outputs · [x] page, docs, changelog
- [x] build VM on c1c5347 and 1fcb1ea (v0.23.0): 214 unit (4 wear) + harnesses,
      clippy clean, `web/rebuild.sh --check` 13 JS + 3 page, dist identical;
      golden `golden-stormdrive-16acd87b7979`, stormcos#313. Not on a real
      wearing SSD yet (the R230 has an HDD; a week of trend is needed)

### #31: 160-bay NVMe support verified by simulation (owner, 2026-10-07) — DONE (v0.22.1)

Owner: "160 bay, we need to simulate … The 160 bay will consume half a mil
or more." No real chassis is coming. stormcos#328 emulates drives in the
engine (ublk, which stormdrive excludes), so it cannot show stormdrive's
slots/VMD/locate/hotplug: the simulation is stormdrive's own.
- discovery `scan_in(sys, dev, mounts, cfg)` and topology `locate_in` /
  `set_locate_in(sys, …)`: the sysfs root as a parameter (the NVMe helpers
  already take one); `scan`/`locate`/`set_locate` call them with /sys, /dev
- `tests/chassis160.rs` builds a 160-bay chassis as a sysfs tree: 120 drives
  behind a PCIe switch, 40 behind VMD (domain 10000), 8 under native
  multipath (with hidden path nodes), slots 1..160 (`attention` on 150, NPEM
  LEDs on 10). Then: discovery sees exactly 160; every drive has its
  `pcie_slot` and bay = slot; locate writes that bay's LED and nothing else;
  pull one (gone, 159), push a new drive into the same slot (`replaces` the
  old one, `monitor::replaced_in_bay`)
- already covered: poller phasing at 160 (poller test), page at 160 NVMe
  rows (page test). Real hardware parts → the NetApp shelf (#30)
Steps:
- [x] sys-root refactor · [x] chassis160 test · [x] docs, changelog
- [x] build VM on 0173550 and d415504 (v0.22.1): chassis160 passes
      (discovery of 160 simulated bays in 6 ms), 210 unit + harnesses,
      clippy clean; golden `golden-stormdrive-17eb7dcf6f56`, stormcos#313

### #24: firmware redundancy gate before a data-serving drive resets (2026-10-07) — DONE (v0.22.0)

Today a fleet (or `in_use_by`) drive updates firmware one at a time behind a
node-wide lock, and nothing checks its volumes first. The engine already
answers the question: `GET /api/v1/volumes?placement=true` (#26,
`usage::volumes_on`) gives, per volume with legs on the drive, its
redundancy `health`, `rebuild` and the worst slab `state` there. No
stormblock issue needed.
- pure `firmware::redundancy_blocker(vols)`: a volume not `healthy`, a
  rebuild not `none`, a slab here not `ok` → the reason; no placement
  reported → "cannot check" (refused unless `force`)
- in `firmware::start`, under the fleet lock, before the download (mode
  0x07 activates on the last chunk): wait while blocked (phase `waiting:
  …`, every 30 s, up to `firmware.redundancy_wait_mins`, default 30) → fail
  with the reason; after a successful update, wait the same way for the
  volumes to be redundant again before the lock goes to the next drive
  (a warning event if they are not)
- `force` skips the gate; out-of-fleet drives without data have no gate
Steps:
- [x] gate + tests · [x] config, docs, changelog
- [x] **v0.22.0 (ec1a977), verified for all of #24 #44 #25 #40 #35 #42 #36 #50
      #26** on a build VM (`SC_BUILD_VM=1 sc-build`; plain sc-build still
      targets the retired dev, stormcentral#521): e743cbf — 210 unit + kube
      (incl. DrivePolicy Refused/Invalid/Active/untouched) + restart (incl.
      events across a second start) + tls + suites + 9, clippy -D warnings
      clean (after #51: two test-code lints), `web/rebuild.sh --check` 12 JS +
      3 page tests (volumes pane), dist identical. Not on real hardware:
      shelf/IOM/fault LED (#30), SATA security erase (spare drive), 160-bay
      (#31)

### #44: shelf RAID sets — bay labels and the failed member's fault LED (stormblock#252, 2026-10-07) — DONE (v0.22.0)

stormblock#252 lays a shelf out as drive-level RAID sets with spares and fails
a member when stormdrive reports its drive `failed`/`missing`.
- labels: already pushed (`shelf=<key>`, `bay=<n>`, uuid = our DriveId) on
  register and on change; docs say to name the stormblock shelf layout
  (`POST /api/v1/shelves {name}`) by stormdrive's shelf key
- **gap found:** `push_health` never reports `missing` (missing drives are
  filtered out) → a pulled fleet drive is reported `missing`, by uuid (its
  /dev path is gone); the response's `raid_member` goes into the event
- fault LED: each fleet tick `GET /api/v1/arrays` → bays of `failed`
  members (drive.uuid → our drive's shelf/bay, else wwn/serial, else the
  member's `shelf=…/bay=…` labels) → SES RQST FAULT on, off when the bay
  has no failed member; lit bays persisted (`Inventory.fault_bays`) so a
  restart can still clear them; events on/off
Steps:
- [x] report missing · [x] arrays client + pure wanted-bays · [x] SES fault
      control + tick · [x] tests, docs, changelog
- [ ] **next:** sc-build (never run: stormcentral#521); live: a NetApp shelf
      laid out as sets on stormblock1 (#30)

### #40: a test step in the drive worker (2026-10-07) — DONE (v0.22.0)

Bulk tests ran in the browser, one request per drive, 8 at a time; closing
the tab stopped the rest. Design (the issue's, no open decision):
- `Step::Test {kind: smoke|read_scan|destructive_sample}`, rank 0 (with the
  low-level steps: HBA lane, before partition/enroll); destroys only when
  destructive_sample (the destroy rules apply then)
- guard: a job whose steps are all read-only tests may run on fleet and
  `in_use_by` drives, as the single-drive route allows; a test needs a
  usable geometry unless a format comes first
- run: `drivetest::start`, progress from bytes, wait for the verdict and the
  drive back to idle; Passed → the done line; Failed/Cancelled → the drive
  job fails, its later steps do not run (the qualify gate before enroll, #34)
- CRD op `test` + `kind`; page: bulk smoke/scan = one worker job; Prepare
  gets a Test choice
Steps:
- [x] step + guard + run + tests · [x] CRD, page (bulk + Prepare) + JS tests
- [x] docs, changelog
- [ ] **next:** sc-build (never run: stormcentral#521) + web dist with #26's

### #35: shelf (IOM) firmware via SES Download Microcode (2026-10-07) — DONE (v0.22.0)

Never automatic; same image store as drive firmware. Design (the issue's;
no open decision — the image source is #29, the live shelf #30):
- `src/iomfw.rs` (pure, tested): Download Microcode Control page 0x0E
  (subenclosure, expected generation, mode 0x07 = offsets + save +
  activate, buffer offset, image length, chunk, 4-byte padded); Download
  Microcode Status page 0x0E (per subenclosure: status, additional, max
  size, expected offset); status → awaiting / updating / done (now, after
  reset, after power cycle) / failed; chunk = min(config, max size), 4-byte
- Linux: per ESP (one IOM): status idle → chunks by SEND DIAGNOSTIC →
  poll status (the IOM may reset and drop off: wait up to 10 min for it to
  answer again) → revision from sysfs `rev` (found again by SAS address)
- orchestration: one IOM at a time, the next only after the first is back
  and answering; refused when a drive on the shelf serves data (fleet or
  `in_use_by`) and has no second path (single-pathed shelf, or a drive
  with one path) unless `allow_path_loss`; one run per shelf
- `EspPath.revision` (sysfs `rev`), `POST/GET /api/v1/shelves/{key}/firmware`
  (gate: a drive operation), events, docs
Steps:
- [x] iomfw.rs pure + tests · [x] Linux run · [x] orchestration + API + gate
- [x] docs, changelog
- [ ] **next:** sc-build (never run: stormcentral#521); live: a real shelf
      (#30) and a NetApp IOM12 image (#29). Not in the page or the feed
      (an action needs an image name)

### #42: offer blank drives; enrol by policy on a node with a data slab (2026-10-07) — DONE (v0.22.0)

From stormcos#48. Auto-enrol is DrivePolicy (#50): stormcos
`docs/STORAGE-TIERS.md` names it "the answer proposed in stormdrive#45 and
#42", so there is no separate `worker.auto_enroll`. What #42 adds:
- **Offer** (default on, `worker.offer`): `Drive.enrolable` — out of fleet,
  idle, designation none, health known and not failing, usable sectors,
  ≥ `worker.offer_min_bytes` (default 1 GiB), nothing on it: no `in_use_by`
  and no `contents` (new: discovery's cached probe also runs
  `contents::holds`, filesystems/slabs/RAID on the disk or any GPT
  partition). Set each monitor tick; an `offer` event once when it turns
  true. On /api/v1/drives, kube Drive status, feed metric
- **DrivePolicy `requireDataSlab`**: act only on a node that already has a
  stormblock data slab (#42's "on a node with a data slab")
Steps:
- [x] contents in probe/Observed/Drive · [x] enrolable (pure) + tick + event
- [x] API (+ `POST /api/v1/drives/{id}/enroll`), kube, feed (metric +
      Enrol action) · [x] policy requireDataSlab · [x] tests, docs,
      changelog (1558a8b)
- [ ] **next:** sc-build (never run: stormcentral#521), then the release with
      #26/#50/#36. The page does not show `enrolable` yet (the feed does)

### #36: ATA SECURITY ERASE for SATA drives without SANITIZE (2026-10-07) — DONE (v0.22.0)

Older SATA drives have the Security feature set and not Sanitize, so the
worker's `sanitize` (SCSI SANITIZE → SAT → ATA SANITIZE) fails on them.
Design (the issue's, no open decision):
- step `security_erase {enhanced?}` (None = enhanced when supported), rank 0
  (low-level, destroys), SATA kinds only (guard)
- `erase.rs` (pure, tested): ATA PASS-THROUGH(16) CDBs (IDENTIFY 0xEC, SET
  PASSWORD 0xF1, ERASE PREPARE 0xF3, ERASE UNIT 0xF4, DISABLE PASSWORD 0xF6),
  IDENTIFY word 128 (supported/enabled/locked/frozen/expired/enhanced) +
  words 89/90 (erase time), the 512-byte password block, timeout from the
  drive's estimate
- worker: IDENTIFY → refuse not supported / frozen ("hotplug or suspend
  unfreezes it", no retry) / count expired / a password not ours; a
  one-time printable password goes into the job record (`ata_password`,
  jobs.json saved) and a warning event **before** SET PASSWORD; then
  PREPARE + ERASE UNIT; IDENTIFY after: security off → password cleared.
  Failure before ERASE UNIT → DISABLE PASSWORD. Restart mid-erase →
  interrupted with the password in the reason; resume re-checks with
  IDENTIFY and erases with the recorded password
- CRD enum, page Prepare option (rides #26's dist rebuild), docs
Steps:
- [x] erase.rs ATA + tests · [x] worker step, guard, record, recover
- [x] CRD, page form (+ JS test), docs, changelog (b7c567a)
- [ ] **next:** sc-build (never run: dev.g8.lo gone, stormcentral#521), web
      dist with #26's, v0.22.0, golden. Live: a spare SATA drive with the
      Security feature set and no Sanitize (the issue: "test on a spare
      SATA drive first") — none here yet

### #50: DrivePolicy — per-node tier, reformat + enrol by policy (stormcos#251, 2026-10-07) — DONE (v0.22.0)

stormblock1 (R230 + 2.5" NetApp shelf) → tier `warm`; stormblock2 (3.5"
Dell) → `cool` (stormcos `docs/STORAGE-TIERS.md`). `tier_map` is per kind
and the config is shared, and auto_add skips 520-byte drives. Design (from
the issue + #45/#42, no open decision; rustkube#210 stamp is shipped):
- CRD `DrivePolicy` (`storage.storm.io/v1`, cluster scope): spec `nodes`
  and/or `nodeSelector.matchLabels` (one required), `drives` {kinds,
  minBytes, maxBytes, blockSizes, model, shelf, bays}, `reformat` (512|4096,
  only on a drive that needs it), `enroll` {role (data), tier (hot|warm|
  cool|cold, required)}, `suspend`, `dryRun`
- `src/policy.rs` (pure): spec parse/validate, node match, drive match,
  verdict per drive (skip + reason, or steps: [format?] partition enroll);
  never fleet/missing/busy/designated (reserved, spare, failed)/in_use_by/
  failing; a drive whose policy job failed is not retried until the policy's
  generation changes
- controller: list policies (404 = CRD absent, quiet); stamped requester
  re-checked (`create driveoperations`, what the worker re-checks per step);
  contents guard (holds a slab/fs → skipped, re-probed every 5 min);
  one worker job per distinct step list, tagged `policy`; interrupted jobs
  resumed once the requester re-checks; deleted policy → queued steps
  cancelled; status per node (`status.nodes.<node>`: phase, requester,
  drives{id: state, job, reason}); Events on the policy (Accepted, Enrolled,
  Failed, Refused)
- deploy/crds.yaml + rbac (drivepolicies get/list/watch, status patch;
  nodes get for nodeSelector)
Steps:
- [x] policy.rs + tests · [x] worker `policy` tag, active set · [x] controller
- [x] CRD + rbac + example · [x] kube harness (Refused unstamped / bob,
      Invalid, other node + unmatched labels untouched, alice Active)
- [x] docs, changelog (ab35310, 21a0e13)
- [ ] **next:** sc-build has never run on it — dev.g8.lo is gone
      (stormcentral#521). When it works: `cargo test --workspace` + clippy,
      fix what fails, then v0.22.0 (with #26's web/dist), golden, close #50.
      Not on a real shelf: the format/partition/enroll steps wait on #30

### #26: the volumes on each drive, from the engine's placement (stormconsole#29, 2026-10-06) — DONE (v0.22.0)

The console reads every node's stormdrive but only its own node's engine;
stormdrive already reads its engine with the node token. stormblock v17.1
`GET /api/v1/volumes?placement=true` → `{items:[{id, name, kind, consumer?,
placement:{drives:[{drive, node, slabs, legs, bytes}], slabs:[{id, drive,
state, legs, shared_legs, bytes, drain?}], legs:{policy, health, …},
rebuild}}]}` (stormblock `src/mgmt/api/placement.rs`). Reduction mirrors
stormconsole `crates/plugins/stormblock/src/placement.rs`.
- [x] `usage.volumes: [{id, name, kind, consumer?, bytes, legs, shared_legs,
      state (worst slab state here), rebuild, policy, health}]`, largest
      first; joined like slabs (WWN → serial → path), fabric drives
      (`scheme://`) skipped; **absent** when no volume carries placement
      (engine before v17.1), `[]` when the engine has no volumes
- [x] fetched on the slab-listing tick (own 30 s timeout: it walks every
      extent map); a failed read keeps the last answer and its time
      (`volumes_collected_at`)
- [x] feed metric `volumes`; page: the drive pane lists them
- [x] code + tests + docs pushed (c73abfc); sc-build on 35983cb
      (2026-10-07): 184 unit (5 new in usage) + harness + kube + restart +
      tls + 9, clippy -D warnings clean. The web half of that job died in
      npm itself ("Exit handler never called!"); then dev.g8.lo went away
      (retired, stormcentral#517/#521)
- [ ] **next:** once sc-build works again (stormcentral#521): `web/rebuild.sh`
      (JS + page tests incl. the new volumes pane), commit web/dist, v0.22.0,
      golden, close #26
- [ ] tests (reduction, absent vs empty, worst state, carry-over), docs
      (README, architecture "Per-drive usage"), changelog, v0.22.0, golden

### #46: slab format + drive close need the admin token or a storage-admin bearer (stormblock#274, 2026-10-06) — DONE (v0.21.1)

stormblock#274: `POST /api/v1/slabs` and `DELETE /api/v1/drives/{id}` are
destructive on the engine; the node token gets 401 under `admin_gate =
enforce`. `DELETE …/drain` stays ordinary (we sent it as admin).
- [x] engine client: destructive calls try, in order, the engine admin token
      (`stormblock.admin_token` / `$STORMBLOCK_ADMIN_TOKEN`, then the file
      `stormblock.admin_token_file` / `$STORMBLOCK_ADMIN_TOKEN_FILE` /
      `/run/stormblock-admin/admin_token`, re-read each call), then
      stormdrive's own Kubernetes credential (`[kubernetes]` token file /
      service account), then the node token (an `audit` engine); next on
      401/403; a final refusal logs what is needed
- [x] format_slab destructive; cancel_drain ordinary
- [x] deploy/rbac.yaml: `slabs` create for stormdrive's SA (drives delete is
      already in stormdrive-controller)
- [x] tests (stand-in engine: admin token / SA bearer / node token order,
      drain cancel on the node token), docs, changelog, v0.21.1, golden
- [x] sc-build on 696ad82 (v0.21.1): 179 unit (stormblock
      `destructive_verbs_present_the_admin_token_then_the_kube_bearer_then_the_node_token`)
      + harness + kube + restart + tls + 9, clippy -D warnings clean; web
      untouched; golden `golden-stormdrive-b773481077ba`, release request
      stormcos#313; stormcos#302 told (rbac.yaml's stormdrive-engine, or mount
      /run/stormblock-admin). Not run against a live enforcing engine

### #19: :9092 is TLS and nothing answers anonymously but health (P2, stormcos#81, 2026-10-06) — DONE (v0.21.0)

Owner's rule (stormcos SECURITY.md, 2026-09-25): every listening API on a
node is TLS with a stormcert pair, every caller authenticates (client
certificate or token), nothing anonymous but health. Pattern: vmimages#16 /
stormcluster#5 (one port, TLS told from plain by the first byte).
- `src/tls.rs`: one listener; a TLS handshake gets TLS from
  `[api] tls_cert_file`/`tls_key_file` (default `/data/stormcert/stormdrive.{crt,key}`,
  re-read on change), client certificates requested and verified against
  `client_ca_files` (default `/data/stormcert/ca.crt`), not required; plain
  HTTP answers health only (`/api/v1/health`, `/healthz`: stormd's probe)
- reads: admin token, a node-CA client certificate, or a Kubernetes bearer
  allowed `get` on `storage.storm.io` (storage-viewer); else 401/403/503.
  Writes as #45, plus a client certificate (CN user, O groups → SAR)
- the page shell without a credential answers 401 with the page (it signs
  in); assets are code only and open; the page sends its bearer on reads
- `[api] allow_anonymous` (transition, default off): plain HTTP and
  credential-less reads as before; a credential that is sent is checked;
  writes keep the #45 gate. The golden's registry config sets it until
  stormcos mints/mounts the pair and the callers (console, ironprom,
  stormlb route, rustkube-node placement) present one → issues there
- test container: https by default, `STORM_STORMDRIVE_CA`, client pair,
  the pod's service-account token; harness over TLS (rcgen)
Steps:
- [x] plan · [x] tls.rs + config · [x] read gate + cert identity · [x] page
- [x] tests (tls.rs: plain health only/403 tls_required, anonymous 401 +
      401 page shell, admin token, alice viewer reads/403 write, bob 403,
      cert reads, cert write by SAR + audit, foreign CA refused, server
      checked, pair appearing later served, allow_anonymous)
- [x] test container TLS (medium `reads-need-a-credential`), harness over
      TLS · [x] docs, changelog, v0.21.0
- [x] sc-build on 4443927 (v0.21.0): 179 unit + kube + restart + suites over
      TLS + tls 3 + 9, clippy -D warnings clean; `web/rebuild.sh --check`:
      9 JS + 3 page tests (new signin), dist identical
- [x] registry config `[api] allow_anonymous = true` (transition, like
      vmimages/stormcos#320); stormcos#352 (pair, mount, ironprom, route,
      check-metrics, test SA); stormconsole#49 + rustkube-node#60 commented.
      Drop the line once stormcos#352 + stormconsole#49 ship
- [x] golden `golden-stormdrive-99a433d7a82c`, release request stormcos#313;
      #19 closed

### #38 (+ #47): the page submits and follows drive worker jobs (2026-10-06) — DONE (v0.20.0)

Every write since #45 needs a storage-admin bearer, so jobs on the page need
#47's sign-in first (same change).
- [x] `lib/api.js`: bearer from `sessionStorage` (`Authorization: Bearer` on
      every request but plain GETs); errors keep `status` + envelope `code`;
      401/403 read "needs storage-admin"
- [x] header: Sign in (paste `oc whoami -t`) / Sign out; `writes.gate` from
      health; while it enforces and no bearer: write controls disabled
      (`<fieldset disabled>`, row actions off), "needs storage-admin"
- [x] `lib/worker.js` (pure, tested): steps from the form, request, which
      refusals need `destroy`, serial match, job summary, cancel/resume
- [x] `Prepare.svelte` (side pane): steps (format 4096/512, sanitize
      block/crypto/overwrite, partition, enroll + tier, role) → dry run
      (runnable / refused + reasons) → type each held drive's serial →
      submit; from the bulk bar (drives), the shelf pane (shelf), a drive
- [x] the page's formats (bulk, shelf, drive) go through Prepare (worker:
      restart-safe, destroy guard); `/api/v1/format` stays for API users
- [x] `Jobs.svelte`: jobs, per-drive state/phase/%/error, cancel, resume
- [x] state column: `prep` (unusable / ready, worker progress)
- [x] page test: sign-in header, prepare dry run → destroy → submit, jobs;
      `web/rebuild.sh`, docs, v0.20.0, golden; close #38 and #47
- [x] sc-build on 016f478 (v0.20.0): 176 unit + harness (short `page`
      passes) + kube + restart + 9, clippy clean; `web/rebuild.sh --check`:
      9 JS unit + 2 page tests (212-drive grid; sign in → shelf → Format →
      4096… → dry run → serial → run with bearer + destroy → Jobs, cancel),
      dist identical; the page's request bodies POSTed to the real daemon:
      all parse and validate (selection errors only), a real submit
      without a bearer 401 `unauthorized`; golden
      `golden-stormdrive-ea34a213c27a`, stormcos#313. Not seen in a real
      browser on a node yet

### #43: drain docs vs the engine; drains that never get tracked (2026-10-06) — DONE (v0.19.1)

stormcos#65 consistency pass: docs say `push_health` sends `drain:false` so
"the engine never decides" and `drain_on_failing=false` stops the drain.
Truth (stormblock `drives.rs` health handler): `failed`/`missing` drain
whatever `drain` says; degraded/failing/failed auto-rebuild (`[rebuild]
automatic`, default on); with a rebuild running the drain waits for it and
`POST …/drain` answers 409. Found behind it, in the code:
- a 409 (rebuild running) on our drain start is never retried (health is
  pushed on change only) → the engine's own drain after the rebuild is
  never adopted → the drive is never retired (no leave, LED, "safe to pull")
- `poll_drains` "resumed" after the engine forgot a drain calls
  `start_drain`, which returns early on the `running` record → never resumed
Plan:
- [x] drain state `pending` (wanted, not started): set when a start fails;
      every fleet tick retries pending drains (`start_drain` adopts an
      engine drain already running); health back to healthy clears it
- [x] "resumed" marks the record pending instead
- [x] docs: architecture (health push, drain, migration flow), README
      `drain_on_failing`/`push_health`, example toml, config comment,
      fleet.rs comment; presentation (placement consumer, status slide)
- [x] tests (pure `drain_due`), sc-build, v0.19.1, golden
- [x] sc-build on 17cd78e (v0.19.1): 176 unit (fleet `a_refused_drain_stays_wanted`)
      + harness + kube + restart + 9, clippy clean, web dist identical;
      golden `golden-stormdrive-12ef14fdf9d0`, stormcos#313. The retry
      loop against a live engine (409 during a rebuild, then adopt) is not
      exercised: the harness has no fleet drives (#30)

### #18: serve /metrics (P1, stormcos#64, 2026-10-06) — DONE (v0.19.0)

`GET /metrics` on :9092, Prometheus text, unauthenticated (a read). Upstream
`smartctl_exporter` names where they fit, `stormdrive_*` otherwise. Every
drive series carries `device`, `serial`, `model`, `enclosure`, `bay`.
stormcos `deploy/test/check-metrics.sh` probes it (200 + one sample).
- [x] NVMe log 0x02: also spare threshold, data units read/written, power
      cycles, unsafe shutdowns, error-log entries (`HealthReport.nvme`)
- [x] `metrics.rs` (pure render from cached state): smartctl_device_*,
      `stormdrive_drive_*` (info, health_status, io_errors_total = SCSI
      ioerr_cnt, last poll, usage), enclosure SES sensors, poller, build info
- [x] route; unit tests (format validity, labels, escaping, missing and
      never-polled drives, shelf); short suite `metrics`; docs; v0.19.0
- [x] sc-build on 51efbe8: 175 unit + harness (short `metrics` passes) +
      kube + restart + 9, clippy clean, web dist identical; release binary
      on dev: 1,159 samples, 0 lines failing check-metrics.sh's regex;
      golden `golden-stormdrive-a59663904c60`, stormcos#313
- HDD reallocated/pending sectors need SCSI log sense / ATA SMART (#22):
  emitted when #22 collects them, not before
- Found on dev: NVMe-oF namespaces (stormblock exports) discovered as
  drives → #49

### #39: a restart leaves drives stuck formatting/testing/updating_firmware (P1, 2026-10-06) — DONE (v0.18.1)

`activity` is persisted; only worker jobs recovered (`worker::recover`). A
legacy format (`format::start`), a test (`drivetest`) or a firmware update
(`firmware`) in flight at a restart left the drive busy forever.
- [x] `worker::orphan_action` (pure): Formatting/Sanitizing owned by a
      running worker job → leave; a SCSI legacy format (record `running`) →
      re-attach; any other Testing/Formatting/Sanitizing/UpdatingFirmware →
      Idle, record `interrupted`, `restart` warning event. Draining/Missing
      untouched
- [x] `format::reattach`: TUR until ready, rescan, verify the block size
      against the record, `finish` shared with a normal run
- [x] recover awaited in main before the monitor and the API
- [x] tests: unit (every activity) + `tests/restart.rs` (real daemon on a
      seeded inventory.json: none busy after, records + events)
- [x] sc-build on 24de9cb (v0.18.1): 171 unit + restart + kube + suites +
      9, clippy -D warnings clean, `web/rebuild.sh --check` identical;
      golden `golden-stormdrive-ecd23cdbb356`, release request stormcos#313.
      Not exercised on a real drive mid-format: that is #30

### Comment mining (2026-09-28)

Findings in issue comments since 2026-09-18 that nobody had filed:
stormipmi#21 (BIOS firmware via Redfish — owner gave BIOS to stormipmi),
#29 Decide: firmware image source, #30 Decide: stormblock1 NetApp live pass,
#31 verify on the first 160-bay chassis; live checks added to #28;
stormblock1 proposed as a test machine on stormcentral#83.
Second pass (comments since 2026-09-25): #39 (P1) a restart leaves a drive
stuck `formatting`/`testing`/`updating_firmware` when the op was not a worker
job; #40 worker test step (bulk test on the server); drive-worker live checks
added to #30 (SAS) and #31 (NVMe).

### #11: short / medium / long test containers (2026-09-28) — DONE

Per stormcentral `docs/test-standard.md`: `test/Containerfile` (FROM
scratch, static `/test`), `test/build.sh` (musl build on the build box),
`/test short|medium|long`, JSON lines + summary, exit 0/1/2. Test machine
today: C2NR0Q2 (R230, 192.168.30.2); stormcentral writes the Job itself.
Pattern: stormstorage's `test/` (workspace member, same deps).

**Safety first — these run on real machines with real drives:** nothing
destructive, ever. No join/leave, format, firmware, destructive test, forget,
drain or `failed` designation. Writes are limited to reversible,
restored-on-exit ones (designation spare↔original on an out-of-fleet drive,
overcommit round-trip, smoke/read-scan+cancel which only read), plus
requests the server must *refuse* (checked before anything starts).
- [x] crate `test/` (workspace member): env, report, api client, suites
- [x] short: health/version, drives (stable ids, verdicts), summary,
      placement + 304, components, kube list, events, monitor, page
- [x] medium: error envelope/404/400s, refusals (409) on guarded drives,
      resolve by wwn/serial/path, designation + overcommit round-trips with
      events, smoke test to a verdict, read-scan cancel, kube watch, usage
      from stormblock on slab-holding drives, shelf/NVMe checks (skip
      without: requires sas-shelf / nvme)
- [x] long: waves until the window ends — API readers sized from the drive
      count + a smoke test on every idle usable drive; per-wave latency,
      monitor sample cost, stuck/timeouts, drives left busy, event growth;
      a wave slower than 2× the first or residue growing = fail
- [x] harness: cargo test in the workspace runs the real daemon (no drives,
      stormblock off) and all three suites against it (sc-build)
- [x] Containerfile, build.sh, stormdrive-test.yaml (metadata), docs
- [x] Verified on dev (a5894a0): harness short 9 pass/2 skip, medium 6/12,
      long 3/0 against the real daemon (no drives there, so the drive
      checks skip); 9 unit tests in test/ (pick safety rules, wave judge);
      `test/build.sh` builds a 3.4 MB FROM-scratch static image; bogus
      suite / unreachable node exit 2
- [x] Live: runs 06827cdeed (short) and 68f6fbbf00 (medium) on C2NR0Q2
      errored before reaching stormdrive — the apiserver never answered
      /readyz (stormcentral#63, stormcos#165). First live run is **#28**,
      proposed --after stormcentral#63
- [x] clippy --workspace -D warnings + sc-build pass on 6e34251 (133 + 2
      harness + 9); #27 (clippy build-failure) closed; #11 closed; golden
      `golden-stormdrive-452835d3e854` (7661bc8), stormcos#131

### #8: a presentation of its purpose and functionality (2026-09-28) — DONE

`docs/presentation.md`, Marp, 8–15 slides, every claim from the code (v0.16.0)
and the #7 docs. Relationships from `stormcentral check`: stormdrive
(storage) → stormblock, stormview, stormd; depended on by stormcos,
stormstorage; consumers checked in code: stormconsole's drive plugin
(every node's :9092), rustkube-node#60 (placement).
- [x] deck written (12 slides); marp-cli 4 renders it on dev (sc-build on
      fef1a7c): 12 sections, dense class on the 6 long slides
- [x] README links it; changelog; #8 closed; golden requested on 4561605:
      unchanged (docs only), stays `golden-stormdrive-406e52043b5f`

### #6: ready for many more drives (2026-09-28) — DONE (v0.16.0)

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
  - [x] v0.16.0 (dc0195c): sc-build 133/133, clippy, `web/rebuild.sh
        --check` (npm ci, 5 + 1 JS tests, dist identical); #6 closed;
        golden `golden-stormdrive-406e52043b5f` (e95ef48), release request
        stormcos#131. Not yet seen in a real browser on a node: rides
        the release. Bulk test/join + per-domain sequencing stay on #5,
        metrics on #18
- [x] Metrics per drive — #18 (v0.19.0)

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
- [x] SCSI/SATA: SG_IO log pages (0x2F, 0x0D, 0x11) + ATA SMART — #22
- [x] Threshold engine → health state machine, hysteresis
- [x] Wear trending: persisted samples (on change or daily)
- [x] Wear-out projection — #23
- [x] Event ring + `GET /api/v1/events` (newest 512 persisted, #25 —
      written, unbuilt)
- [x] Prometheus `/metrics` (#18, v0.19.0)
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
- [x] Redundancy check via stormblock before a fleet drive resets (no
      rebuild in flight, volume not already degraded) — #24
- [ ] Shelf (IOM) firmware via SES download microcode page 0x0E — #35 (written, unbuilt)

### Phase 6: Thermal management
- [x] Per-drive + per-enclosure thermal view (health temps, SES shelf panel)
- [x] Threshold alerts (warn/critical from config → warning + event)
- [ ] SES fan/cooling element control (SG_IO SES-2 control page) — actuation,
      gated behind explicit config (scope: #32)

### Phase 8: Drive crypto (pending — scope to be confirmed with Glenn: #33)

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
  throttling — filed as #32 (Decide).
- **Qualification/burn-in** for new drives before handing to stormblock —
  filed as #34 (Decide).
- **Drive crypto** scope (Phase 8) — filed as #33 (Decide).
- **Firmware image source** — #29 (Decide); the stormblock1 live pass — #30.
- ~~SAS2/SAS3 controller specifics~~: resolved in the code — `hba.rs`
  inventories every PCIe SCSI HBA whatever the driver, and the mpt3sas
  sysfs paths (bay_identifier, enclosure_identifier, version_fw) are used
  where present.

## Rules recap
- Conventional commits, changelog on every change, docs ship with code.
- Commit early/often, push after every logical unit. No claude attribution.
- Check `gh issue list --state open` at session start.
- Bugs found in stormblock/stormd → file issues there, don't fix here.
