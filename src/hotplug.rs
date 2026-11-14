//! Hotplug (#15): the kernel's uevents for block disks, and (#141) for the
//! SCSI-level objects a drive brings before or instead of one (sg node, SCSI
//! device, SAS end device), so a drive pulled
//! from or pushed into one of 160 bays is seen in a couple of seconds
//! rather than at the next discovery pass. The periodic pass stays: it is
//! the backstop for a missed event, and for a node where the netlink
//! socket cannot be opened.
//!
//! One thread reads `NETLINK_KOBJECT_UEVENT` (kernel multicast group 1, no
//! udev needed); the monitor debounces what it reports — pulling a
//! dual-ported drive or a whole shelf fires dozens of events — into one
//! discovery pass.

/// A block-disk event worth a rescan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskEvent {
    /// `add`, `remove` or `change`.
    pub action: String,
    /// Kernel name, `sdq` / `nvme17n1`.
    pub name: String,
}

/// Parse one kernel uevent datagram: `ACTION@DEVPATH\0KEY=VALUE\0…`. Whole
/// disks count (not partitions); `change` covers a resize or a media change
/// (a reformat's rescan). So do a drive's sg node, SCSI device and SAS end
/// device being added or removed (#141).
pub fn parse(buf: &[u8]) -> Option<DiskEvent> {
    let mut fields = buf.split(|&b| b == 0).map(|f| String::from_utf8_lossy(f));
    let head = fields.next()?;
    if !head.contains('@') {
        return None; // a udev-format message (libudev header), not the kernel's
    }
    let (mut action, mut subsystem, mut devtype, mut name) = (None, None, None, None);
    for f in fields {
        let Some((k, v)) = f.split_once('=') else { continue };
        match k {
            "ACTION" => action = Some(v.to_string()),
            "SUBSYSTEM" => subsystem = Some(v.to_string()),
            "DEVTYPE" => devtype = Some(v.to_string()),
            "DEVNAME" => name = Some(v.trim_start_matches("/dev/").to_string()),
            _ => {}
        }
    }
    let action = action?;
    if !matches!(action.as_str(), "add" | "remove" | "change") {
        return None;
    }
    match (subsystem.as_deref(), devtype.as_deref()) {
        (Some("block"), Some("disk")) => Some(DiskEvent { action, name: name? }),
        // A drive that arrives after boot without a block node (#141): an
        // unformatted or half-formatted SAS drive is reached through its sg
        // node only (#131), and a SAS end device or SCSI target appears
        // before either. Each is worth a pass; the name is the sg node
        // when there is one, else the kernel object (`0:0:5:0`,
        // `end_device-0:3`). `change` there is noise (no resize).
        (Some("scsi_generic"), _) | (Some("scsi"), Some("scsi_device")) | (Some("sas_end_device"), _) if action != "change" => {
            let name = name.or_else(|| head.rsplit('/').next().map(str::to_string)).filter(|n| !n.is_empty())?;
            Some(DiskEvent { action, name })
        }
        _ => None,
    }
}

