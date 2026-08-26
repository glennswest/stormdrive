# StormDrive Architecture

**Status:** design v1, 2026-08-26. Written against the stormblock review
([stormblock-review.md](stormblock-review.md)).

## Position in the stack

```
                stormd (per-container supervisor + newer UI)
                   │  [process.ui] proxy + summary card
                   ▼
┌──────────────────────────────┐        ┌─────────────────────────────┐
│  stormdrive  :9092           │  REST  │  stormblock  :9090          │
│  the drive curator           │──────▶ │  the drive consumer         │
│  discovery · health · wear   │        │  slabs · volumes · targets  │
│  thermal · firmware ·        │        │                             │
│  location · sequencing       │        │                             │
└──────────────┬───────────────┘        └──────────────┬──────────────┘
               │ sysfs · ioctl · SG_IO · netlink       │ O_DIRECT/io_uring
               ▼                                        ▼
        physical drives  ◀──── same devices ────▶  slabs on drives
        (NVMe, SAS2/3, SATA; HBAs, expanders, enclosures)
```

## Position in the distributed hierarchy

stormblock is fully distributed and hierarchical: volumes move between
clusters, RAID legs can span clusters, and the physical world carries the
full site hierarchy. Two chains overlay each other:

```
PHYSICAL  site ⊃ building ⊃ floor/room ⊃ row ⊃ rack ⊃ node ⊃ hba ⊃ shelf ⊃ bay
          └──────────────── stormblock owns ───────────────┘└─ stormdrive owns ─┘
LOGICAL   multicluster ⊃ multicluster ⊃ … ⊃ cluster ⊃ node
          (clusters group into multiclusters, recursively — a federation,
           not one flat set; a cluster's nodes may span racks/rooms/sites)
```

Placement reasons over the **physical** chain (what fails together);
cross-cluster moves and RAID legs address the **logical** grouping. The
two meet at the node.

**Tiering is a cross-cluster policy, not only a per-drive property.** A
tier can be an entire cluster (the testbed: 2.5" cluster = high, 3.5" =
medium, PVE = backup), so a volume's tier policy names rungs of the
logical hierarchy, and tier migration is movement *between clusters*.
stormdrive's kind→tier derivation still applies within each cluster — it
decides which drives make that cluster its tier.

**Who owns which rung.** stormblock is an *execution engine* — per-node
mechanism (slabs, volumes, RAID, targets, the /v1 contract with its epoch
fencing) that executes what it is told. The cross-node brain is
**stormstorage** (github.com/glennswest/stormstorage, live since
2026-08-26 — see its docs/architecture.md): the topology registry for
everything node-and-above (rack…site, the multicluster tree), pools,
placement decisions at every rung, cross-cluster tiering policy, and
orchestration of moves and RAID legs — done by driving stormblock's /v1
on many nodes, informed by stormdrive's labels and health.

stormdrive is deliberately **per-node and authoritative only below the
node**: it resolves hba/shelf/bay from the hardware and hands them up as
labels. Three layers, no overlap: stormdrive = hardware truth,
stormblock = execution, stormstorage = fleet policy.

