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
//! - 0x00 Supported Diagnostic Pages: what this ESP answers.
//! - 0x03 Help Text: the enclosure's own words on its state.
//! - 0x05 Threshold In: warning/critical limits per sensor element.
//! - 0x07 Element Descriptor: a text name per element. NetApp shelves put
//!   `KEY=VALUE;` lists here (serials, firmware, part numbers, attached
//!   SAS addresses), kept as [`Element::attributes`].
//! - 0x0A Additional Element Status: per-slot SAS addresses — how a drive
//!   is tied to a bay when the kernel's `ses` module is not around.
//!
//! Every command, and the DS224C's element map, is in docs/netapp-shelf.md.
//!
//! Parsers are portable and tested on synthetic pages; enumeration and
//! I/O are Linux-only.

use crate::drive::Shelf;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::SystemTime;

pub const PAGE_SUPPORTED: u8 = 0x00;
pub const PAGE_CONFIG: u8 = 0x01;
pub const PAGE_STATUS: u8 = 0x02;
pub const PAGE_HELP: u8 = 0x03;
pub const PAGE_THRESHOLD: u8 = 0x05;
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
// NetApp vendor element types, named from what their descriptors carry on
// a DS224C/IOM12 (docs/netapp-shelf.md): the IOM's expander (`SA=` its SAS
// address, `FPI=` the FRU) and the IOM's Ethernet port (`OM=` its MAC).
pub const ET_NETAPP_IOM_EXPANDER: u8 = 0x83;
pub const ET_NETAPP_IOM_ETHERNET: u8 = 0x85;

/// A vendor's own element types, by the enclosure's INQUIRY vendor.
pub fn vendor_type_name(vendor: &str, t: u8) -> Option<&'static str> {
    match (vendor, t) {
        ("NETAPP", ET_NETAPP_IOM_EXPANDER) => Some("iom expander"),
        ("NETAPP", ET_NETAPP_IOM_ETHERNET) => Some("iom ethernet"),
        _ => None,
    }
}

/// SAS connector element byte 1 bits 6:0 (SES-3 table 129).
pub fn connector_type_name(code: u8) -> Option<&'static str> {
    Some(match code & 0x7f {
        0x00 => return None,
        0x01 => "SAS 4x receptacle (SFF-8470)",
        0x02 => "Mini SAS 4x receptacle (SFF-8088)",
        0x03 => "QSFP+ receptacle (SFF-8436)",
        0x04 => "Mini SAS 4x active receptacle (SFF-8088)",
        0x05 => "Mini SAS HD 4x receptacle (SFF-8644)",
        0x06 => "Mini SAS HD 8x receptacle (SFF-8644)",
        0x07 => "Mini SAS HD 16x receptacle (SFF-8644)",
        0x0f => "vendor specific external",
        0x10 => "SAS 4i plug (SFF-8484)",
        0x11 => "Mini SAS 4i receptacle (SFF-8087)",
        0x12 => "Mini SAS HD 4i receptacle (SFF-8643)",
        0x13 => "Mini SAS HD 8i receptacle (SFF-8643)",
        0x20 => "SAS drive backplane receptacle (SFF-8482)",
        0x21 => "SATA host plug",
        0x22 => "SAS drive plug (SFF-8482)",
        0x23 => "SATA device plug",
        0x24 => "Micro SAS receptacle",
        0x25 => "Micro SATA device plug",
        0x26 => "Micro SAS plug",
        0x27 => "Micro SAS/SATA plug",
        0x28 => "12 Gb/s SAS drive backplane receptacle (SFF-8680)",
        0x29 => "12 Gb/s SAS drive plug (SFF-8680)",
        0x2a => "Multifunction 12 Gb/s 6x unshielded receptacle (SFF-8639)",
        0x2b => "Multifunction 12 Gb/s 6x unshielded plug (SFF-8639)",
        0x2f => "SAS virtual connector",
        0x3f => "vendor specific internal",
        _ => "other",
    })
}

