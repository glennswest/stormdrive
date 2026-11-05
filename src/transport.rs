//! What a SCSI disk is attached by, taken from the drive itself (#105; owner:
//! "why are they shown as sata, those are sas12 drives").
//!
//! `megaraid_sas` puts its disks on its own SCSI host, with no SAS transport in
//! sysfs and so no `sas_address`. The drive still says what it is: vendor `ATA`
//! is the SAT layer, so SATA; otherwise a SAS drive names its target port with
//! protocol 6 in VPD 0x83, and its SAS port log page (0x18) carries the same
//! protocol and the rate each phy negotiated.

use crate::scsi::{Device, Dir};
use crate::smart::scsi::{log_params, log_sense_cdb};

/// SPC's protocol identifier for SAS.
const PROTOCOL_SAS: u8 = 6;
/// Protocol-Specific Port log page.
const LOG_SAS_PORT: u8 = 0x18;

/// What [`probe`] learned, cached with the rest of discovery's probe.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Link {
    /// The drive itself says SAS, over SG (a kernel with no `vpd_pg83`, or
    /// only the port log page).
    pub sas: bool,
    /// The fastest rate one of its phys negotiated, in Mb/s.
    pub rate_mbps: Option<u32>,
}

/// VPD 0x83: does a designator name a SAS port? The protocol identifier counts
/// only with PIV set, on a target port or target device association.
pub fn vpd83_says_sas(raw: &[u8]) -> bool {
    if raw.len() < 4 || raw[1] != 0x83 {
        return false;
    }
    let end = (4 + u16::from_be_bytes([raw[2], raw[3]]) as usize).min(raw.len());
    let mut off = 4;
    while off + 4 <= end {
        let (b0, b1, len) = (raw[off], raw[off + 1], raw[off + 3] as usize);
        let association = (b1 >> 4) & 0x03;
        if b0 >> 4 == PROTOCOL_SAS && b1 & 0x80 != 0 && matches!(association, 1 | 2) {
            return true;
        }
        off += 4 + len;
    }
    false
}

/// SPL's negotiated logical link rate, in Mb/s. Anything else is a phy with no
/// link: disabled, in reset, or nothing on it.
fn rate_mbps(code: u8) -> Option<u32> {
    match code {
        0x8 => Some(1_500),
        0x9 => Some(3_000),
        0xa => Some(6_000),
        0xb => Some(12_000),
        0xc => Some(22_500),
        _ => None,
    }
}

/// Log page 0x18, one parameter per port: is any port SAS, and the fastest
/// rate on any phy. A dual-ported drive usually has a link on one.
pub fn parse_sas_port_log(raw: &[u8]) -> Link {
    let mut link = Link::default();
    for (_, p) in log_params(raw, LOG_SAS_PORT) {
        if p.first().map(|b| b & 0x0f) != Some(PROTOCOL_SAS) {
            continue;
        }
        link.sas = true;
        let phys = p.get(3).copied().unwrap_or(0);
        let mut off = 4;
        for _ in 0..phys {
            let Some(d) = p.get(off..) else { break };
            // A descriptor is its length byte + 4, and 48 in the older form.
            let len = d.get(3).map_or(48, |l| if *l < 44 { 48 } else { *l as usize + 4 });
            if let Some(r) = d.get(5).and_then(|b| rate_mbps(b & 0x0f)) {
                link.rate_mbps = link.rate_mbps.max(Some(r));
            }
            off += len;
        }
    }
    link
}

/// Is this disk SAS? Vendor `ATA` is SATA, whatever else is there: a SATA drive
/// behind a SAS HBA has a `sas_address` too. Otherwise any one of the drive's
/// own answers, or the transport's `sas_address`, says SAS.
pub fn is_sas(vendor: Option<&str>, vpd83: Option<&[u8]>, has_sas_address: bool, probed: Link) -> bool {
    vendor != Some("ATA") && (vpd83.is_some_and(vpd83_says_sas) || has_sas_address || probed.sas)
}

/// Ask the drive over SG. The port log page is read only when the drive lists
/// it, so a disk without one is not sent a command it will refuse.
pub fn probe(dev: &Device) -> Link {
    let log = |page: u8| {
        let mut buf = vec![0u8; 1024];
        let n = dev.io(&log_sense_cdb(page, 1024), Dir::FromDevice, &mut buf, 10_000).ok()?;
        buf.truncate(n);
        Some(buf)
    };
    let mut link = Link::default();
    let listed = log(0x00).is_some_and(|raw| {
        let end = raw.len().min(4 + raw.get(2..4).map_or(0, |l| u16::from_be_bytes([l[0], l[1]]) as usize));
        raw.get(4..end).is_some_and(|pages| pages.contains(&LOG_SAS_PORT))
    });
    if listed {
        if let Some(raw) = log(LOG_SAS_PORT) {
            link = parse_sas_port_log(&raw);
        }
    }
    link.sas = link.sas || dev.vpd(0x83).is_ok_and(|r| vpd83_says_sas(&r));
    link
}

