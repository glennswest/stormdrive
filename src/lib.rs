//! StormDrive — physical drive management for the Storm ecosystem.
//!
//! The layer below stormblock: discovery, health/wear/thermal monitoring,
//! physical location (HBA, shelf, bay, PCIe slot), drive tests, sector-size
//! reformat, firmware updates, and the hand-off of drives to stormblock
//! (labels, health, drains, overcommit). See README.md and
//! docs/architecture.md.

pub mod api;
pub mod components;
pub mod config;
pub mod contents;
pub mod discovery;
pub mod drive;
pub mod drivetest;
pub mod events;
pub mod firmware;
pub mod fleet;
pub mod format;
pub mod hba;
pub mod hotplug;
pub mod inventory;
pub mod monitor;
pub mod placement;
pub mod poller;
pub mod scsi;
pub mod ses;
pub mod smart;
pub mod stormblock;
pub mod topology;
pub mod usage;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