/// Start the listener thread. Events go to `tx`; an error opening the
/// socket is returned and the caller keeps polling.
#[cfg(target_os = "linux")]
pub fn listen(tx: tokio::sync::mpsc::UnboundedSender<DiskEvent>) -> std::io::Result<()> {
    use std::io::Error;
    // SAFETY: plain socket/bind/recv on a descriptor this thread owns.
    unsafe {
        let fd = libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            libc::NETLINK_KOBJECT_UEVENT,
        );
        if fd < 0 {
            return Err(Error::last_os_error());
        }
        let mut addr: libc::sockaddr_nl = std::mem::zeroed();
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        addr.nl_groups = 1; // kernel events
        let rc = libc::bind(
            fd,
            &addr as *const libc::sockaddr_nl as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        );
        if rc < 0 {
            let e = Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        std::thread::Builder::new().name("hotplug".into()).spawn(move || {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                let n = libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0);
                if n < 0 {
                    let e = Error::last_os_error();
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    // ENOBUFS: the kernel dropped events (a burst of
                    // hundreds). Say so with a synthetic event: the rescan
                    // finds whatever was missed.
                    if e.raw_os_error() == Some(libc::ENOBUFS) {
                        let _ = tx.send(DiskEvent { action: "overflow".into(), name: String::new() });
                        continue;
                    }
                    tracing::warn!("hotplug listener stopped: {e}");
                    libc::close(fd);
                    return;
                }
                if let Some(ev) = parse(&buf[..n as usize]) {
                    if tx.send(ev).is_err() {
                        libc::close(fd);
                        return;
                    }
                }
            }
        })?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn listen(_tx: tokio::sync::mpsc::UnboundedSender<DiskEvent>) -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "uevents are Linux-only"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(parts: &[&str]) -> Vec<u8> {
        parts.join("\0").into_bytes()
    }

    #[test]
    fn a_disk_added_or_removed_is_an_event() {
        let add = msg(&[
            "add@/devices/pci0000:00/0000:00:01.0/10000:03:00.0/nvme/nvme5/nvme5n1",
            "ACTION=add",
            "DEVPATH=/devices/pci0000:00/0000:00:01.0/10000:03:00.0/nvme/nvme5/nvme5n1",
            "SUBSYSTEM=block",
            "MAJOR=259",
            "MINOR=3",
            "DEVNAME=nvme5n1",
            "DEVTYPE=disk",
            "SEQNUM=4211",
        ]);
        assert_eq!(parse(&add), Some(DiskEvent { action: "add".into(), name: "nvme5n1".into() }));
        let rm = msg(&["remove@/devices/…/sdq", "ACTION=remove", "SUBSYSTEM=block", "DEVNAME=/dev/sdq", "DEVTYPE=disk"]);
        assert_eq!(parse(&rm).unwrap().name, "sdq");
    }

    #[test]
    fn a_drive_without_a_block_node_is_an_event() {
        // forge1 (#141): a SAS drive that spins up after boot, unformatted.
        let scsi = msg(&["add@/devices/pci0000:c0/0000:c1:00.0/host0/port-0:3/end_device-0:3/target0:0:3/0:0:3:0", "ACTION=add", "SUBSYSTEM=scsi", "DEVTYPE=scsi_device"]);
        assert_eq!(parse(&scsi), Some(DiskEvent { action: "add".into(), name: "0:0:3:0".into() }));
        let sg = msg(&["add@/x/0:0:3:0/scsi_generic/sg3", "ACTION=add", "SUBSYSTEM=scsi_generic", "MAJOR=21", "DEVNAME=sg3"]);
        assert_eq!(parse(&sg), Some(DiskEvent { action: "add".into(), name: "sg3".into() }));
        let end = msg(&["add@/x/port-0:3/end_device-0:3/sas_end_device/end_device-0:3", "ACTION=add", "SUBSYSTEM=sas_end_device"]);
        assert_eq!(parse(&end).unwrap().name, "end_device-0:3");
        let gone = msg(&["remove@/x/scsi_generic/sg3", "ACTION=remove", "SUBSYSTEM=scsi_generic", "DEVNAME=sg3"]);
        assert_eq!(parse(&gone).unwrap().action, "remove");
    }

    #[test]
    fn partitions_other_subsystems_and_udev_messages_are_not() {
        let part = msg(&["add@/x/sdq/sdq1", "ACTION=add", "SUBSYSTEM=block", "DEVNAME=sdq1", "DEVTYPE=partition"]);
        assert_eq!(parse(&part), None);
        let host = msg(&["add@/x/host0", "ACTION=add", "SUBSYSTEM=scsi", "DEVTYPE=scsi_host"]);
        assert_eq!(parse(&host), None);
        let sg_change = msg(&["change@/x/sg3", "ACTION=change", "SUBSYSTEM=scsi_generic", "DEVNAME=sg3"]);
        assert_eq!(parse(&sg_change), None);
        let bind = msg(&["bind@/x/sdq", "ACTION=bind", "SUBSYSTEM=block", "DEVNAME=sdq", "DEVTYPE=disk"]);
        assert_eq!(parse(&bind), None);
        let udev = msg(&["libudev", "ACTION=add", "SUBSYSTEM=block", "DEVNAME=sdq", "DEVTYPE=disk"]);
        assert_eq!(parse(&udev), None);
        assert_eq!(parse(b""), None);
    }
}
