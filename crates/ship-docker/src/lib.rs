//! Ship adapter for `docker` and `docker compose` subprocesses.
//!
//! Leaf crate: depends only on `ship-core` (OVERVIEW §2.3). All Docker access
//! in Ship lives here, and all of it goes through [`Runner`]: structured
//! argv, no shell, bounded output, a timeout (ADR-0001 rule 2). Outside this
//! crate a `Runner` can only be configured and handed back to it; every
//! argument vector it runs is built and checked in [`argv`], which has no
//! way to say `down -v`, `--volumes`, `volume rm` or a shell.
//!
//! - [`preflight`]: whether the Engine is reachable and whether the Engine
//!   and the Compose v2 plugin are inside [`supported`]'s ranges (ADR-0001
//!   rule 6). Read-only.
//! - [`compose`]: `config`, `build`, `up`, `stop`, `start`, `restart`,
//!   `down` (never with volumes), `ps` and `logs` on one environment's
//!   Compose project, refusing to act when the project holds an object
//!   without Ship's labels; and the override that labels everything `up`
//!   creates.
//! - [`inspect`]: Ship's containers, selected by [`labels`], read into
//!   [`ship_core::Observation`]s.
//! - [`stream`]: output delivered line by line, stdout and stderr apart.
//!
//! Docker unreachable is [`DockerError::Unreachable`], never confused with
//! a command that ran and failed (ADR-0006 rule 4). This crate logs
//! nothing; `compose config` output in particular stays inside
//! [`ComposeConfig`] (ADR-0008 rule 7).

pub mod argv;
pub mod compose;
pub mod error;
pub mod inspect;
pub mod labels;
pub mod preflight;
pub mod runner;
pub mod stream;
pub mod supported;

pub use argv::{Argv, ComposeFiles, ComposeProject, LogOptions, UpOptions};
pub use compose::{ComposeConfig, DeployLabels, labels_override};
pub use error::DockerError;
pub use inspect::{ContainerObservation, containers};
pub use labels::{Selector, ShipLabels};
pub use preflight::preflight;
pub use runner::{Output, RunError, Runner};
pub use stream::{Line, Stream};
pub use supported::{Version, VersionRange};
