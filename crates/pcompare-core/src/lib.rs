//! Comparison engines for PCompare.
//!
//! The crate is UI-agnostic: every result type is `serde`-serializable so the
//! Tauri shell can hand it straight to the frontend, and every engine can be
//! exercised from plain unit tests.

pub mod error;
pub mod folder;
pub mod grammar; // bc5:text-engine
pub mod hex;
pub mod hex_align; // bc5:hex
pub mod hex_edit; // bc5:hex
pub mod hex_find; // bc5:hex
pub mod patch; // bc5:editor-patch
pub mod secrets;
pub mod session;
pub mod store;
pub mod text;
pub mod textfile;
pub mod vfs;
pub mod vfs_write; // bc5:remote-write
pub mod workspace; // bc5:app-shell

pub use error::{CoreError, Result};

use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PathKind {
    File,
    Directory,
    Missing,
}

/// Classifies a path so callers can pick the right comparison session.
pub fn path_kind(path: impl AsRef<Path>) -> PathKind {
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => PathKind::Directory,
        Ok(_) => PathKind::File,
        Err(_) => PathKind::Missing,
    }
}
