//! A SCSI drive's pages, read whole and said in words (#147, stormblock#636).
//!
//! The owner on forge1 bay 3 against the Sun HGSTs: "lets get the mode pages.
//! and any way to parse vpd pages". Every VPD page the drive lists in 0x00 is
//! read whole (up to 4 KiB), and the mode pages that decide how a drive
//! behaves under the engine (0x03 format device, 0x08 caching, 0x0A control,
//! 0x19 protocol port, 0x1A power condition, 0x1C informational exceptions),
//! each with its current values and the bits MODE SELECT may change.
//!
//! The shapes are stormblock's (`GET /api/v1/storage/hba/{sas}/details`), so a
//! console reads either: `vpd: [{page, hex, said, strings, error}]`,
//! `mode: [{page, hex, changeable, said, error}]`. The standard pages are said
//! in words; a vendor page (0xC0 and up: Sun 0xC0/0xC1, NetApp 0xD1–0xD3) is
//! raw with its printable strings, never interpreted. A page's bytes are data,
//! shown and never trusted: nothing here decides anything.
//!
//! The bits worth comparing between drives are named as well (`control`,
//! `extInquiry`, `caching`), so `kubectl get drives -o custom-columns=…
//! status.pages.control.ato` lines bay 3 up against bay 0 in one command.
//!
//! Read in discovery (cached [`REFRESH`], kept by drive id in memory — not
//! in inventory.json, whose rewrite every tick would carry every page of
//! every drive) and on demand by a DriveOperation `op: details`.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use crate::drive::DriveId;

/// How long a drive's pages are kept before discovery reads them again.
/// They change when someone changes them (MODE SELECT, a firmware update,
/// a format): `op: details` reads them now.
pub const REFRESH: Duration = Duration::from_secs(3600);

/// Largest VPD page read (stormblock reads the same).
pub const VPD_ALLOC: u16 = 4096;

/// The mode pages read, in this order.
pub const MODE_PAGES: [u8; 6] = [0x03, 0x08, 0x0A, 0x19, 0x1A, 0x1C];

/// One VPD page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct VpdPage {
    /// `"0x86"`.
    pub page: String,
    /// The page as the drive returned it, header included.
    pub hex: String,
    /// The page in words, for a standard page; None for a vendor page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub said: Option<String>,
    /// Printable ASCII runs of 4 or more.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub strings: Vec<String>,
    /// The drive refused it, or it could not be asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One mode page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ModePage {
    /// `"0x0A"`.
    pub page: String,
    /// Current values: the page itself, from its page-code byte (no header,
    /// no block descriptor).
    pub hex: String,
    /// The changeable mask (PC=01b): the bits MODE SELECT may set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changeable: Option<String>,
    /// The page in words; `refused: …` when the drive refused it
    /// (stormblock#627's convention).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub said: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The control mode page (0x0A), named.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Control {
    pub tst: u8,
    pub tmf_only: u8,
    pub dpicz: u8,
    pub d_sense: u8,
    pub gltsd: u8,
    pub rlec: u8,
    pub qam: u8,
    pub nuar: u8,
    pub qerr: u8,
    pub rac: u8,
    pub ua_intlck_ctrl: u8,
    pub swp: u8,
    pub ato: u8,
    pub tas: u8,
    pub atmpe: u8,
    pub rwwp: u8,
    pub sblp: u8,
    pub autoload_mode: u8,
    /// BUSY TIMEOUT PERIOD, in 100 ms units (FFFFh = unlimited).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy_timeout: Option<u16>,
    /// EXTENDED SELF-TEST COMPLETION TIME, seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extended_self_test_secs: Option<u16>,
}

/// The Extended INQUIRY Data VPD page (0x86), named.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtInquiry {
    pub activate_microcode: u8,
    pub spt: u8,
    pub grd_chk: u8,
    pub app_chk: u8,
    pub ref_chk: u8,
    pub uask_sup: u8,
    pub group_sup: u8,
    pub prior_sup: u8,
    pub headsup: u8,
    pub ordsup: u8,
    pub simpsup: u8,
    pub wu_sup: u8,
    pub crd_sup: u8,
    pub nv_sup: u8,
    pub v_sup: u8,
    pub no_pi_chk: u8,
    pub p_i_i_sup: u8,
    pub luiclr: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extended_self_test_minutes: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_sense_length: Option<u8>,
}

/// The caching mode page (0x08), named.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Caching {
    pub ic: u8,
    pub abpf: u8,
    pub cap: u8,
    pub disc: u8,
    pub size: u8,
    pub wce: u8,
    pub mf: u8,
    pub rcd: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fsw: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lbcss: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dra: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nv_dis: Option<u8>,
}

/// Everything read from one drive.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Pages {
    pub vpd: Vec<VpdPage>,
    pub mode: Vec<ModePage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control: Option<Control>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext_inquiry: Option<ExtInquiry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caching: Option<Caching>,
    /// When they were read (unix seconds).
    pub read_at: u64,
    /// The drive could not be asked at all (open failed, VPD 0x00 refused).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ------------------------------------------------------------ helpers

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn page_name(p: u8) -> String {
    format!("0x{p:02X}")
}

/// Printable ASCII runs of at least 4 characters, trimmed.
pub fn strings(b: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        let t = cur.trim();
        if t.len() >= 4 {
            out.push(t.to_string());
        }
        cur.clear();
    };
    for &c in b {
        if (0x20..0x7f).contains(&c) {
            cur.push(c as char);
        } else {
            flush(&mut cur, &mut out);
        }
    }
    flush(&mut cur, &mut out);
    out
}

