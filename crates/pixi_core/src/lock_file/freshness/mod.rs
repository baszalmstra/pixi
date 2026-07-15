//! Workspace-scoped "inputs unchanged" freshness fingerprint.
//!
//! This module implements the L0→L1 edge of the unified freshness model: a
//! recorded, re-checkable trace of every input that feeds lock-file
//! resolution (manifest bytes, resolved config, lock bytes, override
//! environment variables, pixi version). When a previously recorded
//! fingerprint validates against the current inputs,
//! [`Workspace::update_lock_file`](crate::Workspace::update_lock_file) can
//! skip building the lock-file resolver and the whole outdated/satisfiability
//! walk.
//!
//! The feature is experimental and gated behind the
//! `[experimental] use-workspace-freshness-cache` configuration flag
//! (default off).
//!
//! Layering:
//! * [`probe`] — the pure probe model. Computation and validation are pure
//!   functions over a [`FingerprintInputs`] snapshot; no `Workspace`, no IO,
//!   no process-global state.
//! * [`store`] — atomic marker IO under `.pixi/freshness/`.
//! * this file — thin glue that adapts a [`Workspace`] to the pure model.
//!
//! Correctness stance: every failure mode (missing marker, corrupt marker,
//! unknown probe kind, unreadable input) degrades to the slow path, never to
//! trusting a stale environment. Fingerprints are only recorded after the
//! lock file has been proven up-to-date or freshly written, and — conservative
//! v1 — never when the lock file contains source packages, whose freshness
//! additionally depends on source files that v1 does not probe.
//!
//! # Known v1 limitations (flag is default-off and experimental)
//!
//! * **In-memory manifest edits.** The manifest probe hashes the manifest
//!   file *on disk*. Flows that mutate the manifest in memory and call
//!   `update_lock_file` before saving (the `WorkspaceMut`-based
//!   channel/platform commands in `pixi_api`) can therefore validate a
//!   fingerprint that does not describe the manifest being resolved: the
//!   in-flight command may skip a required re-solve (self-heals on the next
//!   invocation once the manifest is saved), and a marker recorded before a
//!   failed save binds the old manifest to the new lock. Fixing this needs
//!   the `Workspace` to expose the bytes it was actually parsed from —
//!   planned alongside the LazyLockFile rework of this region.
//! * **TOCTOU on record.** Inputs are re-read from disk at record time, a
//!   moment after they were proven; a concurrent external edit in that window
//!   is attested. The window is milliseconds and requires an external writer
//!   racing pixi.
//! * **Path source archives.** A pypi path dependency recorded while its
//!   location is not a directory (e.g. an sdist archive, or a not-yet
//!   materialized submodule) does not block recording; if that path later
//!   becomes a directory without any other input changing, the gate can skip
//!   the check that would inspect it. FileSet probes (round 2) own this.

mod probe;
mod store;

use std::path::{Path, PathBuf};

use pixi_consts::consts;
use rattler_lock::{LockFile, LockedPackage, UrlOrPath};
use thiserror::Error;

// The deliberate public surface of the freshness module: the two
// `update_lock_file` entry points plus the check result (whose `Stale`
// variant carries a `StaleReason`). Everything else — the probe model, the
// marker store, the raw evaluate/collect primitives — stays module-internal
// so the on-disk format and layering can evolve without breaking consumers.
use probe::{FingerprintInputs, WorkspaceFingerprint, collect_override_env_vars, evaluate_marker};
pub use probe::{FreshnessCheck, StaleReason};

use crate::Workspace;
use crate::environment::LockFileUsage;

