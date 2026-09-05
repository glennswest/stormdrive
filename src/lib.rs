//! StormDrive — physical drive management for the Storm ecosystem.
//!
//! The layer below stormblock: discovery, health/wear/thermal monitoring,
//! physical location, firmware inventory, and the hand-off of qualified
//! drives to stormblock. See docs/architecture.md.

pub mod api;
pub mod components;
pub mod config;
pub mod discovery;
pub mod drive;
pub mod drivetest;
pub mod events;
pub mod fleet;
pub mod format;
pub mod inventory;
pub mod monitor;
pub mod scsi;
pub mod ses;
pub mod smart;
pub mod stormblock;
pub mod topology;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
