//! Ship source snapshot packing and safe unpacking; later git and image
//! sources.
//!
//! Leaf crate: depends only on `ship-core` (OVERVIEW §2.3).
//!
//! Packing (ADR-0005, client side): [`scan`] selects a working tree's files
//! under its ignore rules and the default exclusions, refusing a home
//! directory, a filesystem root or a tree over the limits; the
//! [`Selection`] gives the `--dry-run` report; [`pack`] streams it as a
//! deterministic tar + zstd archive with the SHA-256 of the stream.
//!
//! Unpacking (daemon side, unix only): [`unpack`] validates every entry of
//! an uploaded stream as it arrives and writes only into a staging
//! directory it creates, removing it again on any failure.

mod exclude;
#[cfg(unix)]
#[doc(hidden)]
pub mod fuzzing;
mod pack;
mod scan;
#[cfg(unix)]
mod unpack;

pub use exclude::Exclusion;
pub use pack::{PackError, Packed, ZSTD_LEVEL, pack};
pub use scan::{
    DryRunReport, Entry, EntryKind, Excluded, IGNORE_FILES, Limits, Options, ScanError, Selection,
    SkipReason, Skipped, scan,
};
#[cfg(unix)]
pub use unpack::{
    Limit, MAX_COMPONENT_BYTES, MAX_PATH_BYTES, MAX_TRAILER_BYTES, UnpackError, UnpackLimits,
    Unpacked, UnsafeReason, ZSTD_WINDOW_LOG_MAX, collision_key, unpack,
};
