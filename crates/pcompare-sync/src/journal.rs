//! Hooks through which the executor records its progress durably.
//!
//! The app persists these in its database so a sync interrupted by a crash,
//! power loss or a killed process can be inspected, resumed or rolled back.
//! Operations are idempotent (copying again yields the same file, deleting
//! something already gone succeeds), so a lost "finished" record only costs
//! a redo. The one record that must be durable *before* the executor moves
//! on is [`Journal::backup_staged`]: between moving the old destination
//! aside and putting the new file in place, the journal is the only thing
//! that knows where the old version went.

use std::path::Path;

use crate::exec::OpResult;

pub trait Journal {
    /// An attempt at `op_id` is about to start (`attempt` counts from 1).
    fn op_started(&mut self, op_id: u32, attempt: u32);
    /// The existing destination of `op_id` was renamed to `staged`. Must be
    /// durable when this returns.
    fn backup_staged(&mut self, op_id: u32, staged: &Path);
    /// The final outcome of `op_id`, after any retries.
    fn op_finished(&mut self, result: &OpResult);
    /// Called once when the run ends; flush anything buffered.
    fn flush(&mut self) {}
}

/// A journal that remembers nothing (tests, one-off runs).
pub struct NullJournal;

impl Journal for NullJournal {
    fn op_started(&mut self, _: u32, _: u32) {}
    fn backup_staged(&mut self, _: u32, _: &Path) {}
    fn op_finished(&mut self, _: &OpResult) {}
}

/// Keeps every record in memory; useful for tests and for inspecting a run.
#[derive(Debug, Default)]
pub struct MemoryJournal {
    pub started: Vec<(u32, u32)>,
    pub staged: Vec<(u32, std::path::PathBuf)>,
    pub finished: Vec<OpResult>,
}

impl Journal for MemoryJournal {
    fn op_started(&mut self, op_id: u32, attempt: u32) {
        self.started.push((op_id, attempt));
    }
    fn backup_staged(&mut self, op_id: u32, staged: &Path) {
        self.staged.push((op_id, staged.to_path_buf()));
    }
    fn op_finished(&mut self, result: &OpResult) {
        self.finished.push(result.clone());
    }
}
