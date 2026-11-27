//! Ship adapter for `docker` and `docker compose` subprocesses: inspect and
//! events.
//!
//! Leaf crate: depends only on `ship-core` (OVERVIEW §2.3). All Docker access
//! in Ship lives here, and all of it goes through [`Runner`]: structured
//! argv, no shell, bounded output, a timeout (ADR-0001 rule 2).
//!
//! Phase 0: [`preflight`] reports whether the Engine is reachable and whether
//! the Engine and the Compose v2 plugin are inside [`supported`]'s ranges
//! (ADR-0001 rule 6). It only reads; it creates, changes and removes nothing.

pub mod preflight;
pub mod runner;
pub mod supported;

pub use preflight::preflight;
pub use runner::{Output, RunError, Runner};
pub use supported::{Version, VersionRange};
