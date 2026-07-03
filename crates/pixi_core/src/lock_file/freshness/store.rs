//! On-disk storage for the workspace input fingerprint marker.
//!
//! The marker lives at `<workspace>/.pixi/freshness/workspace.json`. Reads
//! degrade to "no marker" on any failure; writes are atomic (temp file +
//! rename) so a crashed pixi never leaves a truncated marker behind.

use std::path::{Path, PathBuf};

use super::FreshnessError;

/// Name of the directory under `.pixi/` that holds freshness markers.
pub const FRESHNESS_DIR: &str = "freshness";

/// File name of the workspace-scoped input fingerprint marker.
pub const WORKSPACE_FINGERPRINT_FILE: &str = "workspace.json";

/// Returns the path of the workspace fingerprint marker inside the given
/// `.pixi` directory.
pub fn fingerprint_path(pixi_dir: &Path) -> PathBuf {
    pixi_dir
        .join(FRESHNESS_DIR)
        .join(WORKSPACE_FINGERPRINT_FILE)
}

/// Reads the raw marker bytes, returning `None` when the file is missing or
/// unreadable. Never fails: an unreadable marker is simply not a marker.
pub async fn read_marker_bytes(path: &Path) -> Option<Vec<u8>> {
    match fs_err::tokio::read(path).await {
        Ok(bytes) => Some(bytes),
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    "failed to read the freshness marker at `{}`: {err}",
                    path.display()
                );
            }
            None
        }
    }
}

/// Atomically writes the marker bytes, creating parent directories as needed.
pub async fn write_marker_bytes(path: &Path, bytes: &[u8]) -> Result<(), FreshnessError> {
    if let Some(parent) = path.parent() {
        fs_err::tokio::create_dir_all(parent)
            .await
            .map_err(|source| FreshnessError::WriteMarker {
                path: path.to_path_buf(),
                source,
            })?;
    }
    pixi_utils::atomic_write::atomic_write(path, bytes)
        .await
        .map_err(|source| FreshnessError::WriteMarker {
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_path_is_under_freshness_dir() {
        let path = fingerprint_path(Path::new("/workspace/.pixi"));
        assert_eq!(path, Path::new("/workspace/.pixi/freshness/workspace.json"));
    }

    #[tokio::test]
    async fn write_then_read_roundtrips_and_creates_parents() {
        let temp = tempfile::tempdir().unwrap();
        // The `freshness/` directory does not exist yet.
        let path = fingerprint_path(&temp.path().join(".pixi"));

        write_marker_bytes(&path, b"{\"schema_version\":1}")
            .await
            .unwrap();
        assert_eq!(
            read_marker_bytes(&path).await.as_deref(),
            Some(b"{\"schema_version\":1}".as_slice())
        );

        // Overwriting replaces the content.
        write_marker_bytes(&path, b"{}").await.unwrap();
        assert_eq!(
            read_marker_bytes(&path).await.as_deref(),
            Some(b"{}".as_slice())
        );
    }

    #[tokio::test]
    async fn read_missing_marker_is_none() {
        let temp = tempfile::tempdir().unwrap();
        let path = fingerprint_path(&temp.path().join(".pixi"));
        assert_eq!(read_marker_bytes(&path).await, None);
    }
}