/// Errors that can occur while gathering inputs or persisting the marker.
///
/// These are internal to the fast path: callers treat them as "no fast path"
/// (checks degrade to stale, recording is skipped with a debug log), so they
/// never surface as user-visible failures. The underlying error is embedded
/// in the message because these are only ever consumed via `Display` in
/// debug logs.
#[derive(Debug, Error)]
pub(crate) enum FreshnessError {
    /// A required input file could not be read.
    #[error("failed to read {what} at `{path}`: {source}")]
    ReadInput {
        /// What was being read (for diagnostics).
        what: &'static str,
        /// The path that failed to read.
        path: PathBuf,
        /// The underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// A value could not be serialized to JSON.
    #[error("failed to serialize {what}: {source}")]
    Serialize {
        /// What was being serialized (for diagnostics).
        what: &'static str,
        /// The underlying serde error.
        #[source]
        source: serde_json::Error,
    },

    /// The marker file could not be written.
    #[error("failed to write the freshness marker at `{path}`: {source}")]
    WriteMarker {
        /// The path that failed to write.
        path: PathBuf,
        /// The underlying IO error.
        #[source]
        source: std::io::Error,
    },
}

/// Returns the path of the workspace fingerprint marker for `workspace`:
/// `<workspace>/.pixi/freshness/workspace.json`.
///
/// Deliberately anchored at the workspace's default `.pixi` directory (not a
/// detached-environments directory): the marker describes the workspace's
/// inputs and belongs next to them. On workspaces where `.pixi` is not
/// writable the marker write fails softly and the feature simply never
/// engages.
fn workspace_fingerprint_path(workspace: &Workspace) -> PathBuf {
    store::fingerprint_path(&workspace.default_pixi_dir())
}

/// Serializes the resolved configuration to canonical JSON bytes suitable
/// for digesting.
///
/// `Config` contains `HashMap` fields (mirrors, s3-options) whose iteration
/// order is randomized per instance, so a direct `serde_json::to_vec` would
/// produce different bytes on every run — the `resolved-config` digest would
/// then never match and the fast path would silently never engage for users
/// of those settings. Round-tripping through [`serde_json::Value`] with an
/// explicit recursive key sort produces stable bytes regardless of the
/// source map type and regardless of whether serde_json's `preserve_order`
/// feature is enabled somewhere in the dependency graph.
fn canonical_config_json(config: &pixi_config::Config) -> Result<Vec<u8>, serde_json::Error> {
    fn sort_keys(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let mut entries: Vec<(String, serde_json::Value)> = map
                    .into_iter()
                    .map(|(key, value)| (key, sort_keys(value)))
                    .collect();
                entries.sort_by(|(left, _), (right, _)| left.cmp(right));
                serde_json::Value::Object(entries.into_iter().collect())
            }
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.into_iter().map(sort_keys).collect())
            }
            other => other,
        }
    }

    serde_json::to_value(config).and_then(|value| serde_json::to_vec(&sort_keys(value)))
}

/// Gathers the current values of every input covered by the v1 probes.
async fn gather_inputs(workspace: &Workspace) -> Result<FingerprintInputs, FreshnessError> {
    let manifest_path = workspace.workspace.provenance.absolute_path();
    let manifest_bytes = fs_err::tokio::read(&manifest_path)
        .await
        .map_err(|source| FreshnessError::ReadInput {
            what: "the workspace manifest",
            path: manifest_path.clone(),
            source,
        })?;

    let config_json =
        canonical_config_json(workspace.config()).map_err(|source| FreshnessError::Serialize {
            what: "the resolved configuration",
            source,
        })?;

    let lock_file_path = workspace.lock_file_path();
    let lock_file_bytes = match fs_err::tokio::read(&lock_file_path).await {
        Ok(bytes) => Some(bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(FreshnessError::ReadInput {
                what: "the lock file",
                path: lock_file_path,
                source,
            });
        }
    };

    let override_env_vars = collect_override_env_vars(std::env::vars_os().map(|(name, value)| {
        (
            name.to_string_lossy().into_owned(),
            value.to_string_lossy().into_owned(),
        )
    }));

    Ok(FingerprintInputs {
        manifest_path: manifest_path.to_string_lossy().into_owned(),
        manifest_bytes,
        config_json,
        lock_file_bytes,
        override_env_vars,
        pixi_version: consts::PIXI_VERSION.to_string(),
    })
}

