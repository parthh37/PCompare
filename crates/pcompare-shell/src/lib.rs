//! Explorer integration: the right-click menu PCompare registers for itself
//! (per user, under HKCU, so it works for the NSIS installer, the MSI and a
//! portable copy alike), and the spool that turns one-process-per-item
//! launches from a multi-selection into a single comparison.

pub mod menu;
pub mod selection;
