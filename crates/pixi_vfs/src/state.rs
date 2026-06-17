//! Shared VFS state and counters.
//!
//! [`IndexedVfs`] is cheap to clone. Clones share one [`Inner`] containing the
//! mutable index, the configured filesystem backend, and cumulative counters
//! used by tests and diagnostics. All index mutation is serialized by the
//! index mutex; expensive directory walking happens outside the mutex and is
//! committed afterward.

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use parking_lot::Mutex;

use crate::{
    VfsError,
    backend::{RealVfsBackend, VfsBackend, VfsBackendMetadata},
    index::{EntryInfo, Index},
};

/// Point-in-time index and disk-access counters.
///
/// These counters are intentionally cumulative for the lifetime of an
/// [`IndexedVfs`]. Tests use deltas between snapshots to prove that warm-cache
/// and single-file repair paths avoid directory scans.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VfsStats {
    /// Number of paths interned in the index.
    pub interned_paths: usize,
    /// Number of directory records in the index.
    pub dir_count: usize,
    /// Number of existing file records in the index.
    pub file_count: usize,
    /// Number of active glob aggregates maintained by the index.
    pub active_globs: usize,
    /// Number of directory reads sent to the backend.
    pub disk_dir_reads: usize,
    /// Number of metadata reads sent to the backend.
    pub disk_metadata_reads: usize,
    /// Number of file-content reads sent to the backend.
    pub disk_file_reads: usize,
}

/// Shared indexed filesystem state.
///
/// Public methods accept ordinary paths and expose flattened values. Internally
/// those paths are interned to compact identifiers so glob aggregates and
/// directory listings can refer to files without repeatedly cloning paths.
#[derive(Clone)]
pub struct IndexedVfs {
    pub(crate) inner: Arc<Inner>,
}

impl Default for IndexedVfs {
    fn default() -> Self {
        Self::new(Arc::new(RealVfsBackend))
    }
}

pub(crate) struct Inner {
    /// The in-memory path/node index. Keep this lock scoped tightly: disk
    /// operations must happen through the backend helpers below, not while the
    /// index mutex is held.
    pub(crate) index: Mutex<Index>,
    backend: Arc<dyn VfsBackend>,
    disk_dir_reads: AtomicUsize,
    disk_metadata_reads: AtomicUsize,
    disk_file_reads: AtomicUsize,
}

impl Inner {
    fn new(backend: Arc<dyn VfsBackend>) -> Self {
        Self {
            index: Mutex::new(Index::default()),
            backend,
            disk_dir_reads: AtomicUsize::new(0),
            disk_metadata_reads: AtomicUsize::new(0),
            disk_file_reads: AtomicUsize::new(0),
        }
    }

    /// Read and normalize one directory from the backend.
    ///
    /// Entries are sorted by path before being returned so index commits are
    /// deterministic regardless of backend iteration order.
    pub(crate) fn read_dir_entries(&self, path: &Path) -> Result<Vec<EntryInfo>, VfsError> {
        self.disk_dir_reads.fetch_add(1, Ordering::SeqCst);
        let mut entries: Vec<_> = self
            .backend
            .read_dir(path)
            .map_err(|error| VfsError::io("read_dir", path.to_path_buf(), error))?
            .into_iter()
            .map(|entry| EntryInfo {
                path: entry.path,
                kind: entry.kind.into(),
                size: entry.len,
                modified: entry.modified,
            })
            .collect();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    /// Read metadata for one path and update diagnostics counters.
    pub(crate) fn read_metadata(&self, path: &Path) -> Result<VfsBackendMetadata, VfsError> {
        self.disk_metadata_reads.fetch_add(1, Ordering::SeqCst);
        self.backend
            .metadata(path)
            .map_err(|error| VfsError::io("metadata", path.to_path_buf(), error))
    }

    /// Read file contents and update diagnostics counters.
    pub(crate) fn read_file(&self, path: &Path) -> Result<Arc<[u8]>, VfsError> {
        self.disk_file_reads.fetch_add(1, Ordering::SeqCst);
        self.backend
            .read_file(path)
            .map_err(|error| VfsError::io("read_file", path.to_path_buf(), error))
    }
}

impl IndexedVfs {
    /// Create an indexed filesystem backed by `backend`.
    ///
    /// Use this when tests or higher-level services need to provide a custom
    /// source of filesystem data. [`Default`] uses [`RealVfsBackend`].
    pub fn new(backend: Arc<dyn VfsBackend>) -> Self {
        Self {
            inner: Arc::new(Inner::new(backend)),
        }
    }

    /// Create an indexed filesystem backed by `backend`.
    ///
    /// This is an alias for [`IndexedVfs::new`] for call sites that read more
    /// naturally with builder-style naming.
    pub fn with_backend(backend: Arc<dyn VfsBackend>) -> Self {
        Self::new(backend)
    }

    /// Clear all indexed paths, directory records, file records, and active glob
    /// aggregates.
    ///
    /// This is a correctness fallback for watcher overflow or other uncertain
    /// event streams. It does not reset cumulative disk-read counters; callers
    /// can still use counter deltas around the reset for diagnostics.
    pub fn clear_index(&self) {
        *self.inner.index.lock() = Index::default();
    }

    /// Return cumulative index and disk-read stats.
    pub fn stats(&self) -> VfsStats {
        let index = self.inner.index.lock();
        VfsStats {
            interned_paths: index.path_count(),
            dir_count: index.dir_count(),
            file_count: index.file_count(),
            active_globs: index.query_count(),
            disk_dir_reads: self.inner.disk_dir_reads.load(Ordering::SeqCst),
            disk_metadata_reads: self.inner.disk_metadata_reads.load(Ordering::SeqCst),
            disk_file_reads: self.inner.disk_file_reads.load(Ordering::SeqCst),
        }
    }
}
