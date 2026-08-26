# Changelog

## [Unreleased]

### 2026-08-26
- **fix:** first health sample no longer raises a false "media errors
  growing" warning (baseline requires a real prior sample)
- **fix:** nvme-style device names only match 'p'-separated partitions in
  the mounted-drive guard (nvme0n10 is not a partition of nvme0n1)

### 2026-08-26 (bootstrap)
- **feat:** Phase 1 scaffold: stable `DriveId` (uuid5 of WWID/model+serial),
  persistent inventory (atomic writes, wear-trend ring), sysfs discovery
  with exclusion policy + mounted-drive guard, NVMe SMART collector
  (admin ioctl, log page 0x02), SCSI/SATA sysfs collector, threshold engine
  with hysteresis, event ring, topology resolution (PCIe BDF/slot, SAS
  address, SES enclosure/bay) with locate-LED control, axum REST API on
  :9092 incl. stormd summary card, stormblock client with Active-state
  reconcile; deploy files (example config, systemd unit, stormd UI snippet)
- **docs:** StormBlock deep review captured in `docs/stormblock-review.md`
  (drive model, add/remove paths, health/failure gaps, topology gap, mgmt
  surface, stormd UI extension contract)
- **docs:** Architecture design v1 in `docs/architecture.md` (drive model,
  discovery/monitor/topology/firmware/thermal/sequencer subsystems, REST API
  on :9092, stormd summary card, stormblock integration + migration flow)
- **chore:** Project bootstrap — CLAUDE.md work plan, README, changelog,
  .gitignore
