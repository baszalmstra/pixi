//! Filesystem compute keys and [`ComputeCtx`] extension helpers.
//!
//! This crate intentionally uses ordinary [`PathBuf`] values. It provides
//! graph-version consistency through `pixi_compute_engine`, not an atomic or
//! TOCTOU-proof filesystem snapshot.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use futures::{FutureExt, future::BoxFuture};
use pixi_compute_engine::{
    ComputeCtx, ComputeEngine, ComputeUpdater, Key, UpdateError, UpdateResult,
};
pub use pixi_vfs::GlobSetSpec as InputGlobSpec;
use pixi_vfs::{EntryKind, IndexedVfs, WalkMode};

/// Filesystem entry kind used by metadata and directory listings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FsEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// Metadata for one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsMetadata {
    pub path: PathBuf,
    pub exists: bool,
    pub kind: Option<FsEntryKind>,
    pub len: Option<u64>,
    pub modified: Option<SystemTime>,
}

/// One directory entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub path: PathBuf,
    pub file_name: String,
    pub kind: FsEntryKind,
}

/// Newest modification time for paths matched by a glob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GlobMTime {
    NoMatches,
    MatchesFound {
        modified_at: SystemTime,
        designated_file: PathBuf,
    },
}

/// Error returned by filesystem graph reads.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FsError {
    #[error("{operation} failed for {path}: {message}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        kind: io::ErrorKind,
        message: String,
    },
    #[error("invalid glob pattern {pattern:?}: {message}")]
    GlobPattern { pattern: String, message: String },
    #[error("indexed filesystem failed: {message}")]
    Indexed { message: String },
}

