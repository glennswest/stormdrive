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
Location {
    pcie_addr: Option<String>,     // NVMe: BDF of the controller
    pcie_slot: Option<String>,     // /sys/bus/pci/slots physical slot label
    sas_address: Option<String>,
    enclosure: Option<String>,     // SES enclosure identifier
    bay: Option<u32>,              // slot index within the enclosure
    scsi_host: Option<String>,     // hostN (which HBA)
}
DriveState  = Discovered → Available → Active → Draining → Retired
              (∗ → Failed, ∗ → Missing when the device node disappears)
HealthStatus = Unknown | Good | Warning | Failing | Failed
HealthReport {
    status, temperature_c, power_on_hours, media_errors,
    available_spare_pct, wear_pct,            // NVMe percentage_used / SSD endurance
    critical_warning: u8,                     // NVMe bitfield, 0 elsewhere
    messages: Vec<String>, collected_at,
}
```

State meanings: `Discovered` — seen, identified, not yet offered anywhere.
`Available` — qualified, eligible for stormblock. `Active` — registered with
stormblock (present in its `/api/v1/drives`, and/or carrying a slab).
`Draining` — being evacuated ahead of retirement/failure. `Retired` —
deliberately withdrawn. `Failed` — health said so. `Missing` — inventory
remembers it, the node can't see it (pulled, dead, cabling).

The inventory (all `Drive` records + wear-trend samples) persists to
`<data_dir>/inventory.json` with atomic tmp+rename writes, so identity,
first_seen, and trend history survive restarts — deliberately the opposite of
stormblock's in-memory `Vec<DriveInfo>`.

## Subsystems

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
GET  /api/v1/health                    liveness {status, version}
GET  /api/v1/drives                    inventory (all states)
GET  /api/v1/drives/{id}               id = DriveId, path, serial, or wwid
GET  /api/v1/drives/{id}/health        latest HealthReport + trend
POST /api/v1/drives/{id}/locate        {"on": true|false} → SES slot LED
POST /api/v1/drives/{id}/state         explicit transitions (retire, drain…)
GET  /api/v1/events?since=<seq>
GET  /api/v1/summary                   stormd RemoteSummary card
GET  /metrics                          Prometheus
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
