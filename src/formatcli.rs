//! `stormdrive format` (#136): one drive's low-level format from a shell,
//! for stormblock's initramfs (stormcos#902, STORAGE-LAYOUT.md step 2):
//! every drive is formatted at install, before anything is laid on it.
//!
//! No API, no Kubernetes, no config file, no runtime: it opens the device
//! and nothing else, and says what it did on stdout/stderr and in its exit
//! code. The commands are the daemon's own (format.rs): READ CAPACITY(16),
//! MODE SELECT with the drive's own block descriptor (#129), FORMAT UNIT
//! with FMTPINFO, REQUEST SENSE / TEST UNIT READY progress, READ CAPACITY
//! read-back. What it adds is the **fast format** (SBC-4 FORMAT UNIT FFMT
//! = 01b: the drive initialises its own resources without writing the
//! medium), tried first under `--method auto`:
//!
//! 1. READ CAPACITY: at the asked block size and PI already? `auto` and
//!    `none` stop there (exit 4). A drive whose last format was cut off
//!    (31/00) is not at any format.
//! 2. MODE SELECT the block length. Refused = nothing formatted (exit 3),
//!    with the sense and its field pointer.
//! 3. `auto`/`fast`: FORMAT UNIT IMMED with FFMT 01b, unless REPORT
//!    SUPPORTED OPERATION CODES says the drive ignores FFMT (`auto` then
//!    goes straight to step 4; `fast` still tries, the drive decides).
//!    ILLEGAL REQUEST: `fast` puts the old length back and exits refused;
//!    `auto` goes on to the full format.
//! 4. `full` (or `auto` after 3): FORMAT UNIT IMMED, no FFMT; the blocking
//!    form when IMMED is refused without PI.
//! 5. Progress every `--poll-secs`: % and time left from the drive's
//!    progress indication.
//! 6. READ CAPACITY: the block length and PI type must be what was asked;
//!    then the kernel rescans (sd re-added if it still sees 0 blocks).
//!
//! An NVMe namespace gets Format NVM to the LBA format of that size, no
//! metadata (PI on NVMe: refused, not built here).

use crate::erase::nvme_names;
use crate::format::{poll_format, set_length, Attempt, Unit};
use crate::pi::Protection;
use crate::scsi::{self, Error as ScsiError};
use serde::Serialize;
use std::time::Duration;

/// How to format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// Already at the format: nothing. Else fast format where the drive
    /// takes it, else a full FORMAT UNIT.
    Auto,
    /// Fast format only (FFMT 01b); refused = nothing formatted.
    Fast,
    /// A full FORMAT UNIT, even when the drive is already at the format.
    Full,
    /// Check only: say whether the drive is at the format, send nothing.
    None,
}

/// The protection asked for on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Prot {
    None,
    Type1,
}

impl From<Prot> for Protection {
    fn from(p: Prot) -> Self {
        match p {
            Prot::None => Protection::None,
            Prot::Type1 => Protection::Type1,
        }
    }
}

/// What happened, and the exit code that says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Formatted and read back as asked. Exit 0.
    Formatted,
    /// The run failed (transport, a failed format, a read-back that is not
    /// what was asked). The drive may need a format. Exit 1.
    Failed,
    /// The drive refused the format (ILLEGAL REQUEST): nothing was
    /// formatted. Exit 3.
    Refused,
    /// Already at the asked block size and PI (`auto`, `none`). Exit 4.
    Already,
    /// `none`: the drive is not at the format. Exit 5.
    NotAtFormat,
}

impl Outcome {
    pub fn exit_code(self) -> i32 {
        match self {
            Outcome::Formatted => 0,
            Outcome::Failed => 1,
            Outcome::Refused => 3,
            Outcome::Already => 4,
            Outcome::NotAtFormat => 5,
        }
    }
}

/// The drive's format as READ CAPACITY reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Geometry {
    pub block_size: u32,
    /// 0 = no PI, else the PI type.
    pub pi_type: u8,
    pub blocks: u64,
    pub capacity_bytes: u64,
}

impl Geometry {
    fn of(c: &scsi::Capacity) -> Self {
        Geometry { block_size: c.block_len, pi_type: c.prot_type, blocks: c.blocks(), capacity_bytes: c.bytes() }
    }
    fn line(&self) -> String {
        format!(
            "{}-byte blocks, {}, {} ({} blocks)",
            self.block_size,
            if self.pi_type == 0 { "no PI".to_string() } else { format!("PI type {}", self.pi_type) },
            human_bytes(self.capacity_bytes),
            self.blocks
        )
    }
}

/// The whole run, printed at the end (and as JSON with `--json`).
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub device: String,
    pub outcome: Outcome,
    pub exit_code: i32,
    pub method_asked: Method,
    /// `fast`, `full`, `full_blocking`, `format_nvm`; None when nothing ran.
    pub method_used: Option<&'static str>,
    pub block_size: u32,
    pub protection: &'static str,
    /// Before: None when READ CAPACITY gave none (an interrupted format).
    pub before: Option<Geometry>,
    pub after: Option<Geometry>,
    /// `auto`: why the fast format was not used.
    pub fast_skipped: Option<String>,
    /// Refused / failed: what the drive said.
    pub error: Option<String>,
    pub elapsed_secs: u64,
}

