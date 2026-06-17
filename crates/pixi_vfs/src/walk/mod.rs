//! Filesystem walking support for glob aggregates.
//!
//! The walker can operate purely from indexed state, purely from the backend,
//! or in a hybrid repair mode. The public type in this module is [`WalkMode`];
//! the work-stealing implementation lives in the private `parallel` module.

pub(crate) mod parallel;

use std::time::Duration;

use crate::index::{DirId, FileId, Index, NodeId};

/// Controls where the indexed walker is allowed to read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkMode {
    /// Use indexed data when it is complete and clean; repair missing or dirty
    /// data from disk and update the index before returning.
    Hybrid,
    /// Read the relevant filesystem portions from disk and update the index.
    ForceDisk,
    /// Read only from the index. Missing or dirty indexed data is reported as
    /// an error instead of falling back to disk.
    IndexOnly,
}

/// Diagnostic timings and counters for one indexed walk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WalkDiagnostics {
    /// Total wall time spent inside the walker.
    pub total: Duration,
    /// Wall time spent traversing backend directories.
    pub disk_walk: Duration,
    /// Wall time spent traversing clean indexed directories.
    pub index_walk: Duration,
    /// Sum of backend `read_dir` durations across worker threads.
    pub read_dir_cumulative: Duration,
    /// Sum of glob matching durations across worker threads.
    pub glob_match_cumulative: Duration,
    /// Wall time spent committing disk-walk deltas into the index.
    pub commit_total: Duration,
    /// Time spent sorting/deduplicating commit inputs.
    pub sort_dedup: Duration,
    /// Time spent committing visited directories.
    pub commit_dirs: Duration,
    /// Time spent committing matched files and aggregate entries.
    pub commit_files: Duration,
    /// Number of directories visited by the walk.
    pub dirs_visited: usize,
    /// Number of directory entries observed.
    pub entries_seen: usize,
    /// Number of file-like entries checked against the glob.
    pub file_candidates: usize,
    /// Number of matching file entries observed before commit.
    pub matched_files_seen: usize,
    /// Number of directory records inserted/updated during commit.
    pub dirs_indexed: usize,
    /// Number of matching file records inserted/updated during commit.
    pub files_indexed: usize,
}

pub(crate) fn node_as_dir(index: &Index, node: NodeId) -> Option<DirId> {
    index.node_dir(node)
}

pub(crate) fn node_as_file(index: &Index, node: NodeId) -> Option<FileId> {
    index.node_file(node)
}
