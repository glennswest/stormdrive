# Changelog

## [Unreleased]

### 2026-08-28
- **feat:** Kubernetes-shaped resources served by stormdrive (stormblock#80):
  `/apis/storage.storm.io/v1/drives` and `…/enclosures` in the
  `apiVersion/kind/metadata/spec/status` shape, API discovery at `/apis`
  and `/apis/storage.storm.io/v1`, `labelSelector`, `?watch=1` as a
  newline-delimited event stream. `Drive.spec` writes — `designation`,
  `fleet` (`fleet`/`out`; out always drains first), `drain`, `locate` —
  map onto the existing verbs. Labelled `storm.io/component=stormdrive`
  so they sit beside stormblock's `Volume`/`Slab`/`Node`/`Drive` in one
  group.

## [v0.5.0] — 2026-08-28

### 2026-08-28
- **feat:** Close the stormblock loop (stormblock#70/#71, engine v11).
  `stormblock.rs` registers drives with location `labels` and the stable
  `uuid`, relabels (`PUT /drives/{id}/labels`), lists slabs by drive,
  reports health (`POST /drives/{id}/health`) and starts/polls/cancels
  drains. New `fleet.rs` runs after every monitor tick: labels pushed on
  change, Failing/Failed pushed to the engine (which quarantines the slabs
  and distrusts the legs), a fleet drive that fails is drained and — when
  stormblock reports it empty — leaves the fleet with its locate LED on;
  auto-add (`stormblock.auto_add`) registers qualified drives with labels
  and a slab.
- **feat:** `POST/GET/DELETE /api/v1/drives/{id}/drain` (`?leave=true`
  retires when empty); `fleet leave` takes `"drain": true`; the leave guard
  uses the engine's slab-by-drive listing instead of path matching;
  designating a fleet drive Failed reports it and starts its drain.
- **feat:** `Drive` records `pushed_labels`, `pushed_health` and `drain`
  (state, moved/failed/remaining, reason, then_leave), persisted.
- **feat:** Config `stormblock.auto_format_slab`, `push_health`,
  `drain_on_failing` (all default true; `auto_add` stays off).
<!-- New unreleased changes go here -->

## [v0.3.0] — 2026-08-26

### Added
- Shelf-rig topology (NetApp DS-series target): `Location` is now
  controller → shelf → bay, with shelf identity read from the SES
  processor's SCSI device (vendor/model + serial via VPD page 0x80 — the
  canonical key, since a dual-IOM shelf is two enclosure devices with one
  serial); controllers carry scsi_host/PCIe BDF/driver
- Multipath awareness: observations grouped by WWID-derived DriveId — one
  Drive with a `paths` list and a stable sorted-first primary, instead of
  a path that flaps between IOMs every scan; location re-resolved on path
  changes
- `GET /api/v1/topology` — the controller → shelf → drive tree, shelves
  deduplicated by serial
- UI shows shelf model/serial + bay in the location column and a
  path-count badge on multipath drives

### Documentation
- Work plan: pending drive-crypto phase (SED/OPAL, crypto erase — scope to
  confirm); stormblock#71 filed (shelf/controller failure-domain-aware
  slab placement)

## [v0.2.0] — 2026-08-26

### Added
- Fleet membership / designation / activity model replaces the single
  DriveState — a drive is `out|fleet` (stormblock membership), carries an
  operator designation (`none|reserved|spare|failed`, valid both in fleet
  and out), and has an activity (`idle|testing|draining|missing`)
- Fleet actions: `POST /api/v1/drives/{id}/fleet` join (stormblock add +
  optional explicit slab format with derived tier) / leave (guarded by a
  best-effort slab check until stormblock#70; `force` override)
- Drive testing engine: smoke (sampled reads), read_scan (full sequential
  read with progress + cancel, maps past bad regions), destructive_sample
  (write/verify via O_DIRECT read-back; refused in-fleet or mounted);
  verdict events, one test per drive
- Embedded web UI at `/` (vanilla JS, stormd style tokens, proxy-prefix
  aware) — drive table with join/leave, designation dropdown, test buttons
  with progress, locate LED, event feed
- `POST /api/v1/drives/{id}/designation`; fleet reconcile now two-way
  (stormblock listing ↔ membership)

### Breaking
- `POST /api/v1/drives/{id}/state` removed (replaced by fleet/designation
  endpoints); persisted inventories from 0.1.0 load but lifecycle fields
  reset to defaults

### Changed
- Repo made public (matching stormblock); dev.g8.lo pulls directly from
  GitHub over https, bare-repo workaround removed
- Cargo.lock committed (binary crate convention)

## [v0.1.0] — 2026-08-26

### Added
- Phase 1 scaffold: stable `DriveId` (uuid5 of WWID/model+serial),
  persistent inventory (atomic writes, wear-trend ring), sysfs discovery
  with exclusion policy + mounted-drive guard, NVMe SMART collector
  (admin ioctl, log page 0x02), SCSI/SATA sysfs collector, threshold engine
  with hysteresis, event ring, topology resolution (PCIe BDF/slot, SAS
  address, SES enclosure/bay) with locate-LED control, axum REST API on
  :9092 incl. stormd summary card, stormblock client with Active-state
  reconcile
- Deploy files: example config, systemd unit, stormd `[process.ui]` snippet

### Fixed
- First health sample no longer raises a false "media errors growing"
  warning (baseline requires a real prior sample)
- nvme-style device names only match 'p'-separated partitions in the
  mounted-drive guard (nvme0n10 is not a partition of nvme0n1)

### Documentation
- StormBlock deep review captured in `docs/stormblock-review.md` (drive
  model, add/remove paths, health/failure gaps, topology gap, mgmt surface,
  stormd UI extension contract)
- Architecture design v1 in `docs/architecture.md` (drive model,
  discovery/monitor/topology/firmware/thermal/sequencer subsystems, REST
  API on :9092, stormd summary card, stormblock integration + migration
  flow)
- Project bootstrap: CLAUDE.md work plan, README

## [v0.4.0] — 2026-08-26

### Added
- stormview integration — `GET /api/v1/components` + `/ws/components`
  (full-snapshot pushes) serving drives and shelves as ComponentSummary
  entries with relations (shelf has_many drives) and real actions;
  parameter-less action routes for renderers (`/locate/{on|off}`,
  `/fleet/{join|leave}`, `/designation/{value}`, `/test/{kind}`) so
  stormd/stormsh/stormconsole buttons make things happen

### Documentation
- Position in the distributed hierarchy: physical chain
  site ⊃ building ⊃ floor/room ⊃ row ⊃ rack ⊃ node ⊃ hba ⊃ shelf ⊃ bay;
  logical overlay is multicluster-of-multiclusters with tiering across
  clusters; stormblock is an execution engine, the cross-node control
  plane is stormstorage (live at github.com/glennswest/stormstorage);
  shared label vocabulary pinned; testbed recorded (2.5" high / 3.5"
  medium / PVE backup); stormblock#72 filed, amended, and re-scoped to
  engine primitives

