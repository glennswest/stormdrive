//! StormDrive — physical drive management for the Storm ecosystem.
//!
//! The layer below stormblock: discovery, health/wear/thermal monitoring,
//! physical location (HBA, shelf, bay, PCIe slot), drive tests, sector-size
//! reformat, firmware updates, and the hand-off of drives to stormblock
//! (labels, health, drains, overcommit). See README.md and
//! docs/architecture.md.

// The kube Drive status is one big json! (#65 outgrew the default 128).
#![recursion_limit = "256"]

pub mod api;
pub mod assets;
pub mod bays;
pub mod components;
pub mod config;
pub mod contents;
pub mod controller;
pub mod discovery;
pub mod drive;
pub mod drivetest;
pub mod engine;
pub mod erase;
pub mod events;
pub mod firmware;
pub mod fleet;
pub mod format;
pub mod formatcli;
pub mod gpt;
pub mod hba;
pub mod history;
pub mod hotplug;
pub mod inuse;
pub mod inventory;
pub mod iomfw;
pub mod kubeapi;
pub mod kubeauth;
pub mod megaraid;
pub mod metrics;
pub mod monitor;
pub mod placement;
pub mod pi;
pub mod policy;
pub mod poller;
pub mod raidctl;
pub mod scsi;
pub mod ses;
pub mod shelfobj;
pub mod shelfview;
pub mod smart;
pub mod stormblock;
pub mod system;
pub mod thermal;
pub mod tls;
pub mod topology;
pub mod transport;
pub mod usage;
pub mod wear;
pub mod worker;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
