# StormDrive Architecture

**Status:** checked against the code at v0.15.0, 2026-09-28 (#7). The first
version was written 2026-08-26 as a design against the stormblock review
([stormblock-review.md](stormblock-review.md)). A section marked
**Design — not built** describes intent the code does not have yet. Every
other section describes what the code does. The route table, every config key
and how the golden ships are in the [README](../README.md).

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
never touches a fleet drive's data.** For its own bookkeeping stormdrive
reads a drive's sysfs, identify, log and mode pages, and the first sectors of
the disk and of each GPT partition (the slab probe). It writes to a drive only
when an operator asks. Three operations do that:

- a destructive test;
- a sector-size format;
- a firmware download.

The first two refuse a fleet drive, a mounted drive, and a drive that holds a
stormblock slab.

One stormdrive per node, next to that node's stormblock. Cross-node views
belong to whatever aggregates node APIs (stormd cards per node now; a fleet
view later).

## Drive model

`src/drive.rs`, abridged (serde names are snake_case):

```rust
DriveId(Uuid)          // uuid5(stormdrive ns, wwid | "model:serial") — stable
                       // across opens, reboots, path changes and re-bays
Drive {
    id: DriveId,
    path, name: String,        // primary /dev node + kernel name (may change)
    paths: Vec<String>,        // every /dev node (dual-IOM: two); path = first
    kind: DriveKind,           // nvme_ssd | sas_ssd | sas_hdd | sata_ssd | sata_hdd | unknown
    model, serial, firmware: String,
    wwid: Option<String>,
    capacity_bytes: u64,
    block_size, physical_block_size: u32,  // from READ CAPACITY: 520 on NetApp drives
    usable: bool,              // false = kernel refused the sector size
    in_use_by: Option<String>, // a stormblock slab found on the disk
    location: Location,
    membership, designation, activity,     // see below
    overcommit: Overcommit,    // {enabled, ratio}, operator-set (#13)
    health: HealthReport,
    usage: Option<Usage>,      // from stormblock's slab listing (#12)
    format: Option<FormatRecord>, firmware_update: Option<FirmwareRecord>,
    drain: Option<DrainRecord>, replaces: Option<DriveId>,
    pushed_labels, pushed_health,          // what stormblock last accepted
    first_seen, last_seen: SystemTime,
}
Location {                         // controller → shelf → bay hierarchy
    controller: Option<Controller>,  // { scsi_host, pcie_addr, driver }
    shelf: Option<Shelf>,            // { id, logical_id, vendor, model, serial,
                                     //   sas_address }; key = enclosure logical id
    bay: Option<u32>,
    sas_address: Option<String>,     // the drive's own
    sas_phy: Option<u32>, expander: Option<String>,
    pcie_addr, pcie_slot: Option<String>,  // NVMe drives
}
// Lifecycle is three ORTHOGONAL fields, not one state ladder:
Membership  = out | fleet          // is the drive handed to stormblock?
Designation = none | reserved | spare | failed   // operator-set; applies
                                                 // both in fleet and out
Activity    = idle | testing | draining | formatting | updating_firmware | missing
HealthStatus = unknown | good | warning | failing | failed
HealthReport {
    status, temperature_c, power_on_hours, media_errors,
    available_spare_pct, wear_pct,            // NVMe available spare / percentage used
    critical_warning: u8,                     // NVMe bitfield, 0 elsewhere
    messages: Vec<String>, collected_at,
}
```

Discovery finds a drive with membership `out`. An operator (UI, API or
feed action) moves it to the **fleet**: stormblock registers it, and a slab
can be formatted on it with a tier. Independently of membership:

- A drive can be **tested**. The destructive kind needs an out-of-fleet,
  unmounted drive with no slab.
- A drive can be designated **reserved**: it never joins and is never
  formatted.
- A drive can be designated **spare**: standing by, and still joinable.
- A drive can be designated **failed**. This is the operator's verdict; health
  can reach the same conclusion on its own, and the two are kept separate.
  Designating a fleet drive `failed` reports it failed to stormblock and,
  with `drain_on_failing`, starts a drain that retires the drive when it is
  empty.

`missing` means the inventory remembers a drive the node can't see.

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
  sr* fd* nbd* ublkb* zd* pmem* drbd*` — ublkb is stormblock's own export
  surface), apply config include/exclude globs, and skip a drive with a
  mounted partition unless `discovery.manage_mounted`.
- **Who holds it** (`contents.rs`): the node's own system disk is *seen*,
  not skipped — it is the drive whose firmware most needs updating — but it
  is read for stormblock slabs (`STRMSLAB` at LBA 0, or at the first LBA of
  any GPT partition) and marked `in_use_by`. On stormcos that is the only
  reliable signal: the root filesystem is a ublk device stormblock serves
  from slabs on the disk, so nothing on it is in `/proc/mounts`, stormblock
  does not open it `O_EXCL`, and its `/api/v1/drives` does not list it.
  Join, format and the destructive test refuse an `in_use_by` drive (and
  re-read the disk just before starting); firmware updates are allowed but
  serialised like a fleet drive's.
- Per device: size (`size` × 512), `queue/logical_block_size`,
  `queue/rotational`, `device/model`, `device/serial`,
  `device/firmware_rev` (or NVMe equivalents), `wwid`, transport
  classification (nvme vs sd; SAS vs SATA from `device/sas_address`
  presence — except that a SATA drive behind a SAS HBA has a sas_address
  too, so a SCSI vendor of `ATA` (the SAT layer) wins).
- Hotplug (`hotplug.rs`, #15): one thread reads the kernel's
  `NETLINK_KOBJECT_UEVENT` group (no udev needed). An `add`, `remove` or
  `change` of a whole block disk triggers a discovery pass after a 2 s
  settle, so a pulled shelf is one pass, not hundreds. A kernel-side
  overflow (`ENOBUFS`) also triggers a pass. The interval scan is still the
  safety net, and the only mechanism when the socket cannot be opened.
- **Probe cache** (#15): what discovery asks the drive itself (READ
  CAPACITY(16) and the slab probe of LBA 0 and the GPT) is kept per device
  and asked again only when the device changes (`dev` maj:min, size,
  WWID: a swap, a rescan, a reformat) or every 10 minutes. A drive that did
  not answer (mid-format) is asked again every pass. `/proc/mounts` is read
  once per pass. Hidden gendisks and NVMe native-multipath path nodes
  (`nvme0c1n1`) are not drives; the namespace head (`nvme0n1`) is.
- A known drive whose node vanishes → `Missing` + event. A Missing drive
  reappearing keeps its `DriveId` (that's the point of stable identity).

### Monitoring (`monitor.rs`, `poller.rs`, `smart/`)
- Health polling is its own task (#15). Every present drive is sampled
  once per `monitor.interval_secs` (default 60), at its own **phase** in
  the interval (its id's hash, stable across restarts). 160 drives on 60 s
  is ~2.7 samples a second, not a burst of 160. At most
  `monitor.max_concurrent` (8) reads are in flight, with no thread per
  drive. A read that has not answered in `monitor.sample_timeout_secs`
  (10) is a failed sample (`kernel_ok = false`, damped by the hysteresis
  like any worsening). The drive is then *stuck* until the blocking read
  returns: it is not read again, so threads never pile onto a hung device,
  and each due time counts as another timeout, which walks a hung drive to
  Failed after `hysteresis` intervals. A timeout keeps the last readings;
  only the verdict and the reason change. Blocking threads are bounded by
  `max_concurrent` + stuck drives.
- The main loop keeps discovery, stormblock usage/reconcile, the fleet
  policy and persisting on their intervals.
- **NVMe** (`smart/nvme.rs`): `NVME_IOCTL_ADMIN_CMD` Get Log Page 0x02 —
  critical warning bits, composite temp, available spare (+threshold),
  percentage used, POH, unsafe shutdowns, media errors, error-log count.
  (Reference implementation: stormblock `main.rs:2342`, which decodes the
  same page for must-gather.)
- **SAS/SATA** (`smart/scsi.rs`): sysfs only — `device/state`,
  `device/ioerr_cnt` (commands that failed, which the feed and UI label "io
  errs", not media errors) and the hwmon temperature (drivetemp or the SAS
  driver). No command goes to the drive.
- **Design — not built:** SG_IO log sense for SAS/SATA: Informational
  Exceptions (0x2F — the drive's own predicted-failure verdict), Temperature
  (0x0D), Solid State Media (0x11 — endurance used), and ATA SMART READ DATA
  passthrough for SATA behind SAS HBAs.
- **Threshold engine** (`monitor::evaluate`, a pure function):
  - `failed`: the kernel's `device/state` is not `running`, the NVMe log
    read failed, or the read timed out (all `kernel_ok = false`). Also the
    NVMe read-only bit.
  - `failing`: NVMe reliability-degraded or spare-below-threshold bit, spare
    ≤ `spare_crit_pct`, or wear ≥ `wear_crit_pct`.
  - `warning`: NVMe volatile-backup or temperature bit, spare ≤
    `spare_warn_pct`, wear ≥ `wear_warn_pct`, temperature ≥ `temp_warn_c`
    (≥ `temp_crit_c` is still `warning`, with a different message), or
    media errors higher than the previous sample's.
  - A worse verdict must repeat `hysteresis` samples in a row before it
    sticks. A better one applies at once.
- **Wear trending**: SSDs (and any drive reporting wear) keep a ring of
  (time, wear_pct, media_errors) samples (512) persisted with the inventory.
  A sample is recorded when either value changes, or daily when neither does
  (#15). Recording every poll made 160 drives × 512 samples ≈ 5 MB of
  inventory, rewritten every tick, and the ring only covered 8.5 hours.
  `GET /api/v1/drives/{id}/health` returns it raw. **Design — not built:** a
  projected "days to wear-out" from the trend.
- **Persisting** (#15): compact JSON, serialised under the inventory lock
  and written outside it (tmp + rename), skipped when unchanged. One async
  mutex spans both, so two persists never land out of order.

### Cost per poll cycle (#15)

`GET /api/v1/monitor` reports it live:
- `samples`, `timeouts`, and `last`/`avg`/`max_sample_ms`
- `busy_ms_per_interval` (avg × drives)
- `load_pct` (busy / (interval × max_concurrent))
- `stuck` (drives whose read hasn't returned)
- the last discovery pass: `discovery_ms`, `discovery_drives` and
  `discovery_cached`

What one cycle does, per drive:

| Transport | Per sample | At 160 drives, 60 s interval |
|---|---|---|
| NVMe | one admin command, Get Log Page 0x02 (512 B), 5 s command timeout | 160 admin commands a minute, ~2.7 a second, 8 at most in flight |
| SAS/SATA | sysfs only (`device/state`, `ioerr_cnt`, hwmon); no I/O to the drive | 160 × ~4 sysfs reads a minute |

A discovery pass (every 30 s, and on hotplug) reads sysfs attributes per
device. It sends drive I/O only for new or changed devices, plus a
refresh every 10 minutes: READ CAPACITY(16) for `sd*`, and LBA 0, the GPT
header, the GPT entries and each partition's first LBA for the slab probe.
At 160 unchanged drives that is 0 drive commands per pass, and 160 probe
sets every 10 minutes. Location re-resolution is sysfs only: it walks the
PCIe slot table per NVMe drive, about 160 × 160 small reads, tens of
milliseconds.

Not here: per-drive Prometheus series (`/metrics`) is #18. At 160 drives ×
~6 gauges it is ~1,000 series per node, ~10,000 per rack.

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
- **NVMe bays** (#15): a 160-bay NVMe chassis has no SES enclosure. Its
  bays are PCIe hotplug slots, usually behind PCIe switches and often
  inside an Intel VMD domain (`10000:01:00.0` — five-digit domains are
  BDFs too).
  - `pcie_slot` is the `/sys/bus/pci/slots/<n>` whose `address` is a device
    on the drive's PCI chain, nearest the drive first.
  - A numeric slot name (ACPI `_SUN`, the number on the chassis label) is
    also the `bay`, when nothing else gave one.
  - Under native NVMe multipath, `/sys/block/nvme0n1` is a virtual
    subsystem head with no PCIe in its path. Its `multipath/` links are
    followed, first sorted, to a controller.
  - Locate writes the slot's `attention` indicator (pciehp), else the NPEM
    `<bdf>:enclosure:locate` LED of a port on the chain (Linux 6.12+).
- **Replacement** (#15): a new drive whose `bay_key` (shelf + bay, or PCIe
  slot) is a missing drive's gets `replaces: <old id>` and a `replaced`
  event naming both serials. A missing drive still in the fleet is called
  out. If several drives went missing from that bay, the most recently
  seen one is used, and each drive is replaced at most once.
  `DELETE /api/v1/drives/{id}` forgets a missing, out-of-fleet drive's
  record and trend, so years of swaps in 160 bays do not pile up. The feed
  and UI offer it as Forget.
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

### HBAs (`hba.rs`)

Inventory only — stormdrive reports an HBA's firmware and never flashes
it; the node's own BIOS belongs to stormipmi (owner's decision on #2,
2026-09-24). Each discovery tick reads `/sys/class/scsi_host/host*`,
follows each link to its PCIe function (the last BDF in the path; USB
mass storage and non-PCI hosts are skipped) and groups hosts per function,
so an AHCI controller's per-port hosts are one HBA. Per HBA: driver, PCI
vendor:device and subsystem ids, board name/assembly/tracer, host SAS
address, and the versions the driver exposes — firmware (`version_fw`
mpt3sas, `firmware_version` smartpqi/aacraid, `firmware_revision` hpsa,
`fw_version` qla2xxx, `fwrev` lpfc), option-ROM `version_bios` and
`version_nvdata_persistent` (mpt3sas). AHCI and virtio report none.
Served at `/api/v1/hbas`, as `hba` on each topology controller (a card
with no drives still appears there), as `hba:<bdf>` components in the
feed (has_many drives), and as the UI's HBA panel. A card appearing,
going away or coming back with different firmware/BIOS/NVDATA is an
`hba` event.

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
partitions, no stormblock slab on it (`in_use_by`); NVMe is refused (namespace format is a different command).
Many drives run in parallel — the drive does the work, the host polls.
A batch request validates every drive before starting any. There is no
cancel. The result is persisted on the drive (`format`) and reported
as an event; the drive is `usable` again once the kernel re-reads it.

### Sequencing — Design, not built

There is no `sequence.rs`. What exists today is narrower:

- one test, one format and one firmware update per drive (the activity
  guard);
- fleet drives, and a drive `in_use_by` stormblock, update firmware one at a
  time behind a node-wide lock (`fleet_firmware_lock`).

Drains run in stormblock, one per drive, as many as are asked for.

The design is one node-wide queue of **disruptive operations** (firmware
update, drain, retire, qualification). Its invariants:
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
  drive (`firmware_update`). **Not built:** a redundancy check against
  stormblock before a fleet drive resets (is a rebuild running? is the
  volume already degraded?).

### Thermal
There is no `thermal.rs`. What exists:

- **Drive temperatures** come from the health sample and go through the
  threshold engine (`temp_warn_c`, `temp_crit_c` → `warning`) and the event
  stream.
- **Shelf temperatures, fans and PSUs** come from the SES status page. A
  shelf element that goes bad or recovers is a `shelf` event.
- The summary card shows the hottest drive or shelf sensor.

**Design — not built:** actuation (SES cooling-element control via SG_IO). It
is deliberately last and gated behind explicit config, because the review
found no precedent in the ecosystem and fan policy is chassis-specific.

### Events (`events.rs`)
In-memory ring of 4096, each entry `{seq, time, drive_id?, severity, kind,
message}`. It is **not persisted**: a restart starts it empty at seq 1.
Served at `GET /api/v1/events?since=<seq>`. The UI polls it. The components
feed has its own WebSocket (`/ws/components`).

## API (axum, `0.0.0.0:9092`)

```
GET  /                                 embedded UI (also /ui, /ui/; the page
                                       detects stormd's proxy prefix itself)
GET  /api/v1/health                    liveness {status, version}
GET  /api/v1/drives                    inventory (running test inlined; each
                                       drive's `usage`: capacity, slabs, left)
GET  /api/v1/drives/{id}               id = DriveId, wwid (any case), path or
                                       name, serial — in that order
GET  /api/v1/drives/{id}/health        latest HealthReport + trend
POST /api/v1/drives/{id}/locate        {"on": true|false} → SES slot LED
POST /api/v1/drives/{id}/fleet         {"action":"join","format_slab":bool,
                                        "tier"?} | {"action":"leave","force"?}
GET  /api/v1/monitor                    health-poll cost + stuck drives (#15)
DELETE /api/v1/drives/{id}              forget a missing, out-of-fleet drive (#15)
POST /api/v1/drives/{id}/designation   {"designation":"none|reserved|spare|failed"}
GET|PUT|POST /api/v1/drives/{id}/overcommit  {"enabled":false} | {"enabled":true,"ratio":2.0}
POST /api/v1/drives/{id}/overcommit/{off|<ratio>}   body-free form
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
GET  /api/v1/components                stormview feed: drives + shelves
GET  /ws/components                    the same feed, pushed on change
GET  /api/v1/topology                  controller → shelf → drive tree
GET  /api/v1/hbas                      HBAs: driver, PCI ids, firmware,
                                       option-ROM BIOS, NVDATA (inventory)
GET  /api/v1/placement                 where every drive + shelf is, for
                                       mirrors; generation, ETag, ?since=G /
                                       If-None-Match → 304
GET  /api/v1/placement/{id}            one drive's placement record
GET  /api/v1/events?since=<seq>
GET  /api/v1/summary                   stormd RemoteSummary card
GET  /metrics                          Prometheus (phase 2)
```
Errors use stormblock's `{error, code}` envelope shape for familiarity.
Auth: same posture as the rest of the ecosystem for now (none on the node
LAN); token support goes in the config from day one (`api_token`, off by
default) so it can be turned on without a format change.

### The components feed (`/api/v1/components`, `/ws/components`)
Every drive and shelf as a stormview `ComponentSummary`, so stormd, stormsh
and stormconsole render this daemon with no per-UI code: a `belongs_to
shelf` relation groups drives into shelf grids, and the actions are real
parameter-less API routes.

Placement is published as **metrics, not prose**. `detail` is a sentence for
a TUI and is free to change wording; a renderer that has to *place* a drive
reads the metrics instead:

- drive: `bay` (plain number, what a shelf grid orders by) and `hba` (the
  controller's PCIe address, else its SCSI host).
- shelf: one `hba` metric per path — each SES processor's SCSI host resolved
  to the PCIe address its drives report, plus the controllers of the shelf's
  own drives. A controller fails as a unit, so "which card are these four
  drives behind" is answerable from the feed.

### Per-drive usage (`usage` on every drive, #12)
"Look at a drive and know how much storage is left." Each monitor tick
reads stormblock's `/api/v1/slabs`. Since v17.1 (stormblock#136) every
slab names the drive it is on (`drive {serial, wwn, model, path}`; for a
slab in a partition, the disk). The join is on the WWN when the slab names
one, so NVMe-oF namespaces sharing a serial stay apart; otherwise the
serial, then the `/dev` path.

```json
"usage": { "capacity_bytes": 2000398934016,
  "slabs": [{ "id": "…", "role": "data", "tier": "cool", "slot_size": 1073741824,
              "total_bytes": …, "allocated_bytes": …, "free_bytes": … },
            { "id": "…", "role": "system", … }],
  "in_slabs_bytes": …, "used_bytes": …, "free_in_slabs_bytes": …,
  "outside_slabs_bytes": …, "free_bytes": …, "collected_at": … }
```

- `used` = Σ allocated. `free` = capacity − used: the owner's "how much is
  left".
- `free_in_slabs` is what stormblock can hand out now.
- `outside_slabs` = capacity − Σ slab slot area. It is the partition
  table, each slab's metadata region, unpartitioned space and other
  partitions. Only part of it could become a new slab.
- `usage` is `null` until stormblock's slab listing has answered once. A
  failed listing keeps the last usage along with its `collected_at`.
  A drive with no slab reads as all outside, all free.

`usage` is shown on `/api/v1/drives`, on the kube Drive `status.usage`, in
the components feed (`used`, `free` (warn under 10 %), `slabs`) and in the
UI's size column, with the per-slab split on hover. It is not part of the
placement view: it changes with every write and would churn `generation`.

### Per-drive overcommit (`overcommit` on every drive, #13)
"An attribute to drives to allow overcommit or not." The split follows
stormblock `docs/multi-drive.md` §5: stormdrive holds the setting,
stormblock enforces it when a claim binds (stormblock#152), and
rustkube-node publishes the resulting headroom to the scheduler
(rustkube-node#62).

- `overcommit {enabled, ratio}` on the drive, operator-set like
  `designation` and persisted in the inventory. Off by default (ratio 1.0):
  thin clones may not promise more than the drive's slab space. On, `ratio`
  bounds it: 2.0 promises up to twice. Accepted ratios are 1.0–16.0. The cap
  only guards against typos (20 for 2.0).
- `PUT /api/v1/drives/{id}/overcommit` sets it, and an `overcommit` event
  records the change. `GET` returns the setting, whether the engine
  has accepted it (`pushed`), and promisable, committed, written and
  headroom.
- `usage` gains `promisable_bytes` = Σ slab total × ratio (× 1 when off).
  Only slab space can hold volumes. `committed_bytes` is Σ the slabs'
  `committed_bytes` (the virtual size promised out of each), and
  `headroom_bytes` = promisable − committed. Both are `null` until
  stormblock's slab listing carries `committed_bytes`, which #152 adds. One
  slab without the figure leaves the total unknown rather than guessed.
- **Push to the engine** (fleet loop, each tick, on change): for every
  drive with slabs on it (a fleet drive, or the node's system disk):
  ```
  PUT /api/v1/drives/{path}/overcommit
  {"enabled": true, "ratio": 2.0,
   "drive": {"uuid": "…", "wwn": "naa.…", "serial": "…", "path": "/dev/sda"}}
  ```
  `drive` names it by the identity the slab listing uses, so a disk the engine
  holds as `file+…` slabs rather than as an opened drive (stormblock#133) is
  still found. A 404/405 means the engine has no route yet. That is logged
  at debug level and retried after ten minutes. What the engine last
  accepted is kept in memory only, so a restart pushes the setting once more.
- Shown on `/api/v1/drives`, the kube Drive `status.overcommit`, and the
  feed. The feed has an `overcommit` metric (when on, or when there are
  slabs), `committed`, and `headroom` (warn under 10 %), plus an
  `Overcommit 2×` / `Overcommit off` action. The UI adds a selector under
  the designation and a committed/headroom line in the size column.

### The placement view (`/api/v1/placement`, #10)
What rustkube-node attaches to each PV (rustkube-node#60) next to
stormblock's per-volume placement (stormblock#136, v17.1). stormblock names
a volume's drives by `wwn` (the raw sysfs `wwid`, `naa.…`/`eui.…`/`uuid.…`)
and `serial`; every record here carries both, so the join needs no device
name.

```json
{ "node": "storm-06f96d", "generation": 4203381229011457,
  "drives": [{ "id": "28684b23-…", "wwn": "naa.50014ee2bab11f8d",
    "serial": "WD-WX11D28JFS6T", "model": "…", "kind": "sata_hdd",
    "capacity_bytes": 2000398934016, "path": "/dev/sda", "paths": ["/dev/sda"],
    "shelf": { "key": "5000a098…", "logical_id": "5000a098…", "vendor": "NETAPP",
               "model": "DS224-12", "serial": "…", "sas_address": "…" },
    "bay": 4, "sas_address": "0x4433221106000000", "sas_phy": 6,
    "expander": null, "hba": { "scsi_host": "host0", "pcie_addr": "0000:01:00.0",
    "driver": "mpt3sas" }, "pcie_addr": null, "pcie_slot": null,
    "labels": { "shelf": "…", "bay": "4", "hba": "host0" },
    "membership": "fleet", "designation": "none", "activity": "idle",
    "health": "good", "in_use_by": null }],
  "shelves": [{ "key": "5000a098…", "status": "ok", "esp_paths": 2,
    "drives": [{ "bay": 4, "wwn": "…", "serial": "…" }], … }] }
```

- **Only placement** is in a record: shelf, bay, SAS address, `sas_phy`
  (the expander phy behind a shelf, the HBA phy when direct), `expander`
  (its SAS address), HBA, PCIe, fleet `membership`, `designation`
  (reserved/spare/failed), `activity`, and the health *state*. No
  temperatures or counters.
- **`generation`** is a 53-bit FNV-1a hash of the view. It moves when a
  drive moves bays, a shelf appears or goes, or a drive changes role or
  health state, and not on a health sample. Compare it for equality only.
  Because it hashes the content, it is unchanged across a restart when
  nothing moved. `?since=G` and `If-None-Match: "G"` answer 304 while it is
  still G.
- **Moves are seen.** A drive's location is re-resolved on every discovery
  pass, because a drive pushed into another bay can come back under the
  same `/dev` name. On the same shelf, a field a pass could not read (an
  SES page that failed) keeps its known value, and a different value wins.
  A move logs a `location` event ("sdb: moved from DS224C … bay 4 to … bay
  9"), and a fleet drive's new labels are pushed to stormblock.
- The shelves list is the SES scan plus any shelf only a drive's sysfs
  names, each with the bays it holds, so a new shelf shows up with its
  drives.

### The stormd card (`/api/v1/summary`)
Answer within 400 ms (stormd's timeout) from cached state — never collect on
demand. Health mapping: any Failed/Missing-Active → `error`; any
Failing/Warning/Draining → `warn`; otherwise `ok`. Metrics: drives total,
active, warnings, hottest °C, worst wear %.

## StormBlock integration (`stormblock.rs`, `fleet.rs`)

stormblock v11 closed the loop (stormblock#70, #71); this side closed in
stormdrive 0.5.0. `stormblock.rs` is the client, `fleet.rs` the policy the
monitor tick runs after every discovery/health round:

- **Authenticated (stormblock#107, v17).** Every engine call carries
  `Authorization: Bearer <token>`; without it all of `/api/v1` is a 401.
  The token is found the way stormblock's own CLI finds it:
  `stormblock.api_token` / `$STORMBLOCK_API_TOKEN`, then the first readable
  non-empty file of `stormblock.token_file`, `$STORMBLOCK_TOKEN_FILE`,
  `/run/stormblock/engine/api_token` (where stormcos mounts it into this
  container, stormcos#104), `/etc/stormblock/api_token`,
  `/var/lib/stormblock/api_token`. The engine mints it at boot, maybe after
  stormdrive starts, so an absent token is looked up again on every call,
  and a 401 re-reads it and retries once when it changed. `DELETE`s use
  `stormblock.admin_token` / `$STORMBLOCK_ADMIN_TOKEN` when set (the
  engine's `management.admin_token`), else the same token.

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
max_concurrent  = 8                 # health reads in flight at once
sample_timeout_secs = 10            # a read slower than this is a failed sample

[stormblock]
enabled  = true
url      = "http://127.0.0.1:9090"
auto_add = false                    # phase 4; explicit opt-in
token_file = ""                     # engine bearer token; empty = stormblock CLI lookup order
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
