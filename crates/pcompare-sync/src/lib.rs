//! Folder sync and 3-way folder merge engines for PCompare.
//!
//! Like `pcompare-core`, this crate is UI-agnostic: plans, comparison
//! results and reports are `serde`-serializable for the Tauri shell, and the
//! executors take a [`fsops::Deleter`] so tests can delete permanently while
//! the app moves items to the Recycle Bin / Trash.

pub mod fsops;
pub mod merge;
pub mod sync;