impl FsError {
    fn from_io(operation: &'static str, path: PathBuf, error: io::Error) -> Self {
        Self::Io {
            operation,
            path,
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct FileMetadataKey(pub PathBuf);

impl fmt::Display for FileMetadataKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl Key for FileMetadataKey {
    type Value = Result<FsMetadata, FsError>;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        indexed_vfs(ctx)
            .and_then(|vfs| vfs.metadata(&self.0).map_err(fs_error_from_vfs))
            .map(metadata_from_indexed)
    }

    fn equality(a: &Self::Value, b: &Self::Value) -> bool {
        a == b
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct FileContentsKey(pub PathBuf);

impl fmt::Display for FileContentsKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl Key for FileContentsKey {
    type Value = Result<Arc<[u8]>, FsError>;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        indexed_vfs(ctx).and_then(|vfs| vfs.read_file(&self.0).map_err(fs_error_from_vfs))
    }

    fn equality(a: &Self::Value, b: &Self::Value) -> bool {
        a == b
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct DirectoryListingKey(pub PathBuf);

impl fmt::Display for DirectoryListingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl Key for DirectoryListingKey {
    type Value = Result<Arc<[DirectoryEntry]>, FsError>;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        indexed_vfs(ctx)
            .and_then(|vfs| vfs.read_dir(&self.0).map_err(fs_error_from_vfs))
            .map(directory_entries_from_indexed)
    }

    fn equality(a: &Self::Value, b: &Self::Value) -> bool {
        a == b
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct GlobMTimeKey {
    pub root: PathBuf,
    pub pattern: String,
}

impl fmt::Display for GlobMTimeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.root.display(), self.pattern)
    }
}

impl Key for GlobMTimeKey {
    type Value = Result<GlobMTime, FsError>;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        ctx.compute(&InputGlobMTimeKey {
            root: self.root.clone(),
            spec: InputGlobSpec::new([self.pattern.clone()]),
        })
        .await
    }

    fn equality(a: &Self::Value, b: &Self::Value) -> bool {
        a == b
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct InputGlobMTimeKey {
    pub root: PathBuf,
    pub spec: InputGlobSpec,
}

impl fmt::Display for InputGlobMTimeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{:?}", self.root.display(), self.spec)
    }
}

impl Key for InputGlobMTimeKey {
    type Value = Result<GlobMTime, FsError>;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        indexed_vfs(ctx)
            .and_then(|vfs| {
                vfs.latest_mtime_for_spec(&self.root, self.spec.clone(), WalkMode::Hybrid)
                    .map_err(fs_error_from_vfs)
            })
            .map(glob_mtime_from_indexed)
    }

    fn equality(a: &Self::Value, b: &Self::Value) -> bool {
        a == b
    }
}

fn indexed_vfs(ctx: &ComputeCtx) -> Result<Arc<IndexedVfs>, FsError> {
    ctx.global_data()
        .try_get::<Arc<IndexedVfs>>()
        .cloned()
        .ok_or_else(missing_indexed_vfs_error)
}

fn indexed_vfs_from_engine(engine: &ComputeEngine) -> Result<Arc<IndexedVfs>, UpdateError> {
    engine
        .global_data()
        .try_get::<Arc<IndexedVfs>>()
        .cloned()
        .ok_or_else(|| UpdateError::External {
            message: "IndexedVfs is required by pixi_compute_fs".to_owned(),
        })
}

fn missing_indexed_vfs_error() -> FsError {
    FsError::Indexed {
        message: "IndexedVfs is required by pixi_compute_fs".to_owned(),
    }
}

fn metadata_from_indexed(value: pixi_vfs::VfsMetadata) -> FsMetadata {
    FsMetadata {
        path: value.path,
        exists: value.exists,
        kind: value.kind.map(entry_kind_from_indexed),
        len: value.len,
        modified: value.modified,
    }
}

fn directory_entries_from_indexed(
    entries: Arc<[pixi_vfs::VfsDirectoryEntry]>,
) -> Arc<[DirectoryEntry]> {
    Arc::from(
        entries
            .iter()
            .map(|entry| DirectoryEntry {
                path: entry.path.clone(),
                file_name: entry.file_name.clone(),
                kind: entry_kind_from_indexed(entry.kind),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    )
}

fn entry_kind_from_indexed(kind: EntryKind) -> FsEntryKind {
    match kind {
        EntryKind::File => FsEntryKind::File,
        EntryKind::Directory => FsEntryKind::Directory,
        EntryKind::Symlink => FsEntryKind::Symlink,
        EntryKind::Other => FsEntryKind::Other,
    }
}

fn glob_mtime_from_indexed(value: pixi_vfs::GlobMTime) -> GlobMTime {
    match value {
        pixi_vfs::GlobMTime::NoMatches => GlobMTime::NoMatches,
        pixi_vfs::GlobMTime::MatchesFound {
            modified_at,
            designated_file,
        } => GlobMTime::MatchesFound {
            modified_at,
            designated_file,
        },
    }
}

fn fs_error_from_vfs(error: pixi_vfs::VfsError) -> FsError {
    match error {
        pixi_vfs::VfsError::Io {
            operation,
            path,
            source,
        } => FsError::from_io(operation, path, source),
        pixi_vfs::VfsError::GlobPattern { pattern, source } => FsError::GlobPattern {
            pattern,
            message: source.to_string(),
        },
        pixi_vfs::VfsError::GlobSet { message } => FsError::GlobPattern {
            pattern: "<input glob set>".to_owned(),
            message,
        },
        pixi_vfs::VfsError::IndexMiss { .. } => FsError::Indexed {
            message: error.to_string(),
        },
    }
}

/// Extension methods for filesystem reads from a compute context.
pub trait ComputeCtxFsExt {
    fn metadata(&mut self, path: impl AsRef<Path>) -> BoxFuture<'_, Result<FsMetadata, FsError>>;
    fn read_file(&mut self, path: impl AsRef<Path>) -> BoxFuture<'_, Result<Arc<[u8]>, FsError>>;
    fn read_dir(
        &mut self,
        path: impl AsRef<Path>,
    ) -> BoxFuture<'_, Result<Arc<[DirectoryEntry]>, FsError>>;
    fn glob_mtime(
        &mut self,
        root: impl AsRef<Path>,
        pattern: impl AsRef<str>,
    ) -> BoxFuture<'_, Result<GlobMTime, FsError>>;
    fn input_glob_mtime(
        &mut self,
        root: impl AsRef<Path>,
        spec: InputGlobSpec,
    ) -> BoxFuture<'_, Result<GlobMTime, FsError>>;
}

impl ComputeCtxFsExt for ComputeCtx {
    fn metadata(&mut self, path: impl AsRef<Path>) -> BoxFuture<'_, Result<FsMetadata, FsError>> {
        let path = path.as_ref().to_path_buf();
        async move { self.compute(&FileMetadataKey(path)).await }.boxed()
    }

    fn read_file(&mut self, path: impl AsRef<Path>) -> BoxFuture<'_, Result<Arc<[u8]>, FsError>> {
        let path = path.as_ref().to_path_buf();
        async move { self.compute(&FileContentsKey(path)).await }.boxed()
    }

    fn read_dir(
        &mut self,
        path: impl AsRef<Path>,
    ) -> BoxFuture<'_, Result<Arc<[DirectoryEntry]>, FsError>> {
        let path = path.as_ref().to_path_buf();
        async move { self.compute(&DirectoryListingKey(path)).await }.boxed()
    }

