//! Indexed filesystem walking and glob aggregates.
//!
//! `pixi_vfs` sits below the compute graph. It maintains an in-memory index of
//! directories, file metadata, and active glob aggregates so repeated queries can
//! be answered without repeatedly walking the same filesystem tree.
//!
//! Public APIs intentionally use ordinary [`std::path::Path`] and
//! [`std::path::PathBuf`] values. Internally, paths and nodes are interned into
//! compact ids for faster aggregate maintenance. The index provides
//! graph-version consistency to higher layers, not a historical filesystem
//! snapshot: a query observes whatever the configured [`VfsBackend`] reports
//! while repairing or walking.

mod backend;
mod glob;
mod index;
mod read;
mod state;
pub mod walk;

use std::{io, path::PathBuf};

pub use backend::{EntryKind, RealVfsBackend, VfsBackend, VfsBackendEntry, VfsBackendMetadata};
pub use glob::{
    GlobMTime, GlobQueryChange, GlobQueryInvalidation, GlobSetSpec, GlobSpec,
    LatestMTimeDiagnostics, VfsPathRefresh,
};
pub use read::{VfsDirectoryEntry, VfsMetadata};
pub use state::{IndexedVfs, VfsStats};
pub use walk::{WalkDiagnostics, WalkMode};

pub(crate) use glob::{GlobQueryKey, MatchEntry, QueryState, glob_matches, latest_from_entries};
pub(crate) use state::Inner;
pub(crate) use walk::{node_as_dir, node_as_file};

/// Error returned by indexed VFS operations.
#[derive(Debug, thiserror::Error)]
pub enum VfsError {
    /// Backend filesystem operation failed.
    #[error("{operation} failed for {path}: {source}")]
    Io {
        /// Operation attempted by the VFS.
        operation: &'static str,
        /// Path associated with the operation.
        path: PathBuf,
        /// Original backend error.
        #[source]
        source: io::Error,
    },
    /// Glob pattern could not be parsed.
    #[error("invalid glob pattern {pattern:?}: {source}")]
    GlobPattern {
        /// User-provided pattern text.
        pattern: String,
        /// Parser error from the glob crate.
        #[source]
        source: ::glob::PatternError,
    },
    /// Glob-set patterns could not be compiled or rebased.
    #[error("invalid glob set: {message}")]
    GlobSet {
        /// Error message from the glob-set builder.
        message: String,
    },
    /// `WalkMode::IndexOnly` could not answer from clean indexed state.
    #[error("index-only walk needed {needed} for {path}")]
    IndexMiss {
        /// Kind of indexed data required to continue.
        needed: &'static str,
        /// Path whose indexed state was missing or dirty.
        path: PathBuf,
    },
}

impl VfsError {
    pub(crate) fn io(operation: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }
}
