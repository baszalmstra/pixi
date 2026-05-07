//! Tracks the files that the previous source-build invocation wrote
//! into the host prefix, so the next invocation can wipe them before
//! handing the prefix back to the backend.
//!
//! Source-build workspaces (and the host/build prefixes inside them)
//! are intentionally reused across runs that share the same
//! `(source, deps, variants, backend)` — see
//! [`compute_workspace_key`](crate::cache::compute_workspace_key) — so
//! incremental backend state survives. The downside: the package being
//! built also gets installed into the host prefix during the build,
//! and those files don't belong to any conda-meta record so neither
//! the rattler installer nor the backend's own pre/post diffing knows
//! to evict them on the next run. The result is duplicate
//! `*.dist-info/` (and similar) entries leaking into the new artifact
//! whenever the version string changes between builds.
//!
//! After each successful build, we read the relative paths out of the
//! freshly-produced `.conda`'s `info/paths.json` — these are exactly
//! the files the backend deposited into the host prefix and packaged
//! up — and persist them next to the workspace. Before the next
//! backend invocation, we delete those paths from the host prefix
//! (best-effort), restoring the prefix to a state controlled solely
//! by the rattler installer.

use std::path::{Path, PathBuf};

use rattler_conda_types::package::{PathType, PathsJson};
use serde::{Deserialize, Serialize};

/// Filename of the marker written next to the workspace directory.
/// Hidden so it doesn't clash with backend-managed state inside the
/// workspace.
const MARKER_FILENAME: &str = ".pixi-prev-build-output.json";

/// Files that the previous backend invocation packaged into the
/// .conda. Persisted in the workspace directory so it survives across
/// `pixi build` runs that share the same workspace.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct PrevBuildOutput {
    /// Relative paths (under the host prefix) of every non-directory
    /// entry in the previous build's `paths.json`. Directories are
    /// excluded because removing files is enough to break the
    /// duplicate-`dist-info` failure mode and we don't want to risk
    /// removing a directory the backend now wants to reuse.
    pub host_prefix_files: Vec<PathBuf>,
}

impl PrevBuildOutput {
    fn marker_path(work_directory: &Path) -> PathBuf {
        work_directory.join(MARKER_FILENAME)
    }

    /// Read the marker from the workspace, or `None` if no successful
    /// build has been recorded yet (or the marker is unreadable —
    /// callers treat both cases the same way: skip cleanup).
    pub fn read(work_directory: &Path) -> Option<Self> {
        let bytes = fs_err::read(Self::marker_path(work_directory)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Build the marker by streaming `info/paths.json` out of the
    /// just-built `.conda`. Runs synchronous file IO; call it on a
    /// blocking thread.
    pub fn from_built_archive(
        archive: &Path,
    ) -> Result<Self, rattler_package_streaming::ExtractError> {
        let paths_json: PathsJson =
            rattler_package_streaming::seek::read_package_file(archive)?;
        Ok(Self {
            host_prefix_files: paths_json
                .paths
                .into_iter()
                .filter(|entry| !matches!(entry.path_type, PathType::Directory))
                .map(|entry| entry.relative_path)
                .collect(),
        })
    }

    /// Write the marker atomically (write-temp + rename). Errors are
    /// surfaced; callers may choose to log-and-continue since a
    /// missing marker just means the next build skips cleanup.
    pub fn write(&self, work_directory: &Path) -> std::io::Result<()> {
        let dest = Self::marker_path(work_directory);
        let tmp = dest.with_extension("tmp");
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::from)?;
        fs_err::write(&tmp, bytes)?;
        fs_err::rename(&tmp, &dest)
    }

    /// Delete every recorded file from the host prefix, ignoring
    /// entries that are already gone. Other IO errors short-circuit
    /// so the build fails loudly rather than handing the backend a
    /// half-cleaned prefix.
    pub fn cleanup_host_prefix(&self, host_prefix: &Path) -> std::io::Result<()> {
        for relative in &self.host_prefix_files {
            // PathsJson normalizes to forward slashes; PathBuf::join
            // accepts that on Windows and the underlying fs APIs
            // tolerate mixed separators.
            let path = host_prefix.join(relative);
            match fs_err::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;

    fn make(paths: &[&str]) -> PrevBuildOutput {
        PrevBuildOutput {
            host_prefix_files: paths.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn write_then_read_roundtrips() {
        let tmp = TempDir::new().unwrap();
        let original = make(&["lib/python3.11/site-packages/foo/__init__.py", "bin/foo"]);
        original.write(tmp.path()).unwrap();
        let restored = PrevBuildOutput::read(tmp.path()).unwrap();
        assert_eq!(restored.host_prefix_files, original.host_prefix_files);
    }

    #[test]
    fn read_missing_marker_returns_none() {
        let tmp = TempDir::new().unwrap();
        assert!(PrevBuildOutput::read(tmp.path()).is_none());
    }

    #[test]
    fn cleanup_removes_listed_files_and_tolerates_missing() {
        let tmp = TempDir::new().unwrap();
        let prefix = tmp.path().join("host");
        let nested = prefix.join("a/b/c.txt");
        fs_err::create_dir_all(nested.parent().unwrap()).unwrap();
        fs_err::write(&nested, b"x").unwrap();

        let state = make(&["a/b/c.txt", "a/b/missing.txt"]);
        state.cleanup_host_prefix(&prefix).unwrap();

        assert!(!nested.exists(), "listed file should be deleted");
        // Missing entry must not surface as an error.
    }

    #[test]
    fn cleanup_propagates_non_notfound_errors() {
        // Pointing at a path under a nonexistent prefix: ENOENT for
        // each entry, so cleanup must succeed (NotFound is silently
        // ignored).
        let state = make(&["foo/bar.txt"]);
        let result = state.cleanup_host_prefix(Path::new("/nonexistent-pixi-cleanup-test"));
        assert!(result.is_ok());
    }
}