    fn glob_mtime(
        &mut self,
        root: impl AsRef<Path>,
        pattern: impl AsRef<str>,
    ) -> BoxFuture<'_, Result<GlobMTime, FsError>> {
        let root = root.as_ref().to_path_buf();
        let spec = InputGlobSpec::new([pattern.as_ref()]);
        async move { self.compute(&InputGlobMTimeKey { root, spec }).await }.boxed()
    }

    fn input_glob_mtime(
        &mut self,
        root: impl AsRef<Path>,
        spec: InputGlobSpec,
    ) -> BoxFuture<'_, Result<GlobMTime, FsError>> {
        let root = root.as_ref().to_path_buf();
        async move { self.compute(&InputGlobMTimeKey { root, spec }).await }.boxed()
    }
}

/// Extension methods for filesystem invalidations on a compute engine.
///
/// These methods update the VFS first and then dirty the corresponding compute
/// keys in one graph update. They require an [`IndexedVfs`] to be registered in
/// the engine's global data.
pub trait ComputeEngineFsExt {
    /// Invalidate filesystem keys affected by a path-level change.
    ///
    /// Use this when the path may have been created, deleted, renamed, changed
    /// kind, or otherwise affected directory membership. In addition to
    /// refreshing VFS glob aggregates, this dirties:
    ///
    /// - [`FileMetadataKey`] for `path`
    /// - [`FileContentsKey`] for `path`
    /// - [`DirectoryListingKey`] for `path`
    /// - [`DirectoryListingKey`] for `path`'s parent, when it has one
    ///
    /// For a pure content/mtime change to an existing file, prefer
    /// [`Self::invalidate_file`] to avoid dirtying directory listings.
    fn invalidate_path(&self, path: impl AsRef<Path>) -> Result<UpdateResult, UpdateError>;

    /// Invalidate keys for an existing file whose contents or metadata changed.
    ///
    /// Use this when the file still exists at the same path and directory
    /// membership did not change, for example after a file write or mtime
    /// update. In addition to refreshing VFS glob aggregates, this dirties only:
    ///
    /// - [`FileMetadataKey`] for `path`
    /// - [`FileContentsKey`] for `path`
    ///
    /// If the file may have been created, deleted, renamed, or changed kind,
    /// use [`Self::invalidate_path`] instead so affected directory listings are
    /// dirtied too.
    fn invalidate_file(&self, path: impl AsRef<Path>) -> Result<UpdateResult, UpdateError>;

    /// Invalidate filesystem keys affected by multiple path-level changes.
    ///
    /// This is the batched form of [`Self::invalidate_path`]. Use it for rename
    /// pairs or grouped watcher events so all affected compute keys are dirtied
    /// in one graph update.
    fn invalidate_paths<I, P>(&self, paths: I) -> Result<UpdateResult, UpdateError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>;
}

impl ComputeEngineFsExt for ComputeEngine {
    fn invalidate_path(&self, path: impl AsRef<Path>) -> Result<UpdateResult, UpdateError> {
        self.invalidate_paths([path])
    }

    fn invalidate_file(&self, path: impl AsRef<Path>) -> Result<UpdateResult, UpdateError> {
        let mut updater = self.updater();
        let path = path.as_ref().to_path_buf();
        let mut glob_replacements = BTreeMap::new();
        refresh_indexed_vfs_file(self, &path, &mut glob_replacements)?;
        apply_glob_replacements(&mut updater, glob_replacements)?;
        changed_file(&mut updater, path)?;
        updater.commit_report()
    }

