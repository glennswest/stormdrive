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

**Version: 0.1.0** — version locations: `Cargo.toml`, this file.

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
commit  →  push  →  ssh root@dev.g8.lo 'cd /root/stormdrive && git pull && cargo test'
```

**The GitHub repo is currently private**, and dev has no key for private
repos, so dev pulls from a bare repo on dev itself instead: the local
checkout has a second remote `dev` (`ssh://root@dev.g8.lo/root/git/stormdrive.git`)
and `/root/stormdrive` on dev has that bare repo as `origin`. Push to
**both** `origin` and `dev` on every push. If the repo is made public
(matching stormblock), repoint dev's checkout at GitHub over https and drop
the bare repo.

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
  stormblock.rs   client for stormblock :9090 (add drive, format slab, drain)
  api/            axum REST :9092  (/api/v1/*, /api/v1/summary for stormd)
```

**Ports:** stormdrive listens on **:9092** (stormblock has :9090, stormd
:9080).

## Integration contracts

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

### Phase 4: StormBlock integration
- [ ] Client: list/add/remove drives, format slab, SMART passthrough compare
- [ ] Auto-add policy (off by default): qualified drive → POST drives →
      POST slabs with tier derived from kind
- [ ] Failure flow: Failing drive → Draining → (stormblock drain API when it
      exists) → Retired; until then: event + summary card goes warn/error
- [ ] Reconcile loop: our inventory vs stormblock's drive list

### Phase 5: Firmware management
- [ ] Firmware inventory per drive (already collected in Phase 1)
- [ ] Image store (<data_dir>/firmware) + policy file
- [ ] NVMe: Firmware Image Download (0x11) + Commit (0x10)
- [ ] SCSI: WRITE BUFFER mode 0x07/0x0E
- [ ] Sequenced rollout through sequence.rs: one drive at a time,
      health-gated, redundancy-checked via stormblock

### Phase 6: Thermal management
- [ ] Per-drive + per-enclosure thermal view
- [ ] Threshold alerts (warn/critical from config)
- [ ] SES fan/cooling element control (SG_IO SES-2 control page) — actuation,
      gated behind explicit config

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
