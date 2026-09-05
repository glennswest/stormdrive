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
  sysfs write when the `ses` module is bound.
- Without the `ses` module (stormcos images may not carry it), mpt3sas
  still fills `/sys/class/sas_device/end_device-*/{enclosure_identifier,
  bay_identifier}` from the expander's SMP discover; that gives the
  shelf's logical id and the bay, and the SES scan (below) names the
  shelf. Locate then goes through the SES control page ourselves.
- Shelf key is the **enclosure logical id** (page 0x01 / mpt3sas
  `enclosure_identifier`), the same through every IOM. The SES device's
  VPD 0x80 serial is the IOM's serial on NetApp shelves, so it is only a
  fallback key.

### Raw SCSI (`scsi.rs`) and shelves (`ses.rs`)
The kernel wraps what it needs for I/O and nothing else. Three things
here need commands sd never issues, so `scsi.rs` speaks SG_IO directly
(`/dev/sgN` when the sg driver exposes it, the block node otherwise):
READ CAPACITY(16), MODE SENSE/SELECT, FORMAT UNIT, TEST UNIT READY,
INQUIRY/VPD, RECEIVE/SEND DIAGNOSTIC. Sense decoding (fixed and
descriptor formats, including the sense-key-specific progress indication
a formatting drive reports) is portable and unit-tested.

`ses.rs` reads the SES-2 pages from every SCSI type-13 device —
configuration (0x01: logical id, vendor/product, element type table),
enclosure status (0x02: PSU/fan/temperature/voltage/current/slot
elements), element descriptors (0x07: names), additional element status
(0x0A: which SAS address sits in which bay) — and assembles a
`ShelfReport` per shelf, merging the two IOMs of a dual-path shelf by
logical id. It is refreshed every discovery tick, kept in `AppState`,
and feeds `/api/v1/shelves`, the topology tree, the components feed, the
kube `Enclosure`, the summary card and the UI's shelf panel. Events fire
when a shelf appears/disappears, its overall status moves, or an element
goes bad or recovers. Control is limited to IDENT (bay and shelf locate
LEDs) built from a fresh status page so the generation code matches and
no other request bit rides along.

### Sector-size reformat (`format.rs`)
NetApp-formatted drives arrive at 520 (or 528) bytes per sector. Linux
refuses them — `sd: Unsupported sector size 520` — and the block node
attaches with 0 blocks, so nothing above the SCSI layer can touch them.
Discovery keeps them anyway (`usable: false`, `block_size` from READ
CAPACITY, `needs_reformat`), and the format job turns them into drives:

1. MODE SELECT(10) with a block descriptor carrying the new block length
   and a block count of 0 ("all"); MODE SELECT(6) if (10) is refused.
2. FORMAT UNIT, FMTDATA + IMMED, no defect list (FOV=0: drive defaults).
   Blocking form with a day-long timeout if the drive refuses IMMED.
3. TEST UNIT READY every 5 s: NOT READY 04/04 with a progress
   indication until done; 31/xx = failed.
4. `device/rescan` on every path, READ CAPACITY to verify, and a
   delete + targeted host scan when sd still reports 0 blocks.

Guards: out of the fleet, idle, present, not reserved, no mounted
partitions; NVMe is refused (namespace format is a different command).
Many drives run in parallel — the drive does the work, the host polls.
A batch request validates every drive before starting any. There is no
cancel. The result is persisted on the drive (`format`) and reported
as an event; the drive is `usable` again once the kernel re-reads it.

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
- Inventory: `Drive.firmware` from sysfs at discovery, re-read after an
  update (INQUIRY for SCSI, `firmware_rev` for NVMe).
- Image store: `<data_dir>/firmware/<name>`, uploaded raw with
  `PUT /api/v1/firmware/images/{name}` (temp file + rename; size cap
  `firmware.max_image_mib`), listed with size and SHA-256, deleted with
  DELETE. Names are plain file names — no separators.
- SAS/SATA: WRITE BUFFER mode 0x0E (download microcode with offsets,
  save, defer) in chunks of `firmware.chunk_kib` rounded up to the
  drive's READ BUFFER offset boundary, then mode 0x0F (activate
  deferred). A drive that rejects 0x0E on the first chunk gets mode 0x07
  (offsets + save, activates after the last chunk). Then: wait for the
  drive to answer TEST UNIT READY (unit attentions cleared), sysfs
  rescan, INQUIRY revision compared. SATA drives behind SAS HBAs are
  reached through the SAT translation of WRITE BUFFER → DOWNLOAD
  MICROCODE.
- NVMe: Firmware Image Download (0x11) in 4 KiB-aligned chunks, then
  Firmware Commit (0x10) CA=3 (activate without reset); a controller
  that refuses or answers "activation requires reset" is committed with
  CA=1 and the record carries `reset_required` — the new image runs
  after the next reset, and `Drive.firmware` is left as-is until then.
