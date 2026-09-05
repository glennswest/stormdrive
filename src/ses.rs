//! SES-2 enclosure services: what a NetApp shelf (or any SES processor)
//! says about itself — identity, power supplies, fans, temperatures,
//! voltages, slots — and the two things we tell it: light this bay, light
//! this shelf.
//!
//! Pages (RECEIVE DIAGNOSTIC RESULTS, PCV=1):
//! - 0x01 Configuration: enclosure descriptor (logical id, vendor,
//!   product, revision) + type descriptor headers (element type, count).
//! - 0x02 Enclosure Status: one overall + N individual 4-byte statuses per
//!   type, in header order. SEND DIAGNOSTIC of the same page is control.
//! - 0x07 Element Descriptor: a text name per element.
//! - 0x0A Additional Element Status: per-slot SAS addresses — how a drive
//!   is tied to a bay when the kernel's `ses` module is not around.
//!
//! Parsers are portable and tested on synthetic pages; enumeration and
//! I/O are Linux-only.

use crate::drive::Shelf;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::SystemTime;

pub const PAGE_CONFIG: u8 = 0x01;
pub const PAGE_STATUS: u8 = 0x02;
pub const PAGE_DESCRIPTORS: u8 = 0x07;
pub const PAGE_ADDITIONAL: u8 = 0x0a;

// Element types (SES-2 table 61).
pub const ET_POWER_SUPPLY: u8 = 0x02;
pub const ET_COOLING: u8 = 0x03;
pub const ET_TEMPERATURE: u8 = 0x04;
pub const ET_DEVICE_SLOT: u8 = 0x01;
pub const ET_ESC_ELECTRONICS: u8 = 0x07;
pub const ET_ENCLOSURE: u8 = 0x0e;
pub const ET_VOLTAGE: u8 = 0x12;
pub const ET_CURRENT: u8 = 0x13;
pub const ET_ARRAY_DEVICE_SLOT: u8 = 0x17;
pub const ET_SAS_EXPANDER: u8 = 0x18;
pub const ET_SAS_CONNECTOR: u8 = 0x19;

pub fn element_type_name(t: u8) -> &'static str {
    match t {
        0x00 => "unspecified",
        0x01 => "device slot",
        0x02 => "power supply",
        0x03 => "cooling",
        0x04 => "temperature",
        0x05 => "door",
        0x06 => "audible alarm",
        0x07 => "esc electronics",
        0x08 => "scc electronics",
        0x09 => "nonvolatile cache",
        0x0a => "invalid operation reason",
        0x0b => "uninterruptible power supply",
        0x0c => "display",
        0x0d => "key pad",
        0x0e => "enclosure",
        0x0f => "scsi port/transceiver",
        0x10 => "language",
        0x11 => "communication port",
        0x12 => "voltage",
        0x13 => "current",
        0x14 => "scsi target port",
        0x15 => "scsi initiator port",
        0x16 => "simple subenclosure",
        0x17 => "array device slot",
        0x18 => "sas expander",
        0x19 => "sas connector",
        _ => "vendor specific",
    }
}

/// Element status code (byte 0 bits 3:0 of every status element).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElementStatus {
    Unsupported,
    Ok,
    Critical,
    Noncritical,
    Unrecoverable,
    NotInstalled,
    Unknown,
    NotAvailable,
    NoAccessAllowed,
    Reserved,
}

impl ElementStatus {
    pub fn from_code(c: u8) -> Self {
        match c & 0x0f {
            0 => Self::Unsupported,
            1 => Self::Ok,
            2 => Self::Critical,
            3 => Self::Noncritical,
            4 => Self::Unrecoverable,
            5 => Self::NotInstalled,
            6 => Self::Unknown,
            7 => Self::NotAvailable,
            8 => Self::NoAccessAllowed,
            _ => Self::Reserved,
        }
    }

    /// Is this a problem an operator should see?
    pub fn is_bad(self) -> bool {
        matches!(self, Self::Critical | Self::Noncritical | Self::Unrecoverable)
    }
}

/// One element of the enclosure, decoded as far as its type allows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Element {
    pub element_type: u8,
    pub type_name: String,
    /// Index within its type (0-based), i.e. "fan 3".
    pub index: u32,
    /// The overall element for the type (index is then meaningless).
    pub overall: bool,
    /// Text from page 0x07, when the enclosure gives one.
    #[serde(default)]
    pub name: Option<String>,
    pub status: ElementStatus,
    pub predicted_failure: bool,
    pub disabled: bool,
    pub swapped: bool,
    pub ident: bool,
    pub fault: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volts: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amps: Option<f32>,
    /// Device slot / array device slot: the bay number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bay: Option<u32>,
    /// From page 0x0A: the SAS address of what sits in the bay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sas_address: Option<String>,
    /// Power supplies: AC/DC failure, off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
    pub raw: [u8; 4],
}

/// Page 0x01, the part we keep.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Configuration {
    pub generation: u32,
    pub logical_id: Option<String>,
    pub vendor: String,
    pub product: String,
    pub revision: String,
    /// (element type, count, subenclosure id, type text), in page order —
    /// the key to reading page 0x02/0x07.
    pub types: Vec<TypeHeader>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeHeader {
    pub element_type: u8,
    pub count: u8,
    pub subenclosure: u8,
    pub text: String,
}

