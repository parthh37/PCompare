//! Folder sync and 3-way folder merge engines for PCompare.
//!
//! Like `pcompare-core`, this crate is UI-agnostic: plans, comparison
//! results and reports are `serde`-serializable for the Tauri shell, and the
//! executors take a [`fsops::Deleter`] so tests can delete permanently while
//! the app moves items to the Recycle Bin / Trash or a versioning folder.
//!
//! Sync is split into planning ([`sync`]), safe execution with retries,
//! locked-file handling and rollback ([`exec`]), the last-sync [`baseline`]
//! that enables true two-way sync, and the [`journal`] hooks the app uses to
//! make runs crash-resumable.

pub mod baseline;
pub mod exec;
pub mod failure;
pub mod fileops; // bc5:file-ops
pub mod fsops;
pub mod journal;
pub mod matrix; // bc5:sync-matrix
pub mod merge;
pub mod remote; // bc5:remote-write
pub mod sync;