impl Report {
    fn new(device: &str, method: Method, (to, prot): (u32, Protection)) -> Self {
        Report {
            device: device.into(),
            outcome: Outcome::Failed,
            exit_code: 1,
            method_asked: method,
            method_used: None,
            block_size: to,
            protection: if prot == Protection::None { "none" } else { "type1" },
            before: None,
            after: None,
            fast_skipped: None,
            error: None,
            elapsed_secs: 0,
        }
    }
    fn end(mut self, outcome: Outcome, error: Option<String>) -> Self {
        self.outcome = outcome;
        self.exit_code = outcome.exit_code();
        self.error = error;
        self
    }
    /// The human-readable last line.
    pub fn summary(&self) -> String {
        let want = format!("{} with {}", self.block_size, if self.protection == "none" { "no PI" } else { "PI type 1" });
        match self.outcome {
            Outcome::Formatted => format!(
                "{}: formatted ({}) in {}: {}",
                self.device,
                self.method_used.unwrap_or("?"),
                human_secs(self.elapsed_secs),
                self.after.map(|g| g.line()).unwrap_or_default()
            ),
            Outcome::Already => format!(
                "{}: already {want}: {}; nothing sent",
                self.device,
                self.before.map(|g| g.line()).unwrap_or_default()
            ),
            Outcome::NotAtFormat => format!(
                "{}: not at {want}: {}",
                self.device,
                self.before.map(|g| g.line()).unwrap_or_else(|| "no readable format (interrupted format?)".into())
            ),
            Outcome::Refused => format!(
                "{}: refused {want}; nothing was formatted: {}",
                self.device,
                self.error.as_deref().unwrap_or("?")
            ),
            Outcome::Failed => format!("{}: failed: {}", self.device, self.error.as_deref().unwrap_or("?")),
        }
    }
}

pub fn human_bytes(b: u64) -> String {
    const U: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.2} {}", U[i])
    }
}

pub fn human_secs(s: u64) -> String {
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// One progress line: percent and time left when the drive gives them.
pub fn progress_line(device: &str, how: &str, elapsed_secs: u64, progress: Option<u16>) -> String {
    match progress {
        Some(p) => {
            let pct = p as f64 * 100.0 / 65536.0;
            let left = crate::format::eta_secs(elapsed_secs, p)
                .map(|s| format!(", about {} left", human_secs(s)))
                .unwrap_or_default();
            format!("{device}: formatting ({how}) {pct:.1}%{left}, {} so far", human_secs(elapsed_secs))
        }
        None => format!("{device}: formatting ({how}), no progress reported yet, {} so far", human_secs(elapsed_secs)),
    }
}

/// What the CLI needs of a SCSI drive beyond what format.rs's `Unit` has.
pub(crate) trait Target: Unit {
    fn test_unit_ready(&self) -> scsi::Result<()>;
    fn request_sense(&self) -> scsi::Result<Vec<u8>>;
    /// START UNIT, for a drive NOT READY 04/02 (#142).
    fn start_unit(&self) -> scsi::Result<()>;
    /// REPORT SUPPORTED OPERATION CODES on FORMAT UNIT's FFMT bits.
    fn ffmt_supported(&self) -> Option<bool>;
    fn format_unit_ffmt(&self, immed: bool, fmtpinfo: u8, ffmt: u8) -> scsi::Result<()>;
}

impl Target for scsi::Device {
    fn test_unit_ready(&self) -> scsi::Result<()> {
        scsi::Device::test_unit_ready(self)
    }
    fn request_sense(&self) -> scsi::Result<Vec<u8>> {
        scsi::Device::request_sense(self)
    }
    fn start_unit(&self) -> scsi::Result<()> {
        scsi::Device::start_unit(self)
    }
    fn ffmt_supported(&self) -> Option<bool> {
        self.supported_opcode(0x04).ok().as_ref().and_then(scsi::ffmt_supported)
    }
    fn format_unit_ffmt(&self, immed: bool, fmtpinfo: u8, ffmt: u8) -> scsi::Result<()> {
        scsi::Device::format_unit_ffmt(self, immed, fmtpinfo, ffmt)
    }
}

/// How the run talks to the outside: sleeping between polls, progress
/// lines, the kernel rescan. A test passes its own.
pub(crate) struct Io<'a> {
    pub poll: Duration,
    pub sleep: &'a dyn Fn(Duration),
    pub say: &'a dyn Fn(&str),
    pub rescan: &'a dyn Fn(),
}

fn is_illegal(e: &ScsiError) -> bool {
    matches!(e, ScsiError::Sense(s) if s.is_illegal_request())
}

/// Put the block length the drive had back after a refusal, so a refused
/// format leaves the drive as it was.
fn put_back(dev: &impl Target, name: &str, before: Option<Geometry>, to: u32, io: &Io) {
    let Some(b) = before.filter(|b| b.block_size != to) else { return };
    if let Err(Attempt::Refused(e) | Attempt::Failed(e)) = set_length(dev, name, b.block_size) {
        (io.say)(&format!("{name}: putting the {}-byte length back: {e}", b.block_size));
    }
}

