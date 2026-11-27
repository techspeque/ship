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

mod exclude;
mod pack;
mod scan;

pub use exclude::Exclusion;
pub use pack::{PackError, Packed, ZSTD_LEVEL, pack};
pub use scan::{
    DryRunReport, Entry, EntryKind, Excluded, IGNORE_FILES, Limits, Options, ScanError, Selection,
    SkipReason, Skipped, scan,
};