fn bit(b: u8, n: u8) -> u8 {
    (b >> n) & 1
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes([*b.get(at)?, *b.get(at + 1)?, *b.get(at + 2)?, *b.get(at + 3)?]))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    let s = b.get(at..at + 8)?;
    Some(u64::from_be_bytes(s.try_into().ok()?))
}

/// The VPD page's declared bytes: its 4-byte header says how long it is.
fn vpd_body(raw: &[u8]) -> &[u8] {
    if raw.len() < 4 {
        return &[];
    }
    let len = usize::from(u16::from_be_bytes([raw[2], raw[3]]));
    &raw[4..raw.len().min(4 + len)]
}

// --------------------------------------------------------- VPD decoders

/// The page in words, for the standard pages SPC/SBC define. None for a
/// vendor page (0xC0..=0xFF) and for a page we do not decode.
pub fn say_vpd(page: u8, raw: &[u8]) -> Option<String> {
    let body = vpd_body(raw);
    match page {
        0x00 => Some(format!("supported: {}", crate::pi::parse_vpd_list(raw).iter().map(|p| page_name(*p)).collect::<Vec<_>>().join(" "))),
        0x80 => Some(format!("unit serial number: {}", String::from_utf8_lossy(body).trim())),
        0x83 => Some(say_device_id(body)),
        0x86 => ext_inquiry(raw).map(|e| say_ext_inquiry(&e)),
        0x89 => Some(say_ata_info(raw)),
        0x8F => Some("third-party copy".into()),
        0x90 => Some("protocol-specific logical unit information".into()),
        0x91 => Some("protocol-specific port information".into()),
        0xB0 => Some(say_block_limits(raw)),
        0xB1 => Some(say_block_characteristics(raw)),
        0xB2 => Some(say_provisioning(raw)),
        0xB4 => Some(format!(
            "supported block lengths and protection types: {}",
            crate::pi::parse_block_lengths(raw)
                .iter()
                .map(|b| format!("{} (PI {})", b.length, b.pi_types.iter().map(u8::to_string).collect::<Vec<_>>().join(",")))
                .collect::<Vec<_>>()
                .join("; ")
        )),
        0xB7 => Some(say_block_limits_ext(raw)),
        _ => None,
    }
}

/// VPD 0x83, every designation descriptor: `lu naa 5000cca2680ab8d8; port
/// naa 5000cca2680ab8d9; port relative 1`.
pub fn say_device_id(mut body: &[u8]) -> String {
    let mut out = Vec::new();
    while body.len() >= 4 {
        let code_set = body[0] & 0x0f;
        let assoc = (body[1] >> 4) & 0x03;
        let kind = body[1] & 0x0f;
        let len = usize::from(body[3]);
        let Some(d) = body.get(4..4 + len) else { break };
        let assoc = ["lu", "port", "device", "reserved"][usize::from(assoc)];
        let kind = match kind {
            0 => "vendor",
            1 => "t10",
            2 => "eui64",
            3 => "naa",
            4 => "relative",
            5 => "group",
            6 => "lu-group",
            7 => "md5",
            8 => "name",
            _ => "other",
        };
        let value = match (kind, code_set) {
            ("relative" | "group" | "lu-group", _) if d.len() >= 4 => u16::from_be_bytes([d[2], d[3]]).to_string(),
            (_, 2 | 3) => String::from_utf8_lossy(d).trim_end_matches('\0').trim().to_string(),
            _ => hex(d),
        };
        out.push(format!("{assoc} {kind} {value}"));
        body = &body[4 + len..];
    }
    out.join("; ")
}

/// The Extended INQUIRY Data page (0x86) named. None when too short.
pub fn ext_inquiry(raw: &[u8]) -> Option<ExtInquiry> {
    if raw.len() < 8 {
        return None;
    }
    let (b4, b5, b6, b7) = (raw[4], raw[5], raw[6], raw[7]);
    Some(ExtInquiry {
        activate_microcode: b4 >> 6,
        spt: (b4 >> 3) & 0x07,
        grd_chk: bit(b4, 2),
        app_chk: bit(b4, 1),
        ref_chk: bit(b4, 0),
        uask_sup: bit(b5, 5),
        group_sup: bit(b5, 4),
        prior_sup: bit(b5, 3),
        headsup: bit(b5, 2),
        ordsup: bit(b5, 1),
        simpsup: bit(b5, 0),
        wu_sup: bit(b6, 3),
        crd_sup: bit(b6, 2),
        nv_sup: bit(b6, 1),
        v_sup: bit(b6, 0),
        no_pi_chk: bit(b7, 5),
        p_i_i_sup: bit(b7, 4),
        luiclr: bit(b7, 0),
        extended_self_test_minutes: be16(raw, 10),
        max_sense_length: raw.get(13).copied(),
    })
}

pub fn say_ext_inquiry(e: &ExtInquiry) -> String {
    let mut s = format!(
        "extended INQUIRY: ACTIVATE_MICROCODE {} SPT {} GRD_CHK {} APP_CHK {} REF_CHK {} UASK_SUP {} GROUP_SUP {} PRIOR_SUP {} HEADSUP {} ORDSUP {} SIMPSUP {} WU_SUP {} CRD_SUP {} NV_SUP {} V_SUP {} NO_PI_CHK {} P_I_I_SUP {} LUICLR {}",
        e.activate_microcode, e.spt, e.grd_chk, e.app_chk, e.ref_chk, e.uask_sup, e.group_sup, e.prior_sup, e.headsup, e.ordsup, e.simpsup, e.wu_sup, e.crd_sup, e.nv_sup, e.v_sup, e.no_pi_chk, e.p_i_i_sup, e.luiclr
    );
    if let Some(m) = e.extended_self_test_minutes {
        s.push_str(&format!(" EXTENDED_SELF_TEST {m} min"));
    }
    if let Some(m) = e.max_sense_length {
        s.push_str(&format!(" MAX_SENSE_LENGTH {m}"));
    }
    s
}

