# StormDrive

**Physical drive management for one storage node.** stormdrive knows what the
node's drives *are*: which bay they sit in, how healthy and how worn they are,
what firmware they run, and whose data is on them. It hands drives to
[stormblock](https://github.com/glennswest/stormblock) and tells it when to
get data off one.

It is the layer below stormblock. stormblock turns drives into slabs and
volumes; stormdrive curates the drives. stormdrive owns everything **below
the node** (HBA, shelf, bay, PCIe slot). stormblock and stormstorage own the
node and everything above it.

One static binary, `stormdrive`, per node. It serves a REST API, an embedded
web page, a stormview components feed and Kubernetes-shaped resources, all on
**:9092**.

```
            sysfs · SG_IO · NVMe admin ioctl · netlink uevents
drives ─────────────────────────────────────────────▶ stormdrive :9092
HBAs, SAS shelves (SES), PCIe slots                     │  inventory.json
                                                        │
          POST/DELETE drives, labels, slabs, health,    ▼
          drain, overcommit (Bearer token)        stormblock :9090
```

## What it does today (v0.18.0)

Everything below is in the code on `main`. Items the code does **not** do yet
are listed under [Not yet](#not-yet).

- **Discovery** (`src/discovery/`). Walks `/sys/block` on start, every
  `discovery.interval_secs`, and 2 s after a kernel disk uevent
  (`src/hotplug.rs`). It classifies each disk as `nvme_ssd`, `sas_ssd`,
  `sas_hdd`, `sata_ssd`, `sata_hdd` or `unknown`. A SATA drive behind a SAS
  HBA is SATA, because its SCSI vendor is `ATA`.
  - **Stable id:** a UUIDv5 of the WWID, or of model + serial when there is
    no WWID. It is the same across reboots, path changes and re-bays.
  - **Multipath:** a dual-IOM shelf shows one drive on two `/dev` nodes. That
    is one drive with a `paths` list and a stable primary (sorted first).
  - **Skipped:** `loop* ram* zram* dm-* md* sr* fd* nbd* ublkb* zd* pmem*
    drbd*`, NVMe native-multipath path nodes (`nvme0c1n1`), hidden gendisks,
    anything `discovery.exclude` matches or `discovery.include` doesn't, and
    drives with a mounted partition unless `discovery.manage_mounted`.
  - **Missing:** a known drive that vanishes stays in the inventory as
    `missing` and keeps its id when it comes back.
  - **Replacement:** a new drive in a missing drive's bay gets
    `replaces: <old id>` and a `replaced` event.
  - **520-byte drives:** the kernel refuses a NetApp drive at 520 or 528 byte
    sectors and attaches it with 0 blocks. Discovery still lists it
    (`usable: false`, `needs_reformat`), with `block_size` from READ
    CAPACITY(16).
  - **Who holds it** (`src/contents.rs`): it reads a stormblock slab header
    (`STRMSLAB`) at LBA 0 or at the start of any GPT partition. A disk with
    one has `in_use_by` set. On stormcos that is the node's system disk,
    which `/proc/mounts` does not show.
  - **Probe cache:** READ CAPACITY and the slab probe run again only when the
    device changes (dev number, size, WWID), or every 10 minutes.
- **Health** (`src/monitor.rs`, `src/poller.rs`, `src/smart/`). Each drive is
  sampled once per `monitor.interval_secs`, at its own phase in the interval.
  At most `monitor.max_concurrent` reads run at once. A read slower than
  `monitor.sample_timeout_secs` counts as a failed sample, and that drive is
  not read again until the stuck read returns.
  - **NVMe:** Get Log Page 0x02 through `NVME_IOCTL_ADMIN_CMD`: critical
    warning bits, temperature, available spare, percentage used, power-on
    hours, media errors.
  - **SAS/SATA:** sysfs only: `device/state`, `device/ioerr_cnt` (failed
    commands, shown as "io errs") and the hwmon temperature. There is no
    SCSI log sense or ATA SMART yet.
  - **Verdict:** `good`, `warning`, `failing` or `failed`, from the
    thresholds in `[monitor]`. A worse verdict must repeat
    `monitor.hysteresis` samples in a row before it sticks. A better one
    applies at once. Every change is an event.
  - **Trend:** SSDs keep a trend of (wear %, media errors), recorded when a
    value changes or once a day. `GET /api/v1/drives/{id}/health` returns it.
- **Location** (`src/topology.rs`, `src/ses.rs`, `src/hba.rs`).
  - **SAS:** the HBA (SCSI host, PCIe address, driver), the shelf, the bay,
    the SAS address, the expander phy and the expander.
  - **Shelves:** from `/sys/class/enclosure` when the kernel's `ses` module
    is bound. Otherwise stormdrive reads the SES pages itself (0x01, 0x02,
    0x07, 0x0A over SG_IO) and uses mpt3sas's
    `enclosure_identifier`/`bay_identifier`. The two IOMs of a dual-path
    shelf are one shelf, keyed by its logical id.
  - **NVMe:** the PCIe slot on the drive's PCIe chain. A numeric slot name
    is also the bay. Intel VMD domains and native multipath heads are
    followed.
  - **Locate LEDs:** sysfs `locate`, or SES IDENT, or the PCIe slot's
    `attention` indicator, or NPEM.
  - **HBA inventory:** driver, PCI ids, board, and the firmware, option-ROM
    BIOS and NVDATA versions. It is reported only; stormdrive never flashes
    an HBA.
  - Location is resolved again on every discovery pass. A move is a
    `location` event.
- **Fleet** (`src/fleet.rs`, `src/stormblock.rs`). A drive's lifecycle is
  three separate fields:
  - `membership`: `out` or `fleet`. `fleet` means handed to stormblock.
  - `designation`: `none`, `reserved`, `spare` or `failed`. The operator sets
    it.
  - `activity`: `idle`, `testing`, `draining`, `formatting`,
    `updating_firmware` or `missing`.

  Joining registers the drive with stormblock (`POST /api/v1/drives {path,
  labels, uuid}`) and can format a slab on it. The tier comes from
  `tier_map`, else `nvme_ssd` → hot, SSD → warm, HDD → cool. Each fleet tick
  then does five things:
  1. Pushes location labels (`shelf`, `bay`, `hba`, `pcie_slot`) when they
     change.
  2. Pushes the health verdict when it changes (Failing/Failed quarantine
     the drive's slabs).
  3. Pushes each drive's overcommit setting.
  4. Drains a failing or failed fleet drive. When stormblock reports it
     empty, the drive leaves the fleet, its locate LED comes on, and an
     event says it is safe to pull.
  5. With `stormblock.auto_add`, registers every qualified drive.
- **Usage** (`src/usage.rs`). Every drive carries `usage`: capacity, its
  stormblock slabs, used, free, and what lies outside the slabs. It is joined
  from stormblock's `/api/v1/slabs` by WWN, else serial, else path.
- **Overcommit.** Each drive has an `overcommit {enabled, ratio}` setting.
  It is off by default, and the ratio can be 1.0 to 16.0. stormdrive pushes
  it to stormblock. stormblock enforces it (stormblock#152, not on
  stormblock main yet, so the push gets 404 and is retried every 10
  minutes).
- **Drive tests** (`src/drivetest.rs`), one per drive:
  - `smoke`: sampled reads.
  - `read_scan`: a full sequential read, with progress and cancel.
  - `destructive_sample`: write, then verify through O_DIRECT. Only on an
    out-of-fleet, unmounted drive that holds no slab.
- **Sector reformat** (`src/format.rs`, `src/scsi.rs`). This turns 520/528
  byte drives into 512 or 4096. It sends MODE SELECT with the new block
  length, then FORMAT UNIT (IMMED), and polls TEST UNIT READY for progress.
  Then it rescans the sd device and checks the new geometry.
  - Targets: one drive, a list, or every drive on a shelf that needs it.
  - A batch is checked all-or-nothing before any drive starts.
  - Only out-of-fleet, idle, unmounted, not reserved, no slab, not NVMe.
  - There is no cancel.
- **The drive worker** (`src/worker.rs`, `src/erase.rs`, `src/gpt.rs`,
  #5). It prepares drives at fleet scale with one request:
  `POST /api/v1/worker/jobs {select, steps, destroy?, dry_run?}`.
  - **Select:** `drives` (handles), `shelf` with optional `bays` (`"0-11,14"`),
    `model`, `unusable` (520/528-byte drives). The given filters are ANDed.
  - **Steps**, in this order, each at most once:
    1. `format {block_size}`: SCSI FORMAT UNIT (the existing format job) or
       NVMe Format NVM with the LBA format of that data size.
    2. `sanitize {method: block|crypto|overwrite}`: NVMe Sanitize (refused
       while other namespaces share the controller) or SCSI SANITIZE
       (which SAT maps to ATA SANITIZE).
    3. `partition {role: data|system}`: clears the first and last 4 MiB,
       then writes a GPT with one 1 MiB-aligned partition of stormblock's
       slab type (the same GUIDs as a node disk).
    4. `enroll {tier?, role}`: opens the partition (or the whole disk) in
       stormblock with its labels and stable uuid, then formats a slab of
       that role and tier.
  - **Safety:** it never touches a fleet, busy, reserved, missing or
    mounted drive. A drive holding a stormblock slab or a filesystem (ext*,
    xfs, btrfs, vfat, ntfs, swap, LVM2, LUKS, md, on the disk or any GPT
    partition) is refused a destructive step, unless `destroy` names it by
    stable id, WWN or serial (never by `/dev` name). Guards run at submit
    and again before each step. `dry_run` returns the plan (runnable and
    refused drives, with reasons) and changes nothing.
  - **Scheduling:** low-level steps run in parallel, at most
    `worker.max_per_hba` behind one HBA (an NVMe drive is its own lane).
    `enroll` runs `worker.enroll_per_domain` at a time per failure domain
    (shelf, else HBA). Whether low-level steps should also be one per
    domain is #37.
  - **State:** jobs persist in `<data_dir>/jobs.json`. Each drive has a
    `prep` phase (`unusable`, `formatting`/`sanitizing` n%, `ready`,
    `enrolled`) on `/api/v1/drives`, the kube Drive status and the feed.
  - **After a restart:** a SCSI format or sanitize, or an NVMe sanitize,
    still running on the drive is watched to the end. Anything else in
    flight becomes `interrupted` until `POST …/resume`; it is never re-run
    blind. `POST …/cancel` stops the steps that have not started.
  - The same pass covers what was not a worker job (#39): a format from
    `/api/v1/format` (or a drive's/shelf's) still running on a SCSI drive
    is watched to the end, and must then report the block size it was
    formatted to; any other format, a drive test or a firmware update
    left in flight is set idle, its record marked `interrupted`, with a
    `restart` warning event. It all runs before the monitor and the API
    start, so no drive is left reading busy.
- **Firmware** (`src/firmware.rs`). It keeps an image store in
  `<data_dir>/firmware`.
  - **SAS/SATA:** WRITE BUFFER mode 0x0E then 0x0F, falling back to 0x07.
  - **NVMe:** Firmware Image Download (0x11) and Commit (0x10). It tries
    activate-now first; when the drive needs a reset, it commits and records
    `reset_required`.
  - Update one drive, a list, or every drive of a model. It never runs on
    its own.
  - Out-of-fleet drives update in parallel. Fleet drives and the system disk
    update one at a time.
  - Failing/Failed drives are refused unless `force`.
- **The page** (`web/`, #6). A Svelte 5 page built on stormview's
  DataGrid, for hundreds of drives:
  - **Groups:** each shelf is a top-level row, then each HBA's direct
    drives, then NVMe, then unlocated drives. A group's drives are a nested
    grid that can be sorted, and a collapsed shelf is one row.
  - **Filters:** quick filters (attention, needs reformat, out of fleet,
    fleet, busy) and a text filter on name, serial, model, `bay 4`, shelf or
    host.
  - **Detail pane:** clicking a drive or shelf opens it. The drive pane
    holds designation, overcommit, tests, format, firmware, locate, usage,
    drain and progress. The shelf pane shows SES elements and has locate,
    reformat and Prepare shelf.
  - **Bulk bar:** acts on the ticked drives; ticking a group means every
    drive it shows. It offers tests, locate, designation, Prepare, format →
    4096/512 and firmware.
  - **Prepare** (the drive worker, #38): every format on the page (bulk,
    shelf, drive) and the rest of drive preparation go through it. Pick the
    steps (format 4096/512, sanitize block/crypto/overwrite, partition,
    enroll with a tier, role), preview them as a dry run (which drives run,
    which are refused and why), type the serial of each drive that holds a
    slab or a filesystem before it may be destroyed, then run.
  - **Jobs:** every worker job, open ones first. Each drive shows its
    state, step, progress and error, with Cancel (what has not started)
    and Resume (what a restart interrupted). The state column shows each
    out-of-fleet drive's `prep` phase (unusable / ready).
  - **Sign in** (#47, #19): every write needs a storage-admin since
    0.18.0, and every read a credential since 0.21.0. Paste a bearer (`oc
    whoami -t`); it is kept in this tab's `sessionStorage` and sent as
    `Authorization: Bearer` on every request. Asked with no credential,
    the node answers the page itself with a 401, and a read refused 401
    opens the sign-in box. While the node enforces (`writes.gate` in
    `/api/v1/health`) and no bearer is set, write controls are disabled,
    and a 403 reads "needs storage-admin".
  - It polls every 4 s, and rows are keyed, so a refresh updates cells
    instead of rebuilding the table.
- **Events** (`src/events.rs`). An in-memory ring of the last 4096 events,
  numbered by `seq`. It is not persisted, so a restart starts it empty.
- **Inventory** (`src/inventory.rs`). The drive records, including
  designations, overcommit, the last format and firmware results, and the
  trends, are kept in `<data_dir>/inventory.json`. It is compact JSON,
  written tmp + rename and only when it changed. Without `data_dir` it is in
  memory only.

## Build and test

Built and tested with `sc-build`, never on the machine you edit on, and never
as root:

```bash
git push
sc-build                      # cargo build && cargo test on dev.g8.lo, scratch dir, deleted after
sc-build 'cargo clippy --workspace --all-targets -- -D warnings'
```

The drive paths (sysfs, SG_IO, ioctls, netlink) are behind
`cfg(target_os = "linux")`. A build on another OS compiles and runs the
portable tests only. What ships is the release build:

```bash
cargo build --release --target x86_64-unknown-linux-musl
```

The unit tests live beside the code (133 at v0.16.0). Page parsers, sense
decoding, the threshold engine, placement hashing and the token lookup are
tested on synthetic data. A stand-in engine covers the stormblock client.

### Test containers (`test/`, #11)

stormdrive's suites follow stormcentral's
[test standard](https://github.com/glennswest/stormcentral/blob/main/docs/test-standard.md):

- **Image:** one image, `/test short|medium|long`, built from
  `test/Containerfile` (`FROM scratch`). `test/build.sh` builds the static
  musl binary on the build box.
- **Output:** JSON lines on stdout, then a summary. Exit 0 means passed, 1 a
  test failed, 2 could not run.
- **Target:** the suites drive the node's stormdrive at
  `https://STORM_NODE:9092` through its API (plain `http://` is tried when
  the node does not speak TLS yet). The node's certificate is checked
  against `STORM_STORMDRIVE_CA`, else the pod's service-account `ca.crt`,
  else `/data/stormcert/ca.crt`. Who the run is: `STORM_STORMDRIVE_TOKEN`
  (a storage-admin bearer: writes run), a client pair
  `STORM_STORMDRIVE_CERT`/`_KEY`, else the pod's service-account token for
  reads (it needs `storage-viewer`). Without a storage-admin, writes are
  skipped. Checks of features newer than the node's release are
  skipped, not failed.

| Suite | Budget | What it proves |
|---|---|---|
| `short` | < 2 min, read-only | up; drives listed with stable ids and resolvable by WWID; health sampled; card, placement (+304), feed, kube Drives, events, HBAs, the page, `/metrics` (every drive by serial) |
| `medium` | < 30 min | 404 envelope, malformed requests refused (400); join/format/destructive test **refused (409)** on a fleet or stormblock-held drive, which is left unchanged; DELETE refused while present; every handle resolves; designation and overcommit round-trips with events; a smoke test to a verdict; a read scan cancelled; topology, kube watch, placement by WWN; usage read from stormblock (#12/#14); shelves (`requires: [sas-shelf]`), NVMe wear (`requires: [nvme]`), monitor cost, page under `/ui/` |
| `long` | the night window | waves until the window ends: 4 + drives/8 API readers (4–64) and a smoke test on every idle, usable drive; p50/p95, errors, drives left busy, stuck or timed-out health reads, event growth. A wave slower than 2× the first (+250 ms), or leaving residue, fails |

**Never destructive.** The suites run on real machines with real drives,
and `test/src/pick.rs` (unit-tested) decides what they may touch:

- **Refusals:** a request the server must refuse goes only to a drive whose
  record shows the guard the server checks *first*.
- **Writes:** only reversible ones, each restored even when a suite times out.
- **Tests:** they only read.

`test/stormdrive-test.yaml` states the suites' metadata and requirements.

`cargo test` also runs `tests/suites.rs`: all three suites against this
daemon, started on the build box with stormblock off and no drives. That
proves the API contract and the reporting on every sc-build. The
drive-touching checks skip there; they meet real drives on the test machines
(`stormcentral test run stormdrive <suite>`).

**The page** is Svelte + Vite in `web/`. Its build, `web/dist`, is committed
and embedded with `include_str!`, so cargo alone builds the daemon. The
build runs on dev, never on this machine:

```bash
git push && web/rebuild.sh          # npm ci + npm test + vite build on dev → web/dist, web/package-lock.json
git push && web/rebuild.sh --check  # rebuild on dev and fail unless the committed web/dist matches
```

`npm test` runs `node --test` over `web/src/lib/model.js`: grouping,
filters, group-means-its-drives selection, and the eligibility rules that
mirror the server's guards. `npm run test:page` loads the built
`dist/assets/app.js` in jsdom against a mocked API with 212 drives (two
24-bay shelves, 4 direct, 160 NVMe). It checks that the page groups,
expands, selects, opens the pane and filters, and that a row keeps its
element across the 4 s refresh. stormview comes from GitHub `main`, pinned by
`web/package-lock.json`.

## Running it

```
stormdrive [--config PATH] [--listen ADDR] [--data-dir DIR]
```

| Flag | Default | Meaning |
|---|---|---|
| `--config` | `/etc/stormdrive/stormdrive.toml` | config file; a missing file means all defaults |
| `--listen` | from config | overrides `listen_addr` |
| `--data-dir` | from config | overrides `data_dir` |

`RUST_LOG` sets the log filter (default `info`). SIGINT persists the inventory
and exits. SIGTERM is not caught, so it skips that final write (#21). The
inventory is also written every tick. The drive work needs root, or at least `CAP_SYS_ADMIN` +
`CAP_SYS_RAWIO` for SG_IO and the NVMe admin ioctl, and write access to sysfs
for locate LEDs and rescans.

## Configuration

`/etc/stormdrive/stormdrive.toml`. Every key is optional. The defaults below
are read from `src/config.rs`, and
[deploy/stormdrive.example.toml](deploy/stormdrive.example.toml) lists them
all. The daemon refuses to start on an unparseable `listen_addr`, a zero
interval, a zero `max_concurrent`/`sample_timeout_secs`, or `hysteresis = 0`.

| Key | Default | Meaning |
|---|---|---|
| `listen_addr` | `0.0.0.0:9092` | API, UI and feed |
| `data_dir` | unset | inventory + firmware images; unset = in memory, no image store |
| `node_name` | hostname | reported in `/api/v1/health`, placement, feed |
| `discovery.interval_secs` | `30` | full rescan (hotplug rescans sooner) |
| `discovery.exclude` | `[]` | extra `*` patterns on kernel names; built-ins always apply |
| `discovery.include` | `[]` | allow-list; empty = every eligible disk |
| `discovery.manage_mounted` | `false` | list drives with mounted partitions |
| `monitor.interval_secs` | `60` | each drive's health sample period; also the stormblock usage/reconcile and fleet tick |
| `monitor.temp_warn_c` / `temp_crit_c` | `55` / `70` | temperature thresholds (both give `warning`) |
| `monitor.spare_warn_pct` / `spare_crit_pct` | `20` / `10` | NVMe available spare: `warning` / `failing` |
| `monitor.wear_warn_pct` / `wear_crit_pct` | `80` / `95` | wear (NVMe percentage used): `warning` / `failing` |
| `monitor.hysteresis` | `3` | consecutive samples before a worse verdict sticks |
| `monitor.max_concurrent` | `8` | health reads in flight |
| `monitor.sample_timeout_secs` | `10` | a slower read is a failed sample |
| `stormblock.enabled` | `true` | talk to stormblock at all |
| `stormblock.url` | `http://127.0.0.1:9090` | the engine's management API |
| `stormblock.auto_add` | `false` | register qualified drives on its own |
| `stormblock.auto_format_slab` | `true` | …and format a slab on them |
| `stormblock.push_health` | `true` | report Failing/Failed (and recovery) to the engine; the engine drains a drive reported `failed` on its own, and rebuilds its volumes when its `[rebuild] automatic` is on |
| `stormblock.drain_on_failing` | `true` | drain + retire a Failing/Failed fleet drive (a refused start stays `pending` and is retried each tick). `false` stops stormdrive's drain and retire, **not** the engine's drain of a `failed` drive |
| `stormblock.tier_map` | `{}` | kind → slab tier, e.g. `{ sas_hdd = "cold" }` |
| `stormblock.api_token` | `""` | engine bearer token; see below |
| `stormblock.token_file` | `""` | file holding it; see below |
| `stormblock.admin_token` | `""` | engine admin token for slab format + drive close (stormblock#274); empty = `$STORMBLOCK_ADMIN_TOKEN`, then `admin_token_file`; see below |
| `stormblock.admin_token_file` | `""` | file holding it, re-read every destructive call; empty = `$STORMBLOCK_ADMIN_TOKEN_FILE`, then `/run/stormblock-admin/admin_token` |
| `api.admin_token` | `""` | node-local break-glass bearer for writes; empty = `$STORMDRIVE_ADMIN_TOKEN`, then `admin_token_file` (`api_token` is read as this) |
| `api.admin_token_file` | `""` | root-only file holding it |
| `api.admin_gate` | `enforce` | `audit` lets refused writes through and logs them (rollout only) |
| `api.tls_cert_file` | `/data/stormcert/stormdrive.crt` | the serving certificate (PEM, chain first), re-read when it changes; while it is missing a TLS handshake fails and plain HTTP answers health only |
| `api.tls_key_file` | `/data/stormcert/stormdrive.key` | its key |
| `api.client_ca_files` | `["/data/stormcert/ca.crt"]` | client certificates are verified against these (the node CA), read at start; a missing file is skipped |
| `api.allow_anonymous` | `false` | **transition only:** plain HTTP and reads with no credential are served as before #19; a credential that is sent is still checked, and writes keep the gate |
| `kubernetes.api_url` | `""` | the apiserver; empty = `$STORMDRIVE_KUBE_API`, then in-cluster. None = only the admin token writes, no CRD objects |
| `kubernetes.ca_file` | `""` | its CA; empty = `$STORMDRIVE_KUBE_CA`, then the service account's |
| `kubernetes.token_file` | `""` | stormdrive's own credential (re-read every call); empty = `$STORMDRIVE_KUBE_TOKEN_FILE`, then the service account's |
| `kubernetes.insecure` | `false` | skip TLS verification (lab only) |
| `kubernetes.controller` | `true` | keep `Drive` objects, run this node's `DriveOperation`s |
| `kubernetes.interval_secs` | `5` | between controller passes |
| `firmware.chunk_kib` | `32` | download chunk; raised to the drive's offset boundary |
| `firmware.max_image_mib` | `256` | largest image upload (also the request body limit) |
| `worker.max_per_hba` | `8` | drive-worker low-level steps at once behind one HBA |
| `worker.enroll_per_domain` | `1` | drive-worker enrolls at once per failure domain (shelf, else HBA) |

Qualified for `auto_add`: out of the fleet, designation `none`, idle, a
health verdict that is known and not Failing/Failed, not `in_use_by`, and
usable. A failed attempt is retried after 10 minutes.

**Engine token** (stormblock v17 requires `Authorization: Bearer` on all of
`/api/v1`). stormdrive uses the first one it finds:

1. `stormblock.api_token`
2. `$STORMBLOCK_API_TOKEN`
3. the first readable, non-empty file of `stormblock.token_file`,
   `$STORMBLOCK_TOKEN_FILE`, `/run/stormblock/engine/api_token`,
   `/etc/stormblock/api_token`, `/var/lib/stormblock/api_token`

While no token is found, it looks again on every call. On a 401 it re-reads
the token and retries once if it changed.

**Destructive engine verbs** (stormblock#274): formatting a slab (`POST
/api/v1/slabs`, on join and the worker's `enroll`) and closing a drive
(`DELETE /api/v1/drives/{id}`, on leave) are refused the node token under
the engine's `admin_gate = "enforce"`. For these stormdrive presents, until
one is not refused (401/403):

1. the engine's admin token: `stormblock.admin_token`,
   `$STORMBLOCK_ADMIN_TOKEN`, then the file `stormblock.admin_token_file`,
   `$STORMBLOCK_ADMIN_TOKEN_FILE`, `/run/stormblock-admin/admin_token`
   (re-read every call);
2. stormdrive's own Kubernetes credential (`[kubernetes]` token file, else
   the service account's), which the engine reviews for `storage.storm.io`
   `slabs` create / `drives` delete — bind it to `storage-admin` or to
   `deploy/rbac.yaml`'s narrower `stormdrive-engine`;
3. the node token (an engine in `audit`, or one older than #274).

A final refusal is logged with what the engine wants. A drain cancel
(`DELETE …/drain`) is ordinary and always goes on the node token.

## Who may read or change a drive (#19, #45)

**Transport (#19).** The owner's rule (stormcos `docs/SECURITY.md`,
2026-09-25): every API on a node is TLS with a stormcert certificate, every
caller authenticates, and nothing answers anonymously but health. :9092 is
one port for both (`src/tls.rs`):

- a connection that opens with a TLS handshake gets TLS from
  `api.tls_cert_file`/`tls_key_file` (what `stormcert-agent serving --cn
  stormdrive` writes). The pair is re-read when it changes, so a renewal or
  a pair that appears after start needs no restart. Client certificates are
  requested and verified against `api.client_ca_files` (the node CA), not
  required; one from another CA fails the handshake;
- a plain-HTTP connection answers `/api/v1/health` and `/healthz` only
  (stormd's liveness probe); anything else is 403 `tls_required`.

**Reads** need one of: the admin token; a client certificate from the node
CA (the node CA vouches, nothing more is asked); or a Kubernetes bearer the
apiserver allows `get` on `storage.storm.io` (`drives`, `enclosures`,
`driveoperations` for jobs, `firmwareimages`; the release's `storage-viewer`
role). No credential, or one nobody knows → 401; a known user without the
role → 403. Health and the page's code (`/assets/*`) are open; the page's
shell (`/`, `/ui`), asked with no credential, is answered 401 *with the
page*, which then signs in.

**Writes.** Owner: "non admins cant format drives etc." (stormcos#250).
**Every write needs a storage-admin** (`src/kubeauth.rs`):

- a **Kubernetes bearer** (`Authorization: Bearer …`): stormdrive asks the
  apiserver who it is (TokenReview) and whether that user may act
  (SubjectAccessReview on `storage.storm.io`). Format, sanitize, partition,
  enroll, firmware, tests, fleet join/leave and worker jobs are `create
  driveoperations`; designation, overcommit, locate, drain, cancels are
  `update drives/<id>`; forget is `delete drives/<id>`; shelf locate is
  `update enclosures/<key>`; firmware images are `create`/`delete
  firmwareimages`. The release's `storage-admin` role allows all of it,
  `storage-viewer` none of it. Answers are cached a minute;
- or a **client certificate** from the node CA: its CN is the user and each
  O a group, reviewed with the same SubjectAccessReview (a bearer sent with
  it wins: a service acting for a person forwards the person's bearer);
- or the node-local **admin token** (`[api] admin_token`), for a node with no
  apiserver.

No credential or an unknown one → 401; a user without the role → 403 with
the apiserver's reason; the apiserver unreachable → 503. A worker job
**dry run** is gated as a read. `GET /api/v1/health` says how writes are
decided (`writes: {gate, apiserver, admin_token}`) and whether reads with no
credential are served (`reads: {anonymous}`).

**The transition.** `api.allow_anonymous = true` serves plain HTTP and
credential-less reads as before #19, so a release can carry TLS before every
caller (stormconsole, ironprom's scrape, the stormlb route, rustkube-node's
placement mirror) presents a credential and before stormcos mints the pair.
A credential that is sent is still checked; writes keep the gate.

A worker job remembers who asked and **asks the apiserver again before every
step** on every drive: a role revoked mid-batch stops the rest. An apiserver
that cannot answer then interrupts the drive (resume later); it never runs
unchecked.

**Audit:** every write decision (allowed, refused, audit-only) is one JSON
line `{time, who, method, path, resource, verb, target, decision, reason,
status}` in the log and `<data_dir>/audit.log`, and an `audit` event in
`/api/v1/events`.

**Owner:** every drive reports `owner`: `stormblock` (in the fleet, or a slab
on it), `stormraid` (a RAID set superblock, `STORMRD1`), else `free`. An owned
drive refuses format, sanitize and partition until the owner lets go (leave
the fleet; delete the RaidSet) and the operation names it in `destroy` by
stable id, WWN or serial.

### As Kubernetes objects

With `[kubernetes]` set, each node's stormdrive keeps two cluster-scoped kinds
in `storage.storm.io/v1` (`src/controller.rs`; install `deploy/crds.yaml` and
`deploy/rbac.yaml`, stormcos#302):

- **`Drive`**, one per drive, named by its stable id, labelled
  `storm.io/node`, `storm.io/shelf`, `storm.io/bay`, …; status has model,
  size, sector size (`blockSize`, 520 = needs reformat), enclosure, bay, SAS
  address, health, owner, prep. `kubectl get drives`. Writing one changes
  nothing (the node puts its report back).
- **`DriveOperation`**: a drive-worker job as an object (`node`, `select`,
  `steps`, `destroy`, `dryRun`, `resume`) — see
  `deploy/driveoperation.example.yaml`. Only storage-admin can create one.
  The node's stormdrive takes the requester **the apiserver stamped** on it
  (`storage.storm.io/requester`, rustkube#210), re-checks it with a
  SubjectAccessReview, runs the job (which survives restarts in jobs.json),
  and keeps `status` (phase Pending/Refused/Planned/Running/Interrupted/
  Succeeded/Failed/Cancelled, per-drive progress) plus a Kubernetes Event per
  decision. **No stamp → Refused**: the object alone is never trusted.
  Deleting it cancels the steps not yet started; raising `spec.resume`
  resumes interrupted drives.

```bash
kubectl get drives -l storm.io/node=c2nr0q2 -o wide
kubectl apply -f deploy/driveoperation.example.yaml   # dryRun: true first
kubectl get driveoperations
```

## API

All JSON on :9092, over TLS with a credential
([above](#who-may-read-or-change-a-drive-19-45)). Errors are `{"error": "...", "code": "not_found" |
"bad_request" | "conflict" | "stormblock" | "internal" | "unauthorized" |
"forbidden" | "unavailable" | "tls_required"}`. Every non-GET needs a
storage-admin. A drive `{id}` is
its DriveId, WWID (any case), `/dev` path or kernel name, or serial, looked up
in that order. A shelf `{key}` is its logical id (with or without `0x`, any
case), serial, shelf id, or an SES device's SCSI id.

| Method and path | What |
|---|---|
| `GET /api/v1/health`, `/healthz` | health; the only paths open with no credential and over plain HTTP |
| `GET /`, `/ui`, `/ui/` | the embedded page; works behind a proxy prefix (401 with the page when no credential) |
| `GET /assets/app.{js,css}`, `/ui/assets/…` | the page's two assets |
| `GET /api/v1/health` | `{status, version, node, writes}` — liveness, and how writes are decided |
| `GET /api/v1/summary` | stormd `RemoteSummary` card from cached state |
| `GET /api/v1/monitor` | health-poll cost, stuck drives, last discovery pass |
| `GET /metrics` | Prometheus text: per-drive SMART, temperature, wear, errors, last poll; shelf sensors; the poller (see below) |
| `GET /api/v1/drives` | every drive, with any running test/format/firmware run inlined |
| `GET /api/v1/drives/{id}` · `DELETE` | one drive · forget a missing, out-of-fleet drive |
| `GET /api/v1/drives/{id}/health` | health report + trend |
| `POST /api/v1/drives/{id}/locate` | `{"on": bool}` |
| `POST /api/v1/drives/{id}/fleet` | `{"action":"join","format_slab"?,"tier"?}` or `{"action":"leave","drain"?,"force"?}` |
| `GET·POST·DELETE /api/v1/drives/{id}/drain` | status · start (`?leave=true` retires when empty) · cancel |
| `POST /api/v1/drives/{id}/designation` | `{"designation":"none\|reserved\|spare\|failed"}` |
| `GET·PUT·POST /api/v1/drives/{id}/overcommit` | `{"enabled":bool,"ratio"?}`; GET adds promisable/committed/headroom |
| `GET·POST /api/v1/drives/{id}/test`, `POST …/test/cancel` | `{"kind":"smoke\|read_scan\|destructive_sample"}` |
| `GET·POST /api/v1/drives/{id}/format` | `{"block_size":512\|4096}` (default 4096) |
| `GET·POST /api/v1/format` | all runs · `{"drives":[…],"block_size"}` |
| `GET·POST /api/v1/worker/jobs` | drive worker jobs · `{"select":{…},"steps":[…],"destroy"?,"dry_run"?}` |
| `GET /api/v1/worker/jobs/{id}`, `POST …/{id}/cancel`, `POST …/{id}/resume` | one job; stop what hasn't started; continue interrupted drives |
| `GET·POST /api/v1/drives/{id}/firmware` | version + runs · `{"image","force"?}` |
| `GET·POST /api/v1/firmware` | all runs · `{"image","drives"?,"model"?,"force"?}` |
| `GET /api/v1/firmware/images`, `GET·PUT·DELETE …/images/{name}` | image store; PUT takes the raw image |
| `GET /api/v1/shelves`, `GET …/shelves/{key}` | SES identity, status, elements, slots, drives |
| `POST /api/v1/shelves/{key}/locate` | `{"on":bool,"bay"?}` |
| `POST /api/v1/shelves/{key}/format` | `{"block_size","all"?}` — out-of-fleet drives that need it |
| `GET /api/v1/topology` | controller → shelf → drive tree, with HBA firmware |
| `GET /api/v1/hbas` | every PCIe SCSI HBA |
| `GET /api/v1/placement`, `GET …/placement/{id}` | where each drive is; `generation`, ETag, `?since=` / `If-None-Match` → 304 |
| `GET /api/v1/events?since=<seq>` | `{latest_seq, events}` |
| `GET /api/v1/components`, `GET /ws/components` | stormview feed (drives, shelves, HBAs); the socket pushes on change, checked every 2 s |
| `GET /apis/storage.storm.io/v1/{drives,enclosures}[/{name}]` | Kubernetes-shaped `Drive`/`Enclosure`; `?watch=1`, `labelSelector`; `PATCH` a Drive's spec (designation, fleet, drain, locate) |

Body-free forms, for stormview renderers that POST with no body:
`…/locate/{on|off}`, `…/fleet/{join|leave}` (never formats a slab),
`…/designation/{value}`, `…/overcommit/{off|<ratio>}`, `…/test/{kind}`,
`…/format/{block_size}`, `/shelves/{key}/locate/{on|off}`,
`/shelves/{key}/format/{block_size}`.

### `/metrics` (#18)

Prometheus text on the API port, a read like any other (a scraper presents
a node-CA client certificate or a bearer, #19), built from cached state (a scrape sends nothing to a drive). `smartctl_exporter` names where
one fits. Every drive series carries `device`, `serial`, `model`,
`enclosure` (shelf key) and `bay`; empty when unknown.

| Series | What |
|---|---|
| `smartctl_device{interface,firmware_version}` | 1 per drive |
| `smartctl_device_smart_status` | 1 when health is good/warning, 0 failing/failed; absent while unknown |
| `smartctl_device_temperature{temperature_type="current"}` | °C |
| `smartctl_device_power_on_seconds` | NVMe |
| `smartctl_device_percentage_used`, `_available_spare`, `_available_spare_threshold` | NVMe wear |
| `smartctl_device_critical_warning`, `_media_errors`, `_num_err_log_entries`, `_power_cycle_count`, `_bytes_read`, `_bytes_written` | NVMe log 0x02 |
| `smartctl_device_capacity_bytes`, `smartctl_device_block_size{blocks_type}` | geometry (logical as the drive reports it, 520 included) |
| `stormdrive_drive_info{id,wwn,kind,firmware,membership,designation,activity,owner}` | 1 per drive |
| `stormdrive_drive_health_status{status}` | 1 for the current verdict |
| `stormdrive_drive_io_errors_total` | SAS/SATA: sysfs `ioerr_cnt`, failed commands since boot (not media errors) |
| `stormdrive_drive_unsafe_shutdowns_total` | NVMe |
| `stormdrive_drive_last_poll_timestamp_seconds` | last health poll the drive answered |
| `stormdrive_drive_used_bytes`, `_free_bytes` | from stormblock's slabs (#12) |
| `stormdrive_enclosure_info`, `_ok`, `_last_scan_timestamp_seconds` | per shelf |
| `stormdrive_enclosure_element_ok`, `_temperature_celsius`, `_fan_rpm`, `_volts`, `_amps` `{type,index}` | per installed SES element |
| `stormdrive_poll_*`, `stormdrive_discovery_seconds`, `stormdrive_build_info` | the daemon |

A missing drive keeps `smartctl_device`, `stormdrive_drive_info` and its
last-poll time; its readings go. Reallocated / pending sectors and grown
defects on HDDs need SCSI log sense / ATA SMART (#22) and appear when that
lands.

```bash
T="Authorization: Bearer $(oc whoami -t)"     # a storage-admin's bearer
S="https://<node>:9092"; C="--cacert /data/stormcert/ca.crt"   # the node CA
curl -s $C -H "$T" $S/api/v1/drives | python3 -m json.tool
curl -s $C --cert client.crt --key client.key $S/metrics      # a node-CA client pair
curl -s $C -H "$T" -X POST $S/api/v1/drives/sdb/locate -d '{"on":true}' -H 'Content-Type: application/json'
curl -s $C -H "$T" -o /dev/null -w '%{http_code}\n' "$S/api/v1/placement?since=<generation>"
curl -s $C -H "$T" -X POST $S/api/v1/format -H 'Content-Type: application/json' \
     -d '{"drives":["sdb","sdc"],"block_size":4096}'
curl -s $C -H "$T" -X PUT --data-binary @image.lod $S/api/v1/firmware/images/image.lod
curl -s $C -H "$T" -X POST $S/api/v1/firmware -H 'Content-Type: application/json' \
     -d '{"model":"ST1200MM0098","image":"image.lod"}'
```

## How it ships

stormdrive is a **service** component on stormcentral (`stormcentral
component list`). Its golden is built by
`stormcentral component build stormdrive`, which runs stormcos's
`service_golden` recipe. That recipe builds the musl binary and puts it in a
stormd-based golden with:

- `/etc/stormdrive/stormdrive.toml` setting `listen_addr = "0.0.0.0:9092"`
  and `data_dir = "/var/lib/stormdrive"`;
- stormd running `/usr/sbin/stormdrive --config /etc/stormdrive/stormdrive.toml`,
  restarted on exit, with an HTTP liveness probe on `/api/v1/health`, and
  stormd's own API on :9192.

A commit reaches a node only through that path:

1. A golden is built from a pushed commit.
2. stormcos composes the golden into a release.
3. Nodes clone the release copy-on-write. Nothing pulls an image.

`Cargo.lock` pins the one git dependency, `stormview` (branch `main`). A fix
there arrives only after `cargo update -p stormview` and a commit here. How
goldens and releases work is written up in stormcos
[`docs/goldens.md`](https://github.com/glennswest/stormcos/blob/main/docs/goldens.md).

The crate is a Cargo workspace (`.` and `test/`, both default members). The
recipe's release build therefore also compiles the test binary, but the
golden carries only `/usr/sbin/stormdrive`. The test image is built
separately by stormcentral from `test/Containerfile`.

stormcos starts it on every node profile (`boot.d/40-services`). It runs with
the host network, the host's `/dev`, the host's `/sys` **read-only**, its
data volume at `/var/lib/stormdrive`, and the engine token from
`/run/stormblock` (`STORMBLOCK_TOKEN_FILE`). The read-only `/sys` means that
in the golden, sysfs locate LEDs and the post-format rescan fail. SES and
SG_IO paths work (stormcos#166).

The node's HTTPRoute publishes it by name (`drive.storm1.g8.lo` in stormcos
`deploy/manifests/85-routes.yaml`). stormconsole on :9094 reads its
components feed. The stormd `[process.ui]` card snippet
([deploy/stormd-ui.toml](deploy/stormd-ui.toml)) and the systemd unit
([deploy/systemd/stormdrive.service](deploy/systemd/stormdrive.service)) are
for installs outside stormcos.

## Not yet

These are documented as design only; the code does not do them:

- `DriveOperation`s run only once the apiserver stamps their requester
  (rustkube#210) and stormcos installs the CRDs and gives stormdrive a
  credential (stormcos#302)
- SCSI log sense / ATA SMART for SAS and SATA health (#22); wear-out
  projection (#23); persisted events (#25)
- a node-wide sequencer with a stormblock redundancy check before a fleet
  drive's firmware reset (#24)
- in the drive worker: ATA SECURITY ERASE for SATA drives without ATA
  Sanitize (#36); the scheduling default is
  your decision (#37)
- SES shelf (IOM) firmware (#35)
- waiting on your decision:
  - thermal actuation (#32)
  - drive crypto: SED, crypto erase (#33)
  - burn-in before a drive joins the fleet (#34)
  - where vendor firmware images come from (#29)

Built but never exercised on real hardware:

- **Firmware updates:** nothing supplies an image (#29).
- **The drive worker's disk operations** (NVMe Format NVM, both sanitizes,
  GPT on a real disk, BLKRRPART, enroll through a partition) are tested on
  synthetic data. The GPT layout is also checked by `sfdisk --verify` on a
  file. On real drives: #30.
- **The NetApp shelf path:** SES, the 520 → 4096 format, phy/expander (#30).
- **160-bay NVMe:** #31.
- **The test containers on a test machine:** #28.
- **Overcommit enforcement:** it waits on stormblock#152, so today the
  setting is stored and pushed, not enforced.

## Documentation

- [docs/presentation.md](docs/presentation.md) — a 12-slide deck (Marp) on its
  purpose and functionality: `npx @marp-team/marp-cli docs/presentation.md`
- [docs/architecture.md](docs/architecture.md) — the design and how each
  subsystem works
- [docs/stormblock-review.md](docs/stormblock-review.md) — the 2026-08-26
  stormblock review that started this project (historical)
- [CHANGELOG.md](CHANGELOG.md), [CLAUDE.md](CLAUDE.md) — changes, work plan