/// "12 Gb/s", "1.5 Gb/s".
pub fn rate_label(mbps: u32) -> String {
    format!("{} Gb/s", f64::from(mbps) / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A SAS drive's page: NAA of the logical unit, then its target port
    /// (`61 93`: protocol 6, binary; PIV, target port, NAA).
    fn sas_vpd83() -> Vec<u8> {
        let mut raw = vec![0x00, 0x83, 0x00, 0x18];
        raw.extend_from_slice(&[0x01, 0x03, 0x00, 0x08, 0x50, 0, 0x0c, 0xca, 1, 2, 3, 4]);
        raw.extend_from_slice(&[0x61, 0x93, 0x00, 0x08, 0x50, 0, 0x0c, 0xca, 1, 2, 3, 5]);
        raw
    }

    #[test]
    fn vpd83_names_a_sas_port() {
        assert!(vpd83_says_sas(&sas_vpd83()));
        // A logical-unit designator alone, as SAT gives: not a port.
        let lu = [0x00, 0x83, 0x00, 0x0c, 0x01, 0x03, 0x00, 0x08, 0x50, 0, 0, 0, 0, 0, 0, 1];
        assert!(!vpd83_says_sas(&lu));
        // Protocol 6 without PIV is not valid; on the logical unit it is not a port.
        let mut no_piv = sas_vpd83();
        no_piv[17] = 0x13;
        assert!(!vpd83_says_sas(&no_piv));
        let mut on_lu = sas_vpd83();
        on_lu[17] = 0x83;
        assert!(!vpd83_says_sas(&on_lu));
        // Fibre Channel target port.
        let mut fc = sas_vpd83();
        fc[16] = 0x01;
        assert!(!vpd83_says_sas(&fc));
        assert!(!vpd83_says_sas(&[0x00, 0x80, 0x00, 0x00]), "another page");
        assert!(!vpd83_says_sas(&[]));
    }

    /// One port; `rates` are its phys' negotiated rate codes.
    fn port_param(code: u16, protocol: u8, rates: &[u8]) -> Vec<u8> {
        let mut d = vec![protocol, 0, 1, rates.len() as u8];
        for (i, r) in rates.iter().enumerate() {
            let mut phy = vec![0u8; 48];
            phy[1] = i as u8;
            phy[3] = 44;
            phy[4] = 0x10; // attached: an end device
            phy[5] = 0xf0 | r; // the reason nibble must not count
            d.extend_from_slice(&phy);
        }
        let mut p = code.to_be_bytes().to_vec();
        p.extend_from_slice(&[0x00, d.len() as u8]);
        p.extend_from_slice(&d);
        p
    }

    fn page(params: &[Vec<u8>]) -> Vec<u8> {
        let body = params.concat();
        let mut raw = vec![LOG_SAS_PORT, 0];
        raw.extend_from_slice(&(body.len() as u16).to_be_bytes());
        raw.extend_from_slice(&body);
        raw
    }

    #[test]
    fn the_port_log_page_gives_the_protocol_and_the_rate() {
        // forge1's SAS-12 drive, dual ported: a link on one port only.
        let l = parse_sas_port_log(&page(&[port_param(1, 6, &[0xb]), port_param(2, 6, &[0x0])]));
        assert_eq!(l, Link { sas: true, rate_mbps: Some(12_000) });
        let l = parse_sas_port_log(&page(&[port_param(1, 6, &[0x9, 0xa])]));
        assert_eq!(l.rate_mbps, Some(6_000), "the fastest phy");
        assert_eq!(parse_sas_port_log(&page(&[port_param(1, 6, &[0x1])])), Link { sas: true, rate_mbps: None }, "a disabled phy has no rate");
        assert_eq!(parse_sas_port_log(&page(&[port_param(1, 0, &[0xb])])), Link::default(), "not SAS");
        assert_eq!(parse_sas_port_log(&[0x2f, 0, 0, 0]), Link::default(), "another page");
    }

    #[test]
    fn who_is_sas() {
        let sas = sas_vpd83();
        let none = Link::default();
        let port = Link { sas: true, rate_mbps: None };
        // forge1: no sas_address, the drive says so.
        assert!(is_sas(Some("HGST"), Some(&sas), false, none));
        assert!(is_sas(Some("HGST"), None, false, port), "the log page or SG alone");
        // sas_address is enough where it is there, never needed.
        assert!(is_sas(Some("SEAGATE"), None, true, none));
        // SAT wins over everything: a SATA drive on a SAS HBA.
        assert!(!is_sas(Some("ATA"), Some(&sas), true, port));
        // Nothing says SAS.
        assert!(!is_sas(Some("QEMU"), None, false, none));
    }

    #[test]
    fn rate_labels() {
        assert_eq!(rate_label(12_000), "12 Gb/s");
        assert_eq!(rate_label(22_500), "22.5 Gb/s");
        assert_eq!(rate_label(1_500), "1.5 Gb/s");
    }
}
