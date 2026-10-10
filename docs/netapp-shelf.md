# NetApp shelves: what stormdrive reads, and every SAS command it sends

The NetApp DS224C (24 × 2.5", IOM12) is the first shelf stormdrive has seen
for real (#81, on the Dell C2NR0Q2 since 2026-10-10). This page records:
- what the shelf says through SES, and how each element is decoded;
- the SCSI commands stormdrive sends to the shelf and to its drives;
- what multipath looks like on it.

The parsers are in `src/ses.rs` and `src/shelfview.rs`, and the command
builders in `src/scsi.rs`. Every one has unit tests on bytes taken from this
shelf.

## The shelf on the Dell (2026-10-10, stormdrive 0.27.1)

| | |
|---|---|
| SES INQUIRY | `NETAPP` `DS22412IOM12A` rev `0401` |
| Logical id (page 0x01) | `500a09800e359135` (stormdrive's shelf key) |
| Shelf ID (enclosure `ID=`) | `02` |
| Chassis serial (enclosure `SN=`) | `SHFGB2037000386`, part `116-00652+C0` |
| ESP seen | `0:0:18:0`, `/dev/sg18`, SAS `500a09800853bf4c` |
| HBA | Broadcom SAS9300-4i4e (SAS3008, mpt3sas), host0, `500605b00dacc340`, firmware 05.00.00.00 |
| Drives | 12 × `ST1200MM0098` (WWN `5000c500…`, Seagate OUI) at **520**-byte sectors in bays 0–7 and 10–13; 5 × `X425_HCBEP1T2A10` (WWN `5000cca0…`, HGST OUI) at 512 in bays 8, 9, 14–16; bays 17–23 empty |

## Element map (DS224C / IOM12)

SES status (page 0x02) has one **overall** element per type, followed by the
individual elements. On this shelf every overall element is four zero
bytes. Status code 0 means "unsupported", which here only says that the IOM
does not summarise the type. The "1 slot / power supply / 5 vendor elements
unsupported" in #81's first reading were these overall elements. stormdrive
leaves overall elements out of every count, and out of `problems`.

The element descriptors (page 0x07) are `KEY=VALUE;` lists. stormdrive keeps
them as `attributes` on each element (values trimmed, empty ones dropped).

| Type | Count | What it is | Descriptor keys | stormdrive |
|---|---|---|---|---|
| 0x01 device slot | 24 | bays 0–23; byte 1 = slot number | — | bay, IDENT, FAULT, DEVICE OFF; SAS address from page 0x0A |
| 0x02 power supply | 2 | PSU 0 ok, PSU 1 **not installed** | `TP SN FW PN PW IV` (PW = rated watts: 913) | `summary.power_supplies` |
| 0x03 cooling | 4 | 2 fans running (7550, 7320 rpm), 2 not installed (they are in the missing PSU) | — | rpm |
| 0x04 temperature | 11 | 9 sensors, 24–35 °C; 2 not installed | — | °C; limits from page 0x05 |
| 0x07 ESC electronics | 2 | **the two IOMs**, both ok, FW 0401; byte 2 bit 0 (REPORT) = the IOM answering | `TP SN FW CV PN AI NA IF CA CB` | `summary.ioms` |
| 0x0C display | 1 | the shelf-ID display | — | status |
| 0x0E enclosure | 1 | the chassis | `ID MPN MSN WWN PN SN OEM1-4` | shelf ID, chassis serial, part number |
| 0x12 voltage | 4 | 5.07 V and 12.18 V; 2 not installed | — | volts |
| 0x13 current | 4 | 10.85 A and 6.09 A; 2 not installed | — | amps |
| 0x19 SAS connector | 8 | external SAS ports; byte 1 = connector type (0x05: Mini SAS HD 4x, SFF-8644) | `SN VN CT PN AA UA AP` (AA = SAS address at the other end of the cable) | `summary.connectors`, cabled = `AA` set |
| 0x83 vendor | 2 | **IOM expander**, one per IOM | `FI FM SA FPI` (SA = expander SAS address, FPI = `IOM12A`) | type name `iom expander`; IOM path |
| 0x85 vendor | 2 | **IOM Ethernet port** | `OM` (MAC, NetApp OUI D0:39:EA) | type name `iom ethernet`; `ioms[].mac` |
| 0x8C vendor | 1 | not known (raw `01 00 02 11`) | none | raw bytes only |
| 0x8D vendor | 2 | not known (raw `01 00 00 eb`; 1 not installed) | none | raw bytes only |
| 0x8E vendor | 2 | not known (raw `01 00 00 59`; 1 not installed) | none | raw bytes only |

Types 0x8C to 0x8E carry no descriptor, and nothing on the shelf names them.
The 1-of-2 installed pattern of 0x8D and 0x8E matches the PSUs, so they may
be per-PSU readings, but that is a guess. stormdrive shows their raw bytes
and does not decode them. NetApp's vendor diagnostic pages can be read raw
through `GET /api/v1/shelves/{key}/diagnostics[/{page}]` (below), which is
how to find out more.

### Why the shelf reads "noncritical"

The **enclosure element** reports status OK with FAILURE INDICATION set
(byte 2 bit 1; raw `01 00 02 00`): the shelf's own fault LED is lit.
stormdrive lists it in `problems` as "enclosure 0: failure indicated".
Before #81, stormdrive tested the wrong bits of byte 3, the power-off
duration. On 0.27.1 only one individual element is not OK: the **device
slot for bay 5** (drive sdg, ST1200MM0098 `W402BS6M0000K842E9RP`). Its status code is 3
(noncritical), and its flag bits are all clear. The drive's own health is
good: no uncorrected errors and no predicted failure. The IOM does not say
why in the status page. From this release, stormdrive reads page 0x03 (help
text) and reports it as `help_text`, and `summary.problems` names the slot.
The missing second PSU is reported as *not installed*, which is not a
fault.

## Multipath

Both IOMs are installed and OK. **Only one is cabled.** SAS connector 0 is
attached to `500605b00dacc340`, which is this node's HBA (host0). The other
seven connectors are not installed, which means no cable. The IOM whose
expander is `500a09800853bf4d` carries the ESP at `…bf4c` that this node
sees. The second IOM's expander is `500a09800853bfa5`, and no path reaches it.

`summary.multipath` says this in a line. With the shelf as it is now:
"2 IOMs installed, one path seen: no path through IOM 1 (500a09800853bfa5)".

A second path needs a cable from the second IOM to a second HBA port. The
SAS9300-4i4e has **one** external x4 port, and the first IOM already uses
it. So the second path needs another external port on this node. When the
second path is cabled:
- a second ESP appears, with the same logical id;
- discovery merges both ESPs into one shelf (`paths: 2`);
- each drive's two /dev nodes become one drive with two `paths` (one WWID).
The merge code has existed since v0.3.0. On real hardware it has only ever
been seen with one path.

## The commands

### To the shelf (SES processor, `/dev/sgN` of the type-13 device)

| What | Command | `sg_ses` equivalent | When |
|---|---|---|---|
| Supported pages | RECEIVE DIAGNOSTIC RESULTS (0x1C), PCV=1, page 0x00 | `sg_ses -p 0x00` | each scan; `GET …/diagnostics` |
| Configuration | page 0x01 | `sg_ses -p cf` | each scan |
| Status | page 0x02 | `sg_ses -p es` | each scan |
| Help text | page 0x03 | `sg_ses -p ht` | each scan, if listed |
| Thresholds | page 0x05 | `sg_ses -p th` | each scan, if listed |
| Element descriptors | page 0x07 | `sg_ses -p ed` | each scan, if listed |
| Additional element status (bay ↔ SAS address) | page 0x0A | `sg_ses -p aes` | each scan, if listed |
| Download microcode status | page 0x0E | `sg_ses -p dm` | IOM firmware runs (#35) |
| Any page, raw | page N | `sg_ses -p N -r` | `GET /api/v1/shelves/{key}/diagnostics/{page}` |
| Bay / shelf locate | SEND DIAGNOSTIC (0x1D), PF=1, page 0x02 control: SELECT + RQST IDENT on that element only | `sg_ses --index=N --set=ident` | `POST …/shelves/{key}/locate {on, bay?}` |
| Bay fault LED | page 0x02 control, RQST FAULT | `sg_ses --index=N --set=fault` | a failed RAID member's bay (#44) |
| Bay power off / on | page 0x02 control, DEVICE OFF (slot byte 3 bit 4) | `sg_ses --index=N --set=devoff` | `POST …/shelves/{key}/bays/{bay}/power {on}` |
| IOM firmware | page 0x0E Download Microcode control | `sg_ses_microcode` | `POST …/shelves/{key}/firmware` (#35); never automatic |

Every control page is built from a status page read just before it, so the
generation code matches. Only the one target element has SELECT set, and it
keeps the request bits its status already shows: a locate request never
carries a fault or power-off request with it.

`/api/v1/shelves/{key}/diagnostics` only ever sends RECEIVE DIAGNOSTIC
RESULTS. Like any read, it is open to a credential with `get` on
`enclosures`. Bay power changes the drive's state, so it is gated as a drive
operation (storage-admin, `create driveoperations`). It is refused while the
bay's drive is in the fleet, holds data (`in_use_by`) or is busy.

**Shelf ID.** stormdrive reads the shelf ID from the enclosure element's
`ID=` and does not set it. An operator sets it with the shelf's front-panel
button. No standard SES control sets it, and NetApp's own method has not
been looked at.

**IOM firmware, read.** Each IOM's `FW=` (ESC descriptor) and each ESP's
INQUIRY revision (`esps[].revision`). Both read 0401 here.

### To each drive (`/dev/sdX`, or its sg node)

| What | Command | When |
|---|---|---|
| Identity | INQUIRY (0x12): vendor, product, revision via sysfs; VPD 0x80 serial | discovery |
| Real sector size | READ CAPACITY(16) (0x9E/0x10): block length, protection, physical exponent | discovery (sd reports 0 blocks for a 520) |
| Failure prediction, temperature | LOG SENSE (0x4D) page 0x2F, else 0x0D | each health sample |
| Error counters | LOG SENSE pages 0x02, 0x03, 0x05 (write/read/verify: corrected, uncorrected, bytes) | each health sample |
| SSD endurance | LOG SENSE page 0x11 | each sample, SSDs |
| Grown defects | READ DEFECT DATA(12) (0xB7), GLIST, header only (8 bytes): list length ÷ descriptor size | each sample (#81) |
| SATA behind SAT | ATA PASS-THROUGH(16) (0x85): SMART READ DATA / THRESHOLDS | each sample, vendor `ATA` |
| Reformat | MODE SELECT(10) (fallback 6) block descriptor = new length; FORMAT UNIT (0x04) FMTDATA + IMMED, FOV=0 (drive defaults; no protection); progress by TEST UNIT READY sense (02/04/04 + progress indicator) | format / worker `format` step |
| Sanitize | SANITIZE (0x48) block / crypto / overwrite, IMMED | worker `sanitize` step |
| Firmware | WRITE BUFFER (0x3B) mode 0x0E/0x0F (0x07 fallback); READ BUFFER descriptor | firmware update, never automatic |

## The 520 → 4096 live pass (#30)

A dry run against the live shelf (0.27.1, 2026-10-10) refused nothing. This
was the request:

```
POST /api/v1/worker/jobs
{"select":{"shelf":"500a09800e359135"},"steps":[{"op":"format","block_size":4096}],"dry_run":true}
```

All 17 drives were runnable. From this release, the dry run also lists each
drive's shelf, bay, vendor, model, serial, firmware, kind, sector size and
capacity.

What the format does is set by #82:
- The FORMAT UNIT here sends FOV=0, so the drive is formatted without
  protection information.
- A drive that cannot take 4096 refuses the MODE SELECT, and the job fails
  for that drive.
- The ST1200MM0098 is sold as a 512-native drive, and such drives usually
  offer only 512, 520 and 528.

So probe each drive first (VPD 0x86, 0xB4, 0xB1), then choose 4096+PI,
512+PI or plain sizes per drive. The real run also needs a storage-admin
bearer: the shelf's drives are writes, and the gate enforces.