/// A SCSI drive's format, start to end.
pub(crate) fn run_scsi(dev: &impl Target, name: &str, want: (u32, Protection), method: Method, io: &Io) -> Report {
    let (to, prot) = want;
    let mut r = Report::new(name, method, want);
    // A pending unit attention would fail the first real command.
    let _ = dev.test_unit_ready();
    match before_format(dev, name, io) {
        Ok(b) => r.before = b,
        Err(e) => return r.end(Outcome::Failed, Some(e)),
    }
    let at = r.before.is_some_and(|b| b.block_size == to && b.pi_type == prot.prot_type());
    match (method, at) {
        (Method::None, true) | (Method::Auto, true) => return r.end(Outcome::Already, None),
        (Method::None, false) => return r.end(Outcome::NotAtFormat, None),
        _ => {}
    }

    if let Err(a) = set_length(dev, name, to) {
        let refused = matches!(a, Attempt::Refused(_));
        return r.end(if refused { Outcome::Refused } else { Outcome::Failed }, Some(e_of(&a)));
    }

    // The fast format first, where asked and the drive may take it.
    let mut used: Option<&'static str> = None;
    let try_fast = match method {
        Method::Fast => true,
        Method::Auto => match dev.ffmt_supported() {
            Some(false) => {
                r.fast_skipped = Some("REPORT SUPPORTED OPERATION CODES: FORMAT UNIT takes no FFMT".into());
                false
            }
            _ => true,
        },
        _ => false,
    };
    if try_fast {
        (io.say)(&format!("{name}: fast format (FFMT 01b) to {to} with {}", prot.word()));
        match dev.format_unit_ffmt(true, prot.fmtpinfo(), 0b01) {
            Ok(()) => used = Some("fast"),
            Err(e) if is_illegal(&e) && method == Method::Fast => {
                put_back(dev, name, r.before, to, io);
                return r.end(Outcome::Refused, Some(format!("fast format: {e}")));
            }
            Err(e) if is_illegal(&e) => {
                (io.say)(&format!("{name}: fast format refused ({e}); a full format instead"));
                r.fast_skipped = Some(format!("refused: {e}"));
            }
            Err(e) => return r.end(Outcome::Failed, Some(format!("fast format: {e}"))),
        }
    }
    let mut immediate = true;
    if used.is_none() {
        (io.say)(&format!("{name}: full format (FORMAT UNIT) to {to} with {}", prot.word()));
        match dev.format_unit_pi(true, prot.fmtpinfo()) {
            Ok(()) => used = Some("full"),
            Err(e) if is_illegal(&e) && prot != Protection::None => {
                put_back(dev, name, r.before, to, io);
                return r.end(Outcome::Refused, Some(format!("format unit with {}: {e}", prot.word())));
            }
            Err(e) if is_illegal(&e) => {
                (io.say)(&format!("{name}: IMMED refused ({e}); the blocking format, no progress until it ends"));
                match dev.format_unit_pi(false, 0) {
                    Ok(()) => {
                        used = Some("full_blocking");
                        immediate = false;
                    }
                    Err(e) if is_illegal(&e) => {
                        put_back(dev, name, r.before, to, io);
                        return r.end(Outcome::Refused, Some(format!("format unit: {e}")));
                    }
                    Err(e) => return r.end(Outcome::Failed, Some(format!("format unit: {e}"))),
                }
            }
            Err(e) => return r.end(Outcome::Failed, Some(format!("format unit: {e}"))),
        }
    }
    r.method_used = used;
    let how = used.unwrap_or("?");

    // Progress until the drive is ready again.
    if immediate {
        let (elapsed, done) = wait_format(dev, name, how, io);
        r.elapsed_secs = elapsed.as_secs();
        if let Err(e) = done {
            return r.end(Outcome::Failed, Some(format!("{how} format: {e}")));
        }
    }

    // Read back what the drive is now, then let the kernel see it.
    let _ = dev.test_unit_ready();
    let after = match dev.read_capacity16() {
        Ok(c) => Geometry::of(&c),
        Err(e) => {
            (io.rescan)();
            return r.end(Outcome::Failed, Some(format!("read capacity after the format: {e}")));
        }
    };
    r.after = Some(after);
    (io.rescan)();
    if after.block_size != to || after.pi_type != prot.prot_type() {
        return r.end(
            Outcome::Failed,
            Some(format!(
                "the drive reads {} after a {how} format to {to} with {}",
                after.line(),
                prot.word()
            )),
        );
    }
    r.end(Outcome::Formatted, None)
}

/// Poll a running format until the drive is ready, saying its progress.
/// Never stops it (#141): the only ends are ready, the drive saying the
/// format failed, or 48 h.
fn wait_format(dev: &impl Target, name: &str, how: &str, io: &Io) -> (Duration, Result<(), String>) {
    let mut elapsed = Duration::ZERO;
    loop {
        (io.sleep)(io.poll);
        elapsed += io.poll;
        match poll_format(|| dev.request_sense(), || dev.test_unit_ready()) {
            Ok(None) => return (elapsed, Ok(())),
            Ok(Some(p)) => (io.say)(&progress_line(name, how, elapsed.as_secs(), p)),
            Err(e) => return (elapsed, Err(e)),
        }
        if elapsed > crate::format::MAX_WAIT {
            return (elapsed, Err("did not finish within 48 h".into()));
        }
    }
}