/// Evaluates the on-disk workspace fingerprint against the current inputs.
///
/// Self-guarding: when the experimental flag is off this returns
/// [`FreshnessCheck::Absent`] without touching the disk, so no caller can
/// accidentally honor a marker for a user who has opted out.
///
/// Never fails: a missing or corrupt marker is [`FreshnessCheck::Absent`] and
/// an unreadable input is [`FreshnessCheck::Stale`], both of which callers
/// treat as "take the slow path".
pub async fn check_workspace_freshness(workspace: &Workspace) -> FreshnessCheck {
    if !workspace
        .config()
        .experimental_workspace_freshness_cache_usage()
    {
        return FreshnessCheck::Absent;
    }

    let marker_path = workspace_fingerprint_path(workspace);
    let Some(marker_bytes) = store::read_marker_bytes(&marker_path).await else {
        return FreshnessCheck::Absent;
    };

    let inputs = match gather_inputs(workspace).await {
        Ok(inputs) => inputs,
        Err(err) => {
            return FreshnessCheck::Stale(StaleReason::InputUnavailable {
                reason: err.to_string(),
            });
        }
    };

    evaluate_marker(Some(&marker_bytes), &inputs)
}

/// Records the workspace input fingerprint after a successful lock-file
/// check or update.
///
/// This function is the single owner of the record-eligibility predicate;
/// callers pass the flow facts (`lock_file_usage` and whether the on-disk
/// lock file still uses an old format) instead of pre-filtering. Recording
/// is skipped when:
/// * the experimental flag is off,
/// * the on-disk lock file needs a format upgrade (a marker would let the
///   next run fast-path past the upgrade),
/// * the run is a dry run (the on-disk lock file does not match the result),
/// * the lock file contains source packages (conda source packages on any
///   platform, or pypi path dependencies resolving to a directory) — their
///   freshness depends on source files that v1 does not probe.
///
/// Best effort and non-fatal by design: failures are logged at debug level
/// and otherwise ignored (the only consequence is that the next run takes
/// the slow path).
pub async fn record_workspace_freshness(
    workspace: &Workspace,
    lock_file: &LockFile,
    lock_file_usage: LockFileUsage,
    needs_format_upgrade: bool,
) {
    if !workspace
        .config()
        .experimental_workspace_freshness_cache_usage()
    {
        return;
    }

    if needs_format_upgrade || lock_file_usage == LockFileUsage::DryRun {
        return;
    }

    if lock_file_contains_source_packages(lock_file, workspace.root()) {
        tracing::debug!(
            "not recording the workspace input fingerprint: the lock file contains source packages"
        );
        return;
    }

    match try_record(workspace).await {
        Ok(path) => {
            tracing::debug!(
                "recorded the workspace input fingerprint at `{}`",
                path.display()
            );
        }
        Err(err) => {
            tracing::debug!("failed to record the workspace input fingerprint: {err}");
        }
    }
}

/// Computes and atomically persists the fingerprint, returning the marker
/// path on success.
async fn try_record(workspace: &Workspace) -> Result<PathBuf, FreshnessError> {
    let inputs = gather_inputs(workspace).await?;
    let fingerprint = WorkspaceFingerprint::compute(&inputs);
    let bytes = fingerprint
        .to_json_bytes()
        .map_err(|source| FreshnessError::Serialize {
            what: "the workspace input fingerprint",
            source,
        })?;
    let path = workspace_fingerprint_path(workspace);
    store::write_marker_bytes(&path, &bytes).await?;
    Ok(path)
}

