//! Glob latest-mtime aggregates backed by the VFS index.
//!
//! A glob query records the set of matching file ids plus an ordered view by
//! modification time. Warm queries can answer directly from that aggregate.
//! When a known matching file is marked dirty, hybrid mode repairs only that
//! file's metadata instead of rescanning directories.

use std::{
    collections::{BTreeSet, HashSet},
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use glob::{MatchOptions, Pattern};

use crate::{
    VfsError,
    index::{FileId, Index, VfsEntryKind},
    state::IndexedVfs,
    walk::parallel::{WalkOutcome, WalkPlan, walk_latest_mtime},
    walk::{WalkDiagnostics, WalkMode},
};

/// Newest modification time for paths matched by a glob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GlobMTime {
    /// The query matched no files.
    NoMatches,
    /// At least one file matched.
    MatchesFound {
        /// Newest modification timestamp among all matching files.
        modified_at: SystemTime,
        /// Deterministic representative file for `modified_at`.
        ///
        /// If several files share the newest timestamp, the lexicographically
        /// smallest path is chosen.
        designated_file: PathBuf,
    },
}

/// One active glob aggregate changed after an index update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobQueryChange {
    /// Query root whose aggregate changed.
    pub root: PathBuf,
    /// Glob pattern whose aggregate changed.
    pub pattern: String,
    /// Aggregate value before the index update.
    pub previous: GlobMTime,
    /// Aggregate value after the index update.
    pub current: GlobMTime,
}

/// One active glob aggregate was invalidated and should be recomputed lazily.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobQueryInvalidation {
    /// Query root whose aggregate was invalidated.
    pub root: PathBuf,
    /// Glob pattern whose aggregate was invalidated.
    pub pattern: String,
}

/// Result of refreshing one changed filesystem path in the index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VfsPathRefresh {
    /// Active file-level glob aggregates updated incrementally.
    pub glob_changes: Vec<GlobQueryChange>,
    /// Active subtree-level glob aggregates dropped and dirtied for lazy recompute.
    pub invalidated_globs: Vec<GlobQueryInvalidation>,
}

/// Diagnostic timings and counters for one latest-mtime request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LatestMTimeDiagnostics {
    /// Total wall time for the public request.
    pub total: Duration,
    /// Whether the request was answered without a disk or index walk.
    pub cache_hit: bool,
    /// Detailed walk diagnostics. This is zeroed for pure cache hits.
    pub walk: WalkDiagnostics,
}

impl IndexedVfs {
    /// Compute the latest mtime for `pattern` under `root` using the requested
    /// walk mode.
    ///
    /// The first successful query stores an active aggregate. Later calls in
    /// [`WalkMode::Hybrid`] or [`WalkMode::IndexOnly`] can reuse that aggregate
    /// when all matched files are clean.
    pub fn latest_mtime(
        &self,
        root: impl AsRef<Path>,
        pattern: impl AsRef<str>,
        mode: WalkMode,
    ) -> Result<GlobMTime, VfsError> {
        self.latest_mtime_inner(root, pattern, mode, false)
            .map(|(value, _)| value)
    }

    /// Compute the latest mtime and return diagnostic phase timings/counters.
    ///
    /// This is intended for tests and performance investigations; production
    /// callers should usually use [`IndexedVfs::latest_mtime`].
    pub fn latest_mtime_with_diagnostics(
        &self,
        root: impl AsRef<Path>,
        pattern: impl AsRef<str>,
        mode: WalkMode,
    ) -> Result<(GlobMTime, LatestMTimeDiagnostics), VfsError> {
        self.latest_mtime_inner(root, pattern, mode, true)
    }

    fn latest_mtime_inner(
        &self,
        root: impl AsRef<Path>,
        pattern: impl AsRef<str>,
        mode: WalkMode,
        collect_diagnostics: bool,
    ) -> Result<(GlobMTime, LatestMTimeDiagnostics), VfsError> {
        let total_start = Instant::now();
        let root = root.as_ref().to_path_buf();
        let pattern = pattern.as_ref().to_owned();
        let query_key = GlobQueryKey::new(root.clone(), pattern.clone());

        if mode != WalkMode::ForceDisk {
            // Prefer the maintained aggregate. ForceDisk intentionally bypasses
            // it so callers can repair or benchmark a fresh disk walk.
            if let Some(result) = self.try_cached_query(&query_key, mode)? {
                return Ok((
                    result,
                    LatestMTimeDiagnostics {
                        total: total_start.elapsed(),
                        cache_hit: true,
                        walk: WalkDiagnostics::default(),
                    },
                ));
            }
        }

        let plan = WalkPlan::new(root, pattern)?;
        let (outcome, walk) =
            walk_latest_mtime(self.inner.clone(), plan.clone(), mode, collect_diagnostics)?;
        let current = outcome.current(&self.inner.index.lock());
        self.inner
            .index
            .lock()
            .store_query(query_key, QueryState::from_walk(plan.pattern, outcome));
        Ok((
            current,
            LatestMTimeDiagnostics {
                total: total_start.elapsed(),
                cache_hit: false,
                walk,
            },
        ))
    }

