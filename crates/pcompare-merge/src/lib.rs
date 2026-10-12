//! 3-way (and 2-way) text merge engine for PCompare.
//!
//! [`merge3`] aligns two descendants (left and right) against their common
//! ancestor (base) and splits the files into ordered [`MergeChunk`]s. Changes
//! made on only one side, or identically on both, are resolved automatically;
//! overlapping, different changes become conflicts. [`render_output`] turns a
//! result into merged text with conflict markers that the UI (or a mergetool
//! caller) can resolve. [`merge2`] merges two files without an ancestor:
//! every difference is a conflict. [`merge_text`] runs either on whole texts.
//!
//! Like the rest of PCompare's engines this crate is UI-agnostic and every
//! result type is `serde`-serializable.

pub mod diff3;
pub mod lines;

pub use diff3::{
    conflict_end_marker, conflict_start_marker, merge2, merge3, merge_text, merge_trailing_newline, render_output, ChunkKind, LineRange, MergeChunk, MergeResult, MergeStats, MergedText,
    CONFLICT_END, CONFLICT_SEPARATOR, CONFLICT_START,
};
pub use lines::{join_lines, split_lines};