pub fn parse_configuration(raw: &[u8]) -> Option<Configuration> {
    if raw.len() < 8 || raw[0] != PAGE_CONFIG {
        return None;
    }
    let secondaries = raw[1] as usize;
    let generation = u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]);
    let mut cfg = Configuration {
        generation,
        ..Default::default()
    };
    let mut off = 8;
    let mut total_types = 0usize;
    for i in 0..=secondaries {
        if off + 4 > raw.len() {
            return None;
        }
        let n_types = raw[off + 2] as usize;
        let desc_len = raw[off + 3] as usize;
        total_types += n_types;
        if i == 0 && desc_len >= 36 && off + 4 + desc_len <= raw.len() {
            let d = &raw[off + 4..off + 4 + desc_len];
            let id = &d[0..8];
            if id.iter().any(|b| *b != 0) {
                cfg.logical_id = Some(id.iter().map(|b| format!("{b:02x}")).collect());
            }
            cfg.vendor = String::from_utf8_lossy(&d[8..16]).trim().to_string();
            cfg.product = String::from_utf8_lossy(&d[16..32]).trim().to_string();
            cfg.revision = String::from_utf8_lossy(&d[32..36]).trim().to_string();
        }
        off += 4 + desc_len;
    }
    let mut headers = Vec::with_capacity(total_types);
    for _ in 0..total_types {
        if off + 4 > raw.len() {
            return None;
        }
        headers.push((raw[off], raw[off + 1], raw[off + 2], raw[off + 3] as usize));
        off += 4;
    }
    for (t, count, sub, tlen) in headers {
        let text = if tlen > 0 && off + tlen <= raw.len() {
            String::from_utf8_lossy(&raw[off..off + tlen]).trim().to_string()
        } else {
            String::new()
        };
        off += tlen;
        cfg.types.push(TypeHeader {
            element_type: t,
            count,
            subenclosure: sub,
            text,
        });
    }
    Some(cfg)
}

fn decode_element(t: u8, index: u32, overall: bool, b: [u8; 4]) -> Element {
    let mut e = Element {
        element_type: t,
        type_name: element_type_name(t).into(),
        index,
        overall,
        name: None,
        status: ElementStatus::from_code(b[0]),
        predicted_failure: b[0] & 0x40 != 0,
        disabled: b[0] & 0x20 != 0,
        swapped: b[0] & 0x10 != 0,
        ident: false,
        fault: false,
        temperature_c: None,
        rpm: None,
        volts: None,
        amps: None,
        bay: None,
        sas_address: None,
        flags: Vec::new(),
        raw: b,
    };
    match t {
        ET_DEVICE_SLOT | ET_ARRAY_DEVICE_SLOT => {
            e.ident = b[2] & 0x02 != 0;
            e.fault = b[3] & 0x20 != 0 || b[3] & 0x40 != 0;
            if t == ET_DEVICE_SLOT && !overall {
                e.bay = Some(b[1] as u32);
            }
            if b[2] & 0x40 != 0 {
                e.flags.push("do not remove".into());
            }
            if b[3] & 0x10 != 0 {
                e.flags.push("device off".into());
            }
            if b[3] & 0x0f != 0 {
                e.flags.push("bypassed".into());
            }
        }
        ET_POWER_SUPPLY => {
            e.ident = b[1] & 0x80 != 0;
            e.fault = b[3] & 0x40 != 0;
            if b[2] & 0x08 != 0 {
                e.flags.push("dc overvoltage".into());
            }
            if b[2] & 0x04 != 0 {
                e.flags.push("dc undervoltage".into());
            }
            if b[2] & 0x02 != 0 {
                e.flags.push("dc overcurrent".into());
            }
            if b[3] & 0x80 != 0 {
                e.flags.push("hot swap".into());
            }
            if b[3] & 0x10 != 0 {
                e.flags.push("off".into());
            }
            if b[3] & 0x08 != 0 {
                e.flags.push("overtemp failure".into());
            }
            if b[3] & 0x04 != 0 {
                e.flags.push("temp warning".into());
            }
            if b[3] & 0x02 != 0 {
                e.flags.push("ac fail".into());
            }
            if b[3] & 0x01 != 0 {
                e.flags.push("dc fail".into());
            }
        }
        ET_COOLING => {
            e.ident = b[1] & 0x80 != 0;
            e.fault = b[3] & 0x40 != 0;
            let speed = (((b[1] & 0x07) as u32) << 8) | b[2] as u32;
            e.rpm = Some(speed * 10);
            if b[3] & 0x10 != 0 {
                e.flags.push("off".into());
            }
        }
        ET_TEMPERATURE => {
            e.ident = b[1] & 0x80 != 0;
            e.fault = b[1] & 0x40 != 0;
            if b[2] != 0 {
                e.temperature_c = Some(b[2] as i32 - 20);
            }
            if b[3] & 0x08 != 0 {
                e.flags.push("overtemp failure".into());
            }
            if b[3] & 0x04 != 0 {
                e.flags.push("overtemp warning".into());
            }
            if b[3] & 0x02 != 0 {
                e.flags.push("undertemp failure".into());
            }
            if b[3] & 0x01 != 0 {
                e.flags.push("undertemp warning".into());
            }
        }
        ET_VOLTAGE => {
            e.ident = b[1] & 0x80 != 0;
            e.fault = b[1] & 0x40 != 0;
            let mv10 = i16::from_be_bytes([b[2], b[3]]);
            e.volts = Some(mv10 as f32 / 100.0);
            if b[1] & 0x08 != 0 {
                e.flags.push("warn over".into());
            }
            if b[1] & 0x04 != 0 {
                e.flags.push("warn under".into());
            }
            if b[1] & 0x02 != 0 {
                e.flags.push("crit over".into());
            }
            if b[1] & 0x01 != 0 {
                e.flags.push("crit under".into());
            }
        }
        ET_CURRENT => {
            e.ident = b[1] & 0x80 != 0;
            e.fault = b[1] & 0x40 != 0;
            let ma10 = i16::from_be_bytes([b[2], b[3]]);
            e.amps = Some(ma10 as f32 / 100.0);
            if b[1] & 0x08 != 0 {
                e.flags.push("warn over".into());
            }
            if b[1] & 0x02 != 0 {
                e.flags.push("crit over".into());
            }
        }
        ET_ENCLOSURE => {
            e.ident = b[1] & 0x80 != 0;
            e.fault = b[3] & 0xc0 != 0 || b[2] & 0x02 != 0;
        }
        ET_ESC_ELECTRONICS | ET_SAS_EXPANDER | ET_SAS_CONNECTOR => {
            e.ident = b[1] & 0x80 != 0;
            e.fault = b[3] & 0x40 != 0;
        }
        _ => {}
    }
    e
}

