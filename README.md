# StormDrive

**Physical drive management for the Storm ecosystem.**

StormDrive is the layer below StormBlock. StormBlock turns drives into
network volumes; StormDrive curates the drives themselves:

- **Discovery** — sysfs enumeration + hotplug of NVMe, SAS2/3, and SATA
  drives; stable identity that survives reboots and path changes
- **Monitoring** — periodic SMART/health collection (NVMe log pages, SCSI
  log sense), failure prediction, event stream
- **Wear** — endurance tracking and projected-life trending for SSDs
- **Thermal** — per-drive and per-enclosure temperature watch and alerts
- **Firmware** — inventory now; sequenced, health-gated updates later
- **Location** — enclosure/bay mapping (SES), PCIe slot, SAS address,
  locate-LED control; failure-domain labels for placement
- **Fleet lifecycle** — discovery finds drives; the UI moves them into the
  **fleet** (stormblock registration + slab format). Orthogonally, drives
  can be **tested** (smoke / full read scan / destructive write-verify —
  destructive only out of fleet) and designated **reserved**, **spare**, or
  **failed**, both in fleet and out
- **Sequencing** — one disruptive operation at a time, pre/post health
  checks
- **StormBlock hand-off** — registers qualified drives (add + slab format
  with derived tier) and drives the drain/retire flow when one is failing

One daemon per storage node, REST API on **:9092**, plus a
[stormd](../stormd) newer-UI extension (dashboard card via `summary`, full
page via `proxy`).

> **Build on `root@dev.g8.lo`, never on a Mac.** The whole drive path —
> sysfs, admin ioctls, SG_IO, netlink uevents — is behind
> `cfg(target_os = "linux")`. Commit, push, pull on dev, build there.

## Quick start

```bash
# on a storage node
stormdrive --config /etc/stormdrive/stormdrive.toml

curl -s http://localhost:9092/api/v1/drives | python3 -m json.tool
curl -s http://localhost:9092/api/v1/summary
curl -s -X POST http://localhost:9092/api/v1/drives/<id>/locate -d '{"on":true}'
```

## Documentation

- [docs/architecture.md](docs/architecture.md) — full design: drive model,
  subsystems, API, stormblock/stormd integration, migration flow
- [docs/stormblock-review.md](docs/stormblock-review.md) — the stormblock
  review this design is built on
- [CLAUDE.md](CLAUDE.md) — work plan and project rules

## Status

v0.1.0 — bootstrap. Design complete; Phase 1 (discovery + inventory) in
progress. See the work plan in [CLAUDE.md](CLAUDE.md).