fn ascii(b: &[u8], from: usize, to: usize) -> String {
    b.get(from..to.min(b.len())).map(|s| String::from_utf8_lossy(s).trim().to_string()).unwrap_or_default()
}

/// ATA Information (0x89, SAT): the translating layer's names.
pub fn say_ata_info(raw: &[u8]) -> String {
    format!("ATA information: SAT {} {} {}", ascii(raw, 8, 16), ascii(raw, 16, 32), ascii(raw, 32, 36))
}

/// Block Limits (0xB0).
pub fn say_block_limits(raw: &[u8]) -> String {
    let mut s = vec!["block limits:".to_string()];
    let mut put = |name: &str, v: Option<String>| {
        if let Some(v) = v {
            s.push(format!("{name} {v}"));
        }
    };
    put("MAX_COMPARE_AND_WRITE", raw.get(5).map(u8::to_string));
    put("OPTIMAL_GRANULARITY", be16(raw, 6).map(|v| v.to_string()));
    put("MAX_TRANSFER", be32(raw, 8).map(|v| v.to_string()));
    put("OPTIMAL_TRANSFER", be32(raw, 12).map(|v| v.to_string()));
    put("MAX_PREFETCH", be32(raw, 16).map(|v| v.to_string()));
    put("MAX_UNMAP_LBAS", be32(raw, 20).map(|v| v.to_string()));
    put("MAX_UNMAP_DESCRIPTORS", be32(raw, 24).map(|v| v.to_string()));
    put("UNMAP_GRANULARITY", be32(raw, 28).map(|v| v.to_string()));
    put("UNMAP_ALIGNMENT", be32(raw, 32).map(|v| if v & 0x8000_0000 != 0 { (v & 0x7fff_ffff).to_string() } else { "invalid".into() }));
    put("MAX_WRITE_SAME", be64(raw, 36).map(|v| v.to_string()));
    s.join(" ")
}

/// Block Device Characteristics (0xB1).
pub fn say_block_characteristics(raw: &[u8]) -> String {
    let rate = match be16(raw, 4) {
        None => "?".to_string(),
        Some(0) => "not reported".into(),
        Some(1) => "non-rotating".into(),
        Some(r) => format!("{r} rpm"),
    };
    let ff = match raw.get(7).map(|b| b & 0x0f) {
        Some(1) => "5.25\"",
        Some(2) => "3.5\"",
        Some(3) => "2.5\"",
        Some(4) => "1.8\"",
        Some(5) => "under 1.8\"",
        _ => "not reported",
    };
    let b8 = raw.get(8).copied().unwrap_or(0);
    format!(
        "block device characteristics: ROTATION {rate} PRODUCT_TYPE {} FORM_FACTOR {ff} ZONED {} FUAB {} VBULS {}",
        raw.get(6).copied().unwrap_or(0),
        (b8 >> 4) & 0x03,
        bit(b8, 1),
        bit(b8, 0)
    )
}

/// Logical Block Provisioning (0xB2).
pub fn say_provisioning(raw: &[u8]) -> String {
    let b5 = raw.get(5).copied().unwrap_or(0);
    let kind = match raw.get(6).map(|b| b & 0x07) {
        Some(0) => "full",
        Some(1) => "resource",
        Some(2) => "thin",
        _ => "?",
    };
    format!(
        "logical block provisioning: THRESHOLD_EXPONENT {} LBPU {} LBPWS {} LBPWS10 {} LBPRZ {} ANC_SUP {} DP {} PROVISIONING {kind}",
        raw.get(4).copied().unwrap_or(0),
        bit(b5, 7),
        bit(b5, 6),
        bit(b5, 5),
        (b5 >> 2) & 0x07,
        bit(b5, 1),
        bit(b5, 0)
    )
}

/// Block Limits Extension (0xB7).
pub fn say_block_limits_ext(raw: &[u8]) -> String {
    format!(
        "block limits extension: MAX_STREAMS {} OPTIMAL_STREAM_WRITE {} STREAM_GRANULARITY {}",
        be16(raw, 6).unwrap_or(0),
        be16(raw, 8).unwrap_or(0),
        be32(raw, 10).unwrap_or(0)
    )
}

// -------------------------------------------------------- mode decoders

/// The first mode page in a MODE SENSE(10) response: header and block
/// descriptors skipped, the page's own length honoured (SPF: 4-byte page
/// header). None when there is none.
pub fn mode_page_of(raw: &[u8]) -> Option<&[u8]> {
    if raw.len() < 8 {
        return None;
    }
    let data_len = usize::from(u16::from_be_bytes([raw[0], raw[1]])) + 2;
    let end = raw.len().min(data_len);
    let bd_len = usize::from(u16::from_be_bytes([raw[6], raw[7]]));
    let p = raw.get(8 + bd_len..end)?;
    if p.len() < 2 {
        return None;
    }
    let len = if p[0] & 0x40 != 0 {
        4 + usize::from(be16(p, 2)?)
    } else {
        2 + usize::from(p[1])
    };
    Some(&p[..len.min(p.len())])
}