/// Page 0x02 decoded against the configuration's type headers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusPage {
    pub generation: u32,
    pub invop: bool,
    pub info: bool,
    pub noncritical: bool,
    pub critical: bool,
    pub unrecoverable: bool,
    pub elements: Vec<Element>,
}

pub fn parse_status(cfg: &Configuration, raw: &[u8]) -> Option<StatusPage> {
    if raw.len() < 8 || raw[0] != PAGE_STATUS {
        return None;
    }
    let mut page = StatusPage {
        generation: u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]),
        invop: raw[1] & 0x10 != 0,
        info: raw[1] & 0x08 != 0,
        noncritical: raw[1] & 0x04 != 0,
        critical: raw[1] & 0x02 != 0,
        unrecoverable: raw[1] & 0x01 != 0,
        elements: Vec::new(),
    };
    let len = u16::from_be_bytes([raw[2], raw[3]]) as usize + 4;
    let end = len.min(raw.len());
    let mut off = 8;
    for th in &cfg.types {
        for i in 0..=(th.count as u32) {
            if off + 4 > end {
                return Some(page);
            }
            let b = [raw[off], raw[off + 1], raw[off + 2], raw[off + 3]];
            let overall = i == 0;
            let index = if overall { 0 } else { i - 1 };
            page.elements.push(decode_element(th.element_type, index, overall, b));
            off += 4;
        }
    }
    Some(page)
}

/// Page 0x07: text per element, in the same order as page 0x02.
pub fn parse_descriptors(cfg: &Configuration, raw: &[u8]) -> Vec<Option<String>> {
    let mut out = Vec::new();
    if raw.len() < 8 || raw[0] != PAGE_DESCRIPTORS {
        return out;
    }
    let mut off = 8;
    let total: usize = cfg.types.iter().map(|t| t.count as usize + 1).sum();
    for _ in 0..total {
        if off + 4 > raw.len() {
            out.push(None);
            continue;
        }
        let dlen = u16::from_be_bytes([raw[off + 2], raw[off + 3]]) as usize;
        let s = if dlen > 0 && off + 4 + dlen <= raw.len() {
            let t = String::from_utf8_lossy(&raw[off + 4..off + 4 + dlen])
                .trim()
                .to_string();
            (!t.is_empty()).then_some(t)
        } else {
            None
        };
        out.push(s);
        off += 4 + dlen;
    }
    out
}

/// One SAS device-slot descriptor from page 0x0A.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotAddress {
    /// ELEMENT INDEX (when EIP) — individual elements only, page order.
    pub element_index: Option<u8>,
    pub bay: Option<u32>,
    /// SAS addresses of the attached device's phys (usually one; two on a
    /// dual-port drive) — lower-case hex, no prefix.
    pub sas_addresses: Vec<String>,
}

