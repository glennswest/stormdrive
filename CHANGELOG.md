# Changelog

## [Unreleased]
<!-- New unreleased changes go here -->

### 2026-10-10 (#84)
- **BREAKING:** stormdrive serves no page (owner: "stormdrive should be part of stormui, as a plugin"): `web/` (the Svelte page, its committed dist and `web/rebuild.sh`) is removed with the `/`, `/ui`, `/ui/`, `/assets/*` routes and the gate's page exceptions (open assets, the 401 sign-in shell). Those paths are now refused like any read without a credential (401) and unknown with one (404). Its views move to stormconsole's `drive` plugin (stormconsole#131); the API, the components feed and the `storage.storm.io` resources stay (#84)
- **test:** the test container drops the page checks (short `page`, medium `page-under-ui`); `reads-need-a-credential` checks `/` is 401; the TLS harness checks the old page paths are refused (#84)
- **docs:** README, architecture, presentation, `deploy/stormd-ui.toml` (summary card only), CLAUDE.md; version locations are `Cargo.toml`, `Cargo.lock`, CLAUDE.md (#84)

## [v0.29.0] — 2026-10-10

### Added
- `plannedFormat.fallback` + a warning event when a drive does not offer 4096+PI1; dry runs show each drive's PI type and planned format; `GET /api/v1/scsi/not-good` (#82)

### Fixed
- Drive kind from the drive's own rotation rate (VPD 0xB1): a 520-byte 10K disk no longer reads `sas_ssd` (#82)

### 2026-10-10 (#82)
- **fix:** drive kind from the drive's own rotation rate (VPD 0xB1) when it reports one — a 520-byte 10K disk (ST1200MM0098) read `sas_ssd` because sd never attached it and `rotational` reads 0; existing drives are reclassified with an event (#82)
- **feat:** `plannedFormat.fallback`: a plan that is not 4096+PI1 (and not asked for) is flagged with its reason, and a warning event when the drive is probed — an issue, not a quiet fallback (#82)
- **feat:** worker dry runs list each drive's current PI type and default `planned_format` (#82)
- **feat:** `GET /api/v1/scsi/not-good`: SCSI commands stormdrive sent that did not end GOOD, per device and opcode/page, to compare with `io_errors` (#82)

## [v0.28.0] — 2026-10-10

### Added
- NetApp DS224C shelf decode (#81): descriptor attributes, NetApp vendor elements, `summary` (shelf ID, chassis serial, IOMs and their paths, PSUs, connectors, multipath verdict, problems), help text, thresholds; raw diagnostic page reads; bay power (DEVICE OFF); drive `vendor`; grown defect count; dry runs list each bay
- PI formats (#85): `Drive.supports` probe, `GET /api/v1/drives/{id}/supports`, Drive status `supports`/`currentFormat`/`plannedFormat` from `spec.format`, format step `protection: type1` (FMTPINFO), PI read back, controller reset watch

### Fixed
- SES enclosure element failure/warning indication bits (#86); overall elements are not problems (#81)

### Documentation
- docs/netapp-shelf.md: element map, multipath, every SCSI command, PI format flow (#81, #85)

### 2026-10-10
- **feat:** what a drive can be formatted to (#85, `pi.rs`): INQUIRY PROTECT, VPD 0x86 SPT, 0xB4 block lengths × PI types, 0xB1 rotation, MODE SENSE block length, READ CAPACITY PROT_EN/P_TYPE → `Drive.supports`; probed in discovery and by `GET /api/v1/drives/{id}/supports`; a plan (spec.format when offered, else 4096+PI1 → 512+PI1 → 4096 → 512, 4096 never assumed without VPD 0xB4)
- **feat:** Drive objects carry `status.supports`, `currentFormat`, `plannedFormat`; `spec.format {blockSize, protection}` (the user's, never overwritten) feeds the plan (#85)
- **feat:** the worker's format step takes `protection: none|type1` (DriveOperation and REST): FORMAT UNIT FMTPINFO, refused unless the probe says the drive offers it; the result is verified by READ CAPACITY PROT_EN/P_TYPE (#85)
- **feat:** a format watches the HBA's `ioc_reset_count`: a controller reset is recorded on the run, raised as an event, and no new format starts until stormdrive restarts; progress is read every 30 s (#85)
- **fix:** SES enclosure element: FAILURE / WARNING INDICATION read from the right bits (byte 2 bits 1/0, byte 3 bits 1/0 requested), shown as flags; byte 3's power-off duration bits are no longer taken for a fault. The DS224C's fault LED reads lit (#86, #81)
- **feat:** NetApp DS224C (IOM12) shelf management (#81), from the shelf on the Dell: SES descriptors kept as `attributes` (`KEY=VALUE;`); NetApp vendor elements named (0x83 IOM expander, 0x85 IOM Ethernet); chassis serial from the enclosure element (VPD 0x80 is the IOM's); help text (0x03), thresholds (0x05) and supported pages (0x00) read; `/api/v1/shelves` gains `summary` (shelf ID, IOMs with path seen, PSUs, connectors cabled to which HBA, multipath verdict, `problems`) and `help_text`
- **feat:** `GET /api/v1/shelves/{key}/diagnostics[/{page}]`: any SES diagnostic page raw, read only (#81)
- **feat:** `POST /api/v1/shelves/{key}/bays/{bay}/power {on}`: SES DEVICE OFF, a drive operation, refused for a fleet / data-holding / busy drive (#81)
- **feat:** drives carry their INQUIRY `vendor`; shelf drive rows carry vendor, firmware, kind, physical sector size; a worker dry run lists shelf, bay, vendor, model, serial, firmware, kind, sector size and capacity per drive (#81)
- **feat:** SAS health reads the grown defect count (READ DEFECT DATA(12) header) — `smart.grown_defects`, `stormdrive_drive_grown_defects`, a history finding when it grows (#81)
- **fix:** an overall SES element (all zero bytes: "unsupported") no longer reads as a problem: `problems` lists individual elements only (#81)
- **docs:** docs/netapp-shelf.md — the DS224C element map, why it reads noncritical (bay 5), multipath (two IOMs, one cabled; the SAS9300-4i4e has one external port), every SCSI command stormdrive sends to shelves and drives (#81)

### 2026-10-09
- **docs:** README, docs/architecture.md, docs/presentation.md and CLAUDE.md refreshed from the code for everything since 2026-10-02 (v0.18.0 → v0.27.1): every config validation rule, `data_dir` contents, the in-cluster apiserver env and service-account paths; open paths (page assets over TLS only), test/job cancel and resume verbs, health's `reads`, API discovery routes, `block_size{blocks_type="physical"}`; what the golden's registry config sets (`allow_anonymous`, `[kubernetes]` with `controller = false` until stormcos#369) and its `/data/stormcert` and `/data/system-data` mounts; stormview pinned to a rev (#63); `retry/` in the workspace; every suite and integration test; the build VM; the drive model (contents, slab parts, engine finding, enrolable, wear projection, io_errors, not_collected); the firmware redundancy gate is built (#24); the deck's "planned" list now holds only what isn't built; PVCs are served by stormblock's built-in driver; the NetApp shelf's drives are not Seagates (#70)

## [v0.27.1] — 2026-10-08

### Fixed
- Enrol: the engine's slab format gets one try of up to 15 min (`ENGINE_FORMAT`), not 5 s — a large drive's format was cut off and never enrolled (#72)

### Documentation
- Architecture "Cost at enrol: metadata only": every path that touches a drive and its bytes; partition, sampled tests and policy enrol tested at 1 PiB (#72)

### 2026-10-08 (in v0.27.1)
- **fix:** enrol: the engine's slab format (`POST /api/v1/slabs`) gets one try of up to 15 min (`retry::Policy::ENGINE_FORMAT`) instead of 5 s; a large drive's format, which zero-fills the engine's slot table today (stormblock#363), was cut off and abandoned halfway, so the drive never enrolled (#72)
- **test:** #72 — partitioning a 1 PiB disk writes ~8 MiB (counted) in well under a second; a 1 PiB drive is enrolled by partition + slab steps only; smoke and destructive_sample tests cost the same at 1 PiB as at 1 GiB (`drivetest::planned_bytes`)
- **docs:** architecture "Cost at enrol: metadata only" — every path that touches a drive with its bytes; README retry table gains `ENGINE_FORMAT` (#72)

## [v0.27.0] — 2026-10-08

### Added
- One retry helper for every remote call, the workspace crate `retry/`: bounded tries, a whole-call deadline, backoff with jitter, Retry-After, transient vs real answers, idempotency-aware, attempts logged; policies `ENGINE`, `ENGINE_SLOW`, `KUBE`, `TEST` (#71)
- The engine, apiserver and test-container clients retry through it; giving up is classified infrastructure (`stormblock::is_infra`, `KubeError::is_infra`) (#71)

### Changed
- REST: an engine not there through its retries answers 503 `unavailable` (was 502) (#71)
- Test container: a test stopped by infrastructure is a `skip` with `"infrastructure": true` and the run exits 2, not 1 (#71)

### 2026-10-08 (in v0.27.0)
- **feat:** every remote call retries through one helper, the workspace crate `retry/` (`retry::with_backoff`): bounded tries and a whole-call deadline, exponential backoff with jitter, Retry-After honoured, transient failures (timeouts, refused/reset, 5xx, 408, 429) retried and real answers (4xx) returned at once, writes that may not repeat retried only when they never left; "succeeded on attempt N after X s" / "gave up after N attempts / X s" logged; policies `ENGINE`, `ENGINE_SLOW`, `KUBE`, `TEST` (#71)
- **feat:** engine client: reads, label/overcommit PUTs, health reports, drain cancel and drive close (404 after a lost answer = done) retried; drive open, slab format and drain start retried only on connect failure; giving up is `retry::Infra` (`stormblock::is_infra`) (#71)
- **feat:** REST: an engine that was not there through its retries is **503 `unavailable`** (kube routes `ServiceUnavailable`), a refusal from it stays 502 `stormblock` (#71)
- **feat:** apiserver client: get/patch/delete, TokenReview/SubjectAccessReview and Drive creates retried; Event creates only on connect failure; `KubeError::is_infra()` (#71)
- **feat:** test container: its calls to :9092 retry (reads; writes on connect failure only); a test stopped by infrastructure is reported `skip` with `"infrastructure": true`, counted in the summary, and the run exits 2 instead of 1 (#71)
- **docs:** README "Remote calls and retries": policies and every call site (#71)

## [v0.26.0] — 2026-10-08

### Added
- Drive history in system-data: `history/drives/<wwn|serial>/<YYYY-MM>.jsonl`, every counter per drive, recorded on change or hourly; findings (an error counter grew, a lifetime counter went backwards) across restarts and installs, as events; `GET /api/v1/drives/{id}/history`, `GET /api/v1/history` (#64)
- Hardware assets in system-data: `assets/<UTC>-<boot_id>.json` per boot (DMI, CPUs, DIMMs, BMC, NICs, NVMe controllers, HBAs, shelves, drives by bay) with the changes since the previous boot; `GET /api/v1/assets` (#64)
- SAS error counter pages 0x02/0x03/0x05 (uncorrected = `media_errors`), the full SATA SMART table, CRC errors, more NVMe log 0x02 counters (#64)
- `[history]` config; short suite `system-data` (#64)

### Changed
- `stormview` pinned to a rev (#63)


### 2026-10-08 (in v0.26.0)
- **test:** short suite `system-data`: history status, this boot's assets, a record for every sampled drive (skipped while system-data is not mounted); the cargo-test harness gives its daemon a system-data dir (#64)
- **feat:** drive history in system-data: `history/drives/<wwn|serial>/<YYYY-MM>.jsonl`, a record per drive on change or hourly with every counter; findings (an error counter grew, a lifetime counter went backwards) across restarts and installs, as events; the first sample after an install grows from the history's media errors; `GET /api/v1/drives/{id}/history`, `GET /api/v1/history` (#64)
- **feat:** hardware assets in system-data: `assets/<UTC>-<boot_id>.json` per boot (DMI, CPUs, DIMMs from SMBIOS 17, BMC from SMBIOS 38, NICs, NVMe controllers, HBAs, shelves, drives by bay) with the changes since the previous boot, as an event; `GET /api/v1/assets` (#64)
- **feat:** SAS health reads the Write/Read/Verify error counter pages (0x02/0x03/0x05); their uncorrected errors are the drive's `media_errors`. SATA: the whole SMART attribute table (with worst + threshold) and CRC errors (199). NVMe: host read/write commands, busy minutes, minutes over the warning/critical temperature (#64)
- **feat:** `[history]` config: the system-data volume's directory, heartbeat, months kept (#64)
- **build:** `stormview` git dependency pinned to `rev = 81ef1d2` instead of `branch = "main"` (Cargo.lock `?rev=`), so golden builds accept it under stormcentral#571's SBOM rule; moving it forward is now a deliberate rev bump (#63)
- **docs:** work plan: #58 done in v0.25.0 (golden-stormdrive-4a5c915a3ae1)

## [v0.25.0] — 2026-10-08

### Added
- A drive whose slabs the engine doesn't use is a finding: `engine_finding`, warning (suspect) or failing, from the engine's slab report (#58)
- `GET /api/v1/drives/{id}/slabs` (#58)
- `health.not_collected` and a warning event when a drive has no health sample (#58)

### Fixed
- `ioerr_cnt` is `io_errors`, not media errors, and no warning; `media_errors` is the drive's own count (#58)
- The health loop is restarted if it ever stops (#58)

### 2026-10-08
- **fix:** the kernel's `ioerr_cnt` (commands failed since boot, resets included) is `health.io_errors`, no longer `media_errors`, and its growth is no warning. The Dell's "media errors growing 32 → 33" was this counter while the drive's own SMART logs had none. `media_errors` is the drive's own count only (NVMe log 0x02, ATA attribute 187, also in `health.smart.reported_uncorrectable`) (#58)
- **feat:** a drive whose slabs the engine doesn't use is a finding (#58). Each monitor tick reads the engine's `GET /api/v1/health` `slabs` report (stormblock#344). A drive carrying stormblock slab partitions (role from the GPT type) of a half the engine runs `remote`, or on a `diskless` node, gets `engine_finding` and turns `warning` (suspect); `failing` when the boot says it took the disk and it failed. The engine's reason is in the message, and it raises an `engine` event
- **fix:** no health sample is never silent (#58): `health.not_collected` says why, three intervals without a sample is a warning event, and the health loop is restarted (with an error event) if it ever stops
- **feat:** `GET /api/v1/drives/{id}/slabs`: the slabs on the disk, the engine's slabs on it, the engine's slab report and the finding (#58)
- **feat(web):** the drive pane shows media errors, io errors and why health is missing (#58)
- **test:** short suite `drive-slabs`; `health-verdicts` requires a reason for every unsampled drive (#58)
- **docs:** work plan: #21, #22, #52 built and released in v0.24.0 (golden-stormdrive-0352752c1cbe, stormcos#408)

## [v0.24.0] — 2026-10-08

### Added
- SAS/SATA health from the drive: LOG SENSE 0x2F/0x0D/0x11 and ATA SMART; predicted failure → `failing`, pending/offline-uncorrectable sectors → `warning`; `health.smart` and four new Prometheus series (#22)

### Fixed
- SIGTERM stops gracefully and persists, like SIGINT (#21)
- clippy `large_enum_variant` on the poller's outcome (#52)

### 2026-10-07
- **fix:** SIGTERM (what stormd and systemd send) stops the daemon gracefully like SIGINT: a `stopping` event, then the event log and the inventory are written, exit 0. Before, SIGTERM killed it without the final write (#21)
- **feat:** SAS/SATA health from the drive itself (#22). SAS: LOG SENSE Informational Exceptions (0x2F: the drive's own failure prediction, temperature), Temperature (0x0D) and on SSDs Solid State Media (0x11 → `wear_pct`). SATA (vendor `ATA`, also behind SAS HBAs): ATA SMART READ DATA + THRESHOLDS → reallocated / pending / offline-uncorrectable sectors, temperature, power-on hours, SSD wear; a pre-fail attribute at or below its threshold is a predicted failure. A predicted failure makes the drive `failing`, pending or offline-uncorrectable sectors `warning`. Shown in `health.smart` and Prometheus (`stormdrive_drive_predicted_failure`, `_reallocated_sectors`, `_pending_sectors`, `_offline_uncorrectable_sectors`)

## [v0.23.0] — 2026-10-07

### Added
- Days to wear-out from the wear trend: on the API, kube status, feed, Prometheus and the drive pane; a warning event under `monitor.wear_out_warn_days` (#23)

### 2026-10-07
- **feat:** days to wear-out (#23). A least-squares line through the last year of the wear trend gives `wear_projection {rate_pct_per_day, days_left, wear_out_unix, …}` on each drive, refitted with every trend sample. There is none under a week, with fewer than two values, or with wear not growing. It appears on /api/v1/drives, the kube Drive status, the feed (`wear-out`, warn under 180 d), Prometheus (`stormdrive_drive_wear_out_days`, `_wear_rate_pct_per_day`) and the drive pane. A `wear` warning event fires when it first drops under `monitor.wear_out_warn_days` (180)

## [v0.22.1] — 2026-10-07

### Changed
- `discovery::scan_in`, `topology::locate_in` / `set_locate_in` take the sysfs root (#31)

### Tests
- A simulated 160-bay NVMe chassis through the real discovery, topology and locate (#31)

### 2026-10-07
- **test:** a simulated 160-bay NVMe chassis (#31: the owner chose simulation; no real chassis is coming). `tests/chassis160.rs` builds the sysfs tree: 120 drives behind a PCIe switch, 40 behind VMD, 8 under native multipath, 150 attention indicators and 10 NPEM LEDs. Discovery, topology and locate run over it unchanged: 160 drives, bay = slot, the right LED, a pull gone, a push `replaces` the pulled drive
- **refactor:** `discovery::scan_in`, `topology::locate_in` / `set_locate_in`: the sysfs (and /dev) root as a parameter; `scan`/`locate`/`set_locate` call them with /sys and /dev (#31)

## [v0.22.0] — 2026-10-07

### Added
- `DrivePolicy` (`storage.storm.io/v1`): which drives of which nodes become stormblock slabs of which tier, reformatted first when they need it, through the drive worker under the stamped requester; `requireDataSlab` (#50, #42)
- Offer: `enrolable` on blank, healthy, out-of-fleet drives with an event; `POST /api/v1/drives/{id}/enroll` (#42)
- Drive worker steps `security_erase {enhanced?}` (ATA SECURITY ERASE for SATA without Sanitize, #36) and `test {kind}` (server-side bulk tests, a qualify gate before enroll, #40)
- Shelf (IOM) firmware via SES Download Microcode, one IOM at a time (#35)
- `usage.volumes`: the volumes on each drive, from the engine's placement (#26)
- Fault LEDs on bays of failed RAID set members (stormblock#252, #44)
- Firmware redundancy gate before a data-serving drive resets (#24)

### Fixed
- A pulled fleet drive is reported `missing` to stormblock (#44)
- The event log survives a restart; seq continues (#25)

### Changed
- The page's bulk smoke test / read scan is one worker job (#40); the drive pane lists the volumes on the drive (#26)

### 2026-10-07
- **feat:** firmware redundancy gate (#24). A drive that serves data resets only while every volume with legs on it is fully redundant, per the engine's placement (`/api/v1/volumes?placement=true`: redundancy `healthy`, no rebuild, its slab there `ok`). It waits up to `firmware.redundancy_wait_mins` (30) before the download, then again after the update before the lock passes to the next drive. An engine without placement cannot be checked, so that is refused; `force` skips the gate
- **fix:** a pulled fleet drive is reported `missing` to stormblock (by uuid), so the engine fails its RAID set member and takes a spare. Missing drives were never reported before; the answer's `raid_member` goes into the event (#44)
- **feat:** fault LEDs for shelf RAID sets (stormblock#252). Each fleet tick reads `GET /api/v1/arrays` and lights the SES fault LED (RQST FAULT) of the bay holding a `failed` member: found by registration uuid, else WWN/serial, else its `shelf=…/bay=…` labels. The LED is cleared once the bay has no failed member; lit bays persist in the inventory; events on both (#44)
- **docs:** name a stormblock shelf layout after stormdrive's shelf key so the spare pool and the `shelf=` rung agree (#44)
- **fix:** the event log survives a restart (#25). The newest 512 events and the next seq are kept in `<data_dir>/events.json`, written with the inventory when the seq moved. On start they are restored, the sequence continues (a `since=N` poller never sees it go backwards), and a `restart` event says how many were kept. `/api/v1/events` adds `started` and `persisted`
- **feat:** drive worker `test {kind: smoke|read_scan|destructive_sample}` step: the drive test runs on the server with its verdict in the job, and a failed test stops the drive's later steps (a qualify gate before enroll). Only destructive_sample destroys. A job of only read-only tests may read fleet, reserved and mounted drives, like the single-drive route. The CRD accepts `test` + `kind` (#40)
- **feat(web):** the bulk smoke test / read scan is one worker job instead of per-drive calls from the browser; Prepare offers a Test step (#40)
- **feat:** shelf (IOM) firmware via SES Download Microcode, page 0x0E (#35). `POST /api/v1/shelves/{key}/firmware {image, allow_path_loss?}` sends an image from the firmware store one IOM at a time, watching the status page while the IOM restarts. The next IOM waits until the drives' paths are back. It is refused when a drive serving data has no second path, unless `allow_path_loss`. `GET` shows each IOM's revision and the run; each ESP path now carries its `revision` (sysfs `rev`). Never automatic; the write gate treats it as a drive operation
- **feat:** offer blank drives (#42). Discovery's cached probe also records `contents` (any filesystem, slab or RAID member, as the worker's destroy guard reads it). Each monitor tick then sets `enrolable` on a drive that is out of the fleet, idle, undesignated, not failing (verdict known), on usable sectors, at least `worker.offer_min_bytes` (1 GiB) and blank, with an `offer` event when it turns so. The flag is on /api/v1/drives, the kube Drive status and the feed (metric + an Enrol action); `worker.offer` turns it off
- **feat:** `POST /api/v1/drives/{id}/enroll[?tier=]`: an offered drive → partition + enroll as a data slab, through the worker (storage-admin; 409 unless offered) (#42)
- **feat:** DrivePolicy `requireDataSlab`: act only on a node that already has a stormblock data slab (phase Waiting) (#42)
- **feat:** drive worker step `security_erase {enhanced?}`: ATA SECURITY ERASE UNIT through ATA PASS-THROUGH(16), for SATA drives with the Security feature set and no Sanitize. It checks IDENTIFY first, and refuses a drive with no security feature set, a frozen drive (never retried), an expired attempt counter, or a password this job did not set. It works under a one-time password that is saved in the job record and a warning event before the drive sees it, and cleared once IDENTIFY reads security off. A failure before ERASE UNIT takes the password off again. A restart interrupts the step and names the password, and a resume checks the drive before erasing again (#36)
- **feat(web):** Prepare offers ATA security erase (normal / enhanced) (#36)
- **feat:** `deploy/crds.yaml`: DriveOperation steps accept `security_erase` and `enhanced` (#36)
- **feat:** `DrivePolicy` (`storage.storm.io/v1`, storage-admin only): which drives of which nodes become stormblock slabs of which tier. A drive the kernel cannot use (520-byte NetApp) is reformatted first, with no hand-run step. The spec has nodes / nodeSelector, a drive selector (kinds, size, sector size, model, shelf, bays), `reformat` (512/4096), `enroll {role, tier}`, `suspend` and `dryRun`. Each node runs it through the drive worker under the stamped requester (re-checked). It never takes a drive in the fleet, holding data, designated reserved/spare/failed, failing or busy. A failed drive waits for a new generation. Status is per node, with Events on the policy; deleting the policy cancels the queued steps (#50, stormcos#251)
- **feat:** `deploy/crds.yaml` `DrivePolicy`; `deploy/rbac.yaml`: drivepolicies get/list/watch + status, nodes get; `deploy/drivepolicy.example.yaml` (stormblock1 `warm`, stormblock2 `cool`) (#50)
- **docs:** README "As Kubernetes objects", architecture "Drives enrolled by policy" (#50)

### 2026-10-06
- **feat:** `usage.volumes` on every drive: the volumes with legs on it, from the engine's `GET /api/v1/volumes?placement=true` (stormblock v17.1+). Each entry has id, name, kind, consumer, bytes, legs, shared legs, the worst slab state here, rebuild, policy and health, largest first. It is absent while the engine reports no placement, and a failed read keeps the last answer with `volumes_collected_at`. This lets a console show them for every node (#26, stormconsole#29)
- **feat:** feed metric `volumes`; Prometheus `stormdrive_drive_volumes`, `stormdrive_drive_volumes_degraded` (#26)
- **feat(web):** the drive pane lists the volumes on the drive, marking any in trouble there (#26)
- **docs:** README, architecture "Per-drive usage": the volumes on each drive (#26)

## [v0.21.1] — 2026-10-06

### Fixed
- Slab format and drive close on the engine present the admin token, then stormdrive's storage-admin Kubernetes credential, then the node token (stormblock#274) (#46)
- A drain cancel goes on the node token: it is ordinary on the engine (#46)

### Added
- `stormblock.admin_token_file` / `$STORMBLOCK_ADMIN_TOKEN_FILE` (default `/run/stormblock-admin/admin_token`); `deploy/rbac.yaml` `stormdrive-engine` (`slabs` create) (#46)

### 2026-10-06
- **fix:** slab format (`POST /api/v1/slabs`) and drive close (`DELETE /api/v1/drives/{id}`) are destructive on the engine since stormblock#274 and refused the node token: stormdrive now presents the engine's admin token (`stormblock.admin_token`, `$STORMBLOCK_ADMIN_TOKEN`, new `stormblock.admin_token_file` / `$STORMBLOCK_ADMIN_TOKEN_FILE` / `/run/stormblock-admin/admin_token`, re-read every call), then its own `[kubernetes]` credential (storage-admin), then the node token, moving on after a 401/403; a final refusal logs what the engine wants (#46)
- **fix:** a drain cancel (`DELETE …/drain`) is ordinary on the engine and goes on the node token, not the admin token (#46)
- **feat:** `deploy/rbac.yaml` `stormdrive-engine` ClusterRole: `slabs` create on `storage.storm.io` for stormdrive's service account (#46)
- **docs:** README, architecture, example config: destructive engine verbs and their credentials (#46)

## [v0.21.0] — 2026-10-06

### Breaking
- :9092 is TLS from the node's stormcert pair, and nothing answers anonymously but health: plain HTTP answers `/api/v1/health` and `/healthz` only; every read needs the admin token, a node-CA client certificate or a bearer allowed `get` on `storage.storm.io` (`storage-viewer`). `[api] allow_anonymous = true` keeps the old behaviour during the transition (#19)

### Added
- `[api] tls_cert_file`, `tls_key_file` (re-read on change), `client_ca_files`, `allow_anonymous`; `/healthz`; `reads.anonymous` in health (#19)
- Writes by a node-CA client certificate, reviewed as its CN/O groups (#19)
- The page sends its bearer on reads and opens sign-in on a 401; the test container speaks https with the node CA, a client pair or the pod's token (#19)

### 2026-10-06
- **BREAKING:** :9092 is TLS from the stormcert pair (`[api] tls_cert_file`/`tls_key_file`, default `/data/stormcert/stormdrive.{crt,key}`, re-read on change), and nothing answers anonymously but health: plain HTTP answers `/api/v1/health` and `/healthz` only (403 `tls_required`), reads need the admin token, a node-CA client certificate (`client_ca_files`) or a bearer allowed `get` on `storage.storm.io` (#19)
- **feat:** writes also accept a node-CA client certificate, reviewed as its CN/O groups (SubjectAccessReview); audited as `cert:<cn>` (#19)
- **feat:** `[api] allow_anonymous` — the transition: plain HTTP and credential-less reads served as before, sent credentials still checked; `/api/v1/health` says `reads.anonymous` (#19)
- **feat(web):** the page sends its bearer on every request; a read refused 401 opens sign-in; the shell answers 401 with the page when no credential is sent (#19)
- **feat(test):** the test container speaks https (node CA from `STORM_STORMDRIVE_CA` or the pod's service account), presents `STORM_STORMDRIVE_CERT`/`_KEY` or the pod's token for reads, falls back to http for a node without TLS; medium `reads-need-a-credential`; the harness runs over TLS; `tests/tls.rs` (#19)
- **docs:** README (transport, reads, writes by certificate, config, curl over https), architecture, presentation, example config (#19)
- **docs:** work plan — #19; done: v0.21.0 verified on 4443927, golden 99a433d7a82c
- **docs:** work plan — #38 + #47 done: v0.20.0 verified on 016f478, golden ea34a213c27a

## [v0.20.0] — 2026-10-06

### Added
- The page submits and follows drive worker jobs: Prepare (steps, dry-run preview, destroy confirmed by serial) from the bulk bar, shelf and drive panes; a Jobs panel with per-drive progress, cancel and resume; the `prep` phase in the state column (#38)
- The page signs in with a storage-admin bearer (tab-only `sessionStorage`, sent on writes); read only while the node enforces and none is set (#47)

### Changed
- The page's formats go through the drive worker (restart-safe, destroy guard) instead of `/api/v1/format`, which stays for API callers (#38)

### 2026-10-06
- **feat(web):** Prepare — submit drive worker jobs from the page (bulk bar, shelf pane, drive pane): steps, dry-run preview with refusals and reasons, destroy confirmed by typing each held drive's serial (#38)
- **feat(web):** Jobs panel — every worker job with per-drive state, step, progress and error; cancel and resume (#38)
- **feat(web):** the state column shows each out-of-fleet drive's `prep` phase; worker progress shows for formatting/sanitizing (#38)
- **feat(web):** sign in with a storage-admin bearer (sessionStorage, sent on writes); read only while the node enforces and none is set; 401/403 read "needs storage-admin" (#47)
- **change(web):** the page's formats (bulk, shelf, drive) go through the drive worker instead of `/api/v1/format` (#38)
- **docs:** work plan — #38 + #47

### 2026-10-06
- **docs:** work plan — #43 done: v0.19.1 verified on 17cd78e, golden 12ef14fdf9d0

## [v0.19.1] — 2026-10-06

### Fixed
- An automatic drain the engine refuses (409 during a rebuild) or does not answer stays `pending`, is retried each fleet tick and adopts the engine's own drain, so the drive still retires; a drain the engine forgot across a restart is started again (#43)

### Documentation
- The engine drains a drive reported `failed` regardless of `drain`, and rebuilds failing/failed drives; `drain_on_failing = false` stops only stormdrive's drain and retire (#43, stormcos#65)

### 2026-10-06
- **fix:** an automatic drain the engine refuses (409 while it rebuilds the drive's volumes) or does not answer stays `pending` and is retried every fleet tick, adopting the drain the engine starts after its rebuild — the drive now still retires (#43)
- **fix:** a drain the engine forgot across its restart is started again (it was skipped: the record still read `running`) (#43)
- **docs:** the engine drains a drive reported `failed` whatever `drain` says, and auto-rebuilds failing/failed drives; `drain_on_failing = false` stops only stormdrive's drain and retire (#43, stormcos#65); rustkube-node's placement mirror is planned, not built; presentation status
- **docs:** work plan — #43

### 2026-10-06
- **docs:** work plan — #18 done: v0.19.0 verified on 51efbe8, golden a59663904c60; #49 filed (NVMe-oF namespaces discovered as drives)

## [v0.19.0] — 2026-10-06

### Added
- `GET /metrics`: Prometheus text per drive (SMART status, temperature, power-on time, wear, spare, NVMe error and IO counters, SAS/SATA failed commands, health verdict, last poll, usage), per shelf (SES sensors) and for the poller; smartctl_exporter names where one fits (#18, stormcos#64)
- NVMe health keeps the rest of log 0x02 as `health.nvme`

### 2026-10-06
- **feat:** `GET /metrics` — Prometheus text: per-drive SMART status, temperature, power-on time, wear, spare, NVMe error/IO counters, SAS/SATA failed-command count, health verdict, last poll time, usage; shelf SES sensors; poller stats. smartctl_exporter names where one fits; labels `device`, `serial`, `model`, `enclosure`, `bay` (#18, stormcos#64)
- **feat:** the NVMe health sample keeps the rest of log 0x02 (spare threshold, data units read/written, power cycles, unsafe shutdowns, error-log entries) as `health.nvme`
- **test:** short suite checks `/metrics` (every drive by serial)
- **docs:** work plan — #18 /metrics

### 2026-10-06
- **docs:** work plan — #39 done: v0.18.1 verified on 24de9cb, golden ecd23cdbb356

## [v0.18.1] — 2026-10-06

### Fixed
- A restart no longer leaves a drive stuck `formatting`/`testing`/`updating_firmware` when the run was not a worker job: a SCSI format still running on the drive is re-attached and its block size verified; anything else goes idle, its format/firmware record `interrupted`, with a `restart` warning event (#39)

### 2026-10-06
- **fix:** a restart no longer leaves a drive stuck `formatting`/`testing`/`updating_firmware` when the run was not a worker job (#39): a SCSI format still on the drive is re-attached and verified, anything else goes idle with its record `interrupted` and a `restart` event; recovery runs before the monitor and the API
- **docs:** work plan — #39 restart recovery for non-worker operations
- **docs:** work plan — #45 done: v0.18.0 verified on dac3deb, golden e54fd49f3502

## [v0.18.0] — 2026-10-06

### Breaking
- Every write on :9092 needs a storage-admin: a Kubernetes bearer the apiserver allows on `storage.storm.io` (TokenReview + SubjectAccessReview) or the node-local `[api] admin_token`. With neither configured, every write is refused. `[api] api_token` is read as `admin_token` (#45)

### Added
- `Drive` and `DriveOperation` Kubernetes objects (`deploy/crds.yaml`) and their controller; `[kubernetes]` config; `deploy/rbac.yaml`; example operation (#45)
- Worker jobs record and re-check their requester before every step; audit log (`<data_dir>/audit.log`, `audit` events); `owner` on every drive (stormraid members detected); `writes` in `/api/v1/health` (#45)

### 2026-10-06
- **BREAKING:** every write on :9092 needs a storage-admin (#45, stormcos#250): a Kubernetes bearer the apiserver allows (TokenReview + SubjectAccessReview on `storage.storm.io`; drive operations are `create driveoperations`, other drive writes `update drives`) or the node-local `[api] admin_token`. 401 / 403 / 503; reads and worker dry runs stay open; `[api] admin_gate = "audit"` for rollout. No apiserver and no admin token = every write refused
- **feat:** audit — every write decision is a JSON line (who, method, path, resource, verb, target, decision, reason, status) in the log and `<data_dir>/audit.log`, and an `audit` event
- **feat:** worker jobs record their `requester` and re-check it with the apiserver before every step; an unreachable apiserver interrupts the drive (resume), a revoked role fails it
- **feat:** `owner` on every drive (`free` / `stormblock` / `stormraid`): stormraid superblocks (`STORMRD1`) are detected and held like a stormblock slab
- **feat:** `Drive` and `DriveOperation` as Kubernetes objects (`deploy/crds.yaml`, `storage.storm.io/v1`): with `[kubernetes]` set, a controller keeps this node's Drive objects and runs its DriveOperations (requester from the apiserver's stamp, rustkube#210, re-checked by SubjectAccessReview; status, Events, cancel on delete, `spec.resume`); `deploy/rbac.yaml` is its own role; `deploy/driveoperation.example.yaml`
- **feat:** `GET /api/v1/health` reports `writes {gate, apiserver, admin_token}`; kube Drive status adds `owner`, `enclosure`, `bay`, `sasAddress`
- **test:** medium `writes-need-storage-admin`; suites skip (not fail) writes when the run has no bearer; the harness runs with an admin token
- **docs:** README "Who may change a drive", config keys, architecture; work plan for #45

### 2026-09-28
- **docs:** work plan — #5 done: v0.17.0 verified on da27975
- **docs:** work plan — comment mining since 2026-09-25: #39 (restart leaves non-worker ops stuck busy, P1), #40 (worker test step); live-pass additions on #30, #31

## [v0.17.0] — 2026-09-28

### Added
- The drive worker (#5): `POST /api/v1/worker/jobs` — select drives (list, shelf + bays, model, unusable) and take them through format (SCSI FORMAT UNIT / NVMe Format NVM), sanitize (NVMe Sanitize / SCSI SANITIZE), partition (GPT with a stormblock slab partition) and enroll (stormblock drive + slab with role and tier); cancel, resume, dry run; jobs persist and survive a restart
- `prep` phase per drive (API, kube, feed); activity `sanitizing`; `[worker]` config (`max_per_hba`, `enroll_per_domain`)
- Enrolling through a partition: `fleet_partition`, and every engine call addresses the partition

### Changed
- Documentation refreshed from the code (README "Not yet" with issues, presentation, architecture)

### 2026-09-28
- **feat:** the drive worker (#5): `POST /api/v1/worker/jobs {select, steps, destroy?, dry_run?}` takes a selection (drives, shelf + bays, model, unusable) through `format` (SCSI FORMAT UNIT or NVMe Format NVM), `sanitize` (NVMe Sanitize / SCSI SANITIZE: block, crypto, overwrite), `partition` (GPT, one stormblock slab partition, the node-disk type GUIDs) and `enroll` (open the partition in stormblock with labels + uuid, format a slab of a role and tier); `GET …/jobs[/{id}]`, `POST …/{id}/cancel`, `POST …/{id}/resume`
- **feat:** safety — never a fleet, busy, reserved, missing or mounted drive; a drive holding a stormblock slab or a filesystem (ext*, xfs, btrfs, vfat, ntfs, swap, LVM2, LUKS, md) is refused a destructive step unless `destroy` names it by id, WWN or serial; guards run at submit and before every step; `dry_run` changes nothing
- **feat:** low-level steps parallel per HBA (`[worker] max_per_hba`, 8), enroll one per failure domain (`enroll_per_domain`, 1); jobs persist in `<data_dir>/jobs.json`; after a restart a running SCSI format/sanitize or NVMe sanitize is watched to the end, the rest is `interrupted` until resumed
- **feat:** each drive's `prep` phase (unusable → formatting/sanitizing n% → ready → enrolled) on `/api/v1/drives`, the kube Drive status and the feed; new activity `sanitizing`
- **feat:** a drive enrolled through a partition keeps `fleet_partition`; labels, health, drain, overcommit and leave address the partition (`Drive::stormblock_path`); stormblock slab format passes `role`
- **test:** 21 new unit tests (signatures, GPT + CRC32, NVMe/SCSI command building and parsing, guard, lanes, restart, prep); a written GPT passes `sfdisk --verify`; the medium suite checks worker refusals with dry runs only
- **docs:** README, architecture ("The drive worker"), CLAUDE.md, presentation; #36 (ATA security erase), #37 (Decide: scheduling), #38 (page UI) filed
- **docs:** refreshed from the code since the #7 rewrite (v0.16.0, 93fb677): clippy runs with `--workspace`; README's "How it ships" says the workspace's release build also compiles the test binary (the golden carries only `stormdrive`); "Not yet" gives an issue for every item and lists what is built but never run on hardware (#28–#31, stormblock#152); architecture status v0.16.0 and test counts; presentation: #11 no longer planned, golden `452835d3e854`, #28/#30 in status; CLAUDE.md open questions filed as #32 (thermal), #33 (crypto), #34 (burn-in) and the SAS2/SAS3 question marked resolved; shelf IOM firmware filed as #35
- **docs:** work plan — issue-comment mining: stormipmi#21, #29 (Decide), #30 (Decide), #31 filed; #28, stormcentral#83 commented
- **docs:** work plan — #11 done; first live run on C2NR0Q2 is #28 (blocked on stormcentral#63)
- **test:** test containers per stormcentral's test standard (#11): `test/` crate (workspace member), `/test short|medium|long`, JSON lines + summary, exit 0/1/2, `test/Containerfile` (FROM scratch) + `test/build.sh` (static musl); short is read-only, medium checks failure paths and refusals only on guarded drives plus restored round-trips, long runs waves sized from the drive count and fails on slowdown or residue; `test/src/pick.rs` keeps every suite non-destructive
- **test:** `tests/suites.rs` runs all three suites against the real daemon on every `cargo test`
- **build:** the crate is a Cargo workspace (`.` + `test`, both default members)
- **docs:** work plan — #8 done: deck renders with marp-cli on dev (12 slides)
- **docs:** `docs/presentation.md` — a 12-slide Marp deck on purpose and functionality (#8): the problem, where it sits (stormcentral's graph, checked in the consumers' code), how it works, what it does today from the code, interfaces, how it ships, planned work, status
- **docs:** work plan — #6 done: v0.16.0 verified on dc0195c (sc-build 133/133, clippy, web check)
- **docs:** work plan — #6 golden `golden-stormdrive-406e52043b5f` (e95ef48), stormcos#131

## [v0.16.0] — 2026-09-28

### Added
- The page rebuilt for hundreds of drives (#6): Svelte 5 + stormview DataGrid in `web/`; shelves, HBAs, NVMe and unlocated drives as top rows with each group's drives nested; quick and text filters; a drive / shelf / HBA side pane; a bulk bar (tests, locate, designation, format, firmware) where ticking a group means its drives; keyed rows instead of a 4 s innerHTML rebuild
- `/assets/app.{js,css}` (and `/ui/assets/…`) serve the committed `web/dist`
- `web/rebuild.sh` (build on dev through sc-build, `--check`), `npm test`, `npm run test:page` (212-drive jsdom smoke test)
- Test: the example config must parse to the defaults

### Changed
- `src/ui/index.html` (the vanilla page) is gone

### Documentation
- README, docs/architecture.md, CLAUDE.md and the example config rewritten from the code (#7); how the golden ships it (#4)

### 2026-09-28
- **feat:** the page is rebuilt for hundreds of drives (#6): Svelte 5 + stormview DataGrid in `web/`, shelves / HBAs / NVMe / unlocated as top rows with each group's drives nested, quick and text filters, a drive/shelf/HBA side pane, and a bulk bar (tests, locate, designation, format, firmware) where ticking a group means its drives; keyed rows replace the 4 s innerHTML rebuild
- **feat:** `/assets/app.{js,css}` (and `/ui/assets/…`) serve the committed `web/dist`; `src/ui/index.html` is gone
- **build:** `web/rebuild.sh` builds `web/dist` on dev through sc-build (`--check` verifies the committed build); `npm test` covers `web/src/lib/model.js`
- **test:** `npm run test:page` loads the built page in jsdom with 212 drives (two shelves, 4 direct, 160 NVMe) and checks grouping, expand, group selection, the pane, the filter and keyed refresh
- **docs:** work plan — #7 golden `golden-stormdrive-9d1fa8491d44` (stormcos#131); #6 survey: what shipped, what is #5/#18, the UI at scale
- **docs:** work plan — #7 done: sc-build 132/132 + clippy on 024dc54
- **docs:** README rewritten from the code (#7): what it does today, sc-build, every flag and config key with its default, the full API including the kube and body-free routes, how the golden ships it (stormcos `service_golden`, `/sys` read-only → stormcos#166), and what is not built yet
- **docs:** `deploy/stormdrive.example.toml` gains `monitor.max_concurrent`, `monitor.sample_timeout_secs` and `[firmware]`; `[api] api_token` marked not enforced (#19)
- **test:** the example config must parse to the defaults (`config::tests::example_config_is_the_defaults`)
- **docs:** CLAUDE.md: sc-build replaces the ssh-to-dev build section, module map matches `src/`, Phase 0–3/6/7 checklists match the code, stormblock contract list current
- **docs:** docs/architecture.md checked against v0.15.0: drive model, threshold engine, SAS health (sysfs only), events (not persisted), summary card, API design points, deployment, testing; sequencer, wear projection, thermal actuation and SCSI log sense marked design-only (#22–#25)
- **docs:** README says how it ships in a golden and links stormcos `docs/goldens.md` (#4)
- **docs:** module comments: `smart/scsi.rs` (sysfs only, #22), `events.rs` (every event kind; not persisted, #25), `lib.rs`
- **docs:** work plan — #13 done: overcommit ships in golden `golden-stormdrive-01a422df544f`, stormcos#131; enforcement is stormblock#152
- **docs:** work plan — #2 done: HBA inventory ships in golden `golden-stormdrive-01a422df544f`, stormcos#131
- **docs:** work plan — #12 done: usage ships in golden `golden-stormdrive-01a422df544f`, stormcos#131
- **docs:** work plan — #15 done: golden `golden-stormdrive-01a422df544f` (0.15.0 @ 9bb1abb), release request stormcos#131

### 2026-09-27
- **docs:** work plan — v0.15.0 verified; golden waits on stormcentral#111

## [v0.15.0] — 2026-09-27

### 2026-09-27
- **docs:** work plan — #15, 160+ drives per node
- **perf:** health polling at 160+ drives (#15). Each drive is polled at
  its own phase in the interval, so the polls spread out. At most
  `monitor.max_concurrent` (8) reads are in flight. A read slower than
  `monitor.sample_timeout_secs` (10) is a failed sample, and a hung drive
  is not re-read until its read returns, so one hung NVMe no longer stalls
  the rest. `GET /api/v1/monitor` reports the cost per cycle and the stuck
  drives.
- **perf:** trend samples are recorded on change or daily, not on every
  poll. The inventory is compact JSON, written outside the lock, and not
  rewritten when unchanged.
- **perf:** discovery caches READ CAPACITY and the slab probe per device
  (asked again when the device changes, or every 10 minutes), and reads
  `/proc/mounts` once per pass.
- **fix:** NVMe native-multipath path nodes (`nvme0c1n1`) and hidden
  gendisks are no longer drives.
- **feat:** hotplug: kernel disk uevents trigger a debounced discovery
  pass.
- **feat:** NVMe location behind PCIe switches and VMD, and under native
  multipath:
  - the PCIe slot anywhere on the chain
  - `bay` from a numeric slot number
  - locate through the slot's attention indicator or an NPEM LED
- **feat:** a new drive in a missing drive's bay `replaces` it (link +
  event). `DELETE /api/v1/drives/{id}` (Forget in the feed and UI) removes
  a missing, out-of-fleet drive's record.

### 2026-09-27
- **docs:** work plan — v0.14.0 sc-build passes; golden waits on stormcentral#111

## [v0.14.0] — 2026-09-27

### 2026-09-27
- **feat:** per-drive `overcommit` (#13): off by default, or on with a
  ratio (1.0–16.0), set by `PUT /api/v1/drives/{id}/overcommit` (and a
  body-free `/overcommit/{off|ratio}`), persisted, and recorded as an event.
  `usage` gains `promisable_bytes` (slab space × ratio), plus
  `committed_bytes` and `headroom_bytes` once stormblock reports committed
  per slab (stormblock#152). The setting is pushed to the engine as
  `PUT /api/v1/drives/{path}/overcommit` for every drive with slabs; an
  engine without the route is retried quietly. On `/api/v1/drives`, the
  kube Drive status, the feed (metric + toggle action) and the UI
  (selector, committed/headroom line).
- **docs:** work plan — #2: v0.13.0 tagged, sc-build + golden pending
- **docs:** work plan — #12: shipped in v0.13.0, golden pending
- **docs:** work plan — v0.13.0 golden blocked on stormcentral#111 (nvme-tcp on dev)
- **docs:** work plan — #13 per-drive overcommit setting

## [v0.13.0] — 2026-09-27

### 2026-09-27
- **feat:** HBA inventory (#2 — owner's decision: HBA firmware version
  collection is stormdrive's, BIOS is stormipmi's): every PCIe SCSI
  controller from `/sys/class/scsi_host`, grouped per PCIe function, with
  driver, PCI ids, board name, SAS address and firmware / option-ROM BIOS
  / NVDATA versions. `GET /api/v1/hbas`, `hba` on topology controllers,
  `hba:<bdf>` components in the feed, an HBA panel in the UI, and `hba`
  events when a card appears, goes, or its firmware changes.
- **docs:** work plan — #2 live check on the R230 (0.11.0): system disk
  shows `in_use_by`, kind `sata_hdd`, destructive actions disabled.

### 2026-09-27
- **feat:** per-drive `usage` (#12): capacity; each stormblock slab on the
  drive (id, role, tier, total, allocated, free), joined from
  `/api/v1/slabs` by WWN, else serial, else path (stormblock#136);
  `used`, `free_in_slabs`, `outside_slabs` and `free` (= capacity − used).
  On `/api/v1/drives`, the kube Drive status, the components feed
  (`used`/`free`/`slabs` metrics) and the UI size column.

### 2026-09-27
- **docs:** work plan — #10 done: golden-stormdrive-df998ec9c92d, stormcos#131

## [v0.12.0] — 2026-09-27

### 2026-09-27
- **feat:** `GET /api/v1/placement` and `/api/v1/placement/{id}` — where
  every drive and shelf is, for the PV placement mirror (#10,
  rustkube-node#60): per drive `wwn` + `serial` (what stormblock#136 names
  a volume's drives by), shelf, bay, SAS address, `sas_phy`, `expander`,
  HBA, PCIe, membership, designation, activity, health state; shelves with
  their bays. `generation` hashes placement only (not health samples),
  survives restarts, and `?since=` / `If-None-Match` answer 304.
- **feat:** `Location` gains `sas_phy` and `expander` (SAS wiring from
  sysfs).
- **fix:** a drive re-bayed under the same `/dev` name kept its old
  location: location is now re-resolved every discovery pass (known detail
  kept on the same shelf when a pass cannot read it) and a move logs a
  `location` event.
- **fix:** drive lookup by handle tries the WWN first (any case), then
  path/name, then serial — NVMe-oF namespaces can share a serial.

### 2026-09-27
- **docs:** work plan — #14 done: golden-stormdrive-fa8dbebdd485, stormcos#123

## [v0.11.0] — 2026-09-27

### 2026-09-27
- **fix:** present the stormblock engine token (#14, stormcos#104). stormblock
  v17 (stormblock#107) requires `Authorization: Bearer` on all of `/api/v1`,
  so every stormdrive → engine call — drive list, add, labels, slabs, health
  reports, drains — was a 401, and quarantine and drain stopped working.
  Every call now carries the token, found in stormblock's CLI order
  (`stormblock.api_token` / `$STORMBLOCK_API_TOKEN`, then `token_file`,
  `$STORMBLOCK_TOKEN_FILE`, `/run/stormblock/engine/api_token`,
  `/etc/stormblock/api_token`, `/var/lib/stormblock/api_token`). An absent
  token is looked up again on every call (the engine mints it at boot), and
  a 401 re-reads it and retries once. `DELETE`s use `admin_token` when set.

### 2026-09-24
- **docs:** work plan — v0.10.0 passes sc-build; golden request pending (#2)
- **docs:** work plan — golden-stormdrive-b0941b2857a0 built, release request stormcos#70 (#2)

## [v0.10.0] — 2026-09-24

### 2026-09-24
- **fix:** the node's own system disk was offered for Format 4K, the
  destructive test and Join fleet (#2). On the R230, sda carries the
  stormcos GPT with stormblock's system and data slabs and the root
  filesystem is served from it over ublk — yet it read "out of fleet" and
  nothing on it was in `/proc/mounts`, which was the only guard. Discovery
  now reads each drive for stormblock slab headers (whole drive or any GPT
  partition) and records `in_use_by`; join, format and the destructive
  test refuse such a drive in the API (re-reading the disk right before
  starting), the components feed disables those actions and shows an
  `in use` metric, and the UI hides them. Firmware updates stay allowed —
  that is the work this node needs — but are serialised like a fleet
  drive's.
- **fix:** a SATA drive behind a SAS HBA was classified `sas_hdd`/`sas_ssd`
  (the HBA gives it a `sas_address`); a SCSI vendor of `ATA` now means SATA.
- **fix:** for SCSI/SATA drives the feed called sysfs `ioerr_cnt` "media
  errs"; it counts failed commands of any kind. Labelled `io errs` there;
  NVMe keeps `media errs`, which is what its log page reports.

### 2026-09-20
- **feat:** the feed says what a drive *is*, not only where it is. This
  daemon reads model, serial, firmware, wwid and capacity off every drive
  it enumerates and published none of them — a consumer got a kind and a
  size in prose (`sas hdd · 1.8 TB`) and nothing that identifies the
  physical object. Now `model`, `serial`, `firmware`, `wwid`, `dev` and
  `capacity` ride as metrics, muted, because identity is what you read
  once you have decided which row to look at. **The serial is the one
  that matters**: it goes on the RMA, it is what the label on the carrier
  says, and it is the only thing that still names the drive after it has
  been pulled and the bay is empty.
- **feat:** age and SMART, as separate facts. `hours` carries the drive's
  own power-on counter and turns warn past five years of continuous
  running — not a fault, the fact that decides which drive you replace
  first. `smart` carries what SMART itself says, but only when that is
  something other than good or unknown: a column reading "good" on every
  healthy drive is a column nobody looks at. It is separate from the
  component's health on purpose, because the two disagree in the case
  that matters — a `Missing` drive is a red row whose last SMART read
  said Good, and collapsing them loses the half that says whether to
  reseat it or replace it.
- **feat:** the NVMe `critical_warning` byte is published when it is not
  zero, in hex, because that is how the spec numbers the bits — spare
  exhausted, over temperature, media gone read-only, backup power failed.
- Nothing a drive did not report becomes an empty metric: a blank serial
  behind a controller that will not answer is absent, not `serial:` with
  nothing after it. `detail` is unchanged, so this is additive.

## [v0.9.0] — 2026-09-09

### 2026-09-09
- **feat:** The components feed publishes placement as data (#3): every
  drive with a bay carries a `bay` metric and every drive behind a
  controller an `hba` metric (PCIe address, else SCSI host), and each
  shelf carries one `hba` metric per path — each SES processor's SCSI
  host resolved to the PCIe address its drives report, plus the
  controllers of its own drives. `detail` is unchanged, so this is
  additive: a renderer that groups by shelf and orders by bay no longer
  needs `/bay (\d+)/` over a prose sentence, and "which card are these
  drives behind" becomes answerable.
- **docs:** architecture: the components feed is in the route list, with
  the metric contract for placement.

### 2026-09-05
- **fix:** The SG_IO ioctl now passes its request as `as _` rather than a fixed-width `c_ulong`. glibc types the ioctl request as `c_ulong` and musl as `c_int`, so the literal compiled against one and not the other, and the node's binaries are musl-static — `cargo build --target x86_64-unknown-linux-musl` failed with `expected i32, found u64` and no stormdrive golden was produced, which also silently cost stormconsole its stormdrive panel. The NVMe path in `smart/nvme.rs` already did this correctly.

## [v0.8.0] — 2026-09-05

### 2026-09-05
- **feat:** Firmware updates (`firmware.rs`). Image store under
  `<data_dir>/firmware` — `PUT/GET/DELETE /api/v1/firmware/images/{name}`
  (raw upload, SHA-256 listed, `firmware.max_image_mib` cap). SAS/SATA:
  WRITE BUFFER mode 0x0E chunks (`firmware.chunk_kib`, rounded to the
  drive's READ BUFFER offset boundary) + mode 0x0F activate, mode 0x07
  fallback; ready-wait, rescan, INQUIRY verify. NVMe: Firmware Image
  Download + Commit CA=3, CA=1 fallback with `reset_required`.
  `POST /api/v1/drives/{id}/firmware`, `POST /api/v1/firmware {drives
  and/or model, image, force}`, `GET /api/v1/firmware`,
  `GET /api/v1/drives/{id}/firmware`. Out-of-fleet drives in parallel,
  fleet drives serialised, Failing/Failed refused unless forced;
  all-or-nothing validation. `Activity::UpdatingFirmware`,
  `Drive.firmware_update` persisted.
- **feat:** UI: firmware image panel with upload/delete, FW column,
  image picker + "Update firmware on selected", per-drive FW button,
  download/activate progress in the activity column, pending-reset badge.
- **feat:** NVMe admin passthrough is now a shared helper
  (`smart::nvme::linux::admin`) returning the status word.

## [v0.7.0] — 2026-09-05

### 2026-09-05
- **feat:** Raw SCSI over SG_IO (`scsi.rs`): READ CAPACITY(16),
  INQUIRY/VPD, MODE SENSE/SELECT, FORMAT UNIT, TEST UNIT READY with
  progress, RECEIVE/SEND DIAGNOSTIC; fixed + descriptor sense decoding.
- **feat:** NetApp shelf management (`ses.rs`): SES-2 configuration /
  status / descriptor / additional-status pages → `ShelfReport` with
  power supplies, fans, temperatures, voltages, currents, slots and the
  bay ↔ SAS-address map; dual-IOM shelves merged by enclosure logical id;
  shelf and bay IDENT (locate) through the SES control page. Refreshed
  every discovery tick; events on shelf appearance, status change,
  element failure/recovery. `GET /api/v1/shelves[/{key}]`,
  `POST /api/v1/shelves/{key}/locate`.
- **feat:** 520-byte (NetApp-formatted) drives are discovered even though
  the kernel attaches them with 0 blocks: READ CAPACITY(16) is the source
  of `block_size`/capacity; `Drive` gains `physical_block_size`, `usable`,
  `format` and `needs_reformat`; join/destructive tests are refused on
  unusable drives.
- **feat:** Sector-size reformat (`format.rs`): MODE SELECT block length +
  FORMAT UNIT (IMMED, progress via TUR sense; blocking fallback), kernel
  rescan / re-add and verification; out-of-fleet + idle + unmounted only;
  many drives in parallel with all-or-nothing validation.
  `POST /api/v1/drives/{id}/format`, `POST /api/v1/format {drives,
  block_size}`, `POST /api/v1/shelves/{key}/format`, `GET /api/v1/format`.
- **feat:** Location without the `ses` module: bay and shelf from
  mpt3sas `sas_device` `bay_identifier`/`enclosure_identifier`; locate LED
  falls back to SES control. `Shelf.logical_id` is now the shelf key.
- **feat:** UI: shelves panel (status, paths, PSU/fans/temp, element
  detail, locate, "Reformat → 4K"), sector column with an unusable badge,
  per-drive Format button, multi-select + "Format selected"; formatting
  progress in the activity column. Components feed, kube `Enclosure`,
  topology and the summary card carry shelf status and reformat counts.

## [v0.6.0] — 2026-08-28

### 2026-08-28
- **feat:** Kubernetes-shaped resources served by stormdrive (stormblock#80):
  `/apis/storage.storm.io/v1/drives` and `…/enclosures` in the
  `apiVersion/kind/metadata/spec/status` shape, API discovery at `/apis`
  and `/apis/storage.storm.io/v1`, `labelSelector`, `?watch=1` as a
  newline-delimited event stream. `Drive.spec` writes — `designation`,
  `fleet` (`fleet`/`out`; out always drains first), `drain`, `locate` —
  map onto the existing verbs. Labelled `storm.io/component=stormdrive`
  so they sit beside stormblock's `Volume`/`Slab`/`Node`/`Drive` in one
  group.

## [v0.5.0] — 2026-08-28

### 2026-08-28
- **feat:** Close the stormblock loop (stormblock#70/#71, engine v11).
  `stormblock.rs` registers drives with location `labels` and the stable
  `uuid`, relabels (`PUT /drives/{id}/labels`), lists slabs by drive,
  reports health (`POST /drives/{id}/health`) and starts/polls/cancels
  drains. New `fleet.rs` runs after every monitor tick: labels pushed on
  change, Failing/Failed pushed to the engine (which quarantines the slabs
  and distrusts the legs), a fleet drive that fails is drained and — when
  stormblock reports it empty — leaves the fleet with its locate LED on;
  auto-add (`stormblock.auto_add`) registers qualified drives with labels
  and a slab.
- **feat:** `POST/GET/DELETE /api/v1/drives/{id}/drain` (`?leave=true`
  retires when empty); `fleet leave` takes `"drain": true`; the leave guard
  uses the engine's slab-by-drive listing instead of path matching;
  designating a fleet drive Failed reports it and starts its drain.
- **feat:** `Drive` records `pushed_labels`, `pushed_health` and `drain`
  (state, moved/failed/remaining, reason, then_leave), persisted.
- **feat:** Config `stormblock.auto_format_slab`, `push_health`,
  `drain_on_failing` (all default true; `auto_add` stays off).
<!-- New unreleased changes go here -->

## [v0.3.0] — 2026-08-26

### Added
- Shelf-rig topology (NetApp DS-series target): `Location` is now
  controller → shelf → bay, with shelf identity read from the SES
  processor's SCSI device (vendor/model + serial via VPD page 0x80 — the
  canonical key, since a dual-IOM shelf is two enclosure devices with one
  serial); controllers carry scsi_host/PCIe BDF/driver
- Multipath awareness: observations grouped by WWID-derived DriveId — one
  Drive with a `paths` list and a stable sorted-first primary, instead of
  a path that flaps between IOMs every scan; location re-resolved on path
  changes
- `GET /api/v1/topology` — the controller → shelf → drive tree, shelves
  deduplicated by serial
- UI shows shelf model/serial + bay in the location column and a
  path-count badge on multipath drives

### Documentation
- Work plan: pending drive-crypto phase (SED/OPAL, crypto erase — scope to
  confirm); stormblock#71 filed (shelf/controller failure-domain-aware
  slab placement)

## [v0.2.0] — 2026-08-26

### Added
- Fleet membership / designation / activity model replaces the single
  DriveState — a drive is `out|fleet` (stormblock membership), carries an
  operator designation (`none|reserved|spare|failed`, valid both in fleet
  and out), and has an activity (`idle|testing|draining|missing`)
- Fleet actions: `POST /api/v1/drives/{id}/fleet` join (stormblock add +
  optional explicit slab format with derived tier) / leave (guarded by a
  best-effort slab check until stormblock#70; `force` override)
- Drive testing engine: smoke (sampled reads), read_scan (full sequential
  read with progress + cancel, maps past bad regions), destructive_sample
  (write/verify via O_DIRECT read-back; refused in-fleet or mounted);
  verdict events, one test per drive
- Embedded web UI at `/` (vanilla JS, stormd style tokens, proxy-prefix
  aware) — drive table with join/leave, designation dropdown, test buttons
  with progress, locate LED, event feed
- `POST /api/v1/drives/{id}/designation`; fleet reconcile now two-way
  (stormblock listing ↔ membership)

### Breaking
- `POST /api/v1/drives/{id}/state` removed (replaced by fleet/designation
  endpoints); persisted inventories from 0.1.0 load but lifecycle fields
  reset to defaults

### Changed
- Repo made public (matching stormblock); dev.g8.lo pulls directly from
  GitHub over https, bare-repo workaround removed
- Cargo.lock committed (binary crate convention)

## [v0.1.0] — 2026-08-26

### Added
- Phase 1 scaffold: stable `DriveId` (uuid5 of WWID/model+serial),
  persistent inventory (atomic writes, wear-trend ring), sysfs discovery
  with exclusion policy + mounted-drive guard, NVMe SMART collector
  (admin ioctl, log page 0x02), SCSI/SATA sysfs collector, threshold engine
  with hysteresis, event ring, topology resolution (PCIe BDF/slot, SAS
  address, SES enclosure/bay) with locate-LED control, axum REST API on
  :9092 incl. stormd summary card, stormblock client with Active-state
  reconcile
- Deploy files: example config, systemd unit, stormd `[process.ui]` snippet

### Fixed
- First health sample no longer raises a false "media errors growing"
  warning (baseline requires a real prior sample)
- nvme-style device names only match 'p'-separated partitions in the
  mounted-drive guard (nvme0n10 is not a partition of nvme0n1)

### Documentation
- StormBlock deep review captured in `docs/stormblock-review.md` (drive
  model, add/remove paths, health/failure gaps, topology gap, mgmt surface,
  stormd UI extension contract)
- Architecture design v1 in `docs/architecture.md` (drive model,
  discovery/monitor/topology/firmware/thermal/sequencer subsystems, REST
  API on :9092, stormd summary card, stormblock integration + migration
  flow)
- Project bootstrap: CLAUDE.md work plan, README

## [v0.4.0] — 2026-08-26

### Added
- stormview integration — `GET /api/v1/components` + `/ws/components`
  (full-snapshot pushes) serving drives and shelves as ComponentSummary
  entries with relations (shelf has_many drives) and real actions;
  parameter-less action routes for renderers (`/locate/{on|off}`,
  `/fleet/{join|leave}`, `/designation/{value}`, `/test/{kind}`) so
  stormd/stormsh/stormconsole buttons make things happen

### Documentation
- Position in the distributed hierarchy: physical chain
  site ⊃ building ⊃ floor/room ⊃ row ⊃ rack ⊃ node ⊃ hba ⊃ shelf ⊃ bay;
  logical overlay is multicluster-of-multiclusters with tiering across
  clusters; stormblock is an execution engine, the cross-node control
  plane is stormstorage (live at github.com/glennswest/stormstorage);
  shared label vocabulary pinned; testbed recorded (2.5" high / 3.5"
  medium / PVE backup); stormblock#72 filed, amended, and re-scoped to
  engine primitives