    fn invalidate_paths<I, P>(&self, paths: I) -> Result<UpdateResult, UpdateError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut updater = self.updater();
        let mut glob_replacements = BTreeMap::new();
        let mut glob_invalidations = BTreeSet::new();
        for path in paths {
            let path = path.as_ref().to_path_buf();
            refresh_indexed_vfs_path(self, &path, &mut glob_replacements, &mut glob_invalidations)?;
            changed_path(&mut updater, path)?;
        }
        apply_glob_invalidations(&mut updater, &glob_invalidations)?;
        apply_glob_replacements_except(&mut updater, glob_replacements, &glob_invalidations)?;
        updater.commit_report()
    }
}

fn changed_path(updater: &mut ComputeUpdater, path: PathBuf) -> Result<(), UpdateError> {
    changed_file(updater, path.clone())?;
    updater.changed_if_new(DirectoryListingKey(path.clone()))?;
    if let Some(parent) = parent_path(&path) {
        updater.changed_if_new(DirectoryListingKey(parent))?;
    }
    Ok(())
}

fn changed_file(updater: &mut ComputeUpdater, path: PathBuf) -> Result<(), UpdateError> {
    updater.changed_if_new(FileMetadataKey(path.clone()))?;
    updater.changed_if_new(FileContentsKey(path))?;
    Ok(())
}

type GlobReplacementMap = BTreeMap<(PathBuf, InputGlobSpec), GlobMTime>;
type GlobInvalidationSet = BTreeSet<(PathBuf, InputGlobSpec)>;

fn refresh_indexed_vfs_file(
    engine: &ComputeEngine,
    path: &Path,
    glob_replacements: &mut GlobReplacementMap,
) -> Result<(), UpdateError> {
    let vfs = indexed_vfs_from_engine(engine)?;
    let changes = vfs
        .refresh_file(path)
        .map_err(|error| UpdateError::External {
            message: error.to_string(),
        })?;
    for change in changes {
        glob_replacements.insert(
            (change.root, change.spec),
            glob_mtime_from_indexed(change.current),
        );
    }
    Ok(())
}

fn refresh_indexed_vfs_path(
    engine: &ComputeEngine,
    path: &Path,
    glob_replacements: &mut GlobReplacementMap,
    glob_invalidations: &mut GlobInvalidationSet,
) -> Result<(), UpdateError> {
    let vfs = indexed_vfs_from_engine(engine)?;
    let refresh = vfs
        .refresh_path(path)
        .map_err(|error| UpdateError::External {
            message: error.to_string(),
        })?;
    for change in refresh.glob_changes {
        glob_replacements.insert(
            (change.root, change.spec),
            glob_mtime_from_indexed(change.current),
        );
    }
    for invalidation in refresh.invalidated_globs {
        glob_invalidations.insert((invalidation.root, invalidation.spec));
    }
    Ok(())
}

fn apply_glob_invalidations(
    updater: &mut ComputeUpdater,
    glob_invalidations: &GlobInvalidationSet,
) -> Result<(), UpdateError> {
    for (root, spec) in glob_invalidations {
        changed_glob_if_new(updater, root.clone(), spec.clone())?;
    }
    Ok(())
}

fn apply_glob_replacements(
    updater: &mut ComputeUpdater,
    glob_replacements: GlobReplacementMap,
) -> Result<(), UpdateError> {
    apply_glob_replacements_except(updater, glob_replacements, &GlobInvalidationSet::new())
}

fn apply_glob_replacements_except(
    updater: &mut ComputeUpdater,
    glob_replacements: GlobReplacementMap,
    glob_invalidations: &GlobInvalidationSet,
) -> Result<(), UpdateError> {
    for ((root, spec), current) in glob_replacements {
        if glob_invalidations.contains(&(root.clone(), spec.clone())) {
            continue;
        }
        changed_glob_to(updater, root, spec, current)?;
    }
    Ok(())
}

fn changed_glob_if_new(
    updater: &mut ComputeUpdater,
    root: PathBuf,
    spec: InputGlobSpec,
) -> Result<(), UpdateError> {
    updater.changed_if_new(InputGlobMTimeKey { root, spec })?;
    Ok(())
}

fn changed_glob_to(
    updater: &mut ComputeUpdater,
    root: PathBuf,
    spec: InputGlobSpec,
    current: GlobMTime,
) -> Result<(), UpdateError> {
    updater.changed_to(InputGlobMTimeKey { root, spec }, Ok(current))
}

fn parent_path(path: &Path) -> Option<PathBuf> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
}
