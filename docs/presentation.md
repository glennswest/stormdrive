---
marp: true
theme: default
paginate: true
title: stormdrive
description: Physical drive management for a storage node — purpose and functionality
---

<!--
Render: npx @marp-team/marp-cli docs/presentation.md          (HTML)
        npx @marp-team/marp-cli --pdf docs/presentation.md    (PDF)
Every claim here is checkable against the code as of v0.16.0 (src/, web/)
and the docs rewritten from it (#7). README.md is the full reference;
docs/architecture.md says how each part works.
-->

<style>
section { font-size: 24px; }
table { font-size: 19px; }
pre { font-size: 16px; }
section.dense { font-size: 19px; }
section.dense li { margin: 0; }
</style>

# stormdrive

**Knows what a node's drives are, and hands them to stormblock**

Physical drive management for the Storm ecosystem: one Rust daemon per
storage node, REST + page + feed on **:9092**

v0.16.0 · github.com/glennswest/stormdrive

---

## What it is, and the problem it solves

stormblock turns drives into slabs and volumes, but on its own it didn't know
its drives. It had no discovery, no health polling, no failure detection, no
bay and no stable identity (docs/stormblock-review.md, 2026-08-26).

**stormdrive is the curator below it.** For every drive on the node it knows:

- **where** it is: HBA → shelf → bay, or PCIe slot;
- **what** it is: WWID-stable id, model, firmware, sector size;
- **how** it is: health, wear, temperature;
- **whose data** is on it.

It hands good drives to stormblock and tells stormblock when to move data
off a failing one.

Three layers, no overlap: **stormdrive** = hardware truth below the node ·
**stormblock** = per-node execution · **stormstorage** = fleet policy.

---

## Where it sits in stormcos

Group **storage** (stormcentral's relationships graph).

| Direction | Component | How, in the code |
|---|---|---|
| depends on | **stormblock** | engine API `:9090` (Bearer token): register drives, labels, slabs, health, drain, overcommit |
| depends on | **stormview** | crate: the components feed (`/api/v1/components`); npm: the page's DataGrid |
| depends on | **stormd** | the golden is a stormd container; `[process.ui]` card for non-stormcos installs |
| used by | **stormcos** | ships the golden and starts it on every node profile |
| used by | **stormstorage** | fleet policy; names drives by the identity stormdrive uses (via stormblock's `DriveRef`), and stormdrive's labels reach it through stormblock |

Also read by **stormconsole**'s drive plugin (every node's `:9092`) and
by rustkube-node's PV placement mirror (`/api/v1/placement`, rustkube-node#60).

---

## How it works

```
 kernel: sysfs · uevents · SG_IO · NVMe admin ioctl
     │
     ├─ discovery (+hotplug) ──┐   SES shelves · HBAs · PCIe slots
     ├─ health poller ─────────┤   phased, ≤8 in flight, 10 s timeout
     │                         ▼
     │             inventory.json (drives, designations, trends)
     │                         │
     │   jobs: tests · FORMAT UNIT · firmware ◀── operator (page / API / feed)
     │                         │
     │   fleet loop ───────────┴──▶ stormblock :9090
     │   labels · health · drain→retire · overcommit · auto-add
     ▼
 API :9092 ── REST · page (web/) · stormview feed · /apis/storage.storm.io/v1
```

Lifecycle is three separate fields:

- `membership`: out | fleet
- `designation`: none | reserved | spare | failed
- `activity`: idle | testing | draining | formatting | updating_firmware | missing

---

<!-- _class: dense -->

## What it does today: find and place drives

- **Discovery:**
  - walks `/sys/block` every 30 s, and 2 s after a kernel disk uevent;
  - classifies NVMe / SAS / SATA, SSD / HDD (SATA behind a SAS HBA is SATA);
  - **stable id** = UUIDv5 of the WWID (or model + serial): the same across
    reboots, path changes and re-bays;
  - **dual-IOM shelves:** one drive, two `/dev` paths, a stable primary;
  - **520-byte NetApp drives** still listed (`usable: false`, needs reformat);
  - **whose data:** reads for a stormblock slab header, so a stormcos system
    disk is `in_use_by`, not "free".
- **Location:**
  - HBA, shelf (SES pages, or mpt3sas ids), bay, SAS address, phy, expander;
  - NVMe PCIe slot, through VMD and native multipath;
  - HBA firmware, BIOS and NVDATA versions (reported, never flashed);
  - **locate LEDs:** sysfs slot, SES IDENT, PCIe attention, NPEM;
  - a move is a `location` event; a new drive in a missing drive's bay
    `replaces` it.

---

<!-- _class: dense -->

## What it does today: health and the fleet

- **Health:**
  - each drive is sampled once a minute at its own phase, ≤8 reads at once,
    and a hung drive doesn't stall the rest;
  - NVMe: SMART log 0x02. SAS/SATA: sysfs state, I/O errors, temperature;
  - verdict good / warning / failing / failed from config thresholds, with
    hysteresis; every change is an event;
  - SSD wear trend recorded on change or daily.
- **Fleet hand-off to stormblock:**
  - join = register with labels (`shelf`, `bay`, `hba`, `pcie_slot`) and
    the stable uuid, optionally with a slab and a tier (NVMe hot, SSD warm,
    HDD cool);
  - Failing/Failed is pushed so stormblock quarantines the drive's legs, then
    **drain → empty → leave, locate LED on, "safe to pull"**;
  - per-drive **usage** (slabs, used, free) and **overcommit** setting.

---

<!-- _class: dense -->

## What it does today: work on drives, at scale

- **Tests:** smoke, full read scan (progress, cancel), destructive
  write-verify (O_DIRECT). Destructive only when the drive is out of the
  fleet, unmounted and holds no slab.
- **Sector reformat:** 520/528 → 4096 or 512.
  - MODE SELECT + FORMAT UNIT over SG_IO, progress polled with TEST UNIT READY.
  - One drive, a list, or a whole shelf; a batch is checked all-or-nothing
    first.
- **Firmware:** an image store, WRITE BUFFER for SAS/SATA, Download + Commit
  for NVMe.
  - One drive, a list, or every drive of a model.
  - Out-of-fleet drives in parallel, fleet drives one at a time.
- **The page** (v0.16.0, Svelte + stormview DataGrid), built for hundreds of
  drives:
  - shelves, HBAs and NVMe are rows, each with its drives nested;
  - filters, a detail pane, and a bulk bar where ticking a shelf means its
    drives;
  - checked against 212 drives in a jsdom test.

---

## Interfaces: the API on :9092

| | |
|---|---|
| Liveness · card | `GET /api/v1/health` `{status, version, node}` · `GET /api/v1/summary` (stormd card) |
| Drives | `GET /api/v1/drives[/{id}]` · id = uuid, WWID, `/dev` path, name or serial |
| Actions | `POST …/{id}/fleet` `locate` `designation` `overcommit` `test` `format` `firmware` · `GET/POST/DELETE …/{id}/drain` |
| Batches | `POST /api/v1/format` · `POST /api/v1/firmware` · `POST /api/v1/shelves/{key}/format` |
| Where things are | `/api/v1/topology` · `/api/v1/shelves` · `/api/v1/hbas` · `/api/v1/placement` (generation + ETag → 304) |
| Watchers | `/api/v1/events?since=` · `/api/v1/monitor` (poll cost) |
| Renderers | `/api/v1/components` + `/ws/components` (stormview feed; body-free action routes) |
| Kubernetes-shaped | `/apis/storage.storm.io/v1/{drives,enclosures}`, `?watch=1`, PATCH a Drive |
| The page | `/`, `/ui`, `/ui/`, `/assets/app.{js,css}`; works under stormd's proxy prefix |

Errors are `{error, code}`, stormblock's shape. No `/metrics` (#18) and
no auth (#19) yet.

---

<!-- _class: dense -->

## Interfaces: CLI, config, health

- **CLI:** `stormdrive [--config PATH] [--listen ADDR] [--data-dir DIR]`;
  `RUST_LOG` sets the log level.
- **Config** `/etc/stormdrive/stormdrive.toml`. A missing file means
  defaults, and `deploy/stormdrive.example.toml` is tested to *be* the
  defaults.
  - `listen_addr` 0.0.0.0:9092 · `data_dir` (unset = in memory)
  - `[discovery]` interval 30 s, include/exclude patterns, `manage_mounted`
  - `[monitor]` 60 s; temp 55/70 °C; spare 20/10 %; wear 80/95 %;
    hysteresis 3; 8 in flight; 10 s timeout
  - `[stormblock]` url, `auto_add` (off), `push_health`, `drain_on_failing`,
    `tier_map`, token
  - `[firmware]` 32 KiB chunks, 256 MiB image cap
- **Engine token:** config, then `$STORMBLOCK_API_TOKEN`, then the first
  readable token file. It is re-read while absent and on a 401.
- **Health:** `/api/v1/health` is the stormd liveness probe.
  `/api/v1/summary` answers from cache within stormd's 400 ms.

---

<!-- _class: dense -->

## How it ships and is operated

- **Golden kind: service.**
  - `stormcentral component build stormdrive` runs stormcos's
    `service_golden`: a static musl binary in a stormd container golden.
  - The config there sets `listen_addr` 0.0.0.0:9092 and `data_dir`
    /var/lib/stormdrive.
- **Starts on every node profile** (`boot.d/40-services`).
  - Host network, host `/dev`, host `/sys` (read-only: stormcos#166),
    its own data and log volumes, the engine token from `/run/stormblock`.
  - stormd restarts it and probes `/api/v1/health`.
- **Updated** only as a golden composed into a stormcos release; nodes
  clone the release copy-on-write, and a commit alone reaches nothing.
- **Built** with `sc-build` on dev: cargo only, since `web/dist` is
  committed (`web/rebuild.sh` rebuilds it on dev).
- **Reached** at `drive.<node>` (HTTPRoute), in stormconsole's drive view,
  and directly on `:9092`.

---

## Planned (not in the code yet)

| Planned | Issue |
|---|---|
| `/metrics`: SMART, temperature, wear and errors per drive | #18 |
| Auth / TLS on :9092 (`[api] api_token` is parsed, not enforced) | #19 |
| The drive worker: bulk test/join, NVMe format, sanitize, per-failure-domain sequencing | #5 |
| Shelf (IOM) firmware · a vendor firmware image source | #35 · #29 |
| SAS/SATA health: SCSI log sense, ATA SMART | #22 |
| Wear-out projection · persisted events | #23 · #25 |
| Firmware redundancy gate (sequencer) | #24 |
| Thermal actuation · drive crypto · burn-in before joining | your decision: #32 · #33 · #34 |

---

<!-- _class: dense -->

## Status

- **v0.16.0**, golden `golden-stormdrive-452835d3e854` (release request
  stormcos#131).
- **Tested:** 133 unit tests and model/page tests (212 drives) on dev.
  The test containers (short, medium, long) run against the real daemon
  on every build (#11).
- **Not yet run on a test machine:** C2NR0Q2's apiserver doesn't come up
  (stormcentral#63), tracked in #28.
- **Live:** on a Dell R230, one SATA drive behind mpt3sas (v0.11.0). It
  found the stormcos system disk's slabs and marked the disk in use, with
  join, format and the destructive test disabled.
- **Not yet run on real hardware:**
  - the NetApp shelf (SES pages, 520 → 4096 formats): #30;
  - a firmware image: #29;
  - a 160-bay chassis: #31;
  - a drain under I/O load: #30.
- **Issues that matter most:** #18 metrics · #19 auth · #5 drive worker ·
  #30 the shelf live pass · stormcos#166 (read-only `/sys` blocks LEDs and
  rescans in the golden).