pub fn parse_additional(raw: &[u8]) -> Vec<SlotAddress> {
    let mut out = Vec::new();
    if raw.len() < 8 || raw[0] != PAGE_ADDITIONAL {
        return out;
    }
    let len = u16::from_be_bytes([raw[2], raw[3]]) as usize + 4;
    let end = len.min(raw.len());
    let mut off = 8;
    while off + 2 <= end {
        let b0 = raw[off];
        let dlen = raw[off + 1] as usize;
        let invalid = b0 & 0x80 != 0;
        let eip = b0 & 0x10 != 0;
        let proto = b0 & 0x0f;
        let body_start = off + 2;
        let body_end = (body_start + dlen).min(end);
        if !invalid && proto == 0x6 && body_end > body_start {
            let mut p = body_start;
            let mut element_index = None;
            if eip {
                if p + 2 > body_end {
                    break;
                }
                element_index = Some(raw[p + 1]);
                p += 2;
            }
            if p + 4 <= body_end {
                let n_phys = raw[p] as usize;
                let dtype = raw[p + 1] >> 6;
                if dtype == 0 {
                    let bay = if eip { Some(raw[p + 3] as u32) } else { None };
                    p += 4;
                    let mut addrs = Vec::new();
                    for _ in 0..n_phys {
                        if p + 28 > body_end {
                            break;
                        }
                        let sas = &raw[p + 12..p + 20];
                        if sas.iter().any(|b| *b != 0) {
                            addrs.push(sas.iter().map(|b| format!("{b:02x}")).collect());
                        }
                        p += 28;
                    }
                    out.push(SlotAddress {
                        element_index,
                        bay,
                        sas_addresses: addrs,
                    });
                }
            }
        }
        off = body_start + dlen;
        if dlen == 0 {
            break;
        }
    }
    out
}

/// Build a page-0x02 control page from the status page that sets or
/// clears IDENT on one element. Every other element is left with SELECT=0
/// (ignored by the enclosure); the chosen element gets SELECT plus only
/// the request bits that mirror its current state (IDENT/FAULT for slots,
/// IDENT for the rest), so a locate request never smuggles a "remove" or
/// "power off" along with it.
pub fn build_ident_control(status_raw: &[u8], element_offset: usize, element_type: u8, on: bool) -> Option<Vec<u8>> {
    if status_raw.len() < 8 || element_offset + 4 > status_raw.len() {
        return None;
    }
    let mut page = vec![0u8; status_raw.len()];
    page[0] = PAGE_STATUS;
    page[2] = status_raw[2];
    page[3] = status_raw[3];
    page[4..8].copy_from_slice(&status_raw[4..8]);
    let st = &status_raw[element_offset..element_offset + 4];
    let ctl = &mut page[element_offset..element_offset + 4];
    ctl[0] = 0x80; // SELECT
    match element_type {
        ET_DEVICE_SLOT | ET_ARRAY_DEVICE_SLOT => {
            // RQST IDENT byte 2 bit 1, RQST FAULT byte 3 bit 5 — same
            // positions as IDENT / FAULT REQSTD in the status element.
            ctl[3] = st[3] & 0x20;
            ctl[2] = if on { 0x02 } else { 0 };
        }
        _ => {
            // Enclosure, PSU, cooling, temperature, expander, connector,
            // ESC: RQST IDENT is byte 1 bit 7.
            ctl[1] = if on { 0x80 } else { 0 };
        }
    }
    Some(page)
}

/// Byte offset of the n-th element (page order, overall elements
/// included) inside a page 0x02 buffer.
pub fn element_offset(n: usize) -> usize {
    8 + n * 4
}

/// Position in page order of the individual element of `element_type`
/// with the given bay (device/array-device slots are numbered by their
/// index unless page 0x0A said otherwise).
pub fn find_slot_element(elements: &[Element], bay: u32) -> Option<usize> {
    elements.iter().position(|e| {
        !e.overall
            && matches!(e.element_type, ET_DEVICE_SLOT | ET_ARRAY_DEVICE_SLOT)
            && e.bay == Some(bay)
    })
}

pub fn find_enclosure_element(elements: &[Element]) -> Option<usize> {
    elements
        .iter()
        .position(|e| e.element_type == ET_ENCLOSURE && !e.overall)
        .or_else(|| elements.iter().position(|e| e.element_type == ET_ENCLOSURE))
}

/// One SES processor path to a shelf (an IOM). A dual-IOM shelf has two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EspPath {
    /// SCSI id H:C:T:L.
    pub scsi_id: String,
    pub sg_path: Option<String>,
    pub sas_address: Option<String>,
    /// VPD 0x80 of the SES device — on NetApp shelves this is the IOM's
    /// serial, not the shelf's.
    pub serial: Option<String>,
}

/// Everything we know about one shelf, refreshed each monitor tick.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShelfReport {
    /// Canonical key: enclosure logical id (hex), else the first ESP's
    /// serial, else its SCSI id.
    pub key: String,
    pub shelf: Shelf,
    pub esps: Vec<EspPath>,
    pub generation: u32,
    pub critical: bool,
    pub noncritical: bool,
    pub unrecoverable: bool,
    pub info: bool,
    pub elements: Vec<Element>,
    /// bay → SAS addresses seen in that bay (page 0x0A).
    pub slots: BTreeMap<u32, Vec<String>>,
    pub collected_at: SystemTime,
    /// The raw status page, kept so a control page can be built from the
    /// exact generation the enclosure reported.
    #[serde(skip)]
    pub status_raw: Vec<u8>,
}

impl ShelfReport {
    pub fn worst(&self) -> ElementStatus {
        if self.unrecoverable {
            return ElementStatus::Unrecoverable;
        }
        if self.critical {
            return ElementStatus::Critical;
        }
        if self.noncritical {
            return ElementStatus::Noncritical;
        }
        let mut worst = ElementStatus::Ok;
        for e in &self.elements {
            match (worst, e.status) {
                (_, ElementStatus::Unrecoverable) => worst = ElementStatus::Unrecoverable,
                (ElementStatus::Ok | ElementStatus::Noncritical, ElementStatus::Critical) => {
                    worst = ElementStatus::Critical
                }
                (ElementStatus::Ok, ElementStatus::Noncritical) => worst = ElementStatus::Noncritical,
                _ => {}
            }
        }
        worst
    }