/// What the drive is before the format: its geometry, or None when it
/// has no readable format, which is what a format is for (#142):
/// - 31/00 MEDIUM FORMAT CORRUPTED: an interrupted format;
/// - NOT READY 04/02 (initializing command required): START UNIT, then
///   read again;
/// - NOT READY 04/04 (a format in progress, a previous attempt's) or 04/01
///   (becoming ready): wait for it, never stop it, then read again;
/// - any other NOT READY (04/00, 04/03, …): say what it answered and
///   format it.
///
/// Anything else (another sense key, a transport error) is a failure: the
/// drive is not one a format would cure.
fn before_format(dev: &impl Target, name: &str, io: &Io) -> Result<Option<Geometry>, String> {
    let mut acted = false;
    loop {
        let s = match dev.read_capacity16() {
            Ok(c) => return Ok(Some(Geometry::of(&c))),
            Err(ScsiError::Sense(s)) if s.is_medium_format_corrupted() => {
                (io.say)(&format!("{name}: read capacity: {s} (an interrupted format)"));
                return Ok(None);
            }
            Err(ScsiError::Sense(s)) if s.key == 0x2 => s,
            Err(e) => return Err(format!("read capacity: {e}")),
        };
        if acted {
            (io.say)(&format!("{name}: read capacity: {s} (still not ready; formatting it)"));
            return Ok(None);
        }
        acted = true;
        match (s.asc, s.ascq) {
            (0x04, 0x02) => {
                (io.say)(&format!("{name}: read capacity: {s} (START UNIT first)"));
                if let Err(e) = dev.start_unit() {
                    (io.say)(&format!("{name}: start unit: {e}"));
                }
            }
            (0x04, 0x04) | (0x04, 0x01) => {
                let what = if s.ascq == 0x04 { "a format is running; waiting for it" } else { "becoming ready; waiting" };
                (io.say)(&format!("{name}: read capacity: {s} ({what})"));
                if let (_, Err(e)) = wait_format(dev, name, "the drive's own", io) {
                    (io.say)(&format!("{name}: waiting for the running format: {e}"));
                }
            }
            _ => {
                (io.say)(&format!("{name}: read capacity: {s} (not ready; formatting it)"));
                return Ok(None);
            }
        }
    }
}

fn e_of(a: &Attempt) -> String {
    match a {
        Attempt::Refused(e) | Attempt::Failed(e) => e.clone(),
        Attempt::Started { .. } => "started".into(),
    }
}

/// An NVMe namespace: Format NVM to the LBA format of that size.
fn run_nvme(path: &str, name: &str, want: (u32, Protection), method: Method) -> Report {
    let (to, prot) = want;
    let mut r = Report::new(name, method, want);
    let geometry = |f: &crate::erase::LbaFormat| {
        let sectors = sysfs_sectors(name);
        Geometry {
            block_size: f.data_size,
            pi_type: 0,
            blocks: sectors * 512 / f.data_size as u64,
            capacity_bytes: sectors * 512,
        }
    };
    match crate::erase::nvme_current(path, name) {
        Ok(f) => {
            let at = f.data_size == to && f.metadata_size == 0 && prot == Protection::None;
            r.before = Some(geometry(&f));
            match (method, at) {
                (Method::None, true) | (Method::Auto, true) => return r.end(Outcome::Already, None),
                (Method::None, false) => return r.end(Outcome::NotAtFormat, None),
                _ => {}
            }
        }
        Err(e) if method == Method::None => return r.end(Outcome::Failed, Some(e)),
        Err(e) => return r.end(Outcome::Failed, Some(format!("identify: {e}"))),
    }
    if prot != Protection::None {
        return r.end(Outcome::Refused, Some("PI on an NVMe namespace is not built in this CLI (formats with metadata are not chosen)".into()));
    }
    let t0 = std::time::Instant::now();
    let say = |_: Option<u8>, what: &str| eprintln!("{name}: {what}");
    match crate::erase::nvme_format(path, name, to, &say) {
        Ok(_) => {}
        Err(e) if e.starts_with("no ") => return r.end(Outcome::Refused, Some(e)),
        Err(e) => return r.end(Outcome::Failed, Some(e)),
    }
    r.method_used = Some("format_nvm");
    r.elapsed_secs = t0.elapsed().as_secs();
    std::thread::sleep(Duration::from_secs(1));
    match crate::erase::nvme_current(path, name) {
        Ok(f) => {
            r.after = Some(geometry(&f));
            if f.data_size != to {
                return r.end(Outcome::Failed, Some(format!("the namespace reads {}-byte blocks after Format NVM to {to}", f.data_size)));
            }
            r.end(Outcome::Formatted, None)
        }
        Err(e) => r.end(Outcome::Failed, Some(format!("identify after the format: {e}"))),
    }
}

