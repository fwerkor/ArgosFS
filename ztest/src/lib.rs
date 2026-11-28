//! Support library for the ArgosFS ztest harness.
//!
//! The crate is intentionally outside the main Cargo workspace so it can be
//! split into its own package or repository without touching ArgosFS internals.

pub mod atomic;
pub mod combinations;
pub mod fault;
pub mod invariants;
pub mod model;
pub mod oracle;
pub mod supervisor;
pub mod topology;
pub mod workload;

pub use fault::{FaultBackend, FaultConfig, FaultStats, FaultStatsSnapshot};
