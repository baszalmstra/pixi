//! Glob latest-mtime aggregates backed by the VFS index.
//!
//! A glob query records the set of matching file ids plus an ordered view by
//! modification time. Warm queries can answer directly from that aggregate.
//! When a known matching file is marked dirty, hybrid mode repairs only that
//! file's metadata instead of rescanning directories.

use std::{
    collections::{BTreeSet, HashSet},
    io,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use glob::{MatchOptions, Pattern};
use ignore::{
    Match as IgnoreMatch,
    overrides::{Override, OverrideBuilder},
};

use crate::{
    VfsError,
    index::{FileId, Index, VfsEntryKind},
    state::IndexedVfs,
    walk::parallel::{
        DiskDelta, MatchedFileDelta, WalkOutcome, WalkPlan, commit_disk_delta, walk_latest_mtime,
    },
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

/// Ordered input-glob semantics for a VFS latest-mtime query.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct GlobSetSpec {
    /// Ordered include/exclude patterns. Leading `!` denotes an exclusion.
    pub patterns: Vec<String>,
    /// Marker file names that turn matching marker files into leaves and prune
    /// marker directories otherwise.
    pub markers: Vec<String>,
    /// Whether hidden files/directories should be excluded unless explicitly
    /// included by the pattern set.
    pub exclude_hidden: bool,
}

impl GlobSetSpec {
    /// Construct a marker-free spec with hidden entries excluded.
    pub fn new<I, P>(patterns: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<str>,
    {
        Self {
            patterns: patterns
                .into_iter()
                .map(|pattern| pattern.as_ref().to_owned())
                .collect(),
            markers: Vec::new(),
            exclude_hidden: true,
        }
    }

    /// Replace marker file names.
    pub fn with_markers<I, M>(mut self, markers: I) -> Self
    where
        I: IntoIterator<Item = M>,
        M: AsRef<str>,
    {
        self.markers = markers
            .into_iter()
            .map(|marker| marker.as_ref().to_owned())
            .collect();
        self
    }

    /// Configure hidden-file filtering.
    pub fn with_exclude_hidden(mut self, exclude_hidden: bool) -> Self {
        self.exclude_hidden = exclude_hidden;
        self
    }
}

/// Active glob aggregate identity.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum GlobSpec {
    /// A single simple glob pattern.
    Pattern(String),
    /// A rich ordered glob set.
    Set(GlobSetSpec),
}

/// One active glob aggregate changed after an index update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobQueryChange {
    /// Query root whose aggregate changed.
    pub root: PathBuf,
    /// Glob spec whose aggregate changed.
    pub spec: GlobSpec,
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
    /// Glob spec whose aggregate was invalidated.
    pub spec: GlobSpec,
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
        let query_key = GlobQueryKey::new(root.clone(), GlobSpec::Pattern(pattern.clone()));

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

        let plan = WalkPlan::new(root.clone(), pattern)?;
        let (outcome, walk) =
            walk_latest_mtime(self.inner.clone(), plan.clone(), mode, collect_diagnostics)?;
        let current = outcome.current(&self.inner.index.lock());
        self.inner.index.lock().store_query(
            query_key,
            QueryState::from_walk(
                QueryMatcher::Pattern {
                    root,
                    pattern: plan.pattern,
                },
                outcome,
            ),
        );
        Ok((
            current,
            LatestMTimeDiagnostics {
                total: total_start.elapsed(),
                cache_hit: false,
                walk,
            },
        ))
    }

    /// Compute latest mtime for an ordered Pixi-style glob set.
    ///
    /// This supports ordered include/exclude patterns, marker leaf/prune
    /// semantics, hidden filtering, and relative-pattern rebasing. Like
    /// [`Self::latest_mtime`], successful requests are stored as active
    /// aggregates for later warm reads and watcher-driven invalidation.
    pub fn latest_mtime_for_spec(
        &self,
        root: impl AsRef<Path>,
        spec: GlobSetSpec,
        mode: WalkMode,
    ) -> Result<GlobMTime, VfsError> {
        let root = root.as_ref().to_path_buf();
        let query_key = GlobQueryKey::new(root.clone(), GlobSpec::Set(spec.clone()));

        if mode != WalkMode::ForceDisk {
            if let Some(result) = self.try_cached_query(&query_key, mode)? {
                return Ok(result);
            }
        }
        if mode == WalkMode::IndexOnly {
            return Err(VfsError::IndexMiss {
                needed: "active glob-set query",
                path: root,
            });
        }

        let plan = GlobSetPlan::new(root.clone(), spec)?;
        let outcome = walk_glob_set_disk(self.inner.clone(), &plan)?;
        let current = outcome.current(&self.inner.index.lock());
        self.inner.index.lock().store_query(
            query_key,
            QueryState::from_walk(QueryMatcher::Set(plan), outcome),
        );
        Ok(current)
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
                    spec: key.spec.clone(),
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
        let mut invalidated_globs = Vec::new();
        // Clone the affected query keys first so each aggregate can be updated
        // while holding a mutable borrow of the index.
        let keys = index.query_keys_for_path(&path);
        for key in keys {
            let should_invalidate = path_level
                && index
                    .query(&key)
                    .expect("query exists")
                    .invalidate_on_path_level_change(&path);
            if should_invalidate {
                index.remove_query(&key);
                invalidated_globs.push(GlobQueryInvalidation {
                    root: key.root.clone(),
                    spec: key.spec.clone(),
                });
                continue;
            }

            let previous = index
                .query(&key)
                .expect("query key came from active query map")
                .current(&index);
            let matches = refreshed.is_some_and(|(_, exists)| {
                exists && index.query(&key).expect("query exists").matches_file(&path)
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
                    spec: key.spec.clone(),
                    previous,
                    current,
                });
            }
        }
        Ok(VfsPathRefresh {
            glob_changes,
            invalidated_globs,
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
    /// Glob identity supplied by the caller.
    pub(crate) spec: GlobSpec,
}

impl GlobQueryKey {
    fn new(root: PathBuf, spec: GlobSpec) -> Self {
        Self { root, spec }
    }
}

#[derive(Clone)]
enum QueryMatcher {
    Pattern { root: PathBuf, pattern: Pattern },
    Set(GlobSetPlan),
}

impl QueryMatcher {
    fn matches_file(&self, path: &Path) -> bool {
        match self {
            Self::Pattern { root, pattern } => glob_matches(root, path, pattern),
            Self::Set(plan) => plan.matches_file(path),
        }
    }

    fn watch_root(&self) -> &Path {
        match self {
            Self::Pattern { root, .. } => root,
            Self::Set(plan) => &plan.effective_root,
        }
    }

    fn invalidate_on_path_level_change(&self, _path: &Path) -> bool {
        matches!(self, Self::Set(plan) if !plan.markers.is_empty())
    }
}

#[derive(Clone)]
struct GlobSetPlan {
    effective_root: PathBuf,
    overrides: Override,
    markers: Vec<String>,
    skip_hidden_by_walker: bool,
    collect_patterns: bool,
}

impl GlobSetPlan {
    fn new(root: PathBuf, spec: GlobSetSpec) -> Result<Self, VfsError> {
        let has_patterns = !spec.patterns.is_empty();
        let (effective_root, globs) = if has_patterns {
            let rebased =
                WalkRoot::build(spec.patterns.iter().map(String::as_str))?.rebase(&root)?;
            (rebased.root, rebased.globs)
        } else {
            (root, Vec::new())
        };

        let glob_patterns = globs
            .iter()
            .map(|glob| anchor_literal_pattern(glob.to_pattern()))
            .collect::<Vec<_>>();
        let hidden_patterns = spec
            .exclude_hidden
            .then(|| set_ignore_hidden_patterns(&glob_patterns))
            .flatten();

        let mut builder = OverrideBuilder::new(&effective_root);
        for pattern in &glob_patterns {
            builder.add(pattern).map_err(glob_set_build_error)?;
        }
        let skip_hidden_by_walker = if !spec.exclude_hidden {
            false
        } else if let Some(patterns) = hidden_patterns {
            for pattern in patterns {
                builder.add(&pattern).map_err(glob_set_build_error)?;
            }
            false
        } else {
            true
        };
        let overrides = builder.build().map_err(glob_set_build_error)?;

        Ok(Self {
            effective_root,
            overrides,
            markers: spec.markers,
            skip_hidden_by_walker,
            collect_patterns: has_patterns,
        })
    }

    fn matches_file(&self, path: &Path) -> bool {
        if self.skip_hidden_by_walker && has_hidden_component(&self.effective_root, path) {
            return false;
        }
        matches!(
            self.overrides.matched(path, false),
            IgnoreMatch::Whitelist(_)
        )
    }
}

pub(crate) struct QueryState {
    /// Matcher used for incremental file refresh checks.
    matcher: QueryMatcher,
    /// File ids currently matching this query.
    matched_files: HashSet<FileId>,
    /// Matching files ordered by `(modified_at, path, file_id)`.
    max_by_mtime: BTreeSet<MatchEntry>,
    /// Reserved for future query-wide invalidation. Dirty files are tracked on
    /// individual file records today.
    dirty: bool,
}

impl QueryState {
    fn from_walk(matcher: QueryMatcher, outcome: WalkOutcome) -> Self {
        Self {
            matcher,
            matched_files: outcome.matched_files,
            max_by_mtime: outcome.max_by_mtime,
            dirty: false,
        }
    }

    fn matches_file(&self, path: &Path) -> bool {
        self.matcher.matches_file(path)
    }

    pub(crate) fn affects_path(&self, path: &Path) -> bool {
        path.starts_with(self.matcher.watch_root())
    }

    fn invalidate_on_path_level_change(&self, path: &Path) -> bool {
        self.matcher.invalidate_on_path_level_change(path)
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

fn walk_glob_set_disk(
    inner: std::sync::Arc<crate::Inner>,
    plan: &GlobSetPlan,
) -> Result<WalkOutcome, VfsError> {
    let mut delta = DiskDelta::default();
    let mut stack = vec![plan.effective_root.clone()];

    while let Some(dir) = stack.pop() {
        match marker_decision(&inner, plan, &dir)? {
            MarkerDecision::Prune => continue,
            MarkerDecision::Leaf(file) => {
                delta.visited_dirs.push(dir);
                delta.matched_files.push(file);
                continue;
            }
            MarkerDecision::Descend => {}
        }

        delta.visited_dirs.push(dir.clone());
        let entries = match inner.read_dir_entries(&dir) {
            Ok(entries) => entries,
            Err(VfsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                continue;
            }
            Err(error) => return Err(error),
        };

        for entry in entries {
            match entry.kind {
                VfsEntryKind::Directory => {
                    if plan.skip_hidden_by_walker
                        && has_hidden_component(&plan.effective_root, &entry.path)
                    {
                        continue;
                    }
                    stack.push(entry.path);
                }
                kind => {
                    if plan.collect_patterns && plan.matches_file(&entry.path) {
                        delta.matched_files.push(MatchedFileDelta {
                            path: entry.path,
                            kind,
                            size: entry.size,
                            modified: entry.modified,
                        });
                    }
                }
            }
        }
    }

    let mut diagnostics = WalkDiagnostics::default();
    Ok(commit_disk_delta(&inner, delta, false, &mut diagnostics))
}

enum MarkerDecision {
    Descend,
    Leaf(MatchedFileDelta),
    Prune,
}

fn marker_decision(
    inner: &crate::Inner,
    plan: &GlobSetPlan,
    dir: &Path,
) -> Result<MarkerDecision, VfsError> {
    if plan.markers.is_empty() {
        return Ok(MarkerDecision::Descend);
    }

    let mut leaf_match = None;
    for marker in &plan.markers {
        let marker_path = dir.join(marker);
        let metadata = match inner.read_metadata(&marker_path) {
            Ok(metadata) => metadata,
            Err(VfsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                continue;
            }
            Err(error) => return Err(error),
        };
        let kind: VfsEntryKind = metadata.kind.into();
        if kind != VfsEntryKind::File {
            continue;
        }
        match plan.overrides.matched(&marker_path, false) {
            IgnoreMatch::Whitelist(_) if leaf_match.is_none() => {
                leaf_match = Some(MatchedFileDelta {
                    path: marker_path,
                    kind,
                    size: Some(metadata.len),
                    modified: metadata.modified,
                });
            }
            IgnoreMatch::Whitelist(_) => {}
            _ => return Ok(MarkerDecision::Prune),
        }
    }

    Ok(match leaf_match {
        Some(file) => MarkerDecision::Leaf(file),
        None => MarkerDecision::Descend,
    })
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

fn glob_set_build_error(error: ignore::Error) -> VfsError {
    VfsError::GlobSet {
        message: error.to_string(),
    }
}

fn has_hidden_component(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .any(|component| {
            let Component::Normal(name) = component else {
                return false;
            };
            name.to_string_lossy().starts_with('.')
        })
}

fn anchor_literal_pattern(pattern: String) -> String {
    fn needs_anchor(body: &str) -> bool {
        !body.is_empty()
            && !body.starts_with("./")
            && !body.starts_with('/')
            && !body.starts_with("../")
            && !body.contains('/')
            && !body.chars().any(|c| matches!(c, '*' | '?' | '[' | '{'))
    }

    let (negated, body) = pattern
        .strip_prefix('!')
        .map_or((false, pattern.as_str()), |body| (true, body));

    if !needs_anchor(body) {
        return pattern;
    }

    let mut anchored = String::with_capacity(pattern.len() + 2);
    if negated {
        anchored.push('!');
    }
    anchored.push('/');
    anchored.push_str(body);
    anchored
}

fn set_ignore_hidden_patterns(patterns: &[String]) -> Option<Vec<String>> {
    let user_includes_hidden = patterns.iter().any(|pattern| {
        pattern.starts_with('.') || pattern.contains("/.") && !pattern.starts_with("!.")
    });
    let has_negation_for_all_folders = patterns.iter().any(|p| p.starts_with("!**/.*"));
    if has_negation_for_all_folders {
        return None;
    }

    let search_all_hidden = patterns
        .iter()
        .any(|p| matches!(p.as_str(), ".*" | ".**" | "**/.*" | "./.*" | ".**/*"));
    if search_all_hidden {
        return Some(patterns.to_vec());
    }

    let requested_everything = patterns
        .iter()
        .any(|p| matches!(p.as_str(), "**" | "./**" | "**/*" | "./**/*"));
    if requested_everything || user_includes_hidden {
        let mut result = patterns.to_vec();
        result.push("!{**/.*, .*, .**/*}".to_string());

        for pattern in patterns {
            if !(pattern.starts_with('.') || pattern.contains("/.")) || pattern.starts_with("!.") {
                continue;
            }
            let is_specific_file = !pattern.contains('*')
                && !pattern.contains('?')
                && !pattern.contains('[')
                && pattern.contains('/');
            if is_specific_file {
                if let Some(last_slash) = pattern.rfind('/') {
                    let dir = &pattern[..last_slash];
                    result.push(dir.to_string());
                    result.push(format!("!{dir}/*"));
                    result.push(pattern.clone());
                }
                continue;
            }

            let hidden_folder = if pattern.starts_with('.') {
                pattern.split('/').next().unwrap_or(pattern.as_str())
            } else if let Some(idx) = pattern.find("/.") {
                let after_slash = &pattern[idx + 1..];
                after_slash.split('/').next().unwrap_or(pattern.as_str())
            } else {
                continue;
            };
            result.push(hidden_folder.to_string());
        }
        return Some(result);
    }

    None
}

#[derive(Clone, Debug)]
struct SimpleGlob {
    glob: String,
    negated: bool,
}

impl SimpleGlob {
    fn new(glob: String, negated: bool) -> Self {
        Self { glob, negated }
    }

    fn to_pattern(&self) -> String {
        if self.negated {
            format!("!{}", self.glob)
        } else {
            self.glob.clone()
        }
    }
}

#[derive(Debug)]
struct WalkRoot {
    specs: Vec<GlobSpecPart>,
    max_parent_dirs: usize,
}

#[derive(Debug)]
struct GlobSpecPart {
    negated: bool,
    parent_dirs: usize,
    concrete_components: Vec<String>,
    pattern: String,
    skip_rebase: bool,
}

#[derive(Debug, thiserror::Error)]
enum WalkRootsError {
    #[error("after processing glob '{glob}', split into '{prefix}' and empty glob")]
    EmptyGlob { prefix: String, glob: String },
    #[error("glob prefix '{prefix}' must be relative")]
    AbsolutePrefix { prefix: String },
    #[error("cannot ascend {required} level(s) from '{root}'")]
    CannotAscend { required: usize, root: PathBuf },
}

impl From<WalkRootsError> for VfsError {
    fn from(error: WalkRootsError) -> Self {
        VfsError::GlobSet {
            message: error.to_string(),
        }
    }
}

struct RebasedGlobs {
    root: PathBuf,
    globs: Vec<SimpleGlob>,
}

impl WalkRoot {
    fn build<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Result<Self, WalkRootsError> {
        let mut specs = Vec::new();
        let mut max_parent_dirs = 0usize;

        for pattern in patterns {
            let negated = pattern.starts_with('!');
            let pattern_without_negation = if negated { &pattern[1..] } else { pattern };
            let (prefix, glob) = split_path_and_glob(pattern_without_negation);
            if glob.is_empty() {
                return Err(WalkRootsError::EmptyGlob {
                    prefix: prefix.to_string(),
                    glob: pattern_without_negation.to_string(),
                });
            }

            let normalized_prefix = normalize_relative_path(Path::new(prefix));
            let mut parent_dirs = 0usize;
            let mut concrete_components = Vec::new();
            for component in normalized_prefix.components() {
                match component {
                    Component::ParentDir => parent_dirs += 1,
                    Component::CurDir => {}
                    Component::Normal(value) => {
                        concrete_components.push(value.to_string_lossy().into_owned());
                    }
                    Component::RootDir | Component::Prefix(_) => {
                        return Err(WalkRootsError::AbsolutePrefix {
                            prefix: prefix.to_string(),
                        });
                    }
                }
            }

            let skip_rebase =
                negated && normalized_prefix.as_os_str().is_empty() && glob.starts_with("**/");
            max_parent_dirs = max_parent_dirs.max(parent_dirs);
            specs.push(GlobSpecPart {
                negated,
                parent_dirs,
                concrete_components,
                pattern: glob.to_string(),
                skip_rebase,
            });
        }

        Ok(Self {
            specs,
            max_parent_dirs,
        })
    }

    fn rebase(&self, root: &Path) -> Result<RebasedGlobs, WalkRootsError> {
        let available = root
            .components()
            .filter(|component| matches!(component, Component::Normal(_) | Component::Prefix(_)))
            .count();
        if available < self.max_parent_dirs {
            return Err(WalkRootsError::CannotAscend {
                required: self.max_parent_dirs,
                root: root.to_path_buf(),
            });
        }

        let mut effective_root = root.to_path_buf();
        let mut popped = Vec::with_capacity(self.max_parent_dirs);
        for _ in 0..self.max_parent_dirs {
            let Some(name) = effective_root.file_name() else {
                return Err(WalkRootsError::CannotAscend {
                    required: self.max_parent_dirs,
                    root: root.to_path_buf(),
                });
            };
            popped.push(name.to_string_lossy().into_owned());
            effective_root.pop();
        }
        popped.reverse();

        let globs = self
            .specs
            .iter()
            .map(|spec| {
                if spec.skip_rebase {
                    return SimpleGlob::new(spec.pattern.clone(), spec.negated);
                }
                let keep_from_prefix = self.max_parent_dirs.saturating_sub(spec.parent_dirs);
                let mut components = Vec::new();
                components.extend(popped.iter().take(keep_from_prefix).cloned());
                components.extend(spec.concrete_components.iter().cloned());
                let pattern = if components.is_empty() {
                    spec.pattern.clone()
                } else {
                    format!("{}/{}", components.join("/"), spec.pattern)
                };
                SimpleGlob::new(pattern, spec.negated)
            })
            .collect();

        Ok(RebasedGlobs {
            root: effective_root,
            globs,
        })
    }
}

fn split_path_and_glob(input: &str) -> (&str, &str) {
    fn is_meta(c: char) -> bool {
        matches!(c, '*' | '?' | '[' | '{')
    }
    for (index, ch) in input.char_indices() {
        if is_meta(ch) {
            return input[..index]
                .rfind('/')
                .map_or(("", input), |slash| (&input[..=slash], &input[slash + 1..]));
        }
    }
    ("", input)
}

fn normalize_relative_path(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match output.components().next_back() {
                Some(Component::Normal(_)) => {
                    output.pop();
                }
                _ => output.push(".."),
            },
            Component::Normal(value) => output.push(value),
            Component::RootDir | Component::Prefix(_) => output.push(component.as_os_str()),
        }
    }
    output
}
