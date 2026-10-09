# StormDrive Architecture

**Status:** checked against the code at v0.27.1 (d92805f), 2026-10-09.
First checked at v0.15.0 (#7), then v0.16.0. The first
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
│  location · firmware ·       │        │                             │
│  tests · format · fleet      │        │                             │
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
    contents: Option<String>,  // filesystem / slab / RAID on the disk or a GPT partition (#42)
    slab_parts: Vec<SlabPart>, // stormblock slab partitions on the disk (#58)
    engine_finding: Option<EngineFinding>, // engine runs those roles remote, or refused/failed the disk (#58)
    enrolable: bool,           // offered: blank, healthy, out of fleet (#42)
    wear_projection: Option<WearProjection>, // days to wear-out from the trend (#23)
    fleet_partition: Option<u32>,          // the worker enrolled a partition (#5)
    location: Location,
    membership, designation, activity,     // see below
    overcommit: Overcommit,    // {enabled, ratio}, operator-set (#13)
    health: HealthReport,
    usage: Option<Usage>,      // from stormblock's slab listing (#12)
    format: Option<FormatRecord>, firmware_update: Option<FirmwareRecord>,
    drain: Option<DrainRecord>, replaces: Option<DriveId>,
    pushed_labels, pushed_health, pushed_overcommit, // what stormblock last accepted
    first_seen, last_seen: SystemTime,
}
// Derived, not stored: owner() = free | stormblock | stormraid | foreign (#45),
// stormblock_path() = the enrolled partition or the whole disk.
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
Activity    = idle | testing | draining | formatting | sanitizing | updating_firmware | missing
HealthStatus = unknown | good | warning | failing | failed
HealthReport {
    status, temperature_c, power_on_hours,
    media_errors,                             // the drive's own count (NVMe log 0x02, ATA 187)
    io_errors: Option<u64>,                   // sysfs ioerr_cnt: failed commands, never a warning (#58)
    available_spare_pct, wear_pct,            // NVMe available spare / percentage used; SAS/SATA SSD wear
    critical_warning: u8,                     // NVMe bitfield, 0 elsewhere
    messages: Vec<String>, collected_at,
    not_collected: Option<String>,            // why there is no sample (#58)
    nvme: Option<NvmeCounters>,               // the rest of log 0x02 (#18)
    smart: Option<SmartCounters>,             // SAS LOG SENSE / ATA SMART (#22, #64)
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
  Designating a fleet drive `failed` reports it failed to stormblock, and
  the engine drains it on that report alone (see "Health push" below).
  With `drain_on_failing`, stormdrive also starts (or adopts) the drain and
  retires the drive when it is empty.

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
- **What else is on it** (#42): the same cached probe runs
  `contents::holds`, the drive worker's destroy guard: any filesystem
  signature, slab or RAID member on the disk or at a GPT partition start.
  The answer goes into `Drive.contents`; None means blank (or not readable
  yet: a 520-byte drive). From that and the record, each monitor tick
  sets `enrolable` (`Drive::offer_blocker`: out of the fleet, idle, no
  designation, a known health verdict that is not failing, usable sectors,
  ≥ `worker.offer_min_bytes`, blank), with an `offer` event on the
  transition. It is only an offer: `POST /api/v1/drives/{id}/enroll` (the
  feed's Enrol action) or a DrivePolicy does the enrolling. The worker's
  guard re-reads the disk before every destructive step, so a stale
  cached probe (up to `PROBE_REFRESH`, 10 min) never decides one.
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
- **SAS/SATA** (`smart/scsi.rs`). From sysfs: `device/state`,
  `device/ioerr_cnt` (commands that failed since boot, resets included:
  `io_errors`, labelled "io errs"; not media errors and not a warning, #58 —
  the Dell's "media errors growing 32 → 33" was this counter while the
  drive's own logs had none) and the hwmon temperature (drivetemp or the SAS
  driver). Then the drive itself (#22), over SG_IO on its sg node (else
  `/dev/<name>`), with a 10 s timeout per command:
  - **not `ATA`:** LOG SENSE (cumulative) of Informational Exceptions
    (0x2F). Parameter 0's ASC/ASCQ (5Dh = threshold exceeded) is the
    predicted failure; its temperature byte is used when hwmon has none.
    Temperature (0x0D) is read only if still none, and Solid State Media
    (0x11) parameter 1, Percentage Used, on an SSD. Then the error counter
    pages (#64) Write (0x02), Read (0x03) and Verify (0x05): parameters
    0003h (total corrected), 0005h (bytes processed) and 0006h (total
    uncorrected), each a big-endian counter of its own length. The
    uncorrected errors summed are the drive's `media_errors`.
  - **vendor `ATA`:** ATA PASS-THROUGH(16) SMART READ DATA (D0h) and READ
    THRESHOLDS (D1h), with LBA 4Fh/C2h. Attributes 5/197/198 are the sector
    counters, 187 (reported uncorrectable) the drive's `media_errors`, 194
    the temperature, 9 power-on hours, and 233/231/177 an
    SSD's wear (100 − normalized), 199 the interface CRC errors. The whole
    table (id, pre-fail, value, worst, threshold, raw) is kept for the
    drive history (#64), not served with health. Any pre-fail attribute at or below its
    non-zero threshold is the predicted failure. That is the same verdict
    as SMART RETURN STATUS, without needing CK_COND register readback.

  A page or command the drive refuses is not reported. Parsers are pure
  and unit-tested. `health.smart` carries `source`, `predicted_failure`
  and the counters. The threshold engine makes a predicted failure
  `Failing`, and pending or offline-uncorrectable sectors `Warning`.
- **Threshold engine** (`monitor::evaluate`, a pure function):
  - `failed`: the kernel's `device/state` is not `running`, the NVMe log
    read failed, or the read timed out (all `kernel_ok = false`). Also the
    NVMe read-only bit.
  - `failing`: NVMe reliability-degraded or spare-below-threshold bit, spare
    ≤ `spare_crit_pct`, or wear ≥ `wear_crit_pct`.
  - `warning`: NVMe volatile-backup or temperature bit, spare ≤
    `spare_warn_pct`, wear ≥ `wear_warn_pct`, temperature ≥ `temp_warn_c`
    (≥ `temp_crit_c` is still `warning`, with a different message), or
    media errors (the drive's own count) higher than the previous sample's.
  - The engine's finding (below) raises the verdict to its severity
    without hysteresis: it comes from the engine's settled report.
  - A worse verdict must repeat `hysteresis` samples in a row before it
    sticks. A better one applies at once.
- **No sample says why** (#58). `health.not_collected`: "first health
  sample due within N s", the timeout / hung-read message, "last health
  sample N s ago", or "no health sample in the N s since stormdrive
  started" (plus "its health read is hung" when the poller has it stuck).
  Three intervals without a sample is a `health` warning event, once. The
  health loop runs under a supervisor that restarts it with an error event
  if it ever ends.
- **Slabs the engine doesn't use** (#58, `engine.rs`, pure). Discovery's
  slab probe records each slab as `slab_parts {partition, name, role,
  offset_bytes}`, the role from the GPT type GUID (`SLAB` = system,
  `SLAB_DATA` = data; a whole-drive slab is `unknown` and stands for either
  half). Each monitor tick reads the engine's `GET /api/v1/health` `slabs`
  (`diskless`, `system`/`data` = local|remote|mixed|none, `local_disk
  {state: taken|refused|failed|none, drive, reason}`). A drive whose slab
  half the engine runs `remote` (or a `diskless` node) gets
  `engine_finding {severity, roles, message}`: `warning` (suspect), or
  `failing` when `local_disk` says the boot took this drive and `failed`.
  The boot's reason goes into the message when it names this drive (or no
  drive). The finding raises the verdict at once, rides in
  `health.messages`, and is an `engine` event when it appears, changes or
  clears. Owner (2026-10-08): the flow-over onto the local disk is
  mandatory; a node that cannot take its own disk is a hardware fault to
  put in front of the owner, not a quiet diskless run.
- **Wear trending**: SSDs (and any drive reporting wear) keep a ring of
  (time, wear_pct, media_errors) samples (512) persisted with the inventory.
  A sample is recorded when either value changes, or daily when neither does
  (#15). Recording every poll made 160 drives × 512 samples ≈ 5 MB of
  inventory, rewritten every tick, and the ring only covered 8.5 hours.
  `GET /api/v1/drives/{id}/health` returns it raw.
- **Days to wear-out** (#23, `wear.rs`). With every trend sample, a
  least-squares line goes through the last year's wear_pct samples. It
  gives `wear_projection {rate_pct_per_day, days_left, wear_out_unix,
  samples, span_days}` on the drive: the day the line reaches 100 %, the
  vendor's rated endurance. There is none with fewer than two distinct
  values, under a week of samples, or wear that is not growing. It is
  capped at 100 years, and 0 once the line is past 100 %. When
  `days_left` first drops under `monitor.wear_out_warn_days` (180), a
  `wear` warning event fires. The projection is on `/api/v1/drives`, the
  kube Drive status (`wearProjection`), the feed (`wear-out` in days, warn
  under 180), `stormdrive_drive_wear_out_days` /
  `_wear_rate_pct_per_day` and the drive pane.
- **Persisting** (#15): compact JSON, serialised under the inventory lock
  and written outside it (tmp + rename), skipped when unchanged. One async
  mutex spans both, so two persists never land out of order.

### Cost per poll cycle (#15)

`GET /api/v1/monitor` reports it live:
- **Stopping** (#21): SIGINT or SIGTERM ends the server gracefully. A
  `stopping` event is pushed, then `persist()` writes the event log and the
  inventory, so whatever changed since the last tick survives a stop by
  stormd or systemd (`KillSignal=SIGTERM`).
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
| SAS/SATA | sysfs (`device/state`, `ioerr_cnt`, hwmon) + LOG SENSE 0x2F/0x0D/0x11 + error counters 0x02/0x03/0x05 (SAS, ≤ 6 commands, #64) or ATA SMART READ DATA + THRESHOLDS (SATA, 2 commands) (#22) | 160 × ~4 sysfs reads + ≤ 6 drive commands a minute |

A discovery pass (every 30 s, and on hotplug) reads sysfs attributes per
device. It sends drive I/O only for new or changed devices, plus a
refresh every 10 minutes: READ CAPACITY(16) for `sd*`, and LBA 0, the GPT
header, the GPT entries and each partition's first LBA for the slab probe.
At 160 unchanged drives that is 0 drive commands per pass, and 160 probe
sets every 10 minutes. Location re-resolution is sysfs only: it walks the
PCIe slot table per NVMe drive, about 160 × 160 small reads, tens of
milliseconds.

`/metrics` (#18, `metrics.rs`) adds nothing to a poll: it renders the
cached inventory, shelves and poller stats. At 160 NVMe drives × ~25 series
it is ~4,000 series per node, ~40,000 per rack; SAS/SATA drives carry about
half that.

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
- **Verified by simulation** (#31, the owner's call: no real chassis is
  coming). `tests/chassis160.rs` writes a 160-bay chassis as a sysfs tree:
  - 120 drives behind a PCIe switch and 40 behind VMD;
  - 8 under native multipath, with their hidden path nodes;
  - slots 1–160, with `attention` on 150 and NPEM LEDs on 10.

  Then the real code runs over it. `discovery::scan_in` and
  `topology::locate_in`/`set_locate_in` are `scan`/`locate`/`set_locate`
  with the sysfs root as a parameter. The checks:
  - exactly 160 drives;
  - each drive's slot and bay equal the chassis number, and its PCIe
    address is the endpoint (under multipath and VMD too);
  - locate lights that one LED;
  - a pull disappears on the next pass;
  - a drive pushed into the same slot `replaces` the missing one.

  Poller phasing at 160 drives and the page at 160 rows have their own
  tests. Real LEDs, hotplug interrupts and NVMe admin commands wait for
  real hardware: the NetApp shelf (#30).
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
goes bad or recovers. Control is IDENT (bay and shelf locate LEDs) built
from a fresh status page, so the generation code matches and no other
request bit rides along, plus the IOM firmware download (#35, below,
under Firmware). Each ESP path carries its IOM's revision (sysfs `rev`).

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

### The drive worker (`worker.rs`, `erase.rs`, `gpt.rs`, #5)
One request prepares many drives: a **selection** (drives, shelf + bays,
model, unusable) × a list of **steps** (format → sanitize or
security_erase → partition → enroll, each at most once, in that order). A job keeps one record per
drive: state (queued, running, done, failed, refused, interrupted,
cancelled), the current step, phase and percent, and one line per finished
step. It persists in `<data_dir>/jobs.json`; the 64 most recent jobs are
kept.

- **Guard** (`worker::guard`, pure and unit-tested), run at submit and
  again before every step:
  - Refused outright: a fleet drive (leave or drain it first), busy,
    reserved, missing, or mounted.
  - A destructive step on a drive that holds data is refused.
    `contents::holds` looks for a stormblock slab or a filesystem signature
    on the disk and at every GPT partition start. The refusal holds unless
    `destroy` names the drive by id, WWN or serial: stormblock's
    `data_slab_on` rule, applied to identities that don't move.
  - Partition and enroll need a usable 512/4096 geometry, unless a format
    step comes first.
  - Enroll needs stormblock, a healthy drive, and a designation other than
    `failed`. A failed drive may still be sanitized.
  - An NVMe sanitize is controller-wide, so it is refused while another
    namespace shares the controller.
  - `security_erase` is for SATA drives only (#36).
- **Lanes:**
  - Low-level steps take a permit from the drive's HBA semaphore
    (`worker.max_per_hba`; an NVMe controller is its own lane).
  - `enroll` takes one from its failure domain's semaphore (shelf, else
    HBA; `worker.enroll_per_domain`).
  - So a shelf formats in parallel, and joins the pool one drive at a
    time. Whether formats should also go one per domain is #37.
- **Steps:**
  - SCSI format reuses `format::start` (MODE SELECT + FORMAT UNIT, TUR
    progress, rescan, verify).
  - NVMe format picks the LBA format with the right data size, no
    metadata and the best relative performance (Identify Namespace), and
    sends Format NVM without secure erase.
  - Sanitize polls log 0x81 (NVMe) or TEST UNIT READY sense 04/1B (SCSI)
    for progress.
  - Test (#40) runs `drivetest::start` and turns its bytes into percent.
    It waits for the verdict and for the test's own task to put the drive
    back to idle, so the next step's guard does not see it testing.
    Passed is a done line; failed or cancelled fails the drive job, and its
    later steps do not run (a qualify gate before `enroll`). A read-only
    test job is let past the fleet/reserved/mounted refusals, as the
    single-drive route always was.
  - ATA security erase (#36, `erase.rs`): every command goes through
    ATA PASS-THROUGH(16).
    1. IDENTIFY DEVICE: word 82/128 security state, words 89/90 erase
       times. `plan_security_erase` refuses: no Security feature set,
       frozen, an expired attempt counter, or a password this job did not
       set.
    2. A one-time printable password goes into the drive job
       (`ata_password`). jobs.json is saved and an event names it, before
       SECURITY SET PASSWORD.
    3. ERASE PREPARE, then ERASE UNIT, enhanced when supported. ERASE UNIT
       blocks, so its SG_IO timeout is the drive's estimate × 1.5 + 30 min
       (48 h when the drive gives none or says over 508 min). A failure
       before ERASE UNIT takes the password off again (DISABLE PASSWORD).
    4. IDENTIFY again: done only when security reads off. Then the
       password is cleared from the record. A failure that may leave it set
       keeps it, and the error says how to unlock the drive.
  - Partition clears the head and tail, writes both GPT copies, and waits
    for the kernel's partition node after BLKRRPART.
  - Enroll opens `/dev/<disk>1` (or the disk) in stormblock with labels
    and the stable uuid, and formats a slab with `role`. The drive records
    `fleet_partition`, so every later engine call (labels, health, drain,
    overcommit, leave) uses the partition path (`Drive::stormblock_path`).
- **Restart** (`worker::recover`):
  - A SCSI format or sanitize, or an NVMe sanitize, that was running is
    re-attached and watched to its end (the drive keeps going without us).
  - An NVMe format or a partition that was in flight, and every queued
    drive, becomes `interrupted` with the reason. So does an ATA security
    erase: it can't be watched, and the reason includes the recorded
    password. Its resume checks IDENTIFY and, with security still on,
    erases again with that password instead of setting a new one. Nothing destructive
    re-runs until `POST …/resume`.
  - Drives a run outside the worker left busy (#39 — `format::start`,
    `drivetest`, `firmware`: their handles are in memory, `activity` is
    persisted): `worker::orphan_action` decides per drive. A SCSI format
    with a `running` record is re-attached (`format::reattach`: TUR to
    ready, rescan, READ CAPACITY must show the target block size, else
    `failed`). Any other `formatting`/`sanitizing` no running job step
    owns, `testing` and `updating_firmware` go back to `idle`; the format
    or firmware record becomes `interrupted` with the reason, and a
    `restart` warning event names the drive. `draining` (the fleet loop
    resumes it) and `missing` are left alone. `main` awaits all of this
    before the monitor and the API start.
- The page submits and follows jobs (Prepare, Jobs, #38); stormconsole
  sees the `prep` metric in the feed.

### Cost at enrol: metadata only (#72)

Owner's rule (2026-10-08): nothing on a create, enrol, install, import or
replace path may cost O(capacity) — a 15 PB SSD, servers with hundreds of
drives. What every path that touches a drive costs (bytes, whatever the
size):

| Path | Code | Bytes touched | Verdict |
|---|---|---|---|
| Discovery: READ CAPACITY, INQUIRY/VPD | `discovery/`, `scsi.rs` | a few CDBs | metadata |
| Contents probe (slab/fs/RAID signatures) | `contents.rs` | LBA 0, the GPT, each partition's first blocks; cached per drive | metadata |
| Worker `partition` | `worker.rs` `write_layout` | 4 MiB head + 4 MiB tail cleared, both GPT copies (~8 MiB) | metadata (tested at 1 PiB: bytes counted) |
| Worker `enroll`, fleet join, `auto_add`, DrivePolicy enrol | `worker.rs`, `fleet.rs`, `controller.rs` | stormdrive: none (open + labels over HTTP). The engine's `POST /api/v1/slabs` zero-fills its whole slot table (~64 GiB per PiB) | **O(capacity) in the engine → stormblock#363** |
| Tests `smoke`, `destructive_sample` | `drivetest.rs` | 16 MiB sampled | constant (tested at 1 PiB) |
| Test `read_scan` | `drivetest.rs` | the whole surface | explicit only: an operator's test or worker step, never on a policy or auto path |
| Worker `format` (FORMAT UNIT, NVMe Format NVM), `sanitize`, `security_erase` | `format.rs`, `erase.rs` | one command; the drive does the work | explicit, device-side, background, per-HBA bounded, re-attached after a restart |
| DrivePolicy `reformat` before enrol (520-byte drives only) | `policy.rs` | the drive's own FORMAT UNIT (hours on an HDD) | opt-in per policy; on the policy's enrol chain → owner decision (#73) |

stormdrive's side of enrolling is metadata only. The engine's slab format is
not yet; until stormblock#363 makes it so, the engine client gives `POST
/api/v1/slabs` one try of up to 15 min (`ENGINE_FORMAT`): a 5 s try cut off
was a format the engine abandoned halfway, so a large drive never enrolled.
A slow format still holds the fleet loop when `auto_add` is on; the worker
runs it on its own lane.

### Sequencing — Design, not built

There is no `sequence.rs`. What exists today is narrower:

- one test, one format and one firmware update per drive (the activity
  guard);
- fleet drives, and a drive `in_use_by` stormblock, update firmware one at a
  time behind a node-wide lock (`fleet_firmware_lock`);
- **the redundancy gate (#24).** Under that lock, before the download (mode
  0x07 activates on the last chunk), `firmware::wait_redundant` asks the
  engine for the drive's volumes (`GET /api/v1/volumes?placement=true`,
  `usage::volumes_on`). `redundancy_blocker` refuses while any of them has
  redundancy other than `healthy`, a rebuild owed or running, or its slab
  on this drive not `ok`. An engine that reports no placement cannot be
  checked, so that is a refusal too. It re-checks every 30 s for up to
  `firmware.redundancy_wait_mins` (30), with the reason in the run's
  phase, then fails the update "not started — …". After a successful
  update it waits the same way for the volumes to be redundant again
  before the lock passes to the next drive, with a warning event if they
  are not. `force` skips it.

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
- Shelf IOMs (#35, `iomfw.rs`), all over SES diagnostic page 0x0E:
  - **Download.** The image is sent in Download Microcode Control pages:
    subenclosure, the expected generation code from the status page, mode
    0x07 (offsets, save, activate), buffer offset, image length and chunk
    length. Chunks are `firmware.chunk_kib`, padded to 4 bytes, at most
    65,508 per page.
  - **Watching.** Progress comes from the Download Microcode Status page.
    The run refuses an IOM already busy or an image over its `max_size`,
    and stops at the first error status (≥ 0x80). After the last chunk it
    waits up to 15 min for a terminal status (0x10–0x13), or for an IOM
    that dropped off and came back idle (restarted on new code). It finds
    the IOM again by SAS address and reads its new revision from sysfs
    `rev`.
  - **One IOM at a time.** The ESPs go in SCSI-id order. Before the next
    one, every drive on the shelf must have its path count back (10 min,
    then the run stops and says which). It is refused when a drive serving
    data would lose its only path (`path_loss_blocker`), unless
    `allow_path_loss`.
  - **Records.** Runs are in memory per shelf, like drive firmware runs;
    events go out on start, per IOM and at the end.
- Policy: never automatic. Out-of-fleet drives update in parallel;
  fleet drives one at a time behind a node-wide lock; Failing/Failed
  drives are refused unless `force`. One, many, or every drive of a
  model (`POST /api/v1/firmware {drives|model, image}`), validated
  all-or-nothing before any starts. The last update is persisted on the
  drive (`firmware_update`). A data-serving drive waits for its volumes to
  be redundant before the reset and after (#24, above).

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
found no precedent in the ecosystem and fan policy is chassis-specific. The
scope is your decision (#32).

### Drive history and assets in system-data (`history.rs`, `assets.rs`, #64)

app-system-data (stormcos `docs/SYSTEM-DATA.md`, stormcos#456) is the
node's record of itself: a volume in the data half that every install
keeps and only a node reset wipes. stormblock#355 makes and mounts it; stormcos mounts it
into stormdrive's unit; stormdrive writes two of its parts under
`history.dir` (default `/data/system-data`). The directory is never
created: one on the install-wiped root would look kept and not be. While
it is absent nothing is written, and `GET /api/v1/history` says why. It is
checked on every write, so a volume mounted later is picked up.

**Drive history** (`history/drives/<key>/<YYYY-MM>.jsonl`). The key is
what the drive is, not where: `wwn-<WWID>`, else `serial-<model>-<serial>`
(sanitised). Each line is a `Record`: `at`/`unix`, `boot_id`, `node`,
`kernel`, `stormdrive`, `why` (`first`/`changed`/`heartbeat`), `drive`
(id, wwn, serial, model, firmware, kind, path, bay key, capacity),
`status`, `temperature_c`, `counters` (a flat name → value map: health
fields, `nvme.*` = the whole log 0x02, `smart.*`, `sas.<page>.<counter>`,
`ata.<id>` for lifetime ATA counts), `predicted_failure`,
`ata_attributes` (the SATA table) and `findings`.

A record is due (`history::due`, pure) on the drive's first sample, when a
counter whose rule says `writes` changes, when the verdict, firmware, bay
or predicted failure changes, when there are findings, and otherwise every
`heartbeat_secs` (3600). Counters that move on every read on a busy drive
(bytes, commands, busy minutes, power-on hours, SAS corrected/bytes) do not
force a record on their own: at the 60 s poll, a record per sample is about
90 MB a day for 160 drives; this is a few records an hour per drive. Setting
`heartbeat_secs` to the poll interval writes every sample. Lines are
appended and `fdatasync`ed; a new month prunes months older than
`keep_months` (24).

**Findings** (`history::findings`, pure). Each counter has a rule
(`history::spec`): error counters (`media_errors`, reallocated, offline and
reported uncorrectable, CRC, `sas.*.uncorrected`, `nvme.error_log_entries`)
are findings when they grow or go back; pending sectors when they grow;
lifetime counters (power-on hours, bytes, commands, cycles, unsafe
shutdowns, wear, the ATA counts) when they go back. Gauges (`io_errors`,
which is per boot and not media errors (#58), available spare, the critical-warning bits)
never are. The comparison is with the drive's last record, which is read
back from its newest file the first time the drive is sampled (a torn last
line is skipped). So it spans a restart and an install: `other_boot` says
the previous record came from another boot. A finding is in the record and
a `history` warning event. The same last record gives the threshold
engine its media-error baseline for the first sample after a restart or
install, so growth across an install is a `warning` too.

**Assets** (`assets/<YYYYMMDDTHHMMSSZ>-<boot_id>.json`). After every
discovery pass, `assets::host_items` (DMI from `/sys/class/dmi/id`,
`/proc/cpuinfo` per `physical id`, SMBIOS from
`/sys/firmware/dmi/tables/DMI`: type 17 memory devices with a module, type
38 IPMI interface; `/sys/class/net/*` with a `device`; `/sys/class/nvme/*`
with transport `pcie`) and `assets::scanned_items` (the HBA and SES scans,
every present drive by bay key, else by WWN/serial) make a map of items
under stable keys. The map is compared with this boot's file. A new boot
writes a new file with `changes` against the newest other boot's file
(`assets::diff`: added, removed, `field: old → new` on the item's
fields) and an `assets` event (a warning when something was removed). An
unchanged map writes nothing. A changed one within the boot rewrites the
file and appends to `changed_in_boot`. The SMBIOS parser walks the
structures (formatted area, then a string set ending in two NULs), stops at
type 127 or a truncated structure, and is unit-tested on a synthetic
table. Kernel `MemTotal` is recorded and not compared (it moves with the
kernel's reservations). DMI serials and the SMBIOS table are root-only:
without root they are null or absent. The newest 1000 boot files are kept.

### Events (`events.rs`)
A ring of 4096, each entry `{seq, time, drive_id?, severity, kind,
message}`. The newest 512 and the next seq are kept in
`<data_dir>/events.json` (#25). It is written with the inventory
(`AppState::persist_events`, tmp + rename, only when the seq moved), so it
is at most a monitor tick behind. On start, `EventLog::restore` reloads
them and the sequence continues: a poller holding `since=N` never sees it
go backwards. A `restart` event says how many were kept. An unreadable
file starts a new log at seq 1. The response carries `started` (when this
process began) and `persisted`.
Served at `GET /api/v1/events?since=<seq>`. The UI polls it. The components
feed has its own WebSocket (`/ws/components`).

## API (axum, `0.0.0.0:9092`)

The route table, request bodies and handle resolution are in the
[README](../README.md#api). Some design points:

- **Every action has a body-free form** (`…/locate/on`, `…/fleet/join`,
  `…/designation/spare`, …), because a stormview renderer invokes method +
  path with no body. The body-free join never formats a slab. That
  destructive choice needs the JSON body.
- **Kubernetes-shaped resources** (`src/api/kube.rs`, stormblock#80):
  `/apis/storage.storm.io/v1/{drives,enclosures}` with API discovery
  (`/apis`, `/apis/storage.storm.io`, `/apis/storage.storm.io/v1`),
  `?watch=1` (newline-delimited `{type, object}`), `labelSelector`, and
  `PATCH` of a Drive's writable spec (`designation`, `fleet`, `drain`,
  `locate`). Each writable field maps onto an existing REST verb. Every object
  is a projection of the inventory; there is no second store.
- Errors use stormblock's `{error, code}` envelope.
- **Writes need a storage-admin (#45, `kubeauth.rs`).** A middleware
  classifies every non-GET (but a worker job dry run, gated as a read) into
  a `storage.storm.io` resource + verb and
  checks the bearer: the node-local admin token, or TokenReview +
  SubjectAccessReview against the apiserver (cached a minute). The decision
  rides into the handler as a `Requester`; a worker job keeps it and the
  worker re-checks it (uncached) before each step. Every decision is an
  audit line (log, `<data_dir>/audit.log`, event ring). A client certificate
  from the node CA is reviewed the same way, as its CN and O groups.
- **TLS, and nothing anonymous but health (#19, `tls.rs`).** One listener
  tells a TLS handshake (first byte 0x16) from plain HTTP. TLS serves the
  stormcert pair (`/data/stormcert/stormdrive.{crt,key}`, re-read when it
  changes) and verifies optional client certificates against the node CA;
  the peer (TLS or not, the certificate's CN/O) rides into the guard as
  `ConnectInfo<Peer>`. Plain HTTP answers health only, so stormd's liveness
  probe is unchanged. Reads need the admin token, a node-CA certificate, or
  a bearer allowed `get` on `storage.storm.io` (`storage-viewer`); the
  page's shell answers 401 with the page so it can sign in, its assets are
  open. `[api] allow_anonymous` is the rollout: plain HTTP and
  credential-less reads served, sent credentials still checked.
- **Drives and operations as Kubernetes objects (#45, `controller.rs`).**
  With an apiserver, a loop (every `kubernetes.interval_secs`) writes one
  `Drive` per inventory drive (merge-patch + `/status`, only when changed or
  every 5 min; deletes this node's objects whose drive was forgotten) and
  runs this node's `DriveOperation`s: requester from the apiserver's stamp
  (`storage.storm.io/requester`, rustkube#210; none → Refused), a fresh
  SubjectAccessReview, `worker::submit_for` (the job records the operation
  name), status from the job, a Kubernetes Event per transition. Plain
  reqwest (`kubeapi.rs`), no kube-rs. The CRDs and the controller's role are
  `deploy/crds.yaml` and `deploy/rbac.yaml`.
- **Drives enrolled by policy (#50, `policy.rs`, stormcos#251).** stormcos's
  two storage nodes have SAS HDDs on both, but need different tiers
  (stormblock1 `warm`, stormblock2 `cool`, stormcos `docs/STORAGE-TIERS.md`).
  `tier_map` is per drive kind, in a config every node shares, and
  `auto_add` skips a 520-byte drive. So a `DrivePolicy` object decides
  instead:
  - **Pure decision (`policy.rs`).** The spec is parsed and refused when it
    names no node, has an unknown tier, or asks for a reformat other than
    512/4096. `selects_node` matches by name and/or the Node's labels.
    `selects_drive` ANDs kind, size, sector size, model, shelf and bays.
    `requireDataSlab` (#42) holds a node at phase Waiting until the
    engine's slab listing shows a data slab on one of its drives
    (`node_has_data_slab`).
    `verdict` per drive is one of:
    - **Pass:** in the fleet, missing, or already in a job.
    - **Skip, with why:** designated reserved/spare/failed, `in_use_by`,
      busy, failing, a 520-byte drive with no `reformat` in the policy, or
      a usable drive with no health verdict yet.
    - **Run, with steps:** `format` (only when `needs_reformat()`), then
      `partition` and `enroll {role, tier}`.

    `retry_blocked` keeps a drive whose job failed, was refused or was
    cancelled waiting until the policy's generation changes.
  - **Controller pass, after the operations.** A 404 on the list means the
    CRD is not installed: quiet. A policy whose spec cannot be parsed is
    reported Invalid only by the nodes its `spec.nodes` names. The stamped
    requester is re-checked for `create driveoperations`, what the worker
    re-checks per step.
  - **Contents guard.** Before a drive is handed out,
    `worker::context` + `guard` read the disk: a slab, RAID set or
    filesystem on it is a skip. A skipped drive is probed again after
    5 min.
  - **Jobs.** One worker job is submitted per distinct step list, tagged
    `policy`. Records follow the worker's per-drive state; Events go out on
    Accepted, Enrolled and Failed. A job a restart or an unreachable
    apiserver interrupted is resumed under the requester. A deleted policy
    cancels its jobs' queued steps.
  - **Status.** Each node writes only `status.nodes.<node>` (a merge patch
    on its own key), and only when that part changes.
  - **Scheduling.** The low-level steps are bounded per HBA
    (`worker.max_per_hba`) and the enroll runs one per failure domain, as
    for any job. Whether low-level steps should also run one per domain is
    #37.
- **`/metrics`** (#18, `metrics.rs`): Prometheus text, a read like any
  other (#19), from cached state. `smartctl_exporter` names (`smartctl_device_temperature`,
  `_power_on_seconds`, `_percentage_used`, `_media_errors`, …) where one
  fits, `stormdrive_*` otherwise; every drive series is labelled `device`,
  `serial`, `model`, `enclosure`, `bay`. SAS/SATA `ioerr_cnt` is
  `stormdrive_drive_io_errors_total`, never `media_errors`. The NVMe log
  0x02 counters beyond health (spare threshold, data units, power cycles,
  unsafe shutdowns, error-log entries) are kept on `HealthReport.nvme`.

### The page (`web/`, #6)
Built for hundreds of drives: a stormview `DataGrid` whose top rows are
groups, with each group's drives in a nested grid. The groups are:

- one per shelf (by shelf key; an SES shelf with no drives still shows);
- one per HBA with direct-attached drives, plus a card with no drives at
  all;
- NVMe on PCIe;
- unlocated.

A collapsed group renders one row. Rows are keyed by id, so the 4 s poll
diffs cells instead of rebuilding a table. That rebuild was the old
vanilla page's problem: every 4 s the whole table went through `innerHTML`,
which reset open selects and scroll.

DataGrid cells are text, `health` (HealthDot), `metrics` (toned values) or
`actions` (buttons). So a row stays compact, and everything else lives in a
side pane opened by clicking the row. The drive pane holds designation,
overcommit, tests, format, firmware, locate, usage, drain and progress. The
shelf pane shows SES elements and has locate, reformat and Prepare shelf.
The HBA pane shows firmware, BIOS and NVDATA.

**Selection:** a ticked group means every drive it shows under the current
filter (`selectedDrives` in `web/src/lib/model.js`). The bulk bar sends
only the eligible drives and says how many it skipped:

- format (and every other preparation step) goes to the drive worker
  through the Prepare pane (below); firmware goes through the batch
  endpoint;
- a bulk smoke test or read scan is one worker job (`test` step, #40): it
  runs to the end with the tab closed, and Jobs shows each verdict;
- designation and locate are per-drive calls, 8 at a time.

**Prepare and Jobs (#38).** `Prepare.svelte` builds a worker request from
the selection (`{drives}` from the bulk bar or a drive, `{shelf}` or
`{shelf, unusable}` from the shelf pane) and the steps
(`web/src/lib/worker.js`, unit-tested). It always previews first: a
`dry_run` (gated as a read) returns `runnable` and `refused`
with reasons. Refusals that say a drive *holds* a slab or a filesystem are
the ones `destroy` lifts. The pane asks for each such drive's serial, typed
exactly (a /dev name never counts, as on the server), and sends the
serials it matched as `destroy`. Any change to the steps throws the preview
away. `Jobs.svelte` lists `/api/v1/worker/jobs` (polled with the rest), open
first, with per-drive state, step, progress and error, Cancel and Resume.
The page's formats used to call `/api/v1/format`; they go through the
worker now, so they survive a restart and get the destroy guard.
`/api/v1/format` stays for API callers.

**Sign in (#47, #19).** `lib/api.js` keeps a bearer in `sessionStorage`
and sends it on every request (reads need one since #19). Errors keep the
HTTP status and the envelope's `code`; a read refused 401 opens the sign-in
box instead of an error banner, and a 403 reads "needs storage-admin". While `/api/v1/health` → `writes.gate` is `enforce` and no
bearer is set, write controls sit in a `<fieldset disabled>` (the bulk
bar's actions, the drive and shelf panes, firmware upload) and row actions
are off. The server decides regardless.

The eligibility rules in `model.js` mirror `src/drive.rs`'s guards so the
page offers only what the server would accept. The server still checks
every request.

`web/dist` is committed (stormd's and stormconsole's convention), built on
dev by `web/rebuild.sh`, and embedded by `include_str!`. Asset URLs are
relative, so the same build works at `/`, `/ui/` and under stormd's
`/ui/proxy/stormdrive/`.

### The components feed (`/api/v1/components`, `/ws/components`)
Every drive, shelf and HBA as a stormview `ComponentSummary`, so stormd,
stormsh and stormconsole render this daemon with no per-UI code. A `belongs_to
shelf` relation groups drives into shelf grids, an HBA `has_many` drives, and
the actions are real parameter-less API routes. `/ws/components` recomputes
the feed every 2 s and sends it when it changed.

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
the components feed (`used`, `free` (warn under 10 %), `slabs`, `volumes`)
and in the UI's size column, with the per-slab split on hover. It is not
part of the placement view: it changes with every write and would churn
`generation`.

**The volumes on the drive (`usage.volumes`, #26, stormconsole#29).** The
question is "which volumes am I about to lose if this drive goes". The
console can read every node's stormdrive, but only its own node's engine
(each engine is behind that node's token, stormblock#107). stormdrive
already reads its own engine with the node token, so on the same tick it
also reads `GET /api/v1/volumes?placement=true` (stormblock v17.1, #136).
That listing gives, per volume, the slabs and drives holding its legs and
each slab's state; v18.1 adds the consumer. `usage::volumes_on` turns it
round to give the volumes per drive, the same reduction as stormconsole's
`plugins/stormblock/src/placement.rs`:

```json
"volumes": [{ "id": "…", "name": "db", "kind": "volume",
              "consumer": { "kind": "PersistentVolumeClaim", "namespace": "shop", "name": "db" },
              "bytes": 8589934592, "legs": 8, "shared_legs": 0,
              "state": "ok", "rebuild": "none", "policy": "mirror2", "health": "healthy" }],
"volumes_collected_at": …
```

- Drives are matched like slabs: by WWN, then serial, then path. A fabric
  drive (`nvme-tcp://host/…`) belongs to another node, so it never matches.
- `bytes` and `legs` are this volume's on this drive. `shared_legs` counts
  legs shared with another volume (a clone and its golden). `state` is the
  worst state of the volume's slabs on this drive (`missing` > `failed` >
  `quarantined` > `draining` > `ok`). `rebuild`, `policy` and `health` are
  volume-wide. Entries are sorted largest first.
- **Absent, not empty**, when the engine lists volumes and none carries a
  placement (an engine before v17.1). A reader can then tell "not reported"
  from "nothing here". An engine with no volumes gives `[]`.
- The engine walks every volume's extent map to answer, so this request
  gets a 30 s timeout instead of the client's 5 s. If the read fails, the
  last answer is kept along with its `volumes_collected_at`, while the slab
  numbers are refreshed.
- Shown in the drive pane ("Volumes on this drive", with any volume in
  trouble marked), as the feed metric `volumes` (warn when one is in
  trouble here), and as `stormdrive_drive_volumes` / `_volumes_degraded`.

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
What rustkube-node is to attach to each PV (rustkube-node#60, open — not
in rustkube-node yet; the PVs themselves are served by stormblock's
built-in driver, stormdrive only says where their drives are) next to
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
It answers within stormd's 400 ms timeout from cached state and never
collects on demand. The shape is `{health, detail, metrics}`:

- `error`: any drive health Failing/Failed, designated failed, or missing,
  or any shelf with a bad overall status.
- `warn`: any drive health Warning, any drive draining, or any unusable
  (520-byte) drive.
- `idle`: no drives.
- `ok`: otherwise.

Metrics: Drives, Fleet, and Hottest °C and Worst wear % when known. Spare,
Attention, Reformat and Shelves appear only when non-zero.

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
  and a 401 re-reads it and retries once when it changed.
- **Destructive verbs (stormblock#274).** Slab format (`POST
  /api/v1/slabs`) and drive close (`DELETE /api/v1/drives/{id}`) need the
  engine's admin token or a Kubernetes bearer the engine's
  SubjectAccessReview allows (`storage.storm.io` `slabs` create, `drives`
  delete). stormdrive tries, in order and on to the next after a 401/403:
  the admin token (`stormblock.admin_token` / `$STORMBLOCK_ADMIN_TOKEN`, then
  `stormblock.admin_token_file` / `$STORMBLOCK_ADMIN_TOKEN_FILE` /
  `/run/stormblock-admin/admin_token`, re-read every call), its own
  `[kubernetes]` credential (granted by `deploy/rbac.yaml`'s
  `stormdrive-engine` + `stormdrive-controller`), then the node token (an
  `audit` engine). A drain cancel is ordinary: the node token.

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
  `POST …/health {state, drain: false}`: Failing/Failed → stormblock
  quarantines the drive's slabs and every redundant volume stops reading
  that leg *before* an I/O fails; Good/Warning → `healthy`, which lifts the
  quarantine (`Drive.pushed_health`; `stormblock.push_health`). A fleet
  drive that went missing is reported `missing`, named by its uuid because
  its `/dev` path is gone (#44). Before #44 missing drives were never
  reported, so a pulled RAID member was only noticed by its I/O errors. The
  answer's `raid_member {array, slot, failed}` goes into the event. What
  the engine does on its own with the report (stormblock `mgmt/api/drives.rs`,
  checked 2026-10-06, #43):
  - `failed` (and `missing`) **drains the drive whatever `drain` says** —
    `drain` only adds a drain to `failing`/`degraded`. A drive whose slab
    holds the volume metadata is not drained.
  - `failing`/`failed` (and `degraded`) **rebuild** the drive's redundant
    volumes from their surviving members when the engine's `[rebuild]
    automatic` is on (its default). While that rebuild runs, the engine
    holds the drain until it finishes, and a `POST …/drain` answers 409.
- **Fault LED for RAID sets (#44, stormblock#252).** Each tick,
  `GET /api/v1/arrays` (404 = an engine without arrays: nothing). Then
  `failed_member_bays` (pure, tested) works out `<shelf key>/<bay>` for
  every `failed` member:
  - the member's `drive.uuid` is the uuid we registered it with, so our
    record gives the bay even after the drive is pulled;
  - else its WWN or serial;
  - else its `shelf=…/bay=…` labels.

  `ses::set_fault` sends a page-0x02 control element with RQST FAULT
  (byte 3 bit 5) and IDENT kept as the status page shows it. LEDs we lit
  are kept in `Inventory.fault_bays` and put out when their bay has no
  failed member left (a replace and rebuild). Events go out on both. A
  shelf not visible over SES is retried next tick.
- **Drain → retire.** A fleet drive that goes Failing/Failed, or is
  designated Failed by an operator, or is asked to `leave` with `"drain":
  true`, gets `POST …/drain`; the tick polls `GET …/drain` and records it on
  the drive (`Drive.drain`, activity `Draining`). When stormblock says
  `empty`, the drive `DELETE`s out of the fleet, the locate LED comes on and
  an event says *safe to pull*. `stuck` is an error event and the drive
  stays quarantined. A start the engine refuses (409 during a rebuild, or
  no answer) leaves the drain `pending`: every fleet tick tries again, and
  the first try after the rebuild adopts the drain the engine started by
  itself, so the drive still retires. A drain the engine forgot (its
  restart) goes back to `pending` the same way. A pending health drain is
  dropped if the drive reads healthy again.
  `stormblock.drain_on_failing = false` stops stormdrive starting, tracking
  and retiring automatic drains — it does **not** stop the engine draining
  a drive reported `failed` (or rebuilding it); that drive is drained but
  stays in the fleet with its LED off until an operator acts.
  `POST /api/v1/drives/{id}/drain[?leave=true]` is the manual drain.
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

What the engine decides on its own is the health report's consequence: it
drains a `failed` drive and rebuilds a failing one's volumes (above).
stormdrive decides when to report, whether to drain a `failing` drive
(`drain_on_failing`), and when a drive leaves the fleet. `push_health`
sends `drain: false`; to keep a failed drive from being drained, don't let
it be reported `failed` (`push_health = false`, which also gives up the
quarantine).

### Retries (`retry/`, #71)

Every engine call (and every apiserver and test-container call) goes
through `retry::with_backoff` under a named policy (`ENGINE` 4 tries / 30 s,
`ENGINE_SLOW` for the placement walk, `ENGINE_FORMAT` for a slab format —
15 min a try, #72 — `KUBE`, `TEST`). Reads and writes that
set a value retry any transient failure; a drive open, slab format and drain
start retry only a connection that never went out — the fleet loop's own
backoff, pending-drain retry and reconcile are what repeat those. Giving up
is a `retry::Infra` error: the REST API answers it 503 `unavailable`, not
502. The README's "Remote calls and retries" lists every call site.

## Config (`/etc/stormdrive/stormdrive.toml`)

Every key, with its default, is in the [README](../README.md#configuration)
and in [deploy/stormdrive.example.toml](../deploy/stormdrive.example.toml). A
test (`config::tests::example_config_is_the_defaults`) fails when the example
and `Config::default()` disagree. A missing file means defaults (stormblock
convention), and CLI flags override the file.

## Deployment

- A static musl binary, `x86_64-unknown-linux-musl`, built by `sc-build`
  and, for release, by stormcentral into a golden.
- **On stormcos:** a stormd-based service golden. stormcos's
  `service_golden` recipe writes the config from stormcentral's registry
  entry (`listen_addr` 0.0.0.0:9092, `data_dir` /var/lib/stormdrive,
  `[api] allow_anonymous = true` for the #19 transition, `[kubernetes]`
  with the node apiserver and `controller = false` until stormcos#369) and
  a stormd config with an HTTP liveness probe on `/api/v1/health`. The
  container gets:
  - the host network;
  - the host's `/dev`;
  - the host's `/sys`, read-only, which blocks sysfs LED and rescan writes
    (stormcos#166);
  - `/run/stormblock` for the engine token;
  - `/data/stormcert` for the serving pair, the node CA and stormdrive's
    apiserver token (the pair itself: stormcos#352);
  - `/data/system-data` for drive history and assets, once stormcos mounts
    it (stormcos#456); until then history is off.

  It is started on every node profile. See the README's "How it ships".
- **Elsewhere:** the systemd unit in `deploy/systemd/` (After
  network-online and stormblock-target, runs fine without stormblock), or a
  stormd `[[process]]` with the `[process.ui]` block in
  `deploy/stormd-ui.toml` for the dashboard card and proxied page.
- It needs root, or at least CAP_SYS_ADMIN + CAP_SYS_RAWIO, for SG_IO and
  the NVMe admin ioctl, and a writable sysfs for LEDs and rescans.

## Testing

- **Unit tests beside the code** (`cargo test` over the workspace: 241 in
  the daemon at v0.27.1, plus `retry/`'s and `test/`'s), and integration
  tests that run the real daemon: `tests/suites.rs` (the three suites),
  `tests/kube.rs` (stand-in apiserver: gate, DriveOperation controller,
  DrivePolicy), `tests/restart.rs` (restart recovery, SIGTERM),
  `tests/tls.rs` (TLS and credentials on :9092) and `tests/chassis160.rs`
  (a 160-bay NVMe chassis as a sysfs tree, #31). A GPT the
  worker writes is also read back by `sfdisk --json` / `--verify` where the
  build box has util-linux:
  - on synthetic data: the threshold engine and damper, identity
    derivation, multipath grouping, replacement by bay, config and the
    example file, SCSI sense/CDB/page parsers, SES page assembly, the slab
    and GPT probe, usage joins, overcommit, placement hashing, the poller's
    schedule and timeouts, and the HBA sysfs parsing;
  - the stormblock client against a stand-in engine (token absent, minted,
    rotated).
- sysfs, ioctl, SG_IO and netlink code runs only on Linux, and is exercised
  by running the daemon on a node: R230 for SATA behind mpt3sas, stormblock1
  for the NetApp shelf.
- **Test containers** (`test/`, #11): short / medium / long suites that
  drive a node's stormdrive over its API, run by stormcentral on every test
  machine. They are never destructive; `test/src/pick.rs` holds the rules.
  `tests/suites.rs` runs all three against the real daemon on every
  sc-build. See the README's "Test containers".