/// The control mode page (0x0A), from the page itself.
pub fn control(p: &[u8]) -> Option<Control> {
    if p.len() < 6 || p[0] & 0x3f != 0x0A {
        return None;
    }
    let (b2, b3, b4, b5) = (p[2], p[3], p[4], p[5]);
    Some(Control {
        tst: b2 >> 5,
        tmf_only: bit(b2, 4),
        dpicz: bit(b2, 3),
        d_sense: bit(b2, 2),
        gltsd: bit(b2, 1),
        rlec: bit(b2, 0),
        qam: b3 >> 4,
        nuar: bit(b3, 3),
        qerr: (b3 >> 1) & 0x03,
        rac: bit(b4, 6),
        ua_intlck_ctrl: (b4 >> 4) & 0x03,
        swp: bit(b4, 3),
        ato: bit(b5, 7),
        tas: bit(b5, 6),
        atmpe: bit(b5, 5),
        rwwp: bit(b5, 4),
        sblp: bit(b5, 3),
        autoload_mode: b5 & 0x07,
        busy_timeout: be16(p, 8),
        extended_self_test_secs: be16(p, 10),
    })
}

/// The caching mode page (0x08), from the page itself.
pub fn caching(p: &[u8]) -> Option<Caching> {
    if p.len() < 3 || p[0] & 0x3f != 0x08 {
        return None;
    }
    let b2 = p[2];
    let b12 = p.get(12).copied();
    Some(Caching {
        ic: bit(b2, 7),
        abpf: bit(b2, 6),
        cap: bit(b2, 5),
        disc: bit(b2, 4),
        size: bit(b2, 3),
        wce: bit(b2, 2),
        mf: bit(b2, 1),
        rcd: bit(b2, 0),
        fsw: b12.map(|b| bit(b, 7)),
        lbcss: b12.map(|b| bit(b, 6)),
        dra: b12.map(|b| bit(b, 5)),
        nv_dis: b12.map(|b| bit(b, 0)),
    })
}

/// A mode page in words (its own bytes, from the page-code byte).
pub fn say_mode(page: u8, p: &[u8]) -> Option<String> {
    match page {
        0x03 => Some(format!(
            "format device: TRACKS_PER_ZONE {} SECTORS_PER_TRACK {} DATA_BYTES_PER_SECTOR {} SSEC {} HSEC {} RMB {} SURF {}",
            be16(p, 2)?,
            be16(p, 10)?,
            be16(p, 12)?,
            bit(*p.get(20)?, 7),
            bit(p[20], 6),
            bit(p[20], 5),
            bit(p[20], 4)
        )),
        0x08 => caching(p).map(|c| {
            let mut s = format!("caching: WCE {} RCD {} IC {} ABPF {} CAP {} DISC {} SIZE {} MF {}", c.wce, c.rcd, c.ic, c.abpf, c.cap, c.disc, c.size, c.mf);
            if let (Some(fsw), Some(lbcss), Some(dra), Some(nv)) = (c.fsw, c.lbcss, c.dra, c.nv_dis) {
                s.push_str(&format!(" FSW {fsw} LBCSS {lbcss} DRA {dra} NV_DIS {nv}"));
            }
            s
        }),
        0x0A => control(p).map(|c| {
            let mut s = format!(
                "control: DPICZ {} D_SENSE {} QAM {} QERR {} SWP {} ATO {} TAS {} TST {} TMF_ONLY {} GLTSD {} RLEC {} NUAR {} RAC {} UA_INTLCK_CTRL {} ATMPE {} RWWP {} SBLP {} AUTOLOAD {}",
                c.dpicz, c.d_sense, c.qam, c.qerr, c.swp, c.ato, c.tas, c.tst, c.tmf_only, c.gltsd, c.rlec, c.nuar, c.rac, c.ua_intlck_ctrl, c.atmpe, c.rwwp, c.sblp, c.autoload_mode
            );
            if let Some(b) = c.busy_timeout {
                s.push_str(&format!(" BUSY_TIMEOUT {b}"));
            }
            if let Some(t) = c.extended_self_test_secs {
                s.push_str(&format!(" EXTENDED_SELF_TEST {t} s"));
            }
            s
        }),
        0x19 => {
            let proto = match p.get(2)? & 0x0f {
                0 => "FC".to_string(),
                1 => "SPI".into(),
                6 => "SAS".into(),
                x => format!("protocol {x}"),
            };
            let mut s = format!("protocol-specific port: {proto}");
            if p[2] & 0x0f == 6 && p.len() >= 10 {
                s.push_str(&format!(
                    " READY_LED_MEANING {} I_T_NEXUS_LOSS {} ms INITIATOR_RESPONSE_TIMEOUT {} ms REJECT_TO_OPEN_LIMIT {}",
                    bit(p[2], 4),
                    be16(p, 4)?,
                    be16(p, 6)?,
                    be16(p, 8)?
                ));
            }
            Some(s)
        }
        0x1A => {
            let (b2, b3) = (*p.get(2)?, *p.get(3)?);
            let t = |at: usize| be32(p, at).map(|v| format!("{}.{} s", v / 10, v % 10)).unwrap_or_else(|| "-".into());
            Some(format!(
                "power condition: IDLE_A {} {} IDLE_B {} {} IDLE_C {} {} STANDBY_Z {} {} STANDBY_Y {} {} PM_BG_PRECEDENCE {}",
                bit(b3, 1),
                t(4),
                bit(b3, 2),
                t(12),
                bit(b3, 3),
                t(16),
                bit(b3, 0),
                t(8),
                bit(b2, 0),
                t(20),
                b2 >> 6
            ))
        }
        0x1C => {
            let (b2, b3) = (*p.get(2)?, *p.get(3)?);
            Some(format!(
                "informational exceptions: PERF {} EBF {} EWASC {} DEXCPT {} TEST {} EBACKERR {} LOGERR {} MRIE {} INTERVAL {} REPORT_COUNT {}",
                bit(b2, 7),
                bit(b2, 5),
                bit(b2, 4),
                bit(b2, 3),
                bit(b2, 2),
                bit(b2, 1),
                bit(b2, 0),
                b3 & 0x0f,
                be32(p, 4).unwrap_or(0),
                be32(p, 8).unwrap_or(0)
            ))
        }
        _ => None,
    }
}

