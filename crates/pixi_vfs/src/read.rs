//! Indexed point reads: metadata, file contents, and one-level directory listings.
//!
//! These APIs are the building blocks used by `pixi_compute_fs` for direct path
//! queries. They repair the index from the configured backend on cache miss,
//! but they do not recursively scan subtrees. Recursive behavior belongs in
//! glob walking, where the pattern determines how much of the tree to touch.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use crate::{
    IndexedVfs, VfsError,
    backend::EntryKind,
    index::{DirId, VfsEntryKind},
};

/// Metadata for one indexed filesystem path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VfsMetadata {
    /// Path whose metadata was requested.
    pub path: PathBuf,
    /// Whether the path existed when the backend was queried.
    pub exists: bool,
    /// Entry kind for existing paths.
    pub kind: Option<EntryKind>,
    /// Byte length for existing paths when reported by the backend.
    pub len: Option<u64>,
    /// Last modification timestamp when reported by the backend.
    pub modified: Option<SystemTime>,
}

/// One indexed directory entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VfsDirectoryEntry {
    /// Full path to the child entry.
    pub path: PathBuf,
    /// Final path component, cached for stable sorting and display.
    pub file_name: String,
    /// Entry kind observed when the directory listing was read.
    pub kind: EntryKind,
}

impl IndexedVfs {
    /// Return metadata for a path, repairing the index from the backend on miss.
    ///
    /// Missing paths are returned as `Ok(VfsMetadata { exists: false, .. })`
    /// instead of an error. Other backend failures are propagated. Existing
    /// files are inserted into the index; directories are interned as directory
    /// records without marking their child listing clean.
    pub fn metadata(&self, path: impl AsRef<Path>) -> Result<VfsMetadata, VfsError> {
        let path = path.as_ref().to_path_buf();
        if let Some(metadata) = self.cached_file_metadata(&path) {
            return Ok(metadata);
        }

        match self.inner.read_metadata(&path) {
            Ok(metadata) => {
                let kind: VfsEntryKind = metadata.kind.into();
                if kind == VfsEntryKind::Directory {
                    self.inner.index.lock().upsert_query_dir(path.clone());
                } else {
                    self.inner.index.lock().upsert_file_metadata(
                        path.clone(),
                        kind,
                        Some(metadata.len),
                        metadata.modified,
                        true,
                    );
                }
                Ok(VfsMetadata {
                    path,
                    exists: true,
                    kind: Some(metadata.kind),
                    len: Some(metadata.len),
                    modified: metadata.modified,
                })
            }
            Err(VfsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                Ok(VfsMetadata {
                    path,
                    exists: false,
                    kind: None,
                    len: None,
                    modified: None,
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Return file contents from the VFS backend.
    ///
    /// File bytes are not cached in the VFS index. Caching is left to the
    /// compute graph layer, which can memoize `read_file(path)` by graph
    /// version and invalidate it from filesystem events.
    pub fn read_file(&self, path: impl AsRef<Path>) -> Result<Arc<[u8]>, VfsError> {
        self.inner.read_file(path.as_ref())
    }

    /// Return a sorted one-level directory listing.
    ///
    /// A clean indexed listing is reused without touching the backend. Missing
    /// or unknown listings are read from the backend, committed to the index,
    /// and then returned in deterministic `(file_name, path)` order.
    pub fn read_dir(&self, path: impl AsRef<Path>) -> Result<Arc<[VfsDirectoryEntry]>, VfsError> {
        let path = path.as_ref().to_path_buf();
        if let Some(entries) = self.cached_dir_entries(&path) {
            return Ok(entries);
        }

        let entries = self.inner.read_dir_entries(&path)?;
        let dir = self
            .inner
            .index
            .lock()
            .upsert_directory_listing(path, entries);
        self.dir_entries_from_index(dir)
    }

    fn cached_file_metadata(&self, path: &Path) -> Option<VfsMetadata> {
        // Dirty file records have been marked by watcher/invalidator paths and
        // must be repaired from the backend before their metadata is trusted.
        let index = self.inner.index.lock();
        let file = index.file_id_for_path(path)?;
        let record = index.file(file);
        if record.dirty || !record.exists {
            return None;
        }
        Some(VfsMetadata {
            path: index.file_path(file).to_path_buf(),
            exists: true,
            kind: Some(record.kind.into()),
            len: record.size,
            modified: record.modified,
        })
    }

    fn cached_dir_entries(&self, path: &Path) -> Option<Arc<[VfsDirectoryEntry]>> {
        // Directory children are only authoritative after a complete read_dir
        // commit. Query-driven glob walks may intern directories without
        // reading all children, leaving the listing state unknown.
        let index = self.inner.index.lock();
        let dir = index.dir_id_for_path(path)?;
        let entries = index.clean_dir_entries(dir)?;
        Some(entries_from_index(entries))
    }

    fn dir_entries_from_index(&self, dir: DirId) -> Result<Arc<[VfsDirectoryEntry]>, VfsError> {
        let index = self.inner.index.lock();
        let entries = index
            .clean_dir_entries(dir)
            .expect("directory listing was just inserted as clean");
        Ok(entries_from_index(entries))
    }
}

fn entries_from_index(entries: Vec<(PathBuf, VfsEntryKind)>) -> Arc<[VfsDirectoryEntry]> {
    let mut entries: Vec<_> = entries
        .into_iter()
        .map(|(path, kind)| VfsDirectoryEntry {
            file_name: file_name_string(&path),
            path,
            kind: kind.into(),
        })
        .collect();
    entries.sort_by(|a, b| {
        a.file_name
            .cmp(&b.file_name)
            .then_with(|| a.path.cmp(&b.path))
    });
    Arc::from(entries.into_boxed_slice())
}

fn file_name_string(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}