    /// Mark a known file's metadata as dirty without dirtying directory
    /// membership. The next hybrid query that depends on the file will repair
    /// it by statting just that file.
    pub fn mark_file_dirty(&self, path: impl AsRef<Path>) -> Result<(), VfsError> {
        let path = path.as_ref();
        let mut index = self.inner.index.lock();
        let Some(file) = index.file_id_for_path(path) else {
            return Err(VfsError::IndexMiss {
                needed: "known file",
                path: path.to_path_buf(),
            });
        };
        index.file_mut(file).dirty = true;
        Ok(())
    }

    /// Refresh one file from the backend and update active glob aggregates.
    ///
    /// This is the watcher/invalidation fast path for existing file content or
    /// metadata changes that do not affect directory membership. It stats only
    /// `path`, updates the indexed file record, and then adjusts any active glob
    /// query rooted above that path.
    pub fn refresh_file(&self, path: impl AsRef<Path>) -> Result<Vec<GlobQueryChange>, VfsError> {
        self.refresh_path_inner(path, false)
            .map(|refresh| refresh.glob_changes)
    }

    /// Refresh a path-level change from the backend.
    ///
    /// Use this for create/delete/rename/kind changes. File changes can update
    /// active glob aggregates incrementally; directory changes conservatively
    /// drop affected active glob aggregates so the next query repairs them with
    /// a fresh walk.
    pub fn refresh_path(&self, path: impl AsRef<Path>) -> Result<VfsPathRefresh, VfsError> {
        self.refresh_path_inner(path, true)
    }