// ------------------------------------------------------------- assembly

/// What the drive returned for one VPD page, said.
pub fn vpd_page(page: u8, r: Result<Vec<u8>, String>) -> VpdPage {
    match r {
        Ok(raw) => VpdPage {
            page: page_name(page),
            hex: hex(&raw),
            said: if page >= 0xC0 { None } else { say_vpd(page, &raw) },
            strings: strings(vpd_body(&raw)),
            error: None,
        },
        Err(e) => VpdPage { page: page_name(page), error: Some(e), ..Default::default() },
    }
}

/// What the drive returned for one mode page (current, changeable), said.
pub fn mode_page(page: u8, current: Result<Vec<u8>, String>, changeable: Option<Vec<u8>>) -> ModePage {
    match current {
        Ok(raw) => match mode_page_of(&raw) {
            Some(p) => ModePage {
                page: page_name(page),
                hex: hex(p),
                changeable: changeable.as_deref().and_then(mode_page_of).map(hex),
                said: say_mode(page, p),
                error: None,
            },
            None => ModePage { page: page_name(page), said: Some("refused: no page in the answer".into()), error: Some("no page in the answer".into()), ..Default::default() },
        },
        Err(e) => ModePage { page: page_name(page), said: Some(format!("refused: {e}")), error: Some(e), ..Default::default() },
    }
}

/// The pages and their named bits, from what was read.
pub fn assemble(vpd: Vec<(u8, Result<Vec<u8>, String>)>, mode: Vec<(u8, Result<Vec<u8>, String>, Option<Vec<u8>>)>, read_at: u64) -> Pages {
    let ext_inquiry = vpd.iter().find(|(p, _)| *p == 0x86).and_then(|(_, r)| r.as_ref().ok()).and_then(|raw| ext_inquiry(raw));
    let page = |code: u8| mode.iter().find(|(p, _, _)| *p == code).and_then(|(_, r, _)| r.as_ref().ok()).and_then(|raw| mode_page_of(raw).map(<[u8]>::to_vec));
    let control = page(0x0A).and_then(|p| control(&p));
    let caching = page(0x08).and_then(|p| caching(&p));
    Pages {
        vpd: vpd.into_iter().map(|(p, r)| vpd_page(p, r)).collect(),
        mode: mode.into_iter().map(|(p, r, c)| mode_page(p, r, c)).collect(),
        control,
        ext_inquiry,
        caching,
        read_at,
        error: None,
    }
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn read_linux(path: &str) -> Pages {
    use crate::scsi::{cdb, Device, Dir, T_SHORT};
    let dev = match Device::open(path) {
        Ok(d) => d,
        Err(e) => return Pages { error: Some(format!("open {path}: {e}")), read_at: now_unix(), ..Default::default() },
    };
    let _ = dev.test_unit_ready(); // clear a unit attention
    let ask_vpd = |p: u8| -> Result<Vec<u8>, String> {
        let mut buf = vec![0u8; usize::from(VPD_ALLOC)];
        let n = dev.io(&cdb::inquiry(Some(p), VPD_ALLOC), Dir::FromDevice, &mut buf, T_SHORT).map_err(|e| e.to_string())?;
        buf.truncate(n);
        Ok(buf)
    };
    let list = match ask_vpd(0x00) {
        Ok(raw) => raw,
        Err(e) => return Pages { error: Some(format!("VPD 0x00: {e}")), read_at: now_unix(), ..Default::default() },
    };
    let mut codes = crate::pi::parse_vpd_list(&list);
    codes.dedup();
    let mut vpd = vec![(0x00u8, Ok(list))];
    vpd.extend(codes.into_iter().filter(|p| *p != 0x00).map(|p| (p, ask_vpd(p))));
    let ask_mode = |p: u8, pc: u8| -> Result<Vec<u8>, String> {
        let mut buf = vec![0u8; 512];
        let n = dev.io(&cdb::mode_sense10_pc(p, pc, 512), Dir::FromDevice, &mut buf, T_SHORT).map_err(|e| e.to_string())?;
        buf.truncate(n);
        Ok(buf)
    };
    let mode = MODE_PAGES.iter().map(|&p| (p, ask_mode(p, 0), ask_mode(p, 1).ok())).collect();
    assemble(vpd, mode, now_unix())
}

/// Read a SCSI drive's pages through its sg (or block) node. Blocking.
/// Only reads: INQUIRY (EVPD) and MODE SENSE.
pub fn read(path: &str) -> Pages {
    #[cfg(target_os = "linux")]
    {
        read_linux(path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Pages { error: Some("reading a drive's pages requires Linux".into()), read_at: now_unix(), ..Default::default() }
    }
}

// --------------------------------------------------------------- stores

/// Discovery's cache of pages by device (name and its identity), so a pass
/// reads a drive's pages once per [`REFRESH`], not every pass.
#[derive(Default)]
pub struct Cache {
    by_key: HashMap<String, (Pages, Instant)>,
}

impl Cache {
    pub fn get(&self, key: &str, now: Instant) -> Option<Pages> {
        self.by_key.get(key).filter(|(_, at)| now.duration_since(*at) < REFRESH).map(|(p, _)| p.clone())
    }
    pub fn put(&mut self, key: String, p: Pages, now: Instant) {
        self.by_key.insert(key, (p, now));
    }
    /// Pages read outside discovery (`op: details`) replace what it holds
    /// for that device name, so its next pass does not put older ones back.
    pub fn replace_name(&mut self, name: &str, p: &Pages, now: Instant) {
        let prefix = format!("{name}|");
        for (k, v) in self.by_key.iter_mut() {
            if k.starts_with(&prefix) {
                *v = (p.clone(), now);
            }
        }
    }
    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.by_key.retain(|k, _| keep(k));
    }
}

static CACHE: std::sync::OnceLock<Mutex<Cache>> = std::sync::OnceLock::new();

pub fn cache() -> &'static Mutex<Cache> {
    CACHE.get_or_init(Default::default)
}

