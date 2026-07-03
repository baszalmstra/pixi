//! A lazily-parsed handle around the workspace lock file.
//!
//! [`LazyLockFile`] lets [`LockFileDerivedData`] hold either an already
//! parsed [`LockFile`] or only the on-disk location of one, deferring the
//! YAML parse until a consumer actually needs the lock file's contents. A
//! fast path that can prove the lock file is unchanged can hand out an
//! unloaded handle and skip the parse entirely unless the contents are
//! actually used.
//!
//! [`LockFileDerivedData`]: super::LockFileDerivedData

use std::path::PathBuf;

use pixi_manifest::WorkspaceManifest;
use rattler_lock::LockFile;

use super::update::load_lock_file_from_path;

/// The on-disk source of a lock file that has not been parsed yet.
///
/// Next to the path this carries the same context that
/// [`crate::Workspace::load_lock_file`] captures: after parsing, locked
/// platform names are realigned to the manifest's current platform names.
#[derive(Clone)]
struct UnloadedLockFile {
    /// The path to the lock file on disk.
    path: PathBuf,
    /// The manifest whose platform names the parsed lock file is aligned to.
    manifest: WorkspaceManifest,
    /// The workspace root, used to resolve relative paths while aligning
    /// platform names.
    workspace_root: PathBuf,
}

impl UnloadedLockFile {
    /// Parses the lock file from disk, mirroring the semantics of
    /// [`crate::Workspace::load_lock_file`] followed by the graceful
    /// version-mismatch handling of `update_lock_file`: a missing file
    /// yields an empty lock file, and a lock file whose version is newer
    /// than supported prints a warning and also falls back to an empty one.
    async fn load(self) -> miette::Result<LockFile> {
        load_lock_file_from_path(self.path, self.manifest, self.workspace_root)
            .await
            .map(|result| result.into_lock_file_or_empty_with_warning())
    }
}

/// The two states of a [`LazyLockFile`].
enum State {
    /// The lock file is already parsed.
    Loaded(LockFile),
    /// The lock file has not been parsed yet: `cell` caches the parse of
    /// `source` on first access. The source is boxed because the manifest
    /// it carries dwarfs the `Loaded` variant.
    Unloaded {
        cell: async_once_cell::OnceCell<LockFile>,
        source: Box<UnloadedLockFile>,
    },
}

/// A [`LockFile`] that is either already parsed or is parsed from disk on
/// first access.
///
/// An `Unloaded(path) | Loaded(LockFile)` handle: [`LazyLockFile::loaded`]
/// wraps an already parsed lock file while [`LazyLockFile::unloaded`] only
/// records where to find one. Loading goes through the same machinery as
/// [`crate::Workspace::load_lock_file`] (platform-name alignment included),
/// followed by `update_lock_file`'s *graceful* version-mismatch handling: a
/// lock file version newer than supported warns and falls back to an empty
/// lock file. The `--locked`/`--frozen` strictness that turns such a
/// mismatch into a hard error instead is applied by `update_lock_file`
/// before any handle exists; callers that need it must gate before handing
/// out an unloaded handle.
pub struct LazyLockFile {
    /// Whether the lock file is parsed already, and if not, where to load
    /// it from.
    state: State,
}

impl LazyLockFile {
    /// Wraps an already parsed lock file. Accessors never touch the
    /// filesystem.
    pub fn loaded(lock_file: LockFile) -> Self {
        Self {
            state: State::Loaded(lock_file),
        }
    }

    /// Constructs a handle that parses the lock file at `path` on first
    /// access. `manifest` and `workspace_root` provide the platform-name
    /// alignment context that [`crate::Workspace::load_lock_file`] also
    /// applies after parsing.
    pub fn unloaded(path: PathBuf, manifest: WorkspaceManifest, workspace_root: PathBuf) -> Self {
        Self {
            state: State::Unloaded {
                cell: async_once_cell::OnceCell::new(),
                source: Box::new(UnloadedLockFile {
                    path,
                    manifest,
                    workspace_root,
                }),
            },
        }
    }

    /// Returns the lock file, parsing it from disk on first access when this
    /// handle was constructed with [`Self::unloaded`]. Concurrent callers
    /// share a single parse, and every call after the first returns the
    /// cached instance.
    pub async fn get(&self) -> miette::Result<&LockFile> {
        match &self.state {
            State::Loaded(lock_file) => Ok(lock_file),
            State::Unloaded { cell, source } => {
                cell.get_or_try_init(async {
                    // Clone out of the shared handle: parsing hands the
                    // context to a blocking task, which needs ownership.
                    source.as_ref().clone().load().await
                })
                .await
            }
        }
    }