    pub fn max_temperature_c(&self) -> Option<i32> {
        self.elements.iter().filter_map(|e| e.temperature_c).max()
    }

    /// (ok, total) for a type, counting installed individual elements.
    pub fn count(&self, element_type: u8) -> (usize, usize) {
        let installed: Vec<&Element> = self
            .elements
            .iter()
            .filter(|e| e.element_type == element_type && !e.overall)
            .filter(|e| !matches!(e.status, ElementStatus::NotInstalled | ElementStatus::Unsupported))
            .collect();
        let ok = installed.iter().filter(|e| e.status == ElementStatus::Ok).count();
        (ok, installed.len())
    }

    /// Which bay holds this SAS address, per page 0x0A.
    pub fn bay_of(&self, sas_address: &str) -> Option<u32> {
        let want = normalize_sas(sas_address);
        self.slots
            .iter()
            .find(|(_, addrs)| addrs.iter().any(|a| *a == want))
            .map(|(b, _)| *b)
    }
}

/// "0x5000c500b8538d01" / "5000C500B8538D01" → "5000c500b8538d01".
pub fn normalize_sas(s: &str) -> String {
    s.trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X")
        .to_ascii_lowercase()
}

/// Assemble a report from raw pages (portable, so it can be tested).
pub fn assemble(
    esps: Vec<EspPath>,
    sysfs_id: Option<String>,
    cfg_raw: &[u8],
    status_raw: &[u8],
    desc_raw: Option<&[u8]>,
    add_raw: Option<&[u8]>,
) -> Option<ShelfReport> {
    let cfg = parse_configuration(cfg_raw)?;
    let mut status = parse_status(&cfg, status_raw)?;
    if let Some(d) = desc_raw {
        let names = parse_descriptors(&cfg, d);
        for (e, n) in status.elements.iter_mut().zip(names) {
            e.name = n;
        }
    }
    // Array device slots carry no bay number in their status; number them
    // by index, then let page 0x0A override with the real slot number.
    for e in status.elements.iter_mut() {
        if e.element_type == ET_ARRAY_DEVICE_SLOT && !e.overall && e.bay.is_none() {
            e.bay = Some(e.index);
        }
    }
    let mut slots: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    if let Some(a) = add_raw {
        let addrs = parse_additional(a);
        // Individual slot elements in page order, to map ELEMENT INDEX.
        let mut slot_positions: Vec<usize> = status
            .elements
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                !e.overall && matches!(e.element_type, ET_DEVICE_SLOT | ET_ARRAY_DEVICE_SLOT)
            })
            .map(|(i, _)| i)
            .collect();
        let individuals: Vec<usize> = status
            .elements
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.overall)
            .map(|(i, _)| i)
            .collect();
        for (n, sa) in addrs.iter().enumerate() {
            // Prefer the element index (counts all individual elements);
            // fall back to "n-th slot descriptor".
            let pos = sa
                .element_index
                .and_then(|ei| individuals.get(ei as usize).copied())
                .filter(|p| {
                    matches!(
                        status.elements[*p].element_type,
                        ET_DEVICE_SLOT | ET_ARRAY_DEVICE_SLOT
                    )
                })
                .or_else(|| slot_positions.get(n).copied());
            if let Some(p) = pos {
                if let Some(b) = sa.bay {
                    status.elements[p].bay = Some(b);
                }
                let bay = status.elements[p].bay.unwrap_or(n as u32);
                if let Some(first) = sa.sas_addresses.first() {
                    status.elements[p].sas_address = Some(first.clone());
                }
                if !sa.sas_addresses.is_empty() {
                    slots.insert(bay, sa.sas_addresses.clone());
                }
            }
        }
        slot_positions.clear();
    }
    let first = esps.first();
    let key = cfg
        .logical_id
        .clone()
        .or_else(|| first.and_then(|e| e.serial.clone()))
        .or_else(|| first.map(|e| e.scsi_id.clone()))?;
    let shelf = Shelf {
        id: sysfs_id.or_else(|| first.map(|e| e.scsi_id.clone())),
        vendor: (!cfg.vendor.is_empty()).then_some(cfg.vendor.clone()),
        model: (!cfg.product.is_empty()).then_some(cfg.product.clone()),
        serial: first.and_then(|e| e.serial.clone()),
        sas_address: first.and_then(|e| e.sas_address.clone()),
        logical_id: cfg.logical_id.clone(),
    };
    Some(ShelfReport {
        key,
        shelf,
        esps,
        generation: status.generation,
        critical: status.critical,
        noncritical: status.noncritical,
        unrecoverable: status.unrecoverable,
        info: status.info,
        elements: status.elements,
        slots,
        collected_at: SystemTime::now(),
        status_raw: status_raw.to_vec(),
    })
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::scsi::Device;
    use std::path::{Path, PathBuf};

    fn read_trim(p: &Path) -> Option<String> {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Every SCSI device of type 13 (enclosure): (H:C:T:L, sysfs dir).
    pub fn enclosure_devices() -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir("/sys/bus/scsi/devices") else {
            return out;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.contains(':') || name.starts_with("host") || name.starts_with("target") {
                continue;
            }
            if read_trim(&e.path().join("type")).as_deref() == Some("13") {
                out.push((name, e.path()));
            }
        }
        out.sort();
        out
    }

    fn esp_of(scsi_id: &str, dir: &Path) -> EspPath {
        EspPath {
            scsi_id: scsi_id.to_string(),
            sg_path: crate::scsi::sg_path_in(&dir.join("scsi_generic").to_string_lossy()),
            sas_address: read_trim(&dir.join("sas_address")).map(|s| normalize_sas(&s)),
            serial: std::fs::read(dir.join("vpd_pg80"))
                .ok()
                .and_then(|raw| crate::topology::parse_vpd80(&raw)),
        }
    }

    /// The /sys/class/enclosure id (e.g. "0:0:17:0") bound to this SES
    /// device, when the ses module is present.
    fn sysfs_enclosure_id(dir: &Path) -> Option<String> {
        let enc = dir.join("enclosure");
        for e in std::fs::read_dir(enc).ok()?.flatten() {
            return Some(e.file_name().to_string_lossy().to_string());
        }
        None
    }

    fn read_pages(sg: &str) -> Option<(Vec<u8>, Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>)> {
        let dev = Device::open(sg).ok()?;
        let cfg = dev.receive_diagnostic(PAGE_CONFIG).ok()?;
        let status = dev.receive_diagnostic(PAGE_STATUS).ok()?;
        let desc = dev.receive_diagnostic(PAGE_DESCRIPTORS).ok();
        let add = dev.receive_diagnostic(PAGE_ADDITIONAL).ok();
        Some((cfg, status, desc, add))
    }

    /// Read every shelf on the node. Two SES devices with the same
    /// logical id (dual IOM) become one report with two ESP paths.
    pub fn scan() -> BTreeMap<String, ShelfReport> {
        let mut out: BTreeMap<String, ShelfReport> = BTreeMap::new();
        for (scsi_id, dir) in enclosure_devices() {
            let esp = esp_of(&scsi_id, &dir);
            let Some(sg) = esp.sg_path.clone() else {
                tracing::debug!(%scsi_id, "enclosure device without sg node");
                continue;
            };
            let Some((cfg, status, desc, add)) = read_pages(&sg) else {
                tracing::debug!(%scsi_id, %sg, "SES pages unreadable");
                continue;
            };
            let sysfs_id = sysfs_enclosure_id(&dir);
            let Some(rep) = assemble(vec![esp.clone()], sysfs_id, &cfg, &status, desc.as_deref(), add.as_deref())
            else {
                continue;
            };
            match out.get_mut(&rep.key) {
                Some(existing) => {
                    existing.esps.push(esp);
                    // Merge slot addresses seen only through this IOM.
                    for (b, addrs) in rep.slots {
                        existing.slots.entry(b).or_insert(addrs);
                    }
                }
                None => {
                    out.insert(rep.key.clone(), rep);
                }
            }
        }
        out
    }

    /// Set IDENT on a bay (or the enclosure itself when `bay` is None)
    /// through the shelf's first reachable ESP.
    pub fn set_ident(rep: &ShelfReport, bay: Option<u32>, on: bool) -> std::io::Result<()> {
        let err = |m: String| std::io::Error::new(std::io::ErrorKind::Other, m);
        let (pos, et) = match bay {
            Some(b) => {
                let p = find_slot_element(&rep.elements, b)
                    .ok_or_else(|| err(format!("shelf {}: no slot element for bay {b}", rep.key)))?;
                (p, rep.elements[p].element_type)
            }
            None => {
                let p = find_enclosure_element(&rep.elements)
                    .ok_or_else(|| err(format!("shelf {}: no enclosure element", rep.key)))?;
                (p, ET_ENCLOSURE)
            }
        };
        let mut last = None;
        for esp in &rep.esps {
            let Some(sg) = &esp.sg_path else { continue };
            let dev = match Device::open(sg) {
                Ok(d) => d,
                Err(e) => {
                    last = Some(e.to_string());
                    continue;
                }
            };
            // Fresh status page: the generation code must match.
            let status = match dev.receive_diagnostic(PAGE_STATUS) {
                Ok(s) => s,
                Err(e) => {
                    last = Some(e.to_string());
                    continue;
                }
            };
            let Some(page) = build_ident_control(&status, element_offset(pos), et, on) else {
                last = Some("status page too short".into());
                continue;
            };
            match dev.send_diagnostic(&page) {
                Ok(()) => return Ok(()),
                Err(e) => last = Some(e.to_string()),
            }
        }
        Err(err(format!(
            "shelf {}: ident not set: {}",
            rep.key,
            last.unwrap_or_else(|| "no ESP path".into())
        )))
    }
}