/// The cache key of a device: its name, `maj:min` and WWID (a swap or a
/// rescan is another device).
pub fn key(name: &str, dev: &str, wwid: Option<&str>) -> String {
    format!("{name}|{dev}|{}", wwid.unwrap_or(""))
}

/// The cached pages of a device, else read now (and cached). Blocking.
pub fn cached_or_read(key: &str, path: &str) -> Pages {
    let now = Instant::now();
    if let Some(p) = cache().lock().unwrap_or_else(|e| e.into_inner()).get(key, now) {
        return p;
    }
    let p = read(path);
    cache().lock().unwrap_or_else(|e| e.into_inner()).put(key.to_string(), p.clone(), now);
    p
}

static BY_DRIVE: std::sync::OnceLock<Mutex<HashMap<DriveId, Pages>>> = std::sync::OnceLock::new();

fn by_drive() -> &'static Mutex<HashMap<DriveId, Pages>> {
    BY_DRIVE.get_or_init(Default::default)
}

/// The pages last read from a drive, for its Drive object.
pub fn get(id: &DriveId) -> Option<Pages> {
    by_drive().lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned()
}

/// Pages read now for a drive (`op: details`): kept for its Drive object
/// and in discovery's cache under each of its device names.
pub fn set_read(id: DriveId, names: &[String], p: Pages) {
    let now = Instant::now();
    {
        let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
        for n in names {
            c.replace_name(n, &p, now);
        }
    }
    set(id, p);
}

/// One line for a job's record: what was read, and the control page.
pub fn summary(p: &Pages) -> String {
    if let Some(e) = &p.error {
        return format!("pages not read: {e}");
    }
    let refused = p.vpd.iter().filter(|v| v.error.is_some()).count() + p.mode.iter().filter(|m| m.error.is_some()).count();
    let mut s = format!("{} VPD page(s), {} mode page(s)", p.vpd.len(), p.mode.len());
    if refused > 0 {
        s.push_str(&format!(", {refused} refused"));
    }
    if let Some(c) = p.mode.iter().find(|m| m.page == "0x0A").and_then(|m| m.said.as_deref()) {
        s.push_str(&format!("; {c}"));
    }
    s
}

