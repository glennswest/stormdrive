# Changelog

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

## [Unreleased]
<!-- New unreleased changes go here -->