/// Read every shelf on the node (empty on non-Linux).
pub fn scan() -> BTreeMap<String, ShelfReport> {
    #[cfg(target_os = "linux")]
    {
        linux::scan()
    }
    #[cfg(not(target_os = "linux"))]
    {
        BTreeMap::new()
    }
}

/// Locate LED via SES: a bay, or the shelf when `bay` is None.
pub fn set_ident(rep: &ShelfReport, bay: Option<u32>, on: bool) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::set_ident(rep, bay, on)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (rep, bay, on);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "SES control requires Linux",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small DS-like configuration page: one enclosure descriptor, three
    /// types — 2 array device slots, 1 temperature, 1 enclosure.
    fn config_page() -> Vec<u8> {
        let mut p = vec![PAGE_CONFIG, 0, 0, 0, 0, 0, 0, 7];
        // enclosure descriptor: 1 ESP, subenclosure 0, 3 types, len 36
        p.extend_from_slice(&[0x10, 0, 3, 36]);
        p.extend_from_slice(&[0x50, 0x0a, 0x09, 0x80, 0x0e, 0x35, 0x91, 0x35]);
        p.extend_from_slice(b"NETAPP  ");
        p.extend_from_slice(b"DS22412IOM12A   ");
        p.extend_from_slice(b"0401");
        // type headers
        p.extend_from_slice(&[ET_ARRAY_DEVICE_SLOT, 2, 0, 4]);
        p.extend_from_slice(&[ET_TEMPERATURE, 1, 0, 4]);
        p.extend_from_slice(&[ET_ENCLOSURE, 1, 0, 3]);
        p.extend_from_slice(b"Slot");
        p.extend_from_slice(b"Temp");
        p.extend_from_slice(b"Enc");
        let len = (p.len() - 4) as u16;
        p[2..4].copy_from_slice(&len.to_be_bytes());
        p
    }

    fn status_page() -> Vec<u8> {
        let mut p = vec![PAGE_STATUS, 0x04, 0, 0, 0, 0, 0, 7];
        // array device slots: overall, slot 0 ok+ident, slot 1 not installed
        p.extend_from_slice(&[0x01, 0, 0, 0]);
        p.extend_from_slice(&[0x01, 0, 0x02, 0]);
        p.extend_from_slice(&[0x05, 0, 0, 0]);
        // temperature: overall, one at 20+31 = 51 C with OT warning → noncrit
        p.extend_from_slice(&[0x01, 0, 0, 0]);
        p.extend_from_slice(&[0x03, 0, 51, 0x04]);
        // enclosure: overall, one
        p.extend_from_slice(&[0x01, 0, 0, 0]);
        p.extend_from_slice(&[0x01, 0x80, 0, 0]);
        let len = (p.len() - 4) as u16;
        p[2..4].copy_from_slice(&len.to_be_bytes());
        p
    }

    fn additional_page() -> Vec<u8> {
        let mut p = vec![PAGE_ADDITIONAL, 0, 0, 0, 0, 0, 0, 7];
        // one SAS descriptor, EIP, element index 0, slot number 5, one phy
        let mut d = vec![0x16, 0]; // proto 6 | EIP
        let mut body = vec![0u8, 0]; // EIIOE, element index 0
        body.extend_from_slice(&[1, 0, 0, 5]); // 1 phy, dtype 0, rsvd, slot 5
        let mut phy = vec![0u8; 28];
        phy[12..20].copy_from_slice(&[0x50, 0x00, 0xc5, 0x00, 0xb8, 0x53, 0x8d, 0x01]);
        body.extend_from_slice(&phy);
        d[1] = body.len() as u8;
        d.extend_from_slice(&body);
        p.extend_from_slice(&d);
        let len = (p.len() - 4) as u16;
        p[2..4].copy_from_slice(&len.to_be_bytes());
        p
    }

    #[test]
    fn configuration_parses_identity_and_types() {
        let cfg = parse_configuration(&config_page()).unwrap();
        assert_eq!(cfg.generation, 7);
        assert_eq!(cfg.logical_id.as_deref(), Some("500a09800e359135"));
        assert_eq!(cfg.vendor, "NETAPP");
        assert_eq!(cfg.product, "DS22412IOM12A");
        assert_eq!(cfg.revision, "0401");
        assert_eq!(cfg.types.len(), 3);
        assert_eq!(cfg.types[0].element_type, ET_ARRAY_DEVICE_SLOT);
        assert_eq!(cfg.types[0].count, 2);
        assert_eq!(cfg.types[0].text, "Slot");
        assert_eq!(cfg.types[2].text, "Enc");
        assert!(parse_configuration(&[PAGE_STATUS, 0, 0, 0]).is_none());
    }

    #[test]
    fn status_decodes_elements_by_type() {
        let cfg = parse_configuration(&config_page()).unwrap();
        let st = parse_status(&cfg, &status_page()).unwrap();
        assert!(st.noncritical && !st.critical);
        assert_eq!(st.elements.len(), 7);
        let slot0 = &st.elements[1];
        assert_eq!(slot0.element_type, ET_ARRAY_DEVICE_SLOT);
        assert!(!slot0.overall && slot0.ident && slot0.status == ElementStatus::Ok);
        assert_eq!(st.elements[2].status, ElementStatus::NotInstalled);
        let temp = &st.elements[4];
        assert_eq!(temp.temperature_c, Some(31));
        assert_eq!(temp.status, ElementStatus::Noncritical);
        assert!(temp.flags.contains(&"overtemp warning".to_string()));
        let enc = &st.elements[6];
        assert!(enc.ident);
    }

    #[test]
    fn cooling_and_psu_readings() {
        let fan = decode_element(ET_COOLING, 0, false, [0x01, 0x01, 0x90, 0x05]);
        assert_eq!(fan.rpm, Some(4000));
        assert!(!fan.fault);
        let psu = decode_element(ET_POWER_SUPPLY, 1, false, [0x02, 0, 0, 0x42]);
        assert_eq!(psu.status, ElementStatus::Critical);
        assert!(psu.fault);
        assert!(psu.flags.contains(&"ac fail".to_string()));
        let v = decode_element(ET_VOLTAGE, 0, false, [0x01, 0, 0x04, 0xb0]);
        assert_eq!(v.volts, Some(12.0));
        let c = decode_element(ET_CURRENT, 0, false, [0x01, 0, 0x00, 0x96]);
        assert_eq!(c.amps, Some(1.5));
    }

    #[test]
    fn additional_page_maps_sas_to_bay() {
        let slots = parse_additional(&additional_page());
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].bay, Some(5));
        assert_eq!(slots[0].element_index, Some(0));
        assert_eq!(slots[0].sas_addresses, vec!["5000c500b8538d01".to_string()]);
    }

    #[test]
    fn assemble_builds_report_with_slot_addresses() {
        let esp = EspPath {
            scsi_id: "0:0:17:0".into(),
            sg_path: Some("/dev/sg17".into()),
            sas_address: Some("500a09800853bf4c".into()),
            serial: Some("IOMSERIAL".into()),
        };
        let rep = assemble(
            vec![esp],
            None,
            &config_page(),
            &status_page(),
            None,
            Some(&additional_page()),
        )
        .unwrap();
        assert_eq!(rep.key, "500a09800e359135", "logical id is the key");
        assert_eq!(rep.shelf.model.as_deref(), Some("DS22412IOM12A"));
        assert_eq!(rep.shelf.serial.as_deref(), Some("IOMSERIAL"));
        assert_eq!(rep.shelf.key(), Some("500a09800e359135".into()));
        assert_eq!(rep.bay_of("0x5000C500B8538D01"), Some(5));
        assert_eq!(rep.elements[1].bay, Some(5), "page 0x0A renumbered the slot");
        assert_eq!(rep.elements[2].bay, Some(1), "unaddressed slot keeps its index");
        assert_eq!(rep.worst(), ElementStatus::Noncritical);
        assert_eq!(rep.max_temperature_c(), Some(31));
        assert_eq!(rep.count(ET_ARRAY_DEVICE_SLOT), (1, 1), "not-installed slot not counted");
    }

    #[test]
    fn descriptors_line_up_with_elements() {
        let cfg = parse_configuration(&config_page()).unwrap();
        let mut d = vec![PAGE_DESCRIPTORS, 0, 0, 0, 0, 0, 0, 7];
        for name in ["", "Bay 0", "Bay 1", "", "Ambient", "", "Shelf"] {
            d.extend_from_slice(&[0, 0]);
            d.extend_from_slice(&(name.len() as u16).to_be_bytes());
            d.extend_from_slice(name.as_bytes());
        }
        let names = parse_descriptors(&cfg, &d);
        assert_eq!(names.len(), 7);
        assert_eq!(names[1].as_deref(), Some("Bay 0"));
        assert_eq!(names[4].as_deref(), Some("Ambient"));
        assert!(names[0].is_none());
    }

    #[test]
    fn ident_control_selects_only_the_target() {
        let st = status_page();
        // slot 1 (individual, page position 2) on
        let page = build_ident_control(&st, element_offset(2), ET_ARRAY_DEVICE_SLOT, true).unwrap();
        assert_eq!(page[0], PAGE_STATUS);
        assert_eq!(&page[4..8], &st[4..8], "generation preserved");
        assert_eq!(page[8], 0, "overall slot element not selected");
        assert_eq!(page[12], 0, "slot 0 not selected");
        assert_eq!(page[16], 0x80, "slot 1 selected");
        assert_eq!(page[18], 0x02, "RQST IDENT");
        assert_eq!(page[19], 0, "no RQST FAULT smuggled");
        // enclosure element off
        let page = build_ident_control(&st, element_offset(6), ET_ENCLOSURE, false).unwrap();
        assert_eq!(page[32], 0x80);
        assert_eq!(page[33], 0);
        assert!(build_ident_control(&st, 400, ET_ENCLOSURE, true).is_none());
    }

    #[test]
    fn slot_and_enclosure_lookup() {
        let cfg = parse_configuration(&config_page()).unwrap();
        let mut st = parse_status(&cfg, &status_page()).unwrap();
        for e in st.elements.iter_mut() {
            if e.element_type == ET_ARRAY_DEVICE_SLOT && !e.overall {
                e.bay = Some(e.index + 10);
            }
        }
        assert_eq!(find_slot_element(&st.elements, 11), Some(2));
        assert_eq!(find_slot_element(&st.elements, 3), None);
        assert_eq!(find_enclosure_element(&st.elements), Some(6));
    }

    #[test]
    fn sas_normalization() {
        assert_eq!(normalize_sas("0x5000C500B8538D01"), "5000c500b8538d01");
        assert_eq!(normalize_sas(" 5000c500b8538d01\n"), "5000c500b8538d01");
    }
}