/// Returns true when any environment in the lock file contains source
/// packages on any platform: conda source packages, or pypi packages whose
/// location is a path resolving to a directory (the kind of dependency that
/// changes without the lock file changing).
///
/// A sibling of this rule lives inline in `UpdateContextInner::cached_prefix`
/// (`lock_file/update.rs`), scoped to the current platform and manifest
/// declarations. This scan is deliberately broader (all environments, all
/// platforms, locked locations) because it gates skipping the entire
/// up-to-date walk; when changing one, review the other.
fn lock_file_contains_source_packages(lock_file: &LockFile, workspace_root: &Path) -> bool {
    lock_file.environments().any(|(_, environment)| {
        environment.packages_by_platform().any(|(_, mut packages)| {
            packages.any(|package| match package {
                LockedPackage::Conda(conda) => conda.as_source().is_some(),
                LockedPackage::Pypi(pypi) => match &**pypi.location() {
                    UrlOrPath::Path(path) => {
                        let absolute_path = if path.is_absolute() {
                            PathBuf::from(path.as_str())
                        } else {
                            workspace_root.join(Path::new(path.as_str()))
                        };
                        absolute_path.is_dir()
                    }
                    UrlOrPath::Url(_) => false,
                },
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use rattler_conda_types::Platform;
    use rattler_lock::{PlatformData, PypiPackageData, PypiSourceData, SourceData, Verbatim};

    use super::*;

    const MANIFEST: &str = "[workspace]\nname = \"fresh\"\nchannels = []\nplatforms = []\n";

    /// Builds a workspace in a temp dir with the freshness flag set in the
    /// project-local config.
    fn workspace_with_flag(temp: &Path, enabled: bool) -> Workspace {
        let manifest_path = temp.join("pixi.toml");
        fs_err::write(&manifest_path, MANIFEST).unwrap();
        let pixi_dir = temp.join(".pixi");
        fs_err::create_dir_all(&pixi_dir).unwrap();
        fs_err::write(
            pixi_dir.join("config.toml"),
            format!("[experimental]\nuse-workspace-freshness-cache = {enabled}\n"),
        )
        .unwrap();
        Workspace::from_str(&manifest_path, MANIFEST).unwrap()
    }

    /// Builds a lock file whose default environment contains a single pypi
    /// package located at `location`.
    fn lock_with_pypi_path_package(location: &str) -> LockFile {
        let platform = Platform::Linux64;
        let package = PypiPackageData::Source(Box::new(PypiSourceData {
            name: "mypkg".parse().unwrap(),
            location: Verbatim::new(UrlOrPath::Path(location.into())),
            requires_dist: vec![],
            requires_python: None,
            source_data: SourceData::default(),
        }));
        LockFile::builder()
            .with_platforms(vec![PlatformData {
                name: (&platform).into(),
                subdir: platform,
                virtual_packages: vec![],
            }])
            .unwrap()
            .with_pypi_package("default", "linux-64", package)
            .unwrap()
            .finish()
    }

    #[test]
    fn config_digest_is_stable_across_map_iteration_orders() {
        // `Config` contains `HashMap` fields (mirrors, s3-options) whose
        // iteration order is randomized per instance. The serialized config
        // must be canonicalized before hashing, otherwise every process would
        // compute a different `resolved-config` digest and the fast path
        // would silently never engage for users of those settings.
        let mirror_urls: Vec<(url::Url, Vec<url::Url>)> = (0..12)
            .map(|index| {
                (
                    url::Url::parse(&format!("https://channel-{index}.example.com")).unwrap(),
                    vec![url::Url::parse(&format!("https://mirror-{index}.example.com")).unwrap()],
                )
            })
            .collect();

        // Two configs with identical content, built with opposite insertion
        // orders (fresh `HashMap` instances hash-seed differently).
        let forward = pixi_config::Config {
            mirrors: mirror_urls.iter().cloned().collect(),
            ..pixi_config::Config::default()
        };
        let backward = pixi_config::Config {
            mirrors: mirror_urls.iter().rev().cloned().collect(),
            ..pixi_config::Config::default()
        };

        assert_eq!(
            canonical_config_json(&forward).unwrap(),
            canonical_config_json(&backward).unwrap(),
            "equal configs must serialize to identical canonical bytes"
        );
    }

    #[tokio::test]
    async fn record_then_check_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = workspace_with_flag(temp.path(), true);
        assert!(
            workspace
                .config()
                .experimental_workspace_freshness_cache_usage()
        );
        let marker = workspace_fingerprint_path(&workspace);

        // No marker yet.
        assert_eq!(
            check_workspace_freshness(&workspace).await,
            FreshnessCheck::Absent
        );

        // Record against an empty lock file (no source packages).
        record_workspace_freshness(
            &workspace,
            &LockFile::default(),
            LockFileUsage::Update,
            false,
        )
        .await;
        assert!(marker.is_file(), "marker should exist at {marker:?}");

        // Unchanged inputs validate as fresh.
        assert_eq!(
            check_workspace_freshness(&workspace).await,
            FreshnessCheck::Fresh
        );

        // Changing the manifest on disk invalidates the fingerprint.
        fs_err::write(
            temp.path().join("pixi.toml"),
            MANIFEST.replace("fresh", "renamed"),
        )
        .unwrap();
        assert_eq!(
            check_workspace_freshness(&workspace).await,
            FreshnessCheck::Stale(StaleReason::ManifestChanged)
        );

        // A corrupt marker degrades to absent, not an error.
        fs_err::write(&marker, "definitely-not-json").unwrap();
        assert_eq!(
            check_workspace_freshness(&workspace).await,
            FreshnessCheck::Absent
        );
    }

    #[tokio::test]
    async fn lock_file_appearing_invalidates_fingerprint() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = workspace_with_flag(temp.path(), true);

        // Recorded with no lock file on disk (absent sentinel).
        record_workspace_freshness(
            &workspace,
            &LockFile::default(),
            LockFileUsage::Update,
            false,
        )
        .await;
        assert_eq!(
            check_workspace_freshness(&workspace).await,
            FreshnessCheck::Fresh
        );

        // A lock file appearing on disk is a lock content change.
        fs_err::write(workspace.lock_file_path(), "version: 6\n").unwrap();
        assert_eq!(
            check_workspace_freshness(&workspace).await,
            FreshnessCheck::Stale(StaleReason::LockFileChanged)
        );
    }

    #[tokio::test]
    async fn record_skips_when_flag_off() {
        let temp = tempfile::tempdir().unwrap();
        // Explicitly `false` so a developer's global config cannot leak in.
        let workspace = workspace_with_flag(temp.path(), false);

        record_workspace_freshness(
            &workspace,
            &LockFile::default(),
            LockFileUsage::Update,
            false,
        )
        .await;
        assert!(!workspace_fingerprint_path(&workspace).exists());
    }

    #[tokio::test]
    async fn record_skips_with_pypi_directory_source_package() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = workspace_with_flag(temp.path(), true);

        // A pypi path dependency pointing at an existing directory blocks
        // recording (v1 has no probes for source file sets).
        fs_err::create_dir_all(temp.path().join("mypkg")).unwrap();
        let lock = lock_with_pypi_path_package("./mypkg");
        assert!(lock_file_contains_source_packages(&lock, workspace.root()));
        record_workspace_freshness(&workspace, &lock, LockFileUsage::Update, false).await;
        assert!(!workspace_fingerprint_path(&workspace).exists());

        // A path location that is not a directory (e.g. an sdist archive)
        // does not block recording.
        let lock = lock_with_pypi_path_package("./mypkg-1.0.tar.gz");
        assert!(!lock_file_contains_source_packages(&lock, workspace.root()));
        record_workspace_freshness(&workspace, &lock, LockFileUsage::Update, false).await;
        assert!(workspace_fingerprint_path(&workspace).is_file());
    }
}