- Policy: never automatic. Out-of-fleet drives update in parallel;
  fleet drives one at a time behind a node-wide lock; Failing/Failed
  drives are refused unless `force`. One, many, or every drive of a
  model (`POST /api/v1/firmware {drives|model, image}`), validated
  all-or-nothing before any starts. The last update is persisted on the
  drive (`firmware_update`). This is the sequencer's first customer;
  redundancy checks against stormblock (is a rebuild running? is the
  volume already degraded?) are the next gate to add.

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
POST /api/v1/drives/{id}/format        {"block_size": 512|4096} → FORMAT UNIT
GET  /api/v1/drives/{id}/format        current run + last result
POST /api/v1/format                    {"drives":[handles], "block_size"} —
                                       all-or-nothing validation, then parallel
GET  /api/v1/format                    every format run on this node
GET  /api/v1/firmware/images           image store (name, size, sha256)
PUT  /api/v1/firmware/images/{name}    raw body upload; DELETE removes
POST /api/v1/drives/{id}/firmware      {"image", "force"?} → WRITE BUFFER /
                                       NVMe download+commit
GET  /api/v1/drives/{id}/firmware      version, current run, last result
POST /api/v1/firmware                  {"drives":[…] and/or "model", "image"}
GET  /api/v1/firmware                  every update run on this node
GET  /api/v1/shelves                   SES shelves: identity, status, elements
GET  /api/v1/shelves/{key}             key = logical id | serial | sysfs id
POST /api/v1/shelves/{key}/locate      {"on": bool, "bay"?: n} → SES IDENT
POST /api/v1/shelves/{key}/format      {"block_size", "all"?} — every
                                       out-of-fleet drive that needs it
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

## StormBlock integration (`stormblock.rs`, `fleet.rs`)

stormblock v11 closed the loop (stormblock#70, #71); this side closed in
stormdrive 0.5.0. `stormblock.rs` is the client, `fleet.rs` the policy the
monitor tick runs after every discovery/health round:

- **Register with labels + identity.** `POST /api/v1/drives {path, labels,
  uuid}` — `labels` are the location as `Location::labels()` resolves it
  (`shelf`, `bay`, `hba`, `pcie_slot`), `uuid` is our stable `DriveId`. They
  become the failure domain of every slab on the drive, so a volume with
  `mirror:2@shelf` keeps its legs out of one enclosure. Labels are re-pushed
  (`PUT …/labels`) whenever the resolved location differs from what was last
  sent (`Drive.pushed_labels`).
- **Slabs by identity.** `GET /api/v1/drives/{id}/slabs` replaces the
  path-matching guess: a drive with an occupied slab cannot `leave` without
  a drain or `force`.
- **Health push.** On a change of our conclusion for a fleet drive,
  `POST …/health {state}`: Failing/Failed → stormblock quarantines the
  drive's slabs and every redundant volume stops reading that leg *before*
  an I/O fails; Good/Warning → `healthy`, which lifts the quarantine
  (`Drive.pushed_health`; `stormblock.push_health`).
- **Drain → retire.** A fleet drive that goes Failing/Failed, or is
  designated Failed by an operator, or is asked to `leave` with `"drain":
  true`, gets `POST …/drain`; the tick polls `GET …/drain` and records it on
  the drive (`Drive.drain`, activity `Draining`). When stormblock says
  `empty`, the drive `DELETE`s out of the fleet, the locate LED comes on and
  an event says *safe to pull*. `stuck` is an error event and the drive
  stays quarantined. `stormblock.drain_on_failing` turns the automatic
  half off; `POST /api/v1/drives/{id}/drain[?leave=true]` is the manual one.
- **Auto-add** (`stormblock.auto_add`, off by default): a qualified
  out-of-fleet drive with no designation and a known health is registered
  with its labels and given a slab (`auto_format_slab`, tier from
  `tier_map`/kind). A failed attempt waits ten minutes before retrying.

**Migration flow (as it runs now):**
```
Failing detected ──▶ POST health {failing} ──▶ POST drain ──▶ poll GET drain
      │                (slabs quarantined,          │ per-leg moves, progress
      │                 legs distrusted)            ▼
      │                                     empty ──▶ DELETE drive ──▶ Out, locate LED on
      ▼
 tech swaps drive ──▶ hotplug add ──▶ qualify ──▶ auto-add (labels, uuid, slab)
```

The engine never decides any of this; it only executes what this daemon
tells it. `push_health` reports with `drain: false` on purpose — the drain
is our decision, taken by `drain_on_failing`, not something a health report
starts behind our back.

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