fn sysfs_sectors(name: &str) -> u64 {
    std::fs::read_to_string(format!("/sys/block/{name}/size")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

/// Whether anything on the device (or a partition of it) is mounted.
pub fn mounted_on(mounts: &str, name: &str) -> Option<String> {
    mounts.lines().find_map(|l| {
        let mut f = l.split_whitespace();
        let src = f.next()?.strip_prefix("/dev/")?;
        let at = f.next()?;
        let part = src.strip_prefix(name)?;
        // sdb1, nvme0n1p2 — not sdbb.
        let ok = part.is_empty()
            || (name.ends_with(|c: char| c.is_ascii_digit()) && part.starts_with('p') && part[1..].chars().all(|c| c.is_ascii_digit()) && part.len() > 1)
            || (!name.ends_with(|c: char| c.is_ascii_digit()) && part.chars().all(|c| c.is_ascii_digit()));
        ok.then(|| format!("/dev/{src} on {at}"))
    })
}

/// The command line's run: resolve the device, refuse a mounted one, run,
/// print. Returns the exit code.
pub fn main(device: &str, block_size: u32, protection: Prot, method: Method, poll_secs: u64, json: bool) -> i32 {
    let prot: Protection = protection.into();
    let name = device.trim_start_matches("/dev/").to_string();
    if !crate::drive::USABLE_BLOCK_SIZES.contains(&block_size) {
        eprintln!("{device}: block size {block_size} is not one stormdrive formats to ({:?})", crate::drive::USABLE_BLOCK_SIZES);
        return 2;
    }
    if name.is_empty() || name.contains('/') {
        eprintln!("{device}: give a device node under /dev (sdX, sgN, nvmeXnY)");
        return 2;
    }
    if !std::path::Path::new(&format!("/dev/{name}")).exists() {
        eprintln!("/dev/{name}: no such device");
        return 2;
    }
    if method != Method::None {
        if let Some(m) = std::fs::read_to_string("/proc/mounts").ok().and_then(|m| mounted_on(&m, &name)) {
            let r = Report::new(&name, method, (block_size, prot)).end(Outcome::Refused, Some(format!("{m}: unmount it first")));
            return finish(&r, json);
        }
    }
    let r = if nvme_names(&name).is_some() {
        run_nvme(&format!("/dev/{name}"), &name, (block_size, prot), method)
    } else {
        // sg is the classic door, and the one that still works when sd gave
        // up on the drive (520-byte, an interrupted format).
        let path = scsi::sg_path_for_block(&name).unwrap_or_else(|| format!("/dev/{name}"));
        match scsi::Device::open(&path) {
            Ok(dev) => {
                let say = |l: &str| eprintln!("{l}");
                let rescan = || crate::format::rescan(&name);
                let io = Io { poll: Duration::from_secs(poll_secs.max(1)), sleep: &std::thread::sleep, say: &say, rescan: &rescan };
                run_scsi(&dev, &name, (block_size, prot), method, &io)
            }
            Err(e) => Report::new(&name, method, (block_size, prot)).end(Outcome::Failed, Some(format!("open {path}: {e}"))),
        }
    };
    finish(&r, json)
}

fn finish(r: &Report, json: bool) -> i32 {
    if json {
        println!("{}", serde_json::to_string(r).unwrap_or_default());
    } else {
        println!("{}", r.summary());
    }
    r.exit_code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scsi::{FieldPointer, Sense};
    use std::cell::{Cell, RefCell};

    /// A drive that logs what it is sent: at `before` until a format ends,
    /// then at what MODE SELECT asked (or `after` when set).
    struct Fake {
        log: RefCell<Vec<String>>,
        before: Option<(u32, u8)>,
        after: Option<(u32, u8)>,
        ffmt: Option<bool>,
        refuse_ffmt: bool,
        refuse_select: bool,
        refuse_pi: bool,
        polls: Cell<u8>,
        selected: Cell<u32>,
        formatted: Cell<Option<u8>>,
        /// NOT READY (asc, ascq) to READ CAPACITY until formatted (#142).
        not_ready: Cell<Option<(u8, u8)>>,
        /// The sense key it answers with (2, NOT READY, unless a test says).
        key: u8,
        /// START UNIT makes it ready.
        starts: bool,
        /// Polls left of a format already running when the run starts.
        running: Cell<u8>,
    }

    impl Fake {
        fn at(block: u32, pi: u8) -> Self {
            Fake {
                log: RefCell::new(vec![]),
                before: Some((block, pi)),
                after: None,
                ffmt: Some(true),
                refuse_ffmt: false,
                refuse_select: false,
                refuse_pi: false,
                polls: Cell::new(0),
                selected: Cell::new(block),
                formatted: Cell::new(None),
                not_ready: Cell::new(None),
                key: 2,
                starts: false,
                running: Cell::new(0),
            }
        }
        fn not_ready(asc: u8, ascq: u8) -> Self {
            Fake { before: None, not_ready: Cell::new(Some((asc, ascq))), ..Fake::at(512, 0) }
        }
        fn sent(&self) -> Vec<String> {
            self.log.borrow().iter().filter(|l| !l.starts_with("tur") && !l.starts_with("sense") && l.as_str() != "read capacity").cloned().collect()
        }
    }

    const BLOCKS_4K: u64 = 2_441_609_216; // forge1's X378 at 4096

    fn cap(block: u32, pi: u8) -> scsi::Capacity {
        let blocks = BLOCKS_4K * 4096 / block as u64;
        scsi::Capacity { last_lba: blocks - 1, block_len: block, lbppbe: 0, prot_type: pi }
    }

    fn illegal(asc: u8, byte: u16) -> ScsiError {
        ScsiError::Sense(Sense { field: Some(FieldPointer { in_cdb: true, bit: None, byte }), ..Sense::new(5, asc, 0) })
    }

    impl Unit for Fake {
        fn mode_sense10(&self, _: u8) -> scsi::Result<scsi::ModeHeader10> {
            let bd = vec![0xff, 0xff, 0xff, 0xff, 0, 0, 0x02, 0x00];
            Ok(scsi::ModeHeader10 { medium_type: 0, device_specific: 0x10, long_lba: false, block_descriptor: Some(bd) })
        }
        fn mode_select10(&self, data: &[u8]) -> scsi::Result<()> {
            let len = u32::from_be_bytes([0, data[13], data[14], data[15]]);
            self.log.borrow_mut().push(format!("select {len}"));
            if self.refuse_select && len != self.before.map(|b| b.0).unwrap_or(512) {
                return Err(illegal(0x26, 14));
            }
            self.selected.set(len);
            Ok(())
        }
        fn mode_select6(&self, _: &[u8]) -> scsi::Result<()> {
            self.log.borrow_mut().push("select6".into());
            Err(ScsiError::Transport { status: 2, host: 7, driver: 8 })
        }
        fn format_unit_pi(&self, immed: bool, fmtpinfo: u8) -> scsi::Result<()> {
            self.log.borrow_mut().push(format!("format immed={immed} fmtpinfo={fmtpinfo}"));
            if self.refuse_pi && fmtpinfo != 0 {
                return Err(illegal(0x24, 1));
            }
            self.formatted.set(Some(fmtpinfo));
            Ok(())
        }
        fn read_capacity16(&self) -> scsi::Result<scsi::Capacity> {
            self.log.borrow_mut().push("read capacity".into());
            match (self.formatted.get(), self.before) {
                (Some(f), _) if self.polls.get() >= 3 => {
                    let (b, p) = self.after.unwrap_or((self.selected.get(), if f == 0b10 { 1 } else { 0 }));
                    Ok(cap(b, p))
                }
                _ if self.formatted.get().is_none() && self.not_ready.get().is_some() => {
                    let (asc, ascq) = self.not_ready.get().unwrap();
                    Err(ScsiError::Sense(Sense::new(self.key, asc, ascq)))
                }
                (_, Some((b, p))) => Ok(cap(b, p)),
                (_, None) => Err(ScsiError::Sense(Sense::new(3, 0x31, 0))),
            }
        }
    }

    impl Target for Fake {
        fn test_unit_ready(&self) -> scsi::Result<()> {
            self.log.borrow_mut().push("tur".into());
            if self.running.get() > 0 {
                return Err(ScsiError::Sense(Sense { progress: Some(0x8000), ..Sense::new(2, 4, 4) }));
            }
            if self.formatted.get().is_some() && self.polls.get() < 3 {
                return Err(ScsiError::Sense(Sense { progress: Some(0x4000), ..Sense::new(2, 4, 4) }));
            }
            Ok(())
        }
        fn request_sense(&self) -> scsi::Result<Vec<u8>> {
            self.log.borrow_mut().push("sense".into());
            if self.running.get() > 0 {
                let n = self.running.get() - 1;
                self.running.set(n);
                if n == 0 {
                    // The earlier format ended: the drive reads at 512.
                    self.not_ready.set(None);
                } else {
                    return Ok(vec![0x70, 0, 2, 0, 0, 0, 0, 10, 0, 0, 0, 0, 4, 4, 0, 0x80, 0x80, 0]);
                }
            }
            if self.formatted.get().is_some() && self.polls.get() < 3 {
                let n = self.polls.get() + 1;
                self.polls.set(n);
                if n < 3 {
                    let p = (n as u16) * 0x5000;
                    let [hi, lo] = p.to_be_bytes();
                    return Ok(vec![0x70, 0, 2, 0, 0, 0, 0, 10, 0, 0, 0, 0, 4, 4, 0, 0x80, hi, lo]);
                }
            }
            Ok(vec![0x70, 0, 0, 0, 0, 0, 0, 10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        }
        fn start_unit(&self) -> scsi::Result<()> {
            self.log.borrow_mut().push("start".into());
            if self.starts {
                self.not_ready.set(None);
            }
            Ok(())
        }
        fn ffmt_supported(&self) -> Option<bool> {
            self.log.borrow_mut().push("rsoc".into());
            self.ffmt
        }
        fn format_unit_ffmt(&self, immed: bool, fmtpinfo: u8, ffmt: u8) -> scsi::Result<()> {
            self.log.borrow_mut().push(format!("ffmt{ffmt} immed={immed} fmtpinfo={fmtpinfo}"));
            if self.refuse_ffmt {
                return Err(illegal(0x24, 4));
            }
            self.formatted.set(Some(fmtpinfo));
            Ok(())
        }
    }

    fn run(f: &Fake, want: (u32, Protection), m: Method) -> (Report, Vec<String>) {
        let lines = RefCell::new(vec![]);
        let rescans = Cell::new(0);
        let say = |l: &str| lines.borrow_mut().push(l.to_string());
        let sleep = |_: Duration| {};
        let rescan = || rescans.set(rescans.get() + 1);
        let io = Io { poll: Duration::from_secs(600), sleep: &sleep, say: &say, rescan: &rescan };
        let r = run_scsi(f, "sdd", want, m, &io);
        if r.outcome == Outcome::Formatted {
            assert_eq!(rescans.get(), 1, "the kernel rescans after a format");
        }
        (r, lines.into_inner())
    }

    const PI4K: (u32, Protection) = (4096, Protection::Type1);

    #[test]
    fn auto_fast_formats_first_with_progress_and_eta() {
        let f = Fake::at(512, 1);
        let (r, lines) = run(&f, PI4K, Method::Auto);
        assert_eq!(r.outcome, Outcome::Formatted, "{r:?}");
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.method_used, Some("fast"));
        assert_eq!(f.sent(), vec!["select 4096", "rsoc", "ffmt1 immed=true fmtpinfo=2"]);
        let a = r.after.unwrap();
        assert_eq!((a.block_size, a.pi_type, a.blocks), (4096, 1, BLOCKS_4K));
        assert!(lines.iter().any(|l| l.contains("formatting (fast) 31.") && l.contains("left")), "{lines:?}");
        assert!(r.summary().contains("formatted (fast)") && r.summary().contains("10.00 TB"), "{}", r.summary());
        assert_eq!(r.before.unwrap().block_size, 512);
    }

    #[test]
    fn auto_falls_back_to_full_when_fast_is_refused() {
        let f = Fake { refuse_ffmt: true, ..Fake::at(512, 0) };
        let (r, _) = run(&f, PI4K, Method::Auto);
        assert_eq!(r.outcome, Outcome::Formatted, "{r:?}");
        assert_eq!(r.method_used, Some("full"));
        assert!(r.fast_skipped.as_deref().unwrap().contains("field pointer"), "{r:?}");
        assert_eq!(f.sent(), vec!["select 4096", "rsoc", "ffmt1 immed=true fmtpinfo=2", "format immed=true fmtpinfo=2"]);
    }

    #[test]
    fn auto_skips_fast_when_the_drive_lists_no_ffmt() {
        let f = Fake { ffmt: Some(false), ..Fake::at(512, 0) };
        let (r, _) = run(&f, (4096, Protection::None), Method::Auto);
        assert_eq!(r.outcome, Outcome::Formatted, "{r:?}");
        assert_eq!(r.method_used, Some("full"));
        assert_eq!(f.sent(), vec!["select 4096", "rsoc", "format immed=true fmtpinfo=0"]);
    }

    #[test]
    fn fast_refused_formats_nothing_and_puts_the_length_back() {
        let f = Fake { refuse_ffmt: true, ..Fake::at(512, 0) };
        let (r, _) = run(&f, PI4K, Method::Fast);
        assert_eq!(r.outcome, Outcome::Refused);
        assert_eq!(r.exit_code, 3);
        assert_eq!(f.sent(), vec!["select 4096", "ffmt1 immed=true fmtpinfo=2", "select 512"]);
        assert!(r.summary().contains("nothing was formatted"));
    }

    #[test]
    fn already_at_the_format_sends_nothing() {
        let f = Fake::at(4096, 1);
        for m in [Method::Auto, Method::None] {
            let (r, _) = run(&f, PI4K, m);
            assert_eq!(r.outcome, Outcome::Already);
            assert_eq!(r.exit_code, 4);
        }
        assert!(f.sent().is_empty(), "{:?}", f.sent());
        // Asked for a full format: formatted anyway.
        let (r, _) = run(&f, PI4K, Method::Full);
        assert_eq!(r.outcome, Outcome::Formatted);
        assert_eq!(f.sent(), vec!["select 4096", "format immed=true fmtpinfo=2"]);
    }

    #[test]
    fn none_says_not_at_the_format() {
        let f = Fake::at(512, 1);
        let (r, _) = run(&f, PI4K, Method::None);
        assert_eq!((r.outcome, r.exit_code), (Outcome::NotAtFormat, 5));
        assert!(f.sent().is_empty());
        // Same PI, other length; same length, other PI.
        let (r, _) = run(&Fake::at(4096, 0), PI4K, Method::None);
        assert_eq!(r.outcome, Outcome::NotAtFormat);
    }

    #[test]
    fn a_refused_mode_select_names_the_field() {
        let f = Fake { refuse_select: true, ..Fake::at(512, 0) };
        let (r, _) = run(&f, PI4K, Method::Auto);
        assert_eq!(r.outcome, Outcome::Refused);
        assert!(r.error.as_deref().unwrap().contains("field pointer"), "{r:?}");
        assert!(!f.sent().iter().any(|l| l.contains("format") || l.contains("ffmt")), "{:?}", f.sent());
    }

    #[test]
    fn refused_pi_on_the_full_format() {
        let f = Fake { ffmt: Some(false), refuse_pi: true, ..Fake::at(512, 0) };
        let (r, _) = run(&f, PI4K, Method::Full);
        assert_eq!(r.outcome, Outcome::Refused);
        assert_eq!(f.sent(), vec!["select 4096", "format immed=true fmtpinfo=2", "select 512"]);
    }

    #[test]
    fn a_read_back_not_as_asked_fails() {
        let f = Fake { after: Some((512, 1)), ..Fake::at(512, 0) };
        let (r, _) = run(&f, PI4K, Method::Auto);
        assert_eq!((r.outcome, r.exit_code), (Outcome::Failed, 1));
        assert!(r.error.as_deref().unwrap().contains("512-byte blocks"), "{r:?}");
    }

    #[test]
    fn an_interrupted_format_is_formatted_not_already() {
        let f = Fake { before: None, ..Fake::at(512, 0) };
        let (r, lines) = run(&f, PI4K, Method::Auto);
        assert_eq!(r.outcome, Outcome::Formatted, "{r:?}");
        assert!(r.before.is_none());
        assert!(lines.iter().any(|l| l.contains("interrupted format")));
        let (r, _) = run(&Fake { before: None, ..Fake::at(512, 0) }, PI4K, Method::None);
        assert_eq!(r.outcome, Outcome::NotAtFormat);
    }

    /// #142: forge1's NOT READY drives are formatted, not given up on.
    #[test]
    fn not_ready_is_formatted() {
        for (asc, ascq) in [(0x04, 0x00), (0x04, 0x03), (0x04, 0x11)] {
            let f = Fake::not_ready(asc, ascq);
            let (r, lines) = run(&f, PI4K, Method::Auto);
            assert_eq!(r.outcome, Outcome::Formatted, "{asc:02x}/{ascq:02x}: {r:?}");
            assert!(r.before.is_none());
            assert_eq!(f.sent(), vec!["select 4096", "rsoc", "ffmt1 immed=true fmtpinfo=2"]);
            assert!(lines.iter().any(|l| l.contains("not ready; formatting it")), "{lines:?}");
        }
        let (r, _) = run(&Fake::not_ready(0x04, 0x00), PI4K, Method::None);
        assert_eq!(r.outcome, Outcome::NotAtFormat);
    }

    #[test]
    fn not_ready_04_02_gets_a_start_unit_first() {
        // Ready after START UNIT: reads 512, so it is formatted to 4096.
        let f = Fake { before: Some((512, 0)), starts: true, ..Fake::not_ready(0x04, 0x02) };
        let (r, lines) = run(&f, PI4K, Method::Auto);
        assert_eq!(r.outcome, Outcome::Formatted, "{r:?}");
        assert_eq!(r.before.unwrap().block_size, 512);
        assert_eq!(f.sent(), vec!["start", "select 4096", "rsoc", "ffmt1 immed=true fmtpinfo=2"]);
        assert!(lines.iter().any(|l| l.contains("START UNIT first")), "{lines:?}");
        // Still not ready after it: formatted all the same, one START UNIT.
        let f = Fake::not_ready(0x04, 0x02);
        let (r, lines) = run(&f, PI4K, Method::Auto);
        assert_eq!(r.outcome, Outcome::Formatted, "{r:?}");
        assert_eq!(f.sent(), vec!["start", "select 4096", "rsoc", "ffmt1 immed=true fmtpinfo=2"]);
        assert!(lines.iter().any(|l| l.contains("still not ready")), "{lines:?}");
    }

    #[test]
    fn a_format_in_progress_is_waited_for_not_stopped() {
        let f = Fake { before: Some((512, 0)), running: Cell::new(3), ..Fake::not_ready(0x04, 0x04) };
        let (r, lines) = run(&f, PI4K, Method::Auto);
        assert_eq!(r.outcome, Outcome::Formatted, "{r:?}");
        assert_eq!(r.before.unwrap().block_size, 512, "read again once the running format ended");
        // Nothing was sent while it ran: the first command is the new format's.
        assert_eq!(f.sent(), vec!["select 4096", "rsoc", "ffmt1 immed=true fmtpinfo=2"]);
        assert!(lines.iter().any(|l| l.contains("a format is running; waiting")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("formatting (the drive's own) 50.")), "{lines:?}");
        // A running format to what is wanted ends Already, nothing sent.
        let f = Fake { before: Some((4096, 1)), running: Cell::new(2), ..Fake::not_ready(0x04, 0x04) };
        let (r, _) = run(&f, PI4K, Method::Auto);
        assert_eq!(r.outcome, Outcome::Already, "{r:?}");
        assert!(f.sent().is_empty());
    }

    #[test]
    fn another_sense_key_still_fails() {
        // A hardware error to READ CAPACITY is not one a format cures.
        let f = Fake { key: 4, ..Fake::not_ready(0x44, 0x00) };
        let (r, _) = run(&f, PI4K, Method::Auto);
        assert_eq!(r.outcome, Outcome::Failed, "{r:?}");
        assert!(r.error.as_deref().unwrap().starts_with("read capacity:"), "{r:?}");
        assert!(f.sent().is_empty());
    }

    #[test]
    fn mounts_and_words() {
        let m = "/dev/sda2 / ext4 rw 0 0\n/dev/nvme0n1p1 /boot vfat rw 0 0\nproc /proc proc rw 0 0\n";
        assert_eq!(mounted_on(m, "sda").as_deref(), Some("/dev/sda2 on /"));
        assert_eq!(mounted_on(m, "sd"), None);
        assert_eq!(mounted_on(m, "sdd"), None);
        assert_eq!(mounted_on(m, "nvme0n1").as_deref(), Some("/dev/nvme0n1p1 on /boot"));
        assert_eq!(mounted_on(m, "nvme0n"), None);
        assert_eq!(human_secs(59), "59s");
        assert_eq!(human_secs(3725), "1h 02m");
        assert_eq!(human_bytes(10_000_831_348_736), "10.00 TB");
        assert!(progress_line("sdd", "full", 600, Some(0x8000)).contains("50.0%, about 10m 00s left"));
        assert!(progress_line("sdd", "full", 30, None).contains("no progress"));
        let codes: Vec<i32> = [Outcome::Formatted, Outcome::Failed, Outcome::Refused, Outcome::Already, Outcome::NotAtFormat].iter().map(|o| o.exit_code()).collect();
        assert_eq!(codes, vec![0, 1, 3, 4, 5]);
    }
}
