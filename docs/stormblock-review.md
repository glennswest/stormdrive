# StormBlock review — the facts stormdrive is designed against

Reviewed 2026-08-26 against stormblock v9.13.0 (`/Volumes/minihome/gwest/projects/stormblock`).
File:line references are to that tree at that date.

## 1. How stormblock models a drive

- Core trait `BlockDevice` (`src/drive/mod.rs:142`): async read/write/flush/
  discard + `smart_status()` (default: `healthy: true`, everything else
  `None`) and `media_errors()` (default 0).
- Identity `DeviceId { uuid, serial, model, path }` (`mod.rs:33`). **The uuid
  is minted `Uuid::new_v4()` on every open** (`sas.rs:62`, `filedev.rs:80`,
  `iscsi_dev.rs:649`, `partition.rs:43`) — it changes on every restart. The
  slab header stores `device_uuid` at format time (`slab.rs:424`) and
  `Slab::open` never checks it (`slab.rs:488`).
- `DriveType`: `NVMe | SasSsd | SasHdd | File | Iscsi` — but the NVMe/VFIO
  driver is non-functional scaffolding (`nvme.rs:1-6` says so;
  `NvmeDevice` doesn't implement `BlockDevice`, nothing constructs it, and
  its DMA setup writes virtual addresses into ASQ/ACQ with no
  `VFIO_IOMMU_MAP_DMA`). **In practice `/dev/nvme0n1` is opened as a
  `SasDevice`** (`open_one_drive`, `mod.rs:206`) and reported as `SasSsd`.
- `SasDevice` (`sas.rs`): O_DIRECT + io_uring, but the ring is a
  `std::sync::Mutex`-guarded submit-and-wait-1 — effectively depth-1
  synchronous. Identity from sysfs; type from `queue/rotational`.
- Runtime registry: `AppState.drives: RwLock<Vec<DriveInfo{device, path}>>`
  (`mgmt/mod.rs:31,119`). No tier, no health, no serial index, no
  persistence; positional index shifts on close (the pallet API addresses
  drives by index and `drives.rs:272` warns about it).

## 2. How drives enter and leave

Three ways in, none of them discovery:

1. Config/CLI at startup (`DriveConfig { path }` — path only). A drive that
   fails to open is logged and skipped; the node starts anyway.
2. `POST /api/v1/drives {path, size_bytes?}` (`api/drives.rs:157`). Opens the
   device, nothing more — no format, no slab, no placement.
3. Pool-pressure growth (`volume/pressure.rs`): configured sources claimed at
   most once; adopts an existing slab or formats; **always tier Hot**
   (`pressure.rs:389`).

