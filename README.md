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

## What it does today (v0.27.1)

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
  - **SAS/SATA:** from sysfs, `device/state`, `device/ioerr_cnt` (commands
    that failed since boot, resets included: `health.io_errors`, shown as
    "io errs", never a warning and never media errors, #58) and the hwmon
    temperature. From the drive
    itself (#22), two to six commands a sample:
    - SAS: LOG SENSE Informational Exceptions (0x2F: the drive's own failure
      prediction, and temperature), Temperature (0x0D), and on an SSD the
      Solid State Media page (0x11: endurance used, as `wear_pct`). The
      Write, Read and Verify error counter pages (0x02/0x03/0x05: corrected,
      uncorrected, bytes processed, #64) are in `health.smart`; the
      uncorrected errors together are the drive's `media_errors`.
    - SATA (vendor `ATA`, also behind a SAS HBA): ATA SMART READ DATA and
      THRESHOLDS. That gives reallocated, pending and offline-uncorrectable
      sectors, temperature, power-on hours and SSD wear; a pre-fail
      attribute at or below its threshold is the drive predicting its own
      failure. Attribute 187 (reported uncorrectable) is the drive's
      `media_errors`; 199 (interface CRC errors) is `smart.crc_errors`. The
      whole attribute table goes into the drive history (#64).

    A predicted failure makes the drive `failing`; pending or
    offline-uncorrectable sectors make it `warning`. They are in
    `health.smart`.
  - **Verdict:** `good`, `warning`, `failing` or `failed`, from the
    thresholds in `[monitor]`. A worse verdict must repeat
    `monitor.hysteresis` samples in a row before it sticks. A better one
    applies at once. Every change is an event.
  - **No sample is never silent** (#58): `health.not_collected` says why
    there is none (first sample due, the read timed out or is hung, or none
    for three intervals), and three intervals without one is a warning
    event. `collected_at: null` alone never means "no errors". The health
    loop restarts itself, with an error event, if it ever stops.
  - **Slabs the engine doesn't use** (#58): each monitor tick reads the
    engine's `GET /api/v1/health` `slabs` report (where the node's system
    and data halves run, and the boot's verdict on its own disk,
    stormblock#344). A drive carrying stormblock slab partitions of a half
    the engine runs from the network (`remote`, or the node `diskless`) gets
    `engine_finding` and is `warning` ("suspect"); `failing` when the boot
    says it took the disk and could not use it. The engine's reason is in
    the message, and it is an `engine` event. A half mid flow-over
    (`mixed`) is not a finding.
    `GET /api/v1/drives/{id}/slabs` shows the three views: the slabs on the
    disk (`on_disk`: partition, role from the GPT type, offset), the
    engine's slabs on it (`in_engine`) and the engine's report.
  - **Trend:** SSDs keep a trend of (wear %, media errors), recorded when a
    value changes or once a day. `GET /api/v1/drives/{id}/health` returns it.
    It lives in `data_dir`, so a reinstall that replaces `data_dir` starts it
    again; the drive's own counters (ATA SMART, NVMe log) are on the drive
    and survive both, and the drive history below keeps them.
- **Drive history and hardware assets in system-data** (#64, stormcos#456):
  the node's kept volume in the data half, which every install keeps
  (stormblock#355 makes it; stormcos mounts it into stormdrive at
  `history.dir`, default `/data/system-data`). stormdrive never creates that
  directory: while it is absent, history is off and `GET /api/v1/history`
  says so.
  - `history/drives/<wwn-…|serial-…>/<YYYY-MM>.jsonl`: one JSON record a
    line — time, boot id, kernel, stormdrive version, the drive (id, WWN,
    serial, model, firmware, kind, path, bay), its verdict, temperature and
    **every counter** (health, NVMe log 0x02 in full, SAS error counter
    pages, SATA's whole SMART table). Written on a drive's first sample, when
    a counter that matters changes (errors, wear, cycles — not bytes,
    commands or power-on hours, which move on every read), when its verdict,
    firmware or bay changes, and otherwise every `history.heartbeat_secs`.
    Months older than `history.keep_months` are removed.
  - **Findings:** each record is compared with the drive's last one, read
    back from the file at start, so across restarts and installs. An error
    counter that grows (media errors, reallocated, pending, offline or
    reported uncorrectable, CRC, SAS uncorrected, NVMe error-log entries), or
    a lifetime counter that goes backwards (power-on hours, bytes, cycles,
    wear: the drive's SMART was reset, or it is not the drive it was), is a
    finding: in the record and a `history` warning event. `io_errors` (per
    boot) never is. The first sample after an install also grows its
    media-error warning from the history's count.
  - `assets/<YYYYMMDDTHHMMSSZ>-<boot_id>.json`, one per boot: system,
    board and BIOS (DMI), CPUs per socket (model, cores, threads,
    microcode), DIMMs (SMBIOS type 17: slot, size, type, speed, maker,
    serial, part), the BMC's interface (SMBIOS 38), physical NICs (MAC,
    driver, PCIe), NVMe controllers, HBAs (board, firmware, BIOS, NVDATA),
    shelves (each IOM's revision) and the drive in each bay. `changes` is
    the difference from the previous boot's file (added, removed,
    `field: old → new`) and an `assets` event (a warning when something was
    removed). A change within the boot rewrites the file
    (`changed_in_boot`). The newest 1000 boots are kept.
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
     the drive's slabs). A pulled fleet drive is reported `missing`, by
     its uuid (#44); the engine fails its RAID set member and takes a spare.
  3. Pushes each drive's overcommit setting.
  4. Drains a failing or failed fleet drive. When stormblock reports it
     empty, the drive leaves the fleet, its locate LED comes on, and an
     event says it is safe to pull.
  5. With `stormblock.auto_add`, registers every qualified drive.
  6. Shelf RAID sets (stormblock#252, #44). It reads `GET /api/v1/arrays`
     and lights the SES fault LED (RQST FAULT) of every bay holding a
     `failed` member. A member is found by its registration uuid (our drive
     id), else WWN or serial, else its `shelf=…/bay=…` labels. The LED goes
     out once the bay has no failed member any more (replaced, rebuilt).
     Lit bays are kept in the inventory, so a restart can still clear them.
     Drives are registered with `shelf=<shelf key>` and `bay=<n>`. Name a
     stormblock shelf layout (`POST /api/v1/shelves {name}`) after the shelf
     key (`GET /api/v1/shelves` on :9092), so the set's spare pool and its
     `shelf=` rung agree.
- **Offer** (#42, `worker.offer`, on by default). A drive is marked
  `enrolable`, with one `offer` event when it turns so, when it is:
  - out of the fleet and idle, with no designation;
  - not failing, with a health verdict known;
  - on sectors the kernel uses, at least `worker.offer_min_bytes` (1 GiB);
  - **blank**: discovery's cached probe (`contents`) found no filesystem,
    slab or RAID member on the disk or in any GPT partition.

  The flag is on `/api/v1/drives`, the kube Drive status (`enrolable`,
  `contents`) and the feed: an `offer` metric and an **Enrol (data slab)**
  action. That action is `POST /api/v1/drives/{id}/enroll[?tier=]`, a
  worker job (partition + enroll data) that is refused unless the drive is
  offered. Enrolling on its own is a `DrivePolicy`'s job (#50; with
  `requireDataSlab`, only on a node that already has a data slab).
- **Usage** (`src/usage.rs`). Every drive carries `usage`: capacity, its
  stormblock slabs, used, free, and what lies outside the slabs. It is joined
  from stormblock's `/api/v1/slabs` by WWN, else serial, else path.
  `usage.volumes` lists the volumes with legs on the drive, largest first
  (#26), from the engine's `GET /api/v1/volumes?placement=true`
  (stormblock v17.1+). Each entry has id, name, kind, consumer, bytes, legs,
  shared legs, the worst slab state on this drive, rebuild, policy and
  health. The field is absent while the engine reports no placement. A
  console reads this from every node's :9092, including nodes whose engine
  it cannot reach.
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
       (which SAT maps to ATA SANITIZE). Or `security_erase {enhanced?}`
       (#36), for a SATA drive that has the ATA Security feature set and
       not Sanitize. It sends ATA SECURITY ERASE UNIT through ATA
       PASS-THROUGH(16); enhanced is used when the drive supports it unless
       `enhanced: false`. It works under a one-time user password, which is
       saved in the job record (`ata_password` in jobs.json) and a warning
       event **before** the drive sees it, and cleared once IDENTIFY reads
       security off again. A drive that loses power mid-erase stays locked
       with it (`hdparm --user-master u --security-unlock <pw>`). It is
       refused on a frozen drive (a hot-replug or a host suspend/resume
       unfreezes it; never retried), on an expired password counter, and
       on a drive whose password this job did not set.
       Also among the low-level steps: `test {kind:
       smoke|read_scan|destructive_sample}` (#40). It runs the drive test
       on the server with its verdict in the job. A failed test stops that
       drive's later steps, so `format → test → partition → enroll`
       qualifies a drive before it is enrolled. Only `destructive_sample`
       destroys, and the destroy rules apply to it. A job of only smoke
       tests and read scans may also read fleet, reserved and mounted
       drives, as `POST /api/v1/drives/{id}/test` does.
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
  - **Redundancy gate** (#24). A drive that serves data (fleet, or holding a
    slab) resets only while every volume with legs on it is fully
    redundant, per the engine's placement: redundancy `healthy`, no rebuild
    owed or running, its slab there `ok`. It waits up to
    `firmware.redundancy_wait_mins` (30) before the download, then again
    after the update before the next drive starts. `force` skips it.
  - **Shelf IOMs** (#35, `src/iomfw.rs`): `POST /api/v1/shelves/{key}/firmware
    {image, allow_path_loss?}` sends an image from the same store through
    SES Download Microcode (page 0x0E, SEND DIAGNOSTIC, mode 0x07), one IOM
    (ESP path) at a time. Each IOM's progress is watched on its status page
    while it restarts, for up to 15 min. The next IOM is touched only once
    the drives' paths through the first are back. It is refused when a
    drive serving data (fleet, or holding a slab) has no second path, e.g.
    a single-pathed shelf, unless `allow_path_loss`. `GET …/firmware` gives
    each IOM's revision (sysfs `rev`) and the run.
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
    steps (format 4096/512, sanitize block/crypto/overwrite or ATA
    security erase, partition,
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
- **Events** (`src/events.rs`). A ring of the last 4096 events, numbered by
  `seq`. The newest 512 are kept in `<data_dir>/events.json` (#25), so a
  restart keeps them, the sequence continues, and a `restart` event says
  so.
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
sc-build                      # cargo build && cargo test on the build box, scratch volume, deleted after
sc-build 'cargo clippy --workspace --all-targets -- -D warnings'
SC_BUILD_VM=1 sc-build        # the same on a fresh build VM (dev.g8.lo is retired, stormcentral#521)
```

The drive paths (sysfs, SG_IO, ioctls, netlink) are behind
`cfg(target_os = "linux")`. A build on another OS compiles and runs the
portable tests only. What ships is the release build:

```bash
cargo build --release --target x86_64-unknown-linux-musl
```

The unit tests live beside the code (241 at v0.27.1), plus the `retry/`
crate's own and the test crate's. Page parsers, sense decoding, the
threshold engine, placement hashing and the token lookup are tested on
synthetic data. A stand-in engine covers the stormblock client. The
integration tests in `tests/` run the real daemon: `suites.rs` (the three
suites, below), `kube.rs` (a stand-in apiserver: the write gate, the
DriveOperation controller, DrivePolicy), `restart.rs` (recovery after a
restart, SIGTERM), `tls.rs` (:9092's TLS and credentials) and
`chassis160.rs` (a simulated 160-bay NVMe chassis, #31).

### Test containers (`test/`, #11)

stormdrive's suites follow stormcentral's
[test standard](https://github.com/glennswest/stormcentral/blob/main/docs/test-standard.md):

- **Image:** one image, `/test short|medium|long`, built from
  `test/Containerfile` (`FROM scratch`). `test/build.sh` builds the static
  musl binary on the build box.
- **Output:** JSON lines on stdout, then a summary. Exit 0 means passed, 1 a
  test failed, 2 could not run — including a test infrastructure stopped
  (`"infrastructure": true`, see "Remote calls and retries").
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
| `short` | < 2 min, read-only | up; drives listed with stable ids and resolvable by WWID; health sampled, or the reason it is not; slabs on each disk vs the engine's report (`drive-slabs`, #58); system-data (this boot's assets, history for every sampled drive; skipped while not mounted, #64); card, placement (+304), feed, kube Drives, events, HBAs, the page, `/metrics` (every drive by serial) |
| `medium` | < 30 min | 404 envelope, malformed requests refused (400); reads with no or a bogus credential refused (`reads-need-a-credential`, #19); writes without a storage-admin refused (`writes-need-storage-admin`, #45); malformed worker jobs refused, dry run only (`worker-refusals`); join/format/destructive test **refused (409)** on a fleet or stormblock-held drive, which is left unchanged; DELETE refused while present; every handle resolves; designation and overcommit round-trips with events; a smoke test to a verdict; a read scan cancelled; topology, kube watch, placement by WWN; usage read from stormblock (#12/#14); shelves (`requires: [sas-shelf]`), NVMe wear (`requires: [nvme]`), monitor cost, page under `/ui/` |
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
build runs through `sc-build` (prefix `SC_BUILD_VM=1` for a build VM),
never on this machine:

```bash
git push && web/rebuild.sh          # npm install + npm test + vite build + page test → web/dist, web/package-lock.json
git push && web/rebuild.sh --check  # npm ci, the same tests, fail unless the committed web/dist matches
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
| `--version`, `--help` | | print and exit |

`RUST_LOG` sets the log filter (default `info`). SIGINT or SIGTERM (what
stormd and systemd send) stops it gracefully: a `stopping` event, then the
event log and the inventory are written, and it exits 0 (#21). The
inventory is also written every tick. The drive work needs root, or at least `CAP_SYS_ADMIN` +
`CAP_SYS_RAWIO` for SG_IO and the NVMe admin ioctl, and write access to sysfs
for locate LEDs and rescans.

## Configuration

`/etc/stormdrive/stormdrive.toml`. Every key is optional. The defaults below
are read from `src/config.rs`, and
[deploy/stormdrive.example.toml](deploy/stormdrive.example.toml) lists them
all. The daemon refuses to start on an unparseable `listen_addr`, a zero
interval (`discovery`, `monitor`, `kubernetes`), a zero
`max_concurrent`/`sample_timeout_secs`, `hysteresis = 0`,
`history.keep_months = 0`, a zero `worker.max_per_hba` or
`worker.enroll_per_domain`, an `api.admin_gate` other than `enforce`/`audit`,
or only one of `api.tls_cert_file`/`tls_key_file` set.

| Key | Default | Meaning |
|---|---|---|
| `listen_addr` | `0.0.0.0:9092` | API, UI and feed |
| `data_dir` | unset | `inventory.json`, `events.json`, `jobs.json`, `audit.log` and the firmware image store; unset = in memory, no image store |
| `node_name` | hostname | reported in `/api/v1/health`, placement, feed |
| `discovery.interval_secs` | `30` | full rescan (hotplug rescans sooner) |
| `discovery.exclude` | `[]` | extra `*` patterns on kernel names; built-ins always apply |
| `discovery.include` | `[]` | allow-list; empty = every eligible disk |
| `discovery.manage_mounted` | `false` | list drives with mounted partitions |
| `monitor.interval_secs` | `60` | each drive's health sample period; also the stormblock usage/reconcile and fleet tick |
| `monitor.temp_warn_c` / `temp_crit_c` | `55` / `70` | temperature thresholds (both give `warning`) |
| `monitor.spare_warn_pct` / `spare_crit_pct` | `20` / `10` | NVMe available spare: `warning` / `failing` |
| `monitor.wear_warn_pct` / `wear_crit_pct` | `80` / `95` | wear (NVMe percentage used): `warning` / `failing` |
| `monitor.wear_out_warn_days` | `180` | a warning event once the projected days to wear-out drop under this (#23) |
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
| `kubernetes.api_url` | `""` | the apiserver; empty = `$STORMDRIVE_KUBE_API`, then in-cluster (`https://$KUBERNETES_SERVICE_HOST:$KUBERNETES_SERVICE_PORT`, port default `443`, only when the service-account token exists). None = only the admin token writes, no CRD objects |
| `kubernetes.ca_file` | `""` | its CA; empty = `$STORMDRIVE_KUBE_CA`, then the service account's (`/var/run/secrets/kubernetes.io/serviceaccount/ca.crt`) |
| `kubernetes.token_file` | `""` | stormdrive's own credential (re-read every call); empty = `$STORMDRIVE_KUBE_TOKEN_FILE`, then the service account's (`…/serviceaccount/token`) |
| `kubernetes.insecure` | `false` | skip TLS verification (lab only) |
| `kubernetes.controller` | `true` | keep `Drive` objects, run this node's `DriveOperation`s and the `DrivePolicy`s that select it |
| `kubernetes.interval_secs` | `5` | between controller passes |
| `firmware.chunk_kib` | `32` | download chunk; raised to the drive's offset boundary |
| `firmware.max_image_mib` | `256` | largest image upload (also the request body limit) |
| `firmware.redundancy_wait_mins` | `30` | how long a data-serving drive's update waits for its volumes to be redundant, before the reset and after (#24) |
| `worker.max_per_hba` | `8` | drive-worker low-level steps at once behind one HBA |
| `worker.enroll_per_domain` | `1` | drive-worker enrolls at once per failure domain (shelf, else HBA) |
| `worker.offer` | `true` | mark blank, healthy, out-of-fleet drives `enrolable`, with an event (#42) |
| `worker.offer_min_bytes` | `1073741824` | …at least this big |
| `history.dir` | `/data/system-data` | the system-data volume as mounted for stormdrive (#64); absent = no history or assets, never created |
| `history.heartbeat_secs` | `3600` | a drive record when a counter changes, else this often (`monitor.interval_secs` = every sample) |
| `history.keep_months` | `24` | months of drive history kept |

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
role → 403. Health is open on any connection; the page's code
(`/assets/*`, `/ui/assets/*`) is open over TLS (plain HTTP gets 403
`tls_required` for it like anything else); the page's
shell (`/`, `/ui`), asked with no credential, is answered 401 *with the
page*, which then signs in.

**Writes.** Owner: "non admins cant format drives etc." (stormcos#250).
**Every write needs a storage-admin** (`src/kubeauth.rs`):

- a **Kubernetes bearer** (`Authorization: Bearer …`): stormdrive asks the
  apiserver who it is (TokenReview) and whether that user may act
  (SubjectAccessReview on `storage.storm.io`). Format, sanitize, partition,
  enroll, firmware, tests, fleet join/leave and worker jobs are `create
  driveoperations`; designation, overcommit, locate, drain and
  cancelling a test are `update drives/<id>`; cancelling or resuming a
  worker job is `update driveoperations/<job>`; forget is `delete
  drives/<id>`; shelf locate is `update enclosures/<key>`; shelf format and
  shelf (IOM) firmware are `create driveoperations`; firmware images are `create`/`delete
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

With `[kubernetes]` set, each node's stormdrive keeps three cluster-scoped kinds
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
- **`DrivePolicy`** (#50, stormcos#251) says which drives of which nodes
  become stormblock slabs, and of which tier, without anyone submitting a
  job. See `deploy/drivepolicy.example.yaml`: stormblock1's NetApp shelf
  becomes `warm`, stormblock2's SAS HDDs `cool`. The spec has:
  - **nodes:** `nodes` and/or `nodeSelector.matchLabels`; one of them is
    required.
  - **drives:** `kinds`, `minBytes`, `maxBytes`, `blockSizes`, `model`,
    `shelf`, `bays`.
  - **`reformat`** (512 or 4096): applied only to a drive the kernel cannot
    use, such as a 520-byte NetApp drive.
  - **`enroll`:** `role` (default `data`) and `tier` (`hot`, `warm`, `cool`
    or `cold`).
  - **`requireDataSlab`** (#42): act only on a node that already has a
    stormblock data slab (phase Waiting until it does).
  - **`suspend`** and **`dryRun`**.

  Each pass (every `kubernetes.interval_secs`), every node the policy
  selects:
  - takes the stamped requester and re-checks that they may `create
    driveoperations` (no stamp: Refused);
  - looks at each drive the selector picks;
  - hands the ones it may take to the drive worker as a job tagged with the
    policy: `format` if needed, then `partition` and `enroll`. The worker's
    guards and lanes apply, the enroll runs one per failure domain, and the
    requester is re-checked before every step.

  It never takes a drive that is in the fleet, holds data (`in_use_by`, or
  a slab, RAID set or filesystem found on the disk), is designated
  reserved, spare or failed, is failing, or is busy. A drive whose job
  failed is not tried again until the policy is edited (a new
  generation). Interrupted jobs resume once the requester re-checks.
  Deleting the policy cancels the steps not yet started.

  Status is per node, in `status.nodes.<node>`: phase (Active, Planned,
  Suspended, Refused, Pending, Invalid), requester, and per drive: state
  (skipped and why, planned, queued, running, enrolled, failed), job and
  steps. Each decision is also a Kubernetes Event on the policy.

```bash
kubectl get drives -l storm.io/node=c2nr0q2 -o wide
kubectl apply -f deploy/driveoperation.example.yaml   # dryRun: true first
kubectl get driveoperations
kubectl apply -f deploy/drivepolicy.example.yaml      # dryRun: true first
kubectl get drivepolicy stormblock1-warm -o jsonpath='{.status.nodes}'
```

## API

All JSON on :9092, over TLS with a credential
([above](#who-may-read-or-change-a-drive-19-45)). Errors are `{"error": "...", "code": "not_found" |
"bad_request" | "conflict" | "stormblock" | "internal" | "unauthorized" |
"forbidden" | "unavailable" | "tls_required"}`. Every non-GET needs a
storage-admin, except a worker job dry run (gated as a read). A drive `{id}` is
its DriveId, WWID (any case), `/dev` path or kernel name, or serial, looked up
in that order. A shelf `{key}` is its logical id (with or without `0x`, any
case), serial, shelf id, or an SES device's SCSI id.

| Method and path | What |
|---|---|
| `GET /api/v1/health`, `/healthz` | `{status, version, node, writes, reads}` — liveness, how writes are decided, whether anonymous reads are served; the only paths open over plain HTTP, and with no credential besides the page's assets |
| `GET /`, `/ui`, `/ui/` | the embedded page; works behind a proxy prefix (401 with the page when no credential) |
| `GET /assets/app.{js,css}`, `/ui/assets/…` | the page's two assets (open over TLS) |
| `GET /api/v1/summary` | stormd `RemoteSummary` card from cached state |
| `GET /api/v1/monitor` | health-poll cost, stuck drives, last discovery pass |
| `GET /metrics` | Prometheus text: per-drive SMART, temperature, wear, errors, last poll; shelf sensors; the poller (see below) |
| `GET /api/v1/drives` | every drive, with any running test/format/firmware run inlined |
| `GET /api/v1/drives/{id}` · `DELETE` | one drive · forget a missing, out-of-fleet drive |
| `GET /api/v1/drives/{id}/health` | health report + trend |
| `GET /api/v1/drives/{id}/history?limit=` | the drive's records from system-data, oldest first, the newest `limit` (100) (#64) |
| `GET /api/v1/history` | the system-data directory, whether history is written, records and findings this run, this boot's assets file (#64) |
| `GET /api/v1/assets` | this boot's hardware record and what changed since the previous boot (#64); 404 before it is taken |
| `GET /api/v1/drives/{id}/slabs` | slabs on the disk, the engine's slabs on it, the engine's slab report and the finding (#58) |
| `GET /api/v1/drives/{id}/supports[?want=4096+type1]` | probe now (INQUIRY PROTECT, VPD 0x00/0x86/0xB1/0xB4, MODE SENSE, READ CAPACITY(16); reads only): block lengths and PI types the drive offers, its current format, and `plannedFormat` (`fallback: true` when it is not 4096+PI1) (#85, #82) |
| `GET /api/v1/scsi/not-good` | every SCSI command stormdrive sent that did not end GOOD, per device and opcode/page, since start — to hold against `health.io_errors` (the kernel's `ioerr_cnt`, every sender) (#82) |
| `POST /api/v1/drives/{id}/locate` | `{"on": bool}` |
| `POST /api/v1/drives/{id}/fleet` | `{"action":"join","format_slab"?,"tier"?}` or `{"action":"leave","drain"?,"force"?}` |
| `GET·POST·DELETE /api/v1/drives/{id}/drain` | status · start (`?leave=true` retires when empty) · cancel |
| `POST /api/v1/drives/{id}/designation` | `{"designation":"none\|reserved\|spare\|failed"}` |
| `GET·PUT·POST /api/v1/drives/{id}/overcommit` | `{"enabled":bool,"ratio"?}`; GET adds promisable/committed/headroom |
| `GET·POST /api/v1/drives/{id}/test`, `POST …/test/cancel` | `{"kind":"smoke\|read_scan\|destructive_sample"}` |
| `POST /api/v1/drives/{id}/enroll[?tier=]` | an offered (`enrolable`) drive → partition + enroll as a data slab (worker job; 409 otherwise) (#42) |
| `GET·POST /api/v1/drives/{id}/format` | `{"block_size":512\|4096}` (default 4096) |
| `GET·POST /api/v1/format` | all runs · `{"drives":[…],"block_size"}` |
| `GET·POST /api/v1/worker/jobs` | drive worker jobs · `{"select":{…},"steps":[…],"destroy"?,"dry_run"?}` |
| `GET /api/v1/worker/jobs/{id}`, `POST …/{id}/cancel`, `POST …/{id}/resume` | one job; stop what hasn't started; continue interrupted drives |
| `GET·POST /api/v1/drives/{id}/firmware` | version + runs · `{"image","force"?}` |
| `GET·POST /api/v1/firmware` | all runs · `{"image","drives"?,"model"?,"force"?}` |
| `GET /api/v1/firmware/images`, `GET·PUT·DELETE …/images/{name}` | image store; PUT takes the raw image |
| `GET /api/v1/shelves`, `GET …/shelves/{key}` | SES identity, status, elements (with descriptor `attributes`, thresholds), slots, drives (vendor, model, firmware, sector size); `summary`: shelf ID, chassis serial, IOMs, PSUs, connectors, multipath, `problems`; `help_text` (#81, docs/netapp-shelf.md) |
| `GET /api/v1/shelves/{key}/diagnostics` | the diagnostic pages each ESP answers (page 0x00) |
| `GET /api/v1/shelves/{key}/diagnostics/{page}?esp=` | one page raw as hex (`0a`, `0x80`): RECEIVE DIAGNOSTIC RESULTS only — `sg_ses -p N -r` |
| `POST /api/v1/shelves/{key}/locate` | `{"on":bool,"bay"?}` |
| `POST /api/v1/shelves/{key}/bays/{bay}/power` | `{"on":bool}` — SES DEVICE OFF; a drive operation; refused while the bay's drive is in the fleet, holds data or is busy (#81) |
| `GET·POST /api/v1/shelves/{key}/firmware` | `{"image","allow_path_loss"?}` — the shelf's IOMs, one at a time (#35); GET: each IOM's revision + the run |
| `POST /api/v1/shelves/{key}/format` | `{"block_size","all"?}` — out-of-fleet drives that need it |
| `GET /api/v1/topology` | controller → shelf → drive tree, with HBA firmware |
| `GET /api/v1/hbas` | every PCIe SCSI HBA |
| `GET /api/v1/placement`, `GET …/placement/{id}` | where each drive is; `generation`, ETag, `?since=` / `If-None-Match` → 304 |
| `GET /api/v1/events?since=<seq>` | `{latest_seq, started, persisted, events}`; seq continues across restarts (#25) |
| `GET /api/v1/components`, `GET /ws/components` | stormview feed (drives, shelves, HBAs); the socket pushes on change, checked every 2 s |
| `GET /apis`, `/apis/storage.storm.io`, `/apis/storage.storm.io/v1` | API discovery for kubectl-style clients |
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
| `smartctl_device_capacity_bytes`, `smartctl_device_block_size{blocks_type}` | geometry; `blocks_type` `logical` (as the drive reports it, 520 included) and `physical` |
| `stormdrive_drive_info{id,wwn,kind,firmware,membership,designation,activity,owner}` | 1 per drive |
| `stormdrive_drive_health_status{status}` | 1 for the current verdict |
| `stormdrive_drive_io_errors_total` | SAS/SATA: sysfs `ioerr_cnt`, failed commands since boot (not media errors) |
| `stormdrive_drive_predicted_failure` | SAS/SATA: 1 when the drive predicts its own failure (LOG SENSE 0x2F / ATA SMART threshold) (#22) |
| `stormdrive_drive_reallocated_sectors`, `_pending_sectors`, `_offline_uncorrectable_sectors` | SATA: ATA SMART 5 / 197 / 198 (#22) |
| `stormdrive_drive_unsafe_shutdowns_total` | NVMe |
| `stormdrive_drive_last_poll_timestamp_seconds` | last health poll the drive answered |
| `stormdrive_drive_used_bytes`, `_free_bytes` | from stormblock's slabs (#12) |
| `stormdrive_drive_wear_out_days`, `_wear_rate_pct_per_day` | a line through the wear trend (#23); absent until there is a week of it with the wear moving |
| `stormdrive_drive_volumes`, `_volumes_degraded` | volumes with legs on the drive, and those whose slabs here are draining/quarantined/failed/missing (#26); absent until the engine reports placement |
| `stormdrive_enclosure_info`, `_ok`, `_last_scan_timestamp_seconds` | per shelf |
| `stormdrive_enclosure_element_ok`, `_temperature_celsius`, `_fan_rpm`, `_volts`, `_amps` `{type,index}` | per installed SES element |
| `stormdrive_poll_*`, `stormdrive_discovery_seconds`, `stormdrive_build_info{version,node}` | the daemon |

A missing drive keeps `smartctl_device`, `stormdrive_drive_info` and its
last-poll time; its readings go. Grown defects on SAS HDDs (READ DEFECT
DATA) are not read.

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

## Remote calls and retries (#71)

Three clients leave the process; nothing else does (SG_IO, netlink, sysfs
and `/dev` are local; there is no ssh, and DNS happens only inside the HTTP
clients). Every one of their calls goes through one helper, the workspace
crate `retry/` (`retry::with_backoff(policy, what, op, classify)`):

- **bounded**: at most `attempts` tries, none started past the
  whole-call `deadline`; each try's own timeout is cut to what is left;
- **backoff with jitter**: `base · 2^(n-1)` capped at `max_delay`, then
  half fixed + half random; a `Retry-After` is a floor;
- **transient vs. answer**: timeouts, refused/reset connections, 5xx, 408
  and 429 are retried; any other 4xx (and a validation error) is a real
  answer, returned at once;
- **idempotency**: a write a repeat could apply twice is retried only when
  it never reached the server (connect failed) or was refused unprocessed
  (429);
- **logged**: `stormblock GET /api/v1/drives: succeeded on attempt 3 after
  1.4 s`, or `… infrastructure: gave up after 4 attempts / 27.9 s: HTTP 503`
  (warn);
- **classified**: giving up is infrastructure — `retry::Infra` in the
  engine client's error (`stormblock::is_infra`), `KubeError::is_infra()`
  for the apiserver. The REST API answers an engine that was not there
  with **503 `unavailable`** (kube routes: `ServiceUnavailable`), and a
  refusal from it with 502 `stormblock`.

| Policy | Used by | Tries | Base | Max delay | Deadline | One try |
|---|---|---|---|---|---|---|
| `ENGINE` | the engine (:9090) | 4 | 250 ms | 4 s | 30 s | 5 s |
| `ENGINE_SLOW` | `GET /api/v1/volumes?placement=true` | 3 | 1 s | 5 s | 100 s | 30 s |
| `ENGINE_FORMAT` | `POST /api/v1/slabs` (slab format, #72) | 4 | 250 ms | 4 s | 20 min | 15 min |
| `KUBE` | the apiserver (gate reviews, controller) | 3 | 200 ms | 2 s | 20 s | 10 s |
| `TEST` | the test container → the node's :9092 | 4 | 500 ms | 5 s | 150 s | 60 s |

Call sites and what each repeats:

| Call | File | Retried | Why |
|---|---|---|---|
| engine `GET` drives, slabs, drive slabs, health, arrays, drain status, volumes | `src/stormblock.rs` | yes | reads |
| engine `PUT` labels, overcommit | `src/stormblock.rs` | yes | a whole value: the same twice |
| engine `POST …/health` | `src/stormblock.rs` | yes | sets a state |
| engine `DELETE …/drain` | `src/stormblock.rs` | yes | stopping a stopped drain is a no-op |
| engine `DELETE /drives/{id}` (admin) | `src/stormblock.rs` | yes | a 404 after an earlier try that may have landed = closed |
| engine `POST /drives` (open) | `src/stormblock.rs` | connect only | a second open is refused; the fleet loop's backoff and reconcile retry it |
| engine `POST /slabs` (format, admin) | `src/stormblock.rs` | connect only | a format must not run twice; one try may take 15 min (`ENGINE_FORMAT`): a try cut off is a format the engine abandons (#72) |
| engine `POST …/drain` | `src/stormblock.rs` | connect only | a running drain answers 409; the fleet tick retries pending drains (#43) |
| apiserver `GET`, merge `PATCH`, `DELETE` | `src/kubeapi.rs` | yes | reads; a patch sets fields; callers take 404 as done |
| apiserver TokenReview, SubjectAccessReview | `src/kubeapi.rs` | yes | nothing is stored |
| apiserver create Drive | `src/controller.rs` → `create_retried` | yes | fixed name; 409 AlreadyExists is done |
| apiserver create Event | `src/controller.rs` → `create` | connect only | a repeat is a second Event |
| test container reads (`get`, `get_as`, `If-None-Match`, watch) | `test/src/api.rs` | yes | reads |
| test container writes (`post`, `post_empty`, `delete`, `post_as`) | `test/src/api.rs` | connect only | they start things (a smoke test, a designation) a repeat would find busy |

The engine's own 401 handling stays inside one try: a 401 re-reads the
token and sends again at once (a rotated token, not a flaky engine).

**The test container** reports a test that infrastructure stopped — the
node's stormdrive not answering through `TEST`, a transport error on a write
it would not repeat, or a 503/504 from stormdrive (its engine or apiserver
was not there) — as `{"status": "skip", "infrastructure": true, …}`, counts
it in the summary's `infrastructure`, and exits **2** (could not run), never
1, unless a test really failed. Retries are logged on its stderr.

## How it ships

stormdrive is a **service** component on stormcentral (`stormcentral
component list`). Its golden is built by
`stormcentral component build stormdrive`, which runs stormcos's
`service_golden` recipe. That recipe builds the musl binary and puts it in a
stormd-based golden with:

- `/etc/stormdrive/stormdrive.toml`, which is the registry entry's config
  text (`stormcentral component edit stormdrive`), today: `listen_addr =
  "0.0.0.0:9092"`, `data_dir = "/var/lib/stormdrive"`, `[api]
  allow_anonymous = true` (the #19 transition, until stormcos#352 and
  stormconsole#49 ship: #56), and `[kubernetes]` with `api_url =
  "https://127.0.0.1:6443"`, `ca_file = "/data/stormcert/ca.crt"`, `token_file
  = "/data/stormcert/stormdrive.token"` (the `kube-system/stormdrive`
  storage-admin token) and `controller = false` (until the release installs
  the CRDs and the controller role: stormcos#369);
- stormd running `/usr/sbin/stormdrive --config /etc/stormdrive/stormdrive.toml`,
  restarted on exit, with an HTTP liveness probe on `/api/v1/health`, and
  stormd's own API on :9192.

A commit reaches a node only through that path:

1. A golden is built from a pushed commit.
2. stormcos composes the golden into a release.
3. Nodes clone the release copy-on-write. Nothing pulls an image.

The one git dependency, `stormview`, is pinned to a `rev` in `Cargo.toml`
(#63; golden builds refuse an unpinned git dependency). A fix there arrives
only with a deliberate rev bump here. How
goldens and releases work is written up in stormcos
[`docs/goldens.md`](https://github.com/glennswest/stormcos/blob/main/docs/goldens.md).

The crate is a Cargo workspace (`.`, `test/` and `retry/`, all default
members). The
recipe's release build therefore also compiles the test binary, but the
golden carries only `/usr/sbin/stormdrive`. The test image is built
separately by stormcentral from `test/Containerfile`.

stormcos starts it on every node profile (`boot.d/40-services`). It runs with
the host network, the host's `/dev`, the host's `/sys` **read-only**, its
data volume at `/var/lib/stormdrive`, and the engine token from
`/run/stormblock` (`STORMBLOCK_TOKEN_FILE`). The read-only `/sys` means that
in the golden, sysfs locate LEDs and the post-format rescan fail. SES and
SG_IO paths work (stormcos#166; a writable `/sys` is in stormpump and ships
with a release that carries it, #54). The serving pair and node CA are read
from `/data/stormcert` (the defaults of `[api] tls_*`; the pair itself is
stormcos#352), and drive history + assets go to `/data/system-data` once
stormcos mounts it into the unit (stormcos#456, stormblock#355); until then
`GET /api/v1/history` says it is off.

The node's HTTPRoute publishes it by name (`drive.storm1.g8.lo` in stormcos
`deploy/manifests/85-routes.yaml`). stormconsole on :9094 reads its
components feed. The stormd `[process.ui]` card snippet
([deploy/stormd-ui.toml](deploy/stormd-ui.toml)) and the systemd unit
([deploy/systemd/stormdrive.service](deploy/systemd/stormdrive.service)) are
for installs outside stormcos.

## Not yet

Built, but not running in the golden yet:

- `DriveOperation`s and `DrivePolicy`s: the registry config has
  `[kubernetes] controller = false` until the release installs the CRDs and
  the controller role (stormcos#369). stormdrive's credential is in the
  registry `[kubernetes]` (stormcos#296) and the requester stamp
  (rustkube#210) has shipped
- drive history and assets: written once system-data is mounted
  (stormcos#456)

Design only; the code does not do them:

- a node-wide sequencer for every disruptive operation (the firmware
  redundancy gate itself is #24, built)
- the page: shelf (IOM) firmware (#60, API only) and offered drives (#55,
  feed only)
- waiting on your decision: the drive worker's scheduling default (#37)
- waiting on your decision:
  - thermal actuation (#32)
  - drive crypto: SED, crypto erase (#33)
  - burn-in before a drive joins the fleet (#34)
  - where vendor firmware images come from (#29)

Built but never exercised on real hardware:

- **Firmware updates:** nothing supplies an image (#29).
- **The drive worker's disk operations** (NVMe Format NVM, both sanitizes,
  the ATA security erase, GPT on a real disk, BLKRRPART, enroll through a partition) are tested on
  synthetic data. The GPT layout is also checked by `sfdisk --verify` on a
  file. On real drives: #30.
- **The NetApp shelf path:** SES, the 520 → 4096 format, phy/expander, IOM
  firmware (#35; no image yet, #29) (#30).
- **160-bay NVMe:** verified by simulation only (#31, `tests/chassis160.rs`);
  no real chassis is planned.
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
