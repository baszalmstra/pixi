//! Glob aggregate commit helpers.
//!
//! The rich glob-set walker in `glob.rs` performs traversal and then commits a
//! compact delta into the indexed VFS in one lock-held phase. This module owns
//! the shared delta/outcome types so the index update logic stays separate from
//! matching semantics.

use std::{
    collections::{BTreeSet, HashSet},
    path::PathBuf,
    time::{Instant, SystemTime},
};

use crate::{
    GlobMTime, Inner, MatchEntry, WalkDiagnostics,
    index::{FileId, VfsEntryKind},
    latest_from_entries,
};

pub(crate) struct WalkOutcome {
    pub(crate) matched_files: HashSet<FileId>,
    pub(crate) max_by_mtime: BTreeSet<MatchEntry>,
}

impl WalkOutcome {
    pub(crate) fn new() -> Self {
        Self {
            matched_files: HashSet::new(),
            max_by_mtime: BTreeSet::new(),
        }
    }

    pub(crate) fn current(&self, index: &crate::index::Index) -> GlobMTime {
        latest_from_entries(&self.max_by_mtime, index)
    }
}

#[derive(Default)]
pub(crate) struct DiskDelta {
    /// Directories that were actually visited by the backend walk. They are
    /// committed as unknown listings because the glob walk may intentionally
    /// skip non-matching files.
    pub(crate) visited_dirs: Vec<PathBuf>,
    /// Files that matched the query and should be stored in the query index.
    pub(crate) matched_files: Vec<MatchedFileDelta>,
}

pub(crate) struct MatchedFileDelta {
    pub(crate) path: PathBuf,
    pub(crate) kind: VfsEntryKind,
    pub(crate) size: Option<u64>,
    pub(crate) modified: Option<SystemTime>,
}

pub(crate) fn commit_disk_delta(
    inner: &Inner,
    mut delta: DiskDelta,
    collect_diagnostics: bool,
    diagnostics: &mut WalkDiagnostics,
) -> WalkOutcome {
    let commit_start = collect_diagnostics.then(Instant::now);
    let sort_start = collect_diagnostics.then(Instant::now);
    // Directories must be inserted parent-before-child so parent links can be
    // filled while preserving stable ids. Matched files are not sorted here:
    // each directory entry is visited once, and avoiding a global file sort is
    // important for cold-populate performance.
    delta
        .visited_dirs
        .sort_by_key(|path| path.components().count());
    delta.visited_dirs.dedup();
    if let Some(sort_start) = sort_start {
        diagnostics.sort_dedup = sort_start.elapsed();
        diagnostics.dirs_indexed = delta.visited_dirs.len();
        diagnostics.files_indexed = delta.matched_files.len();
    }

    let mut index = inner.index.lock();
    let commit_dirs_start = collect_diagnostics.then(Instant::now);
    for dir in delta.visited_dirs {
        index.upsert_query_dir(dir);
    }
    if let Some(commit_dirs_start) = commit_dirs_start {
        diagnostics.commit_dirs = commit_dirs_start.elapsed();
    }

    let commit_files_start = collect_diagnostics.then(Instant::now);
    let mut outcome = WalkOutcome::new();
    for matched in delta.matched_files {
        let file = index.upsert_query_file(
            matched.path.clone(),
            matched.kind,
            matched.size,
            matched.modified,
        );
        outcome.matched_files.insert(file);
        if let Some(modified) = matched.modified {
            outcome
                .max_by_mtime
                .insert(MatchEntry::new(modified, matched.path, file));
        }
    }
    if let Some(commit_files_start) = commit_files_start {
        diagnostics.commit_files = commit_files_start.elapsed();
    }
    if let Some(commit_start) = commit_start {
        diagnostics.commit_total = commit_start.elapsed();
    }
    outcome
}
