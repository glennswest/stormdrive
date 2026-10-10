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
Every claim here is checkable against the code as of v0.30.0 (src/)
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
storage node, REST + feed on **:9092**; its UI is stormconsole's drive plugin

v0.27.1 · github.com/glennswest/stormdrive

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
| depends on | **stormview** | crate: the components feed (`/api/v1/components`) |
| depends on | **stormd** | the golden is a stormd container; `[process.ui]` card for non-stormcos installs |
| used by | **stormcos** | ships the golden and starts it on every node profile |
| used by | **stormstorage** | fleet policy; names drives by the identity stormdrive uses (via stormblock's `DriveRef`), and stormdrive's labels reach it through stormblock |

Also read by **stormconsole**'s drive plugin (every node's `:9092`) and
by rustkube-node's planned PV placement mirror (`/api/v1/placement`,
rustkube-node#60 — not in rustkube-node yet). PVCs are served by
stormblock's built-in driver; stormdrive only says where their drives are.

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
     │   jobs: tests · FORMAT UNIT · firmware ◀── operator (console / API / feed)
     │                         │
     │   fleet loop ───────────┴──▶ stormblock :9090
     │   labels · health · drain→retire · overcommit · auto-add
     ▼
 API :9092 ── REST · stormview feed · /apis/storage.storm.io/v1
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
  - NVMe: SMART log 0x02. SAS: LOG SENSE (informational exceptions,
    temperature, SSD wear, error counters). SATA: ATA SMART attributes
    and thresholds. Failed commands (`io_errors`) are kept apart from
    media errors;
  - the engine's own slab report: a disk whose slabs the engine runs
    remote, or refused, is suspect (#58);
  - verdict good / warning / failing / failed from config thresholds, with
    hysteresis; every change is an event;
  - SSD wear trend recorded on change or daily, projected to days to
    wear-out (#23).
- **Fleet hand-off to stormblock:**
  - join = register with labels (`shelf`, `bay`, `hba`, `pcie_slot`) and
    the stable uuid, optionally with a slab and a tier (NVMe hot, SSD warm,
    HDD cool);
  - Failing/Failed is pushed so stormblock quarantines the drive's legs, then
    **drain → empty → leave, locate LED on, "safe to pull"**;
  - per-drive **usage** (slabs, used, free, the volumes on it) and
    **overcommit** setting;
  - a failed RAID-set member's bay gets its **fault LED** (#44).

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
- **The drive worker** (v0.17.0): one request takes a selection (drives, a
  shelf and bays, a model, "all unusable") through format → sanitize →
  partition → enroll, plus ATA security erase and a test step. It is
  parallel per HBA, enrolls one at a time per shelf, refuses any drive
  holding data unless named by id, WWN or serial, and survives a restart.
  Enrolling writes metadata only, whatever the drive's size (#72).
- **DrivePolicy** (CRD): per node, which drives get which tier, reformat
  first if 520-byte; blank healthy drives are **offered** (#42, #50).
- **Firmware:** an image store, WRITE BUFFER for SAS/SATA, Download + Commit
  for NVMe.
  - One drive, a list, or every drive of a model.
  - Out-of-fleet drives in parallel, fleet drives one at a time, each
    waiting until its volumes are redundant (#24).
  - Shelf IOM firmware through SES, one IOM at a time (#35, API only).
- **No page of its own** (#84, v0.30.0): the UI is stormconsole's
  `drive` plugin, over the API, the feed and the `storage.storm.io`
  resources.

---

## Interfaces: the API on :9092

| | |
|---|---|
| Liveness · card | `GET /api/v1/health` `{status, version, node, writes, reads}` · `GET /api/v1/summary` (stormd card) |
| Drives | `GET /api/v1/drives[/{id}]` · `DELETE` (forget) · `…/{id}/health` `slabs` `history` · id = uuid, WWID, `/dev` path, name or serial |
| Actions | `POST …/{id}/fleet` `enroll` `locate` `designation` `overcommit` (also `PUT`) `test` `format` `firmware` (those three also `GET`) · `GET/POST/DELETE …/{id}/drain` |
| Batches | `POST /api/v1/worker/jobs` (+ `{id}`, `cancel`, `resume`) · `POST /api/v1/format` · `POST /api/v1/firmware` (+ `images`) · `POST /api/v1/shelves/{key}/format` `firmware` (IOM) |
| Where things are | `/api/v1/topology` · `/api/v1/shelves` · `/api/v1/hbas` · `/api/v1/placement` (generation + ETag → 304) |
| Watchers | `/api/v1/events?since=` · `/api/v1/monitor` (poll cost) · `/api/v1/history` · `/api/v1/assets` (system-data, #64) |
| Renderers | `/api/v1/components` + `/ws/components` (stormview feed; body-free action routes) |
| Kubernetes-shaped | `/apis/storage.storm.io/v1/{drives,enclosures}`, `?watch=1`, PATCH a Drive |

Errors are `{error, code}`, stormblock's shape. `/metrics` serves
per-drive SMART, temperature, wear and errors in Prometheus text (#18).
TLS from the node's stormcert pair; nothing answers anonymously but
health (#19) — once the golden drops its `allow_anonymous` transition (#56).

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
    hysteresis 3; 8 in flight; 10 s timeout; wear-out warning 180 days
  - `[stormblock]` url, `auto_add` (off), `push_health`, `drain_on_failing`,
    `tier_map`, token
  - `[api]` TLS pair + client CA from `/data/stormcert`, `admin_gate`
  - `[kubernetes]` apiserver, credential, `controller`
  - `[firmware]` 32 KiB chunks, 256 MiB image cap, 30 min redundancy wait
  - `[worker]` 8 per HBA, 1 enrol per domain, offer ≥ 1 GiB
  - `[history]` `/system-data`, hourly heartbeat, 24 months
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
  - The config there (stormcentral's registry) sets `listen_addr`
    0.0.0.0:9092, `data_dir` /var/lib/stormdrive, `allow_anonymous`
    (TLS transition, #56) and `[kubernetes]` with `controller = false`
    until the CRDs ship (stormcos#369).
- **Starts on every node profile** (`boot.d/40-services`).
  - Host network, host `/dev`, host `/sys` (read-only: stormcos#166),
    its own data and log volumes, the engine token from `/run/stormblock`,
    `/data/stormcert`; `/system-data` once mounted (stormcos#456).
  - stormd restarts it and probes `/api/v1/health`.
- **Updated** only as a golden composed into a stormcos release; nodes
  clone the release copy-on-write, and a commit alone reaches nothing.
- **Built** with `sc-build` (a build VM since dev.g8.lo retired): cargo
  only.
- **Remote calls retry** by idempotency, with backoff and a deadline (#71).
- **Reached** at `drive.<node>` (HTTPRoute), in stormconsole's drive view,
  and directly on `:9092`.

---

## Planned (not in the code yet)

| Planned | Issue |
|---|---|
| Drive worker scheduling default | decision #37 |
| A vendor firmware image source | decision #29 |
| Configuration kept across installs · what was done to a drive in its history | #67 · #68 |
| Thermal actuation · drive crypto · burn-in before joining | your decision: #32 · #33 · #34 |

---

<!-- _class: dense -->

## Status

- **v0.27.1** (October 2026). Each release's golden and release request
  are recorded in CLAUDE.md's work plan and in CHANGELOG.md.
- **Tested:** 241 unit tests, and the
  daemon run against a stand-in apiserver, a seeded restart, TLS, and a
  simulated 160-bay chassis, on a build VM.
  The test containers (short, medium, long) run against the real daemon
  on every build (#11).
- **Not yet run on a test machine:** C2NR0Q2's apiserver doesn't come up
  (stormcentral#63), tracked in #28.
- **Live:** on a Dell R230, one SATA drive behind mpt3sas (v0.11.0; it
  runs 0.27.1 today). It
  found the stormcos system disk's slabs and marked the disk in use, with
  join, format and the destructive test disabled.
- **Not yet run on real hardware:**
  - the NetApp shelf (SES pages, 520 → 4096 formats): #30;
  - a firmware image: #29;
  - a 160-bay chassis (simulated only, #31);
  - a drain under I/O load: #30.
- **Issues that matter most:** #67 configuration lost on install ·
  #65 every SES enclosure a shelf · #30 the shelf live pass ·
  stormcos#166 (read-only `/sys` blocks LEDs and rescans in the golden).