Making a drive usable is a separate call: `POST /api/v1/slabs
{device_path, tier, slot_size?}` (`api/slabs.rs:127`) — which opens the path
with **buffered `FileDevice` unconditionally** (`slabs.rs:141`; related:
stormblock#30), and does **not** link the slab to any `state.drives` entry.

Removal: `DELETE /api/v1/drives/{id}?force=` refuses when a slab lives on the
drive — but the check is `Arc::ptr_eq` (`drives.rs:226`), which never matches
a slab formatted via the API (it opened its own FileDevice), so the guard
silently doesn't fire.

**No sysfs scan, no udev, no hotplug.** (`mgmt/discovery.rs` is node
discovery — UDP multicast beacons on 239.255.42.99:7447.)

## 3. Health / failure / migration — the gap

- **Nothing polls health.** `smart_status()` callers: the on-demand
  `GET /api/v1/drives/{id}/smart` and RAID's on-demand aggregate. The
  Prometheus gauges `stormblock_drive_healthy`, `_temperature_celsius`,
  `_media_errors` are declared (`metrics.rs:35-46`) and **never set**.
- SAS SMART is sysfs-only (`device/state`, `device/ioerr_cnt`, hwmon temp) —
  no SG_IO, no log pages, no power-on-hours, no wear.
- The only real SMART decode is `nvme_smart()` (`main.rs:2342`):
  NVME_IOCTL_ADMIN_CMD, Get Log Page 0x02, decodes critical warning, temp,
  spare, percentage-used, POH, unsafe shutdowns, media errors — **used only
  by `must-gather`**. Good reference code for stormdrive's collector.
- **Drive failure is undefined behavior**: I/O errors propagate and are not
  recorded. Nothing ever sets a RAID member to `Failed` (`set_member_state`
  is test-only); degraded-read paths are production-dead. The inline RAID 1
  rebuild has no rate limit and on error silently `return`s, leaving the
  member `Rebuilding` forever; `migrate_to_local` polls states and treats
  "nothing Rebuilding" as success — a failed rebuild reads as complete
  (`migrate.rs:83-97`).
- `PlacementEngine::evacuate_slab` (`placement/mod.rs:412`) **stops at the
  first failed extent** (`:451` — comment says skip, code breaks).
- Migration tooling that exists: `migrate_to_slab` (CLI `migrate
  --local-disk`, not HTTP), volume moves (`POST /api/v1/moves`, two-phase,
  offline), slab GC. All operator-triggered, all healthy-source. **Slabs have
  no redundancy of their own — a dead drive takes its slab's extents with
  it** unless the volume is on RAID or replicated.

## 4. Location / topology

- **No physical location anywhere**: no enclosure, bay, slot, SAS address,
  WWN, PCI BDF, phy, expander, HBA (grepped).
- What exists: `StorageTier` Hot/Warm/Cool/Cold (assigned by hand at slab
  format), `Locality` Local/Remote, `StorageDevice{tier,locality,priority}`
  (never constructed outside tests), and node-level `[management].topology`
  string labels reported via `GET /v1/nodes/capacity`.
- `SlabRegistry` indexes by tier only — cannot answer "which drive is this
  extent on", cannot spread across drives, cannot express a failure domain
  below the node. stormblock's CLAUDE.md lists failure-domain topology as an
  open roadmap item and says: *"stormblock has to know its own drives first,
  then where those drives are."*

## 5. Management plane / integration surface

- axum 0.8, single listener, default `0.0.0.0:9090`. Everything —
  `/api/v1/*`, `/v1` (CSI contract), `/serve/v1` (+`/mk/v1` alias),
  `/metrics`, `/ui`, raft — on that one port.
- **Auth**: only `/v1` has bearer auth, and only when
  `[management].api_token` is set. `/api/v1/*`, `/metrics`, `/ui` are open —
  a known, deliberate gap delegated to profile crates
  (`serve/api.rs:1-16`).
- Relevant endpoints for stormdrive:
  - `GET/POST /api/v1/drives`, `GET /api/v1/drives/{id}`,
    `DELETE /api/v1/drives/{id}?force=`, `GET /api/v1/drives/{id}/smart`
  - `GET/POST /api/v1/slabs`, `GET /api/v1/slabs/pool` (usage + pressure),
    `DELETE /api/v1/slabs/{id}` (refuses when allocated)
  - `POST/GET /api/v1/moves` (+ commit/abort) — offline volume relocation
  - `GET /serve/v1/ready` (503 + blockers), `GET /serve/v1/status`,
    `GET /metrics`, `GET /api/v1/sessions`
- **No event mechanism at all**: no SSE, WS, webhook, NATS. Outbound HTTP
  from stormblock exists only as the StormFS registration heartbeat
  (fixed shape, hardcoded path). Any "notify stormblock" flow is therefore a
  plain REST call *into* stormblock; any "subscribe to stormblock" flow is
  polling.
- UI: the embedded stormblock `/ui` is Askama+HTMX (old style). The **newer
  UI lives in stormd** (see below); stormblock's own UI has no pages for
  slabs, pallets, moves, or `/v1`.

## 6. stormd — the newer UI and its extension contract

stormd v0.4.0 (Svelte 5 SPA embedded via rust-embed, axum 0.8). Extension is
**declarative TOML in stormd's config** — no runtime registration:

```toml
[[process]]
name = "stormdrive"
command = "/usr/sbin/stormdrive"

[process.ui]
label   = "Drives"                                  # nav tab
proxy   = "http://127.0.0.1:9092/ui/"               # iframed page
summary = "http://127.0.0.1:9092/api/v1/summary"    # dashboard card
```

- Nav tab → `#/ext/stormdrive` → iframe of `/ui/proxy/stormdrive/` →
  reverse proxy to our `proxy` URL. Proxy limitations: String bodies (binary
  will mangle), only content-type forwarded, **no WebSocket** — live updates
  must poll. The proxied app must survive a path prefix it doesn't own
  (mkube injects a `<base>` tag from
  `location.pathname.match(/^(\/ui\/(?:proxy|ext)\/[^\/]+\/)/)`; nextnfs
  uses a `--base-path` flag).
- `summary` is fetched with a **400 ms timeout** on every components pass
  (2 s cadence); shape `RemoteSummary { health?: "error|warn|ok|idle|unknown",
  detail?: string, metrics: [{label, value, unit?, tone?}] }` — health/detail
  replace the supervisor's view of the process card, metrics append. Failure
  is silent. First consumer of this mechanism ships with stormdrive.
- Style tokens (optional but recommended): `--bg:#0f0f1a`, `--panel:#16192e`,
  `--border:#2a2d45`, `--brand:#e94560`, `--ok:#50fa7b`, `--accent:#8be9fd`,
  48px nav. stormd README §style guide.
- stormd has no service discovery and no auth (its `auth_token` is parsed,
  never read).

## 7. Bugs found (filed on stormblock per rule 11)

1. `DeviceId.uuid` unstable across opens; slab `device_uuid` written, never
   verified.
2. `DELETE /api/v1/drives` slab-in-use guard is `Arc::ptr_eq` and never fires
   for API-formatted slabs.
3. `evacuate_slab` breaks on first extent failure instead of skipping.
4. `stormblock_drive_*` gauges declared but never set;
   `stormblock_drives_total`/`_capacity_bytes` set once at startup only.
5. RAID member failure states unreachable in production; rebuild error leaves
   `Rebuilding` forever; `migrate_to_local` reads a failed rebuild as
   success.
6. (Enhancement) Drive-plane integration surface for stormdrive: stable drive
   identity accepted on open, slab↔drive linkage, drive-scoped
   drain/evacuate over HTTP, per-drive failure-domain labels.