    /// Consumes the handle and returns the lock file, parsing it from disk
    /// first when it has not been accessed before.
    pub async fn into_inner(self) -> miette::Result<LockFile> {
        match self.state {
            State::Loaded(lock_file) => Ok(lock_file),
            State::Unloaded { cell, source } => match cell.into_inner() {
                Some(lock_file) => Ok(lock_file),
                None => (*source).load().await,
            },
        }
    }

    /// Returns the lock file if it has already been parsed, without
    /// triggering a load. For synchronous contexts that can tolerate the
    /// lock file's absence.
    pub fn as_loaded(&self) -> Option<&LockFile> {
        match &self.state {
            State::Loaded(lock_file) => Some(lock_file),
            State::Unloaded { cell, .. } => cell.get(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use pixi_manifest::{HasWorkspaceManifest as _, WorkspaceManifest};
    use rattler_lock::LockFile;

    use super::LazyLockFile;
    use crate::Workspace;

    /// A minimal workspace manifest supplying the platform-name alignment
    /// context that loading a lock file from disk requires.
    fn test_manifest(root: &Path) -> WorkspaceManifest {
        let manifest_toml = r#"
        [project]
        name = "lazy-lock-file-test"
        channels = []
        platforms = []
        "#;
        let workspace =
            Workspace::from_str(root.join("pixi.toml").as_path(), manifest_toml).unwrap();
        (&workspace).workspace_manifest().clone()
    }

    /// A lock file with a recognizable environment so tests can tell a lock
    /// file parsed from disk apart from `LockFile::default()`.
    fn distinctive_lock_file() -> LockFile {
        LockFile::builder()
            .with_channels("lazy-test-env", Vec::<rattler_lock::Channel>::new())
            .finish()
    }

    #[tokio::test]
    async fn loaded_get_returns_the_lock_file_without_touching_the_filesystem() {
        let lazy = LazyLockFile::loaded(distinctive_lock_file());

        // The value is available synchronously; there is no on-disk source
        // that could be consulted.
        assert!(lazy.as_loaded().is_some());
        let lock_file = lazy.get().await.unwrap();
        assert!(lock_file.environment("lazy-test-env").is_some());
    }

    #[tokio::test]
    async fn unloaded_get_parses_the_lock_file_once() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_file_path = tmp.path().join("pixi.lock");
        distinctive_lock_file().to_path(&lock_file_path).unwrap();

        let lazy = LazyLockFile::unloaded(
            lock_file_path,
            test_manifest(tmp.path()),
            tmp.path().to_path_buf(),
        );
        assert!(lazy.as_loaded().is_none());

        // The first access parses the file from disk.
        let first = lazy.get().await.unwrap();
        assert!(first.environment("lazy-test-env").is_some());

        // The second access returns the cached instance (`OnceCell`
        // semantics), not a re-parse.
        let second = lazy.get().await.unwrap();
        assert!(std::ptr::eq(first, second));
        assert!(lazy.as_loaded().is_some());
    }

    #[tokio::test]
    async fn unloaded_get_falls_back_to_an_empty_lock_file_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let lazy = LazyLockFile::unloaded(
            tmp.path().join("does-not-exist").join("pixi.lock"),
            test_manifest(tmp.path()),
            tmp.path().to_path_buf(),
        );

        // Mirrors `Workspace::load_lock_file`: a missing lock file behaves
        // like an empty one instead of erroring.
        let lock_file = lazy.get().await.unwrap();
        assert_eq!(lock_file.environments().len(), 0);
    }

    #[tokio::test]
    async fn into_inner_returns_the_lock_file_in_both_states() {
        // Loaded: hands back the value without any I/O.
        let loaded = LazyLockFile::loaded(distinctive_lock_file());
        let lock_file = loaded.into_inner().await.unwrap();
        assert!(lock_file.environment("lazy-test-env").is_some());

        // Unloaded: parses from disk on consumption.
        let tmp = tempfile::tempdir().unwrap();
        let lock_file_path = tmp.path().join("pixi.lock");
        distinctive_lock_file().to_path(&lock_file_path).unwrap();
        let unloaded = LazyLockFile::unloaded(
            lock_file_path,
            test_manifest(tmp.path()),
            tmp.path().to_path_buf(),
        );
        let lock_file = unloaded.into_inner().await.unwrap();
        assert!(lock_file.environment("lazy-test-env").is_some());
    }
}