/// A NetApp descriptor `TP=7D;SN=PSQ0942…;FW=0111;PW=913 ;` as pairs:
/// keys as given, values trimmed, empty values dropped. Not a `K=V;` list
/// (no `=`, or no `;`) → empty.
pub fn parse_attributes(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if !text.contains('=') || !text.contains(';') {
        return out;
    }
    for part in text.split(';') {
        let Some((k, v)) = part.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        if !k.is_empty() && !v.is_empty() {
            out.insert(k.to_string(), v.to_string());
        }
    }
    out
}

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
    /// The descriptor (page 0x07) as `KEY=VALUE` pairs, when it is a list.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, String>,
    /// Page 0x05 limits, for temperature/voltage/current sensors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thresholds: Option<Thresholds>,
    pub raw: [u8; 4],
}

/// Threshold In (page 0x05) for one sensor: °C for temperature, percent of
/// nominal for voltage and current (SES-3: 0.5 % units). None = not set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Thresholds {
    pub unit: String,
    pub high_critical: Option<f32>,
    pub high_warning: Option<f32>,
    pub low_warning: Option<f32>,
    pub low_critical: Option<f32>,
}

/// One element's four threshold bytes → [`Thresholds`] (sensors only).
pub fn decode_thresholds(element_type: u8, b: [u8; 4]) -> Option<Thresholds> {
    let (unit, f): (&str, fn(u8) -> Option<f32>) = match element_type {
        ET_TEMPERATURE => ("°C", |v| (v != 0).then_some(v as f32 - 20.0)),
        ET_VOLTAGE | ET_CURRENT => ("% of nominal", |v| (v != 0).then_some(v as f32 / 2.0)),
        _ => return None,
    };
    if b == [0; 4] {
        return None;
    }
    Some(Thresholds {
        unit: unit.into(),
        high_critical: f(b[0]),
        high_warning: f(b[1]),
        low_warning: f(b[2]),
        low_critical: f(b[3]),
    })
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
        attributes: BTreeMap::new(),
        thresholds: None,
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
            if b[2] & 0x08 != 0 {
                e.flags.push("ready to insert".into());
            }
            if b[2] & 0x04 != 0 {
                e.flags.push("remove requested".into());
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

/// Page 0x03: the enclosure's help text (empty → None).
pub fn parse_help_text(raw: &[u8]) -> Option<String> {
    if raw.len() < 4 || raw[0] != PAGE_HELP {
        return None;
    }
    let len = u16::from_be_bytes([raw[2], raw[3]]) as usize;
    let end = (4 + len).min(raw.len());
    let t = String::from_utf8_lossy(&raw[4..end])
        .trim_matches(|c: char| c.is_whitespace() || c == '\0')
        .to_string();
    (!t.is_empty()).then_some(t)
}

/// Page 0x05: four threshold bytes per element, page order (overall
/// elements included, as in page 0x02).
pub fn parse_thresholds(cfg: &Configuration, raw: &[u8]) -> Vec<[u8; 4]> {
    let mut out = Vec::new();
    if raw.len() < 8 || raw[0] != PAGE_THRESHOLD {
        return out;
    }
    let len = u16::from_be_bytes([raw[2], raw[3]]) as usize + 4;
    let end = len.min(raw.len());
    let total: usize = cfg.types.iter().map(|t| t.count as usize + 1).sum();
    let mut off = 8;
    while out.len() < total && off + 4 <= end {
        out.push([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]);
        off += 4;
    }
    out
}

/// Page 0x00: the diagnostic pages this ESP answers.
pub fn parse_supported_pages(raw: &[u8]) -> Vec<u8> {
    if raw.len() < 4 || raw[0] != PAGE_SUPPORTED {
        return Vec::new();
    }
    let len = u16::from_be_bytes([raw[2], raw[3]]) as usize;
    raw[4..(4 + len).min(raw.len())].to_vec()
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

/// Like [`build_ident_control`], for a slot's FAULT indicator (#44): RQST
/// FAULT (byte 3 bit 5) set or cleared, its IDENT kept as the status page
/// shows it. None for anything but a (array) device slot.
pub fn build_fault_control(status_raw: &[u8], element_offset: usize, element_type: u8, on: bool) -> Option<Vec<u8>> {
    if !matches!(element_type, ET_DEVICE_SLOT | ET_ARRAY_DEVICE_SLOT) {
        return None;
    }
    let mut page = build_ident_control(status_raw, element_offset, element_type, false)?;
    let st = &status_raw[element_offset..element_offset + 4];
    let ctl = &mut page[element_offset..element_offset + 4];
    ctl[2] = st[2] & 0x02;
    ctl[3] = if on { 0x20 } else { 0 };
    Some(page)
}

/// A slot's power: DEVICE OFF (control byte 3 bit 4) set to turn the
/// drive in the bay off, cleared to turn it back on. The slot's IDENT and
/// RQST FAULT are kept as the status page shows them. Only for a (array)
/// device slot; whether the IOM honours it is the IOM's business (read the
/// slot's "device off" flag back).
pub fn build_power_control(status_raw: &[u8], element_offset: usize, element_type: u8, off: bool) -> Option<Vec<u8>> {
    if !matches!(element_type, ET_DEVICE_SLOT | ET_ARRAY_DEVICE_SLOT) {
        return None;
    }
    let mut page = build_ident_control(status_raw, element_offset, element_type, false)?;
    let st = &status_raw[element_offset..element_offset + 4];
    let ctl = &mut page[element_offset..element_offset + 4];
    ctl[2] = st[2] & 0x02;
    ctl[3] = (st[3] & 0x20) | if off { 0x10 } else { 0 };
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
    /// The SES device's INQUIRY revision (sysfs `rev`): the IOM's firmware
    /// version (#35).
    #[serde(default)]
    pub revision: Option<String>,
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
    /// Page 0x03, when the enclosure says anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help_text: Option<String>,
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
            .find(|(_, addrs)| addrs.contains(&want))
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

/// The raw pages one ESP answered. Configuration and status are needed;
/// the rest are optional.
#[derive(Debug, Clone, Copy, Default)]
pub struct RawPages<'a> {
    pub config: &'a [u8],
    pub status: &'a [u8],
    pub descriptors: Option<&'a [u8]>,
    pub additional: Option<&'a [u8]>,
    pub help: Option<&'a [u8]>,
    pub thresholds: Option<&'a [u8]>,
}

/// Assemble a report from raw pages (portable, so it can be tested).
pub fn assemble(esps: Vec<EspPath>, sysfs_id: Option<String>, pages: RawPages<'_>) -> Option<ShelfReport> {
    let status_raw = pages.status;
    let cfg = parse_configuration(pages.config)?;
    let mut status = parse_status(&cfg, status_raw)?;
    if let Some(d) = pages.descriptors {
        let names = parse_descriptors(&cfg, d);
        for (e, n) in status.elements.iter_mut().zip(names) {
            if let Some(n) = &n {
                e.attributes = parse_attributes(n);
            }
            e.name = n;
        }
    }
    if let Some(t) = pages.thresholds {
        for (e, b) in status.elements.iter_mut().zip(parse_thresholds(&cfg, t)) {
            if !e.overall {
                e.thresholds = decode_thresholds(e.element_type, b);
            }
        }
    }
    for e in status.elements.iter_mut() {
        if let Some(n) = vendor_type_name(&cfg.vendor, e.element_type) {
            e.type_name = n.into();
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
    if let Some(a) = pages.additional {
        let addrs = parse_additional(a);
        // Individual slot elements in page order, to map ELEMENT INDEX.
        let slot_positions: Vec<usize> = status
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
    }
    let first = esps.first();
    let key = cfg
        .logical_id
        .clone()
        .or_else(|| first.and_then(|e| e.serial.clone()))
        .or_else(|| first.map(|e| e.scsi_id.clone()))?;
    // The enclosure element's descriptor names the chassis itself (NetApp:
    // `SN=SHFGB…`); the ESP's VPD serial is its IOM's, so it comes second.
    let chassis_serial = status
        .elements
        .iter()
        .find(|e| e.element_type == ET_ENCLOSURE && !e.overall)
        .and_then(|e| e.attributes.get("SN").cloned());
    let shelf = Shelf {
        id: sysfs_id.or_else(|| first.map(|e| e.scsi_id.clone())),
        vendor: (!cfg.vendor.is_empty()).then_some(cfg.vendor.clone()),
        model: (!cfg.product.is_empty()).then_some(cfg.product.clone()),
        serial: chassis_serial.or_else(|| first.and_then(|e| e.serial.clone())),
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
        help_text: pages.help.and_then(parse_help_text),
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
            revision: read_trim(&dir.join("rev")),
        }
    }

    /// The /sys/class/enclosure id (e.g. "0:0:17:0") bound to this SES
    /// device, when the ses module is present.
    fn sysfs_enclosure_id(dir: &Path) -> Option<String> {
        std::fs::read_dir(dir.join("enclosure"))
            .ok()?
            .flatten()
            .next()
            .map(|e| e.file_name().to_string_lossy().to_string())
    }

    /// The pages one ESP answered (owned; see [`RawPages`]).
    struct Pages {
        config: Vec<u8>,
        status: Vec<u8>,
        descriptors: Option<Vec<u8>>,
        additional: Option<Vec<u8>>,
        help: Option<Vec<u8>>,
        thresholds: Option<Vec<u8>>,
    }

    impl Pages {
        fn raw(&self) -> RawPages<'_> {
            RawPages {
                config: &self.config,
                status: &self.status,
                descriptors: self.descriptors.as_deref(),
                additional: self.additional.as_deref(),
                help: self.help.as_deref(),
                thresholds: self.thresholds.as_deref(),
            }
        }
    }

    fn read_pages(sg: &str) -> Option<Pages> {
        let dev = Device::open(sg).ok()?;
        let config = dev.receive_diagnostic(PAGE_CONFIG).ok()?;
        let status = dev.receive_diagnostic(PAGE_STATUS).ok()?;
        // Optional pages: only those page 0x00 lists (an ESP may answer an
        // unlisted page with an error, or with garbage).
        let supported = dev.receive_diagnostic(PAGE_SUPPORTED).ok().map(|r| parse_supported_pages(&r));
        let has = |p: u8| supported.as_ref().map_or(true, |s| s.contains(&p));
        let opt = |p: u8| if has(p) { dev.receive_diagnostic(p).ok() } else { None };
        Some(Pages {
            descriptors: opt(PAGE_DESCRIPTORS),
            additional: opt(PAGE_ADDITIONAL),
            help: opt(PAGE_HELP),
            thresholds: opt(PAGE_THRESHOLD),
            config,
            status,
        })
    }

    /// One diagnostic page, raw, through the shelf's ESP `scsi_id` (or the
    /// first that answers): RECEIVE DIAGNOSTIC RESULTS only, so a read.
    pub fn read_page(rep: &ShelfReport, esp: Option<&str>, page: u8) -> std::io::Result<(String, Vec<u8>)> {
        let mut last = None;
        for e in rep.esps.iter().filter(|e| esp.map_or(true, |w| w == e.scsi_id)) {
            let Some(sg) = &e.sg_path else { continue };
            match Device::open(sg).and_then(|d| d.receive_diagnostic(page)) {
                Ok(raw) => return Ok((e.scsi_id.clone(), raw)),
                Err(err) => last = Some(err.to_string()),
            }
        }
        Err(std::io::Error::other(format!(
            "shelf {}: page 0x{page:02x} not read: {}",
            rep.key,
            last.unwrap_or_else(|| "no ESP path".into())
        )))
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
            let Some(pages) = read_pages(&sg) else {
                tracing::debug!(%scsi_id, %sg, "SES pages unreadable");
                continue;
            };
            let sysfs_id = sysfs_enclosure_id(&dir);
            let Some(rep) = assemble(vec![esp.clone()], sysfs_id, pages.raw()) else {
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

    /// Set or clear a bay's FAULT indicator through the shelf's first
    /// reachable ESP (#44).
    pub fn set_fault(rep: &ShelfReport, bay: u32, on: bool) -> std::io::Result<()> {
        let err = std::io::Error::other;
        let pos = find_slot_element(&rep.elements, bay).ok_or_else(|| err(format!("shelf {}: no slot element for bay {bay}", rep.key)))?;
        let et = rep.elements[pos].element_type;
        let mut last = None;
        for esp in &rep.esps {
            let Some(sg) = &esp.sg_path else { continue };
            let r = Device::open(sg).and_then(|dev| {
                let status = dev.receive_diagnostic(PAGE_STATUS)?;
                let page = build_fault_control(&status, element_offset(pos), et, on)
                    .ok_or(crate::scsi::Error::Unsupported("status page too short"))?;
                dev.send_diagnostic(&page)
            });
            match r {
                Ok(()) => return Ok(()),
                Err(e) => last = Some(e.to_string()),
            }
        }
        Err(err(format!("shelf {}: fault LED not set: {}", rep.key, last.unwrap_or_else(|| "no ESP path".into()))))
    }

    /// Turn the drive in a bay off (DEVICE OFF) or back on, through the
    /// shelf's first reachable ESP.
    pub fn set_power(rep: &ShelfReport, bay: u32, off: bool) -> std::io::Result<()> {
        let err = std::io::Error::other;
        let pos = find_slot_element(&rep.elements, bay).ok_or_else(|| err(format!("shelf {}: no slot element for bay {bay}", rep.key)))?;
        let et = rep.elements[pos].element_type;
        let mut last = None;
        for esp in &rep.esps {
            let Some(sg) = &esp.sg_path else { continue };
            let r = Device::open(sg).and_then(|dev| {
                let status = dev.receive_diagnostic(PAGE_STATUS)?;
                let page = build_power_control(&status, element_offset(pos), et, off)
                    .ok_or(crate::scsi::Error::Unsupported("status page too short"))?;
                dev.send_diagnostic(&page)
            });
            match r {
                Ok(()) => return Ok(()),
                Err(e) => last = Some(e.to_string()),
            }
        }
        Err(err(format!("shelf {}: bay {bay} power not set: {}", rep.key, last.unwrap_or_else(|| "no ESP path".into()))))
    }

    /// Set IDENT on a bay (or the enclosure itself when `bay` is None)
    /// through the shelf's first reachable ESP.
    pub fn set_ident(rep: &ShelfReport, bay: Option<u32>, on: bool) -> std::io::Result<()> {
        let err = std::io::Error::other;
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

/// Every SCSI enclosure device: (H:C:T:L, sysfs dir) — Linux only.
#[cfg(target_os = "linux")]
pub use linux::enclosure_devices;

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

/// A bay's power via SES DEVICE OFF (#81).
pub fn set_power(rep: &ShelfReport, bay: u32, off: bool) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::set_power(rep, bay, off)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (rep, bay, off);
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "SES control requires Linux"))
    }
}

/// One raw diagnostic page through a shelf ESP: (ESP SCSI id, bytes).
pub fn read_page(rep: &ShelfReport, esp: Option<&str>, page: u8) -> std::io::Result<(String, Vec<u8>)> {
    #[cfg(target_os = "linux")]
    {
        linux::read_page(rep, esp, page)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (rep, esp, page);
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "SES requires Linux"))
    }
}

/// A bay's fault LED via SES (#44).
pub fn set_fault(rep: &ShelfReport, bay: u32, on: bool) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::set_fault(rep, bay, on)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (rep, bay, on);
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "SES control requires Linux"))
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
            revision: Some("0300".into()),
        };
        let (cfg, st, add) = (config_page(), status_page(), additional_page());
        let rep = assemble(
            vec![esp],
            None,
            RawPages { config: &cfg, status: &st, additional: Some(&add), ..Default::default() },
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
    fn fault_control_sets_rqst_fault_and_keeps_ident() {
        let mut st = status_page();
        // slot 1 shows IDENT on (byte 2 bit 1) in the status page.
        st[element_offset(2) + 2] |= 0x02;
        let page = build_fault_control(&st, element_offset(2), ET_ARRAY_DEVICE_SLOT, true).unwrap();
        assert_eq!(&page[4..8], &st[4..8], "generation preserved");
        assert_eq!(page[16], 0x80, "slot 1 selected");
        assert_eq!(page[18], 0x02, "its IDENT kept");
        assert_eq!(page[19], 0x20, "RQST FAULT");
        assert_eq!(page[12], 0, "slot 0 untouched");
        let off = build_fault_control(&st, element_offset(2), ET_ARRAY_DEVICE_SLOT, false).unwrap();
        assert_eq!(off[19], 0, "fault cleared");
        assert!(build_fault_control(&st, element_offset(6), ET_ENCLOSURE, true).is_none(), "only a slot has a fault LED here");
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
    fn power_control_sets_device_off_and_keeps_ident_and_fault() {
        let mut st = status_page();
        st[element_offset(2) + 2] |= 0x02; // IDENT on
        st[element_offset(2) + 3] |= 0x20; // FAULT REQSTD
        let page = build_power_control(&st, element_offset(2), ET_ARRAY_DEVICE_SLOT, true).unwrap();
        assert_eq!(&page[4..8], &st[4..8], "generation preserved");
        assert_eq!(page[16], 0x80, "slot 1 selected");
        assert_eq!(page[18], 0x02, "IDENT kept");
        assert_eq!(page[19], 0x30, "RQST FAULT kept + DEVICE OFF");
        assert_eq!(page[12], 0, "slot 0 untouched");
        let on = build_power_control(&st, element_offset(2), ET_ARRAY_DEVICE_SLOT, false).unwrap();
        assert_eq!(on[19], 0x20, "DEVICE OFF cleared");
        assert!(build_power_control(&st, element_offset(6), ET_ENCLOSURE, true).is_none());
    }

    #[test]
    fn help_thresholds_and_supported_pages() {
        let mut h = vec![PAGE_HELP, 0, 0, 9];
        h.extend_from_slice(b" all ok \0");
        assert_eq!(parse_help_text(&h).as_deref(), Some("all ok"));
        assert_eq!(parse_help_text(&[PAGE_HELP, 0, 0, 0]), None);

        let cfg = parse_configuration(&config_page()).unwrap();
        // slots: overall + 2; temperature: overall + 1; enclosure: overall + 1
        let mut t = vec![PAGE_THRESHOLD, 0, 0, 0, 0, 0, 0, 7];
        for b in [[0u8; 4], [0; 4], [0; 4], [0; 4], [80, 70, 25, 20], [0; 4], [0; 4]] {
            t.extend_from_slice(&b);
        }
        let l = (t.len() - 4) as u16;
        t[2..4].copy_from_slice(&l.to_be_bytes());
        let thr = parse_thresholds(&cfg, &t);
        assert_eq!(thr.len(), 7);
        let d = decode_thresholds(ET_TEMPERATURE, thr[4]).unwrap();
        assert_eq!((d.high_critical, d.high_warning, d.low_warning, d.low_critical), (Some(60.0), Some(50.0), Some(5.0), Some(0.0)));
        let v = decode_thresholds(ET_VOLTAGE, [20, 10, 10, 0]).unwrap();
        assert_eq!((v.high_critical, v.low_critical), (Some(10.0), None), "0.5 % units; 0 = unset");
        assert!(decode_thresholds(ET_DEVICE_SLOT, [1, 2, 3, 4]).is_none());
        assert!(decode_thresholds(ET_TEMPERATURE, [0; 4]).is_none());

        assert_eq!(parse_supported_pages(&[0, 0, 0, 4, 0, 1, 2, 0x0a]), vec![0, 1, 2, 0x0a]);
        assert!(parse_supported_pages(&[1, 0, 0, 1, 0]).is_empty());
    }

    #[test]
    fn connector_types() {
        assert_eq!(connector_type_name(0x05), Some("Mini SAS HD 4x receptacle (SFF-8644)"));
        assert_eq!(connector_type_name(0x80), None, "bit 7 is IDENT");
        assert_eq!(vendor_type_name("NETAPP", 0x83), Some("iom expander"));
        assert_eq!(vendor_type_name("DELL", 0x83), None);
    }

    #[test]
    fn sas_normalization() {
        assert_eq!(normalize_sas("0x5000C500B8538D01"), "5000c500b8538d01");
        assert_eq!(normalize_sas(" 5000c500b8538d01\n"), "5000c500b8538d01");
    }
}