    fn refresh_path_inner(
        &self,
        path: impl AsRef<Path>,
        path_level: bool,
    ) -> Result<VfsPathRefresh, VfsError> {
        let path = path.as_ref().to_path_buf();
        let metadata = match self.inner.read_metadata(&path) {
            Ok(metadata) => Some(metadata),
            Err(VfsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };

        let mut index = self.inner.index.lock();
        if path_level {
            index.mark_directory_listing_unknown(&path);
            if let Some(parent) = path.parent() {
                index.mark_directory_listing_unknown(parent);
            }
        }

        let was_dir = index.dir_id_for_path(&path).is_some();
        let is_dir = metadata
            .as_ref()
            .is_some_and(|metadata| VfsEntryKind::from(metadata.kind) == VfsEntryKind::Directory);
        let directory_change = path_level && (was_dir || is_dir);

        let refreshed = if let Some(metadata) = metadata {
            let kind: VfsEntryKind = metadata.kind.into();
            if kind == VfsEntryKind::Directory {
                let removed_file = index.remove_file_path(&path).map(|file| (file, false));
                index.upsert_query_dir(path.clone());
                removed_file
            } else {
                let file = index.upsert_file_metadata(
                    path.clone(),
                    kind,
                    Some(metadata.len),
                    metadata.modified,
                    true,
                );
                Some((file, true))
            }
        } else {
            index.remove_file_path(&path).map(|file| (file, false))
        };

        if directory_change {
            let keys = index.query_keys_for_path(&path);
            let invalidated_globs = keys
                .iter()
                .map(|key| GlobQueryInvalidation {
                    root: key.root.clone(),
                    pattern: key.pattern.clone(),
                })
                .collect();
            for key in keys {
                index.remove_query(&key);
            }
            return Ok(VfsPathRefresh {
                glob_changes: Vec::new(),
                invalidated_globs,
            });
        }

        let mut glob_changes = Vec::new();
        // Clone the affected query keys first so each aggregate can be updated
        // while holding a mutable borrow of the index.
        let keys = index.query_keys_for_path(&path);
        for key in keys {
            let previous = index
                .query(&key)
                .expect("query key came from active query map")
                .current(&index);
            let matches = refreshed.is_some_and(|(_, exists)| {
                exists
                    && glob_matches(
                        &key.root,
                        &path,
                        index.query(&key).expect("query exists").pattern(),
                    )
            });
            let entry = refreshed
                .and_then(|(file, exists)| exists.then_some(file))
                .and_then(|file| MatchEntry::from_file(file, &index));
            let query = index.query_mut(&key).expect("query exists");
            match refreshed {
                Some((file, _)) if matches => query.upsert_match_entry(file, entry),
                Some((file, _)) => query.remove_match_by_id(file),
                None => {}
            }
            let current = index.query(&key).expect("query exists").current(&index);
            if previous != current {
                glob_changes.push(GlobQueryChange {
                    root: key.root.clone(),
                    pattern: key.pattern.clone(),
                    previous,
                    current,
                });
            }
        }
        Ok(VfsPathRefresh {
            glob_changes,
            invalidated_globs: Vec::new(),
        })
    }

    fn try_cached_query(
        &self,
        key: &GlobQueryKey,
        mode: WalkMode,
    ) -> Result<Option<GlobMTime>, VfsError> {
        let dirty_files = {
            let index = self.inner.index.lock();
            let Some(query) = index.query(key) else {
                return Ok(None);
            };
            if query.dirty {
                return Ok(None);
            }
            let dirty_files = query.dirty_files(&index);
            if dirty_files.is_empty() {
                return Ok(Some(query.current(&index)));
            }
            if mode == WalkMode::IndexOnly {
                return Err(VfsError::IndexMiss {
                    needed: "clean file metadata",
                    path: index.file_path(dirty_files[0]).to_path_buf(),
                });
            }
            dirty_files
        };

        // Drop the index lock while repairing dirty files from the backend.
        for file in dirty_files {
            let path = self.inner.index.lock().file_path(file).to_path_buf();
            self.refresh_file(path)?;
        }

        let index = self.inner.index.lock();
        Ok(index.query(key).map(|query| query.current(&index)))
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct GlobQueryKey {
    /// Root directory supplied by the caller.
    pub(crate) root: PathBuf,
    /// Original pattern text. The compiled pattern lives in `QueryState`.
    pub(crate) pattern: String,
}

impl GlobQueryKey {
    fn new(root: PathBuf, pattern: String) -> Self {
        Self { root, pattern }
    }
}

pub(crate) struct QueryState {
    /// Compiled pattern used for incremental file refresh checks.
    pattern: Pattern,
    /// File ids currently matching this query.
    matched_files: HashSet<FileId>,
    /// Matching files ordered by `(modified_at, path, file_id)`.
    max_by_mtime: BTreeSet<MatchEntry>,
    /// Reserved for future query-wide invalidation. Dirty files are tracked on
    /// individual file records today.
    dirty: bool,
}

impl QueryState {
    fn from_walk(pattern: Pattern, outcome: WalkOutcome) -> Self {
        Self {
            pattern,
            matched_files: outcome.matched_files,
            max_by_mtime: outcome.max_by_mtime,
            dirty: false,
        }
    }

    fn pattern(&self) -> &Pattern {
        &self.pattern
    }

    fn dirty_files(&self, index: &Index) -> Vec<FileId> {
        self.matched_files
            .iter()
            .copied()
            .filter(|file| index.file(*file).dirty)
            .collect()
    }

    fn upsert_match_entry(&mut self, file: FileId, entry: Option<MatchEntry>) {
        // Remove before insert so timestamp or path changes cannot leave a
        // stale ordering entry behind.
        self.remove_match_by_id(file);
        if let Some(entry) = entry {
            self.matched_files.insert(file);
            self.max_by_mtime.insert(entry);
        }
    }

    fn remove_match_by_id(&mut self, file: FileId) {
        self.matched_files.remove(&file);
        self.max_by_mtime.retain(|entry| entry.file != file);
    }

    fn current(&self, index: &Index) -> GlobMTime {
        latest_from_entries(&self.max_by_mtime, index)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct MatchEntry {
    /// First ordering key: newest mtime wins via `BTreeSet::next_back`.
    modified_at: SystemTime,
    /// Tie-breaker used to choose a deterministic designated file.
    path: PathBuf,
    /// Stable file id lets incremental refresh remove stale entries.
    file: FileId,
}

impl MatchEntry {
    pub(crate) fn new(modified_at: SystemTime, path: PathBuf, file: FileId) -> Self {
        Self {
            modified_at,
            path,
            file,
        }
    }

    pub(crate) fn from_file(file: FileId, index: &Index) -> Option<Self> {
        let record = index.file(file);
        Some(Self::new(
            record.modified?,
            index.path(record.path).to_path_buf(),
            file,
        ))
    }
}

pub(crate) fn latest_from_entries(entries: &BTreeSet<MatchEntry>, _index: &Index) -> GlobMTime {
    let Some(max_time) = entries.iter().next_back().map(|entry| entry.modified_at) else {
        return GlobMTime::NoMatches;
    };
    let designated = entries
        .iter()
        .filter(|entry| entry.modified_at == max_time)
        .min_by(|a, b| a.path.cmp(&b.path))
        .expect("max_time came from set");
    GlobMTime::MatchesFound {
        modified_at: designated.modified_at,
        designated_file: designated.path.clone(),
    }
}

/// Match a path against a glob pattern using slash-separated path components.
pub(crate) fn glob_matches(root: &Path, path: &Path, pattern: &Pattern) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let relative = relative
        .iter()
        .map(|component| component.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    pattern.matches_with(
        &relative,
        MatchOptions {
            require_literal_separator: true,
            ..MatchOptions::new()
        },
    )
}
