//! Source-filesystem access for the indexed VFS.
//!
//! The index stores paths, directory membership, and metadata needed by glob
//! queries, but it deliberately does not own the operating-system access
//! policy. [`VfsBackend`] is that boundary. Production uses
//! [`RealVfsBackend`]; tests can inject an in-memory or failing backend while
//! exercising the same indexing code.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use crate::index::VfsEntryKind;

/// Filesystem entry kind tracked by the index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link. The current walker records symlinks but does not
    /// descend through symlinked directories.
    Symlink,
    /// Any platform-specific file type that is not one of the above.
    Other,
}

impl From<VfsEntryKind> for EntryKind {
    fn from(value: VfsEntryKind) -> Self {
        match value {
            VfsEntryKind::File => Self::File,
            VfsEntryKind::Directory => Self::Directory,
            VfsEntryKind::Symlink => Self::Symlink,
            VfsEntryKind::Other => Self::Other,
        }
    }
}

impl From<EntryKind> for VfsEntryKind {
    fn from(value: EntryKind) -> Self {
        match value {
            EntryKind::File => Self::File,
            EntryKind::Directory => Self::Directory,
            EntryKind::Symlink => Self::Symlink,
            EntryKind::Other => Self::Other,
        }
    }
}

/// Metadata returned by a VFS backend for an existing path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VfsBackendMetadata {
    /// Type of the path at the time it was read.
    pub kind: EntryKind,
    /// Byte length reported by the backend.
    pub len: u64,
    /// Last modification timestamp, if the backend/platform provides it.
    pub modified: Option<SystemTime>,
}

/// Directory entry returned by a VFS backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VfsBackendEntry {
    /// Absolute or caller-relative path for the child entry.
    pub path: PathBuf,
    /// Type of the child entry.
    pub kind: EntryKind,
    /// Byte length for file-like entries when cheaply available.
    pub len: Option<u64>,
    /// Last modification timestamp when cheaply available.
    pub modified: Option<SystemTime>,
}

/// Disk/source backend used by the indexed filesystem.
///
/// Implementations should behave like `symlink_metadata`, `read`, and
/// `read_dir`: metadata should not follow symlinks, `read_file` returns file
/// bytes, and `read_dir` returns immediate children only. Errors are preserved
/// as [`io::Error`] so callers can distinguish missing paths from other
/// failures.
pub trait VfsBackend: Send + Sync + 'static {
    /// Read metadata for a single path.
    fn metadata(&self, path: &Path) -> io::Result<VfsBackendMetadata>;
    /// Read the full contents of a file.
    fn read_file(&self, path: &Path) -> io::Result<Arc<[u8]>>;
    /// Read one directory's immediate children.
    fn read_dir(&self, path: &Path) -> io::Result<Vec<VfsBackendEntry>>;
}

/// VFS backend backed by normal blocking filesystem operations.
///
/// The parallel glob walker calls this backend from worker threads, so the
/// implementation intentionally uses blocking `fs_err` operations rather than
/// Tokio filesystem APIs.
#[derive(Clone, Copy, Debug, Default)]
pub struct RealVfsBackend;

impl VfsBackend for RealVfsBackend {
    fn metadata(&self, path: &Path) -> io::Result<VfsBackendMetadata> {
        let metadata = fs_err::symlink_metadata(path)?;
        Ok(VfsBackendMetadata {
            kind: kind_from_file_type(metadata.file_type()).into(),
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }

    fn read_file(&self, path: &Path) -> io::Result<Arc<[u8]>> {
        fs_err::read(path).map(|bytes| Arc::from(bytes.into_boxed_slice()))
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<VfsBackendEntry>> {
        let read_dir = fs_err::read_dir(path)?;
        let mut entries = Vec::new();
        for entry in read_dir {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            let metadata = entry.metadata().ok();
            entries.push(VfsBackendEntry {
                path,
                kind: kind_from_file_type(file_type).into(),
                len: metadata.as_ref().map(std::fs::Metadata::len),
                modified: metadata.and_then(|metadata| metadata.modified().ok()),
            });
        }
        Ok(entries)
    }
}

fn kind_from_file_type(file_type: std::fs::FileType) -> VfsEntryKind {
    if file_type.is_file() {
        VfsEntryKind::File
    } else if file_type.is_dir() {
        VfsEntryKind::Directory
    } else if file_type.is_symlink() {
        VfsEntryKind::Symlink
    } else {
        VfsEntryKind::Other
    }
}