/// Keep a drive's pages (discovery's or `op: details`').
pub fn set(id: DriveId, p: Pages) {
    by_drive().lock().unwrap_or_else(|e| e.into_inner()).insert(id, p);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// forge1 bay 3's control page shape (NA02 firmware): DPICZ 0 D_SENSE 0
    /// QAM 1 QERR 0 SWP 0 ATO 1 TAS 0, busy timeout FFFFh, self-test 5640 s.
    fn control_page() -> Vec<u8> {
        vec![0x8A, 0x0A, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xff, 0xff, 0x16, 0x08]
    }

    fn mode_answer(page: &[u8], bd: usize) -> Vec<u8> {
        let mut v = vec![0u8; 8 + bd];
        let len = (6 + bd + page.len()) as u16;
        v[0..2].copy_from_slice(&len.to_be_bytes());
        v[6..8].copy_from_slice(&(bd as u16).to_be_bytes());
        v.extend_from_slice(page);
        v.extend_from_slice(&[0xee; 8]); // past the data length: ignored
        v
    }

    #[test]
    fn the_control_page_is_named_and_said() {
        let p = control_page();
        let c = control(&p).unwrap();
        assert_eq!((c.dpicz, c.d_sense, c.qam, c.qerr, c.swp, c.ato, c.tas), (0, 0, 1, 0, 0, 1, 0));
        assert_eq!(c.busy_timeout, Some(0xffff));
        assert_eq!(c.extended_self_test_secs, Some(0x1608));
        let said = say_mode(0x0A, &p).unwrap();
        assert!(said.starts_with("control: DPICZ 0 D_SENSE 0 QAM 1 QERR 0 SWP 0 ATO 1 TAS 0"), "{said}");
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["ato"], 1);
        assert_eq!(v["dSense"], 0);
        assert_eq!(v["dpicz"], 0);
    }

    #[test]
    fn a_mode_answer_skips_the_header_and_descriptors() {
        let page = control_page();
        assert_eq!(mode_page_of(&mode_answer(&page, 0)).unwrap(), &page[..]);
        assert_eq!(mode_page_of(&mode_answer(&page, 8)).unwrap(), &page[..]);
        assert_eq!(mode_page_of(&mode_answer(&page, 16)).unwrap(), &page[..]);
        assert!(mode_page_of(&[0, 6, 0, 0, 0, 0, 0, 0]).is_none(), "no page");
        // A subpage-format page (SPF) has a 4-byte header.
        let spf = [0x59, 0x01, 0x00, 0x02, 0xaa, 0xbb];
        assert_eq!(mode_page_of(&mode_answer(&spf, 0)).unwrap(), &spf[..]);
        // A page longer than what came back is cut, not a panic.
        let cut = [0x08, 0x12, 0x04];
        assert_eq!(mode_page_of(&mode_answer(&cut, 0)).unwrap(), &cut[..]);
    }

    #[test]
    fn caching_says_wce() {
        let mut p = vec![0x88, 0x12, 0b0000_0100];
        p.resize(20, 0);
        p[12] = 0b0010_0000;
        let c = caching(&p).unwrap();
        assert_eq!((c.wce, c.rcd, c.dra), (1, 0, Some(1)));
        assert!(say_mode(0x08, &p).unwrap().starts_with("caching: WCE 1 RCD 0"));
        assert!(caching(&control_page()).is_none(), "another page is not caching");
    }

    #[test]
    fn ext_inquiry_is_named() {
        // SPT 001b, GRD_CHK APP_CHK REF_CHK, ACTIVATE_MICROCODE 01b; HEADSUP ORDSUP SIMPSUP; V_SUP.
        let mut raw = vec![0x00, 0x86, 0x00, 0x3C, 0b0100_1111, 0b0000_0111, 0b0000_0001, 0b0001_0000];
        raw.resize(64, 0);
        raw[10..12].copy_from_slice(&94u16.to_be_bytes());
        raw[13] = 252;
        let e = ext_inquiry(&raw).unwrap();
        assert_eq!((e.activate_microcode, e.spt, e.grd_chk, e.app_chk, e.ref_chk), (1, 1, 1, 1, 1));
        assert_eq!((e.headsup, e.ordsup, e.simpsup, e.v_sup, e.p_i_i_sup), (1, 1, 1, 1, 1));
        assert_eq!(e.extended_self_test_minutes, Some(94));
        let said = say_vpd(0x86, &raw).unwrap();
        assert!(said.starts_with("extended INQUIRY: ACTIVATE_MICROCODE 1 SPT 1 GRD_CHK 1 APP_CHK 1 REF_CHK 1"), "{said}");
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["grdChk"], 1);
        assert_eq!(v["appChk"], 1);
        assert_eq!(v["refChk"], 1);
        assert!(ext_inquiry(&raw[..6]).is_none());
    }

    #[test]
    fn device_identification_names_every_designator() {
        let mut body = vec![];
        // LU, NAA, binary.
        body.extend_from_slice(&[0x01, 0x03, 0x00, 0x08, 0x50, 0x00, 0xcc, 0xa2, 0x68, 0x0a, 0xb8, 0xd8]);
        // Target port (SAS, PIV), NAA.
        body.extend_from_slice(&[0x61, 0x93, 0x00, 0x08, 0x50, 0x00, 0xcc, 0xa2, 0x68, 0x0a, 0xb8, 0xd9]);
        // Relative target port 1.
        body.extend_from_slice(&[0x61, 0x94, 0x00, 0x04, 0x00, 0x00, 0x00, 0x01]);
        // T10 vendor id, ASCII.
        body.extend_from_slice(&[0x02, 0x01, 0x00, 0x08]);
        body.extend_from_slice(b"HGST    ");
        let mut raw = vec![0x00, 0x83, 0x00, body.len() as u8];
        raw.extend_from_slice(&body);
        assert_eq!(say_vpd(0x83, &raw).unwrap(), "lu naa 5000cca2680ab8d8; port naa 5000cca2680ab8d9; port relative 1; lu t10 HGST");
        // A descriptor longer than the page ends it.
        assert_eq!(say_device_id(&[0x01, 0x03, 0x00, 0x20, 0x50]), "");
    }

    #[test]
    fn vendor_pages_are_raw_with_strings_never_said() {
        let mut raw = vec![0x00, 0xD1, 0x00, 0x00];
        raw.extend_from_slice(b"NETAPP\x00\x01X378_HLBRE10TA07\x00ab");
        raw[3] = (raw.len() - 4) as u8;
        let p = vpd_page(0xD1, Ok(raw.clone()));
        assert_eq!(p.page, "0xD1");
        assert_eq!(p.said, None);
        assert_eq!(p.strings, vec!["NETAPP", "X378_HLBRE10TA07"]);
        assert_eq!(p.hex, hex(&raw));
        let refused = vpd_page(0xC0, Err("sense 5/2400".into()));
        assert_eq!(refused.error.as_deref(), Some("sense 5/2400"));
        let v = serde_json::to_value(&refused).unwrap();
        assert!(v.get("said").is_none() && v.get("strings").is_none());
    }

    #[test]
    fn the_standard_vpd_pages_are_said() {
        assert_eq!(say_vpd(0x00, &[0, 0, 0, 3, 0x00, 0x80, 0x86]).unwrap(), "supported: 0x00 0x80 0x86");
        assert_eq!(say_vpd(0x80, b"\x00\x80\x00\x0a  2TJ98VMD").unwrap(), "unit serial number: 2TJ98VMD");
        let mut b1 = vec![0, 0xB1, 0, 0x3C, 0x1C, 0x20, 0, 0x02, 0];
        b1.resize(64, 0);
        assert!(say_vpd(0xB1, &b1).unwrap().contains("ROTATION 7200 rpm PRODUCT_TYPE 0 FORM_FACTOR 3.5\""));
        let mut b0 = vec![0, 0xB0, 0, 0x3C];
        b0.resize(64, 0);
        b0[8..12].copy_from_slice(&65535u32.to_be_bytes());
        assert!(say_vpd(0xB0, &b0).unwrap().contains("MAX_TRANSFER 65535"));
        assert!(say_vpd(0xB0, &b0).unwrap().contains("UNMAP_ALIGNMENT invalid"));
        let b2 = [0, 0xB2, 0, 4, 0, 0b1100_0001, 0x02, 0];
        assert_eq!(say_vpd(0xB2, &b2).unwrap(), "logical block provisioning: THRESHOLD_EXPONENT 0 LBPU 1 LBPWS 1 LBPWS10 0 LBPRZ 0 ANC_SUP 0 DP 1 PROVISIONING thin");
        assert!(say_vpd(0x8F, &[0, 0x8F, 0, 0]).unwrap().contains("third-party copy"));
        assert!(say_vpd(0x42, &[0, 0x42, 0, 0]).is_none(), "not one we decode");
        // Short pages say what they can and never panic.
        for p in [0x00u8, 0x80, 0x83, 0x86, 0x89, 0xB0, 0xB1, 0xB2, 0xB4, 0xB7] {
            let _ = say_vpd(p, &[]);
            let _ = say_vpd(p, &[0, p]);
        }
        for p in MODE_PAGES {
            let _ = say_mode(p, &[]);
            let _ = say_mode(p, &[p, 0]);
        }
    }

    #[test]
    fn the_other_mode_pages_are_said() {
        let mut pc = vec![0x9A, 0x26, 0x00, 0b0000_0011];
        pc.resize(40, 0);
        pc[4..8].copy_from_slice(&20u32.to_be_bytes());
        let s = say_mode(0x1A, &pc).unwrap();
        assert!(s.starts_with("power condition: IDLE_A 1 2.0 s IDLE_B 0"), "{s}");
        assert!(s.contains("STANDBY_Z 1 0.0 s"), "{s}");
        let ie = [0x9C, 0x0A, 0b0000_1000, 0x06, 0, 0, 0x0b, 0xb8, 0, 0, 0, 1];
        assert_eq!(say_mode(0x1C, &ie).unwrap(), "informational exceptions: PERF 0 EBF 0 EWASC 0 DEXCPT 1 TEST 0 EBACKERR 0 LOGERR 0 MRIE 6 INTERVAL 3000 REPORT_COUNT 1");
        let sas = [0x99, 0x0E, 0x06, 0, 0x07, 0xD0, 0x07, 0xD0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(say_mode(0x19, &sas).unwrap(), "protocol-specific port: SAS READY_LED_MEANING 0 I_T_NEXUS_LOSS 2000 ms INITIATOR_RESPONSE_TIMEOUT 2000 ms REJECT_TO_OPEN_LIMIT 0");
        let mut fd = vec![0x83, 0x16];
        fd.resize(24, 0);
        fd[12..14].copy_from_slice(&512u16.to_be_bytes());
        fd[20] = 0x40;
        assert!(say_mode(0x03, &fd).unwrap().contains("DATA_BYTES_PER_SECTOR 512 SSEC 0 HSEC 1"));
    }

    #[test]
    fn assembled_pages_carry_the_named_bits() {
        let mut x86 = vec![0x00, 0x86, 0x00, 0x3C, 0b0000_1101, 0, 0, 0];
        x86.resize(64, 0);
        let vpd = vec![(0x00, Ok(vec![0, 0, 0, 2, 0x00, 0x86])), (0x86, Ok(x86))];
        let mut changeable = control_page();
        changeable[2..].fill(0);
        changeable[5] = 0x80; // ATO may be changed
        let mode = vec![
            (0x0A, Ok(mode_answer(&control_page(), 8)), Some(mode_answer(&changeable, 8))),
            (0x1A, Err("sense 5/2400".to_string()), None),
        ];
        let p = assemble(vpd, mode, 42);
        assert_eq!(p.control.as_ref().unwrap().ato, 1);
        let e = p.ext_inquiry.as_ref().unwrap();
        assert_eq!((e.spt, e.grd_chk, e.app_chk, e.ref_chk), (1, 1, 0, 1));
        assert_eq!(p.mode[0].changeable.as_deref(), Some(hex(&changeable).as_str()));
        assert_eq!(p.mode[1].said.as_deref(), Some("refused: sense 5/2400"));
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["control"]["ato"], 1);
        assert_eq!(v["extInquiry"]["refChk"], 1);
        assert_eq!(v["vpd"][1]["page"], "0x86");
        assert_eq!(v["mode"][0]["page"], "0x0A");
        assert_eq!(v["readAt"], 42);
        assert!(v.get("caching").is_none());
        assert_eq!(summary(&p), "2 VPD page(s), 2 mode page(s), 1 refused; control: DPICZ 0 D_SENSE 0 QAM 1 QERR 0 SWP 0 ATO 1 TAS 0 TST 0 TMF_ONLY 0 GLTSD 0 RLEC 0 NUAR 0 RAC 0 UA_INTLCK_CTRL 0 ATMPE 0 RWWP 0 SBLP 0 AUTOLOAD 0 BUSY_TIMEOUT 65535 EXTENDED_SELF_TEST 5640 s");
        assert_eq!(summary(&Pages { error: Some("open /dev/sg3: gone".into()), ..Default::default() }), "pages not read: open /dev/sg3: gone");
    }

    #[test]
    fn the_cache_keeps_pages_for_their_refresh() {
        let mut c = Cache::default();
        let t = Instant::now();
        c.put(key("sdb", "8:16", Some("naa.5000")), Pages { read_at: 1, ..Default::default() }, t);
        assert!(c.get(&key("sdb", "8:16", Some("naa.5000")), t).is_some());
        assert!(c.get(&key("sdb", "8:16", Some("naa.5001")), t).is_none(), "another drive in the name");
        assert!(c.get(&key("sdb", "8:16", Some("naa.5000")), t + REFRESH).is_none(), "stale");
        let newer = Pages { read_at: 2, ..Default::default() };
        c.replace_name("sdb", &newer, t);
        assert_eq!(c.get(&key("sdb", "8:16", Some("naa.5000")), t).unwrap().read_at, 2, "op: details' read replaces discovery's");
        c.retain(|k| !k.starts_with("sdb|"));
        assert!(c.get(&key("sdb", "8:16", Some("naa.5000")), t).is_none());
        assert_eq!(strings(b"ab\x00  abcd  \x01xyz"), vec!["abcd"]);
    }
}