**Label vocabulary (must stay agreed across the stack):** `site`,
`building`, `room`, `row`, `rack`, `node`, `cluster` (stormblock's half);
`hba`, `shelf`, `bay`, `pcie_slot` (stormdrive's half, emitted by
`Location::labels()`). One vocabulary at every rung is what lets a single
placement mechanism express "spread across shelves", "across racks", and
"across clusters" as the same operation at different depths — which is
exactly what cross-cluster RAID needs (stormblock#72).

Separation of duties: **stormblock never has to learn hardware, stormdrive
never touches data.** stormdrive reads identify/log/mode pages and sysfs; it
never reads or writes a drive's data blocks. The one write-class thing it does
to a drive is firmware download/commit, and that only through the sequencer.

One stormdrive per node, next to that node's stormblock. Cross-node views
belong to whatever aggregates node APIs (stormd cards per node now; a fleet
view later).

## Drive model

```rust
DriveId(Uuid)          // uuid5(STORMDRIVE_NS, wwid | model+serial) — stable
                       // across opens, reboots, and path changes
Drive {
    id: DriveId,
    path: String,              // current /dev node (may change; id doesn't)
    kind: DriveKind,           // NvmeSsd | SasSsd | SasHdd | SataSsd | SataHdd | Unknown
    model, serial, firmware: String,
    wwid: Option<String>,
    capacity_bytes: u64,
    block_size: u32,
    location: Location,
    state: DriveState,
    health: HealthReport,
    first_seen, last_seen: SystemTime,
}
Location {                         // controller → shelf → bay hierarchy
    controller: Option<Controller>,  // { scsi_host, pcie_addr, driver }
    shelf: Option<Shelf>,            // { id, vendor, model, serial, sas_address }
                                     //   identity = the SES processor's SCSI
                                     //   device; serial from VPD page 0x80 is
                                     //   the canonical key (a dual-IOM shelf
                                     //   is two enclosure devices, one serial)
    bay: Option<u32>,                // slot index within the shelf
    sas_address: Option<String>,     // the drive's own
    pcie_addr / pcie_slot,           // NVMe drives
}
// Lifecycle is three ORTHOGONAL fields, not one state ladder:
Membership  = out | fleet          // is the drive handed to stormblock?
Designation = none | reserved | spare | failed   // operator-set; applies
                                                 // both in fleet and out
Activity    = idle | testing | joining | draining | missing
HealthStatus = Unknown | Good | Warning | Failing | Failed
HealthReport {
    status, temperature_c, power_on_hours, media_errors,
    available_spare_pct, wear_pct,            // NVMe percentage_used / SSD endurance
    critical_warning: u8,                     // NVMe bitfield, 0 elsewhere
    messages: Vec<String>, collected_at,
}
```

The flow: discovery finds drives (membership `out`); the UI moves them to
the **fleet** (register with stormblock, optionally format a slab with a
tier derived from the drive kind). Independently, a drive can be **tested**
(destructive kinds only out of fleet and unmounted), or designated
**reserved** (never join), **spare** (standing by; still joinable when
pressed into service), or **failed** (operator verdict — health can reach
the same conclusion on its own; the two are kept separate). `missing`
means the inventory remembers a drive the node can't see. A `failed`
designation on an in-fleet drive raises a drain-needed warning; automating
the drain itself waits on stormblock#70.

The inventory (all `Drive` records + wear-trend samples) persists to
`<data_dir>/inventory.json` with atomic tmp+rename writes, so identity,
first_seen, and trend history survive restarts — deliberately the opposite of
stormblock's in-memory `Vec<DriveInfo>`.

## Subsystems

**Multipath (dual-IOM shelves).** A NetApp shelf with two IOMs presents
one physical drive as two /dev nodes with one WWID. Discovery groups
observations by DriveId: one `Drive`, a `paths` list, and a stable primary
(sorted-first path). SMART, tests, and stormblock hand-off use the
primary; the path list is visible in the API and UI. Handing stormblock a
dm-multipath device instead is a later option — one path is correct until
path failover is actually needed.

### Discovery (`discovery/`)
- Full scan on startup and every `discovery.interval_secs`: walk
  `/sys/block`, skip virtual/managed devices (`loop* ram* zram* dm-* md*
  sr* fd* nbd* ublkb*` — ublkb is stormblock's own export surface), skip the
  boot drive, apply config include/exclude globs.
- Per device: size (`size` × 512), `queue/logical_block_size`,
  `queue/rotational`, `device/model`, `device/serial`,
  `device/firmware_rev` (or NVMe equivalents), `wwid`, transport
  classification (nvme vs sd; SAS vs SATA from `device/sas_address`
  presence / `transport` links).
- Hotplug: netlink kobject-uevent socket (add/remove of block devices)
  triggers targeted re-scan; the interval scan remains the safety net.
- A known drive whose node vanishes → `Missing` + event. A Missing drive
  reappearing keeps its `DriveId` (that's the point of stable identity).

### Monitoring (`monitor.rs`, `smart/`)
- One loop, every `monitor.interval_secs` (default 60): collect per drive,
  evaluate, transition, persist, emit events, update metrics.
- **NVMe** (`smart/nvme.rs`): `NVME_IOCTL_ADMIN_CMD` Get Log Page 0x02 —
  critical warning bits, composite temp, available spare (+threshold),
  percentage used, POH, unsafe shutdowns, media errors, error-log count.
  (Reference implementation: stormblock `main.rs:2342`, which decodes the
  same page for must-gather.)
- **SAS/SATA** (`smart/scsi.rs`): phase 1 is sysfs (`device/state`,
  `ioerr_cnt`, hwmon temp). Phase 2 adds SG_IO log-sense: Informational
  Exceptions (0x2F — the drive's own predicted-failure verdict), Temperature
  (0x0D), Solid State Media (0x11 — percentage used endurance), plus ATA
  SMART READ DATA passthrough for SATA behind SAS HBAs.
- **Threshold engine** (config-driven):
  `Warning` — temp ≥ warn, spare ≤ spare_warn, wear ≥ wear_warn, media-error
  growth over window. `Failing` — NVMe critical_warning reliability bit,
  SCSI IE predicted failure, spare ≤ spare_crit, sustained error growth.
  `Failed` — device errors on identify/log reads, kernel offlined it
  (`device/state != running`), read-only NVMe bit. Transitions are
  hysteresis-guarded (N consecutive samples) so one bad poll doesn't flap.
- **Wear trending**: per-drive ring of (time, wear_pct, media_errors)
  samples persisted with the inventory; linear projection → "days to
  wear-out" surfaced in the API and UI.

### Location (`topology.rs`)
- SAS bays: `/sys/class/enclosure/*/` — each enclosure device exposes
  `slot*/` dirs with `device` symlinks to the SCSI device; map block dev →
  (enclosure id, bay). Locate/fault LEDs are writable `locate`/`fault`
  attrs on the slot dir — `POST /api/v1/drives/{id}/locate {on}` is a
  sysfs write, no SES passthrough needed for phase 1.
- NVMe: controller BDF from the sysfs device path; physical slot from
  `/sys/bus/pci/slots/*/address` matching.
- SAS addresses and expander chain from `/sys/class/sas_device` /
  `sas_end_device` when present (mpt3sas exposes these).
- Output doubles as **failure-domain labels** (`enclosure=...`, `bay=...`,
  `hba=...`) — the per-drive layer stormblock's topology roadmap item needs;
  handed over when drives are registered (pending the stormblock issue).

### Sequencing (`sequence.rs`)
One node-wide queue of **disruptive operations** (firmware update, drain,
retire, qualification). Invariants:
- At most one disruptive op in flight per node.
- Pre-flight: target drive's health, and stormblock's view — no degraded
  redundancy, no slab under evacuation, `serve/v1/ready` green (when
  present).
- Post-op re-check before the next item dequeues.
This is what makes "update firmware on 24 drives" safe: one at a time,
health-gated, abort-on-regression.

### Firmware (`firmware.rs`)
- Inventory ships in phase 1 (already collected at discovery).
- Update engine (phase 5): image store under `<data_dir>/firmware/`,
  a policy map (model → desired version), NVMe Firmware Image Download
  (0x11) + Commit (0x10) with slot/action handling, SCSI WRITE BUFFER
  (mode 0x07 staged / 0x0E activate). Only ever executed through the
  sequencer; never automatic unless the policy explicitly says so.

### Thermal (`thermal.rs`)
- Phase: observe → alert → actuate. Per-drive temps from SMART; enclosure
  temp/fan elements from SES pages later.
- Alerting through the shared threshold engine and event stream.
- Actuation (SES cooling-element control via SG_IO) is deliberately last and
  gated behind explicit config — the review found no precedent in the
  ecosystem and fan policy is chassis-specific.

### Events (`events.rs`)
In-memory ring (persisted tail in the inventory file), each entry
`{time, drive_id?, severity, kind, message}`. Served at
`GET /api/v1/events?since=`. The stormd proxy can't do WebSockets, so the UI
polls this; SSE can come later for direct consumers.

## API (axum, `0.0.0.0:9092`)

```
GET  /                                 embedded UI (also /ui, /ui/; the page
                                       detects stormd's proxy prefix itself)
GET  /api/v1/health                    liveness {status, version}
GET  /api/v1/drives                    inventory (running test inlined)
GET  /api/v1/drives/{id}               id = DriveId, name, path, serial, wwid
GET  /api/v1/drives/{id}/health        latest HealthReport + trend
POST /api/v1/drives/{id}/locate        {"on": true|false} → SES slot LED
POST /api/v1/drives/{id}/fleet         {"action":"join","format_slab":bool,
                                        "tier"?} | {"action":"leave","force"?}
POST /api/v1/drives/{id}/designation   {"designation":"none|reserved|spare|failed"}
POST /api/v1/drives/{id}/test          {"kind":"smoke|read_scan|destructive_sample"}
GET  /api/v1/drives/{id}/test          current/last run (progress, errors)
POST /api/v1/drives/{id}/test/cancel
GET  /api/v1/topology                  controller → shelf → drive tree
GET  /api/v1/events?since=<seq>
GET  /api/v1/summary                   stormd RemoteSummary card
GET  /metrics                          Prometheus (phase 2)
```
Errors use stormblock's `{error, code}` envelope shape for familiarity.
Auth: same posture as the rest of the ecosystem for now (none on the node
LAN); token support goes in the config from day one (`api_token`, off by
default) so it can be turned on without a format change.

### The stormd card (`/api/v1/summary`)
Answer within 400 ms (stormd's timeout) from cached state — never collect on
demand. Health mapping: any Failed/Missing-Active → `error`; any
Failing/Warning/Draining → `warn`; otherwise `ok`. Metrics: drives total,
active, warnings, hottest °C, worst wear %.

## StormBlock integration (`stormblock.rs`)

What works against stormblock today:
- **Register**: `POST /api/v1/drives {path}` then
  `POST /api/v1/slabs {device_path, tier}` with tier derived from kind
  (NvmeSsd→hot, SasSsd/SataSsd→warm, HDD→cool; overridable per-drive).
  Auto-add is a config flag, **off by default** — phase 4 turns the loop on.
- **Cross-check**: `GET /api/v1/drives` + `/api/v1/slabs/pool` to reconcile
  Active state and to see pressure.
- **Withdraw**: `DELETE /api/v1/drives/{id}` — noting the in-use guard bug
  filed against stormblock; stormdrive checks slab presence itself first.

What needs stormblock work (issue filed): a drive-scoped **drain** —
"evacuate every slab on this device over HTTP, report progress, tell me when
it's empty" — plus accepting a stable drive identity + location labels at
registration. Until that lands, the failure flow stops at *notify*:
stormdrive marks the drive Failing/Draining, raises events, flips the stormd
card to warn/error, and a human (or a script against stormblock's CLI
`migrate`) does the move. The moment the API exists, `sequence.rs` gets a
`Drain` op and the loop closes.

**Migration flow (target state):**
```
Failing detected ──▶ state=Draining ──▶ stormblock drain(drive)
      │                                     │ per-slab evacuate, progress polled
      │                                     ▼
      │              slabs empty ──▶ DELETE stormblock drive ──▶ state=Retired
      ▼
 locate LED on ──▶ tech swaps drive ──▶ hotplug add ──▶ qualify ──▶ register
```

## Config (`/etc/stormdrive/stormdrive.toml`)

```toml
listen_addr = "0.0.0.0:9092"
data_dir    = "/var/lib/stormdrive"
node_name   = ""                    # default: hostname

[discovery]
interval_secs = 30
exclude = []                        # extra glob patterns; built-ins always apply
include = []                        # explicit allow-list (empty = all eligible)

[monitor]
interval_secs   = 60
temp_warn_c     = 55
temp_crit_c     = 70
spare_warn_pct  = 20
spare_crit_pct  = 10
wear_warn_pct   = 80
hysteresis      = 3                 # consecutive samples before a transition

[stormblock]
enabled  = true
url      = "http://127.0.0.1:9090"
auto_add = false                    # phase 4; explicit opt-in
tier_map = { nvme_ssd = "hot", sas_ssd = "warm", sata_ssd = "warm", hdd = "cool" }

[api]
api_token = ""                      # empty = no auth (matches ecosystem posture)
```

Missing file → defaults (stormblock convention). CLI flags override file.

## Deployment

- musl static binary, `x86_64-unknown-linux-musl` (aarch64 later for JBOD
  heads), built on dev.g8.lo.
- systemd unit `stormdrive.service` (After=network-online, wants
  stormblock-target ordering but must run without it), or as a stormd
  `[[process]]` in containerized deployments — the stormd path is what
  lights up the UI extension.
- Needs root (sysfs writes for LEDs, admin ioctls, SG_IO); CAP_SYS_ADMIN +
  CAP_SYS_RAWIO is the tightening target once the ioctl set is final.

## Testing strategy

- Pure-logic (threshold engine, state machine, identity derivation, trend
  projection, config) — portable unit tests.
- sysfs parsing against fixture trees (copied from real nodes into
  `tests/fixtures/sys/`) — portable.
- ioctl/SG_IO/netlink paths — Linux-only tests on dev.g8.lo, tagged like
  stormblock's (the 45-test delta lesson).
- Against a live stormblock: integration tests using FileDevice-backed
  stormblock on dev.
