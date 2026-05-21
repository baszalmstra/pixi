use rattler_networking::LazyClient;
/// Derived from `uv-git` implementation
/// Source: https://github.com/astral-sh/uv/blob/4b8cc3e29e4c2a6417479135beaa9783b05195d3/crates/uv-git/src/source.rs
/// This module expose `GitSource` type that represents a remote Git source that
/// can be checked out locally.
use std::{
    borrow::Cow,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
};
use tracing::instrument;

use crate::{
    GitError, GitUrl, Reporter,
    credentials::GIT_STORE,
    git::GitRemote,
    resolver::RepositoryReference,
    sha::{GitOid, GitSha},
    url::RepositoryUrl,
};

/// Environment variable controlling whether Git LFS objects are fetched for
/// Git sources by default.
///
/// * Truthy values (`1`, `true`, `yes`, `on`) → `Some(true)`: always fetch LFS.
/// * Falsy values (`0`, `false`, `no`, `off`) → `Some(false)`: never fetch LFS.
/// * Unset or empty → `None`: leave the decision to the caller / git config.
pub const PIXI_GIT_LFS_ENV: &str = "PIXI_GIT_LFS";

/// Reads [`PIXI_GIT_LFS_ENV`] and returns its tri-state value.
///
/// `Some(true)`/`Some(false)` mean the user explicitly opted in/out;
/// `None` means the variable is unset and the caller should fall back to
/// whatever its own default is.
pub fn lfs_enabled_from_env() -> Option<bool> {
    let raw = std::env::var(PIXI_GIT_LFS_ENV).ok()?;
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    if value == "0"
        || value.eq_ignore_ascii_case("false")
        || value.eq_ignore_ascii_case("no")
        || value.eq_ignore_ascii_case("off")
    {
        return Some(false);
    }
    if value == "1"
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("yes")
        || value.eq_ignore_ascii_case("on")
    {
        return Some(true);
    }
    // Unknown value: treat as enabled, but log so the user can spot typos.
    tracing::warn!(
        "unrecognised value for {PIXI_GIT_LFS_ENV}: {raw:?}; treating as enabled. \
         Expected one of: 1/0, true/false, yes/no, on/off."
    );
    Some(true)
}

/// A remote Git source that can be checked out locally.
pub struct GitSource {
    /// The Git reference from the manifest file.
    git: GitUrl,
    /// The HTTP client to use for fetching.
    client: LazyClient,
    /// The path to the Git source database.
    cache: PathBuf,
    /// The reporter to use for this source.
    reporter: Option<Arc<dyn Reporter>>,
    /// Whether to fetch Git LFS objects for this source.
    ///
    /// * `Some(true)`  — always fetch LFS (and let the smudge filter run on
    ///   checkout).
    /// * `Some(false)` — never fetch LFS, and force `GIT_LFS_SKIP_SMUDGE=1`
    ///   on the git subprocesses that touch the working tree.
    /// * `None`        — no opinion: don't fetch LFS, and don't override
    ///   `GIT_LFS_SKIP_SMUDGE` either, deferring to the user's git config.
    lfs: Option<bool>,
}

impl GitSource {
    /// Initialize a new Git source.
    pub fn new(git: GitUrl, client: LazyClient, cache: impl Into<PathBuf>) -> Self {
        Self {
            git,
            client,
            cache: cache.into(),
            reporter: None,
            lfs: lfs_enabled_from_env(),
        }
    }

    /// Set the [`Reporter`] to use for the [`GitSource`].
    #[must_use]
    pub fn with_reporter(self, reporter: Arc<dyn Reporter>) -> Self {
        Self {
            reporter: Some(reporter),
            ..self
        }
    }

    /// Explicitly enable or disable Git LFS fetching for this source.
    ///
    /// Sets the tri-state LFS preference:
    /// * `Some(true)`  → always fetch LFS.
    /// * `Some(false)` → never fetch LFS, and force `GIT_LFS_SKIP_SMUDGE=1`.
    /// * `None`        → no opinion; defer to the user's git config and the
    ///   [`PIXI_GIT_LFS_ENV`] environment variable.
    #[must_use]
    pub fn with_lfs(self, lfs: Option<bool>) -> Self {
        Self { lfs, ..self }
    }

    /// Fetch the underlying Git repository at the given revision.
    #[instrument(skip(self), fields(repository = %self.git.repository, rev = self.git.precise.map(tracing::field::display)))]
    pub fn fetch(self) -> Result<Fetch, GitError> {
        // Compute the canonical URL for the repository.
        let canonical = RepositoryUrl::new(&self.git.repository);

        // The path to the repo, within the Git database.
        let ident = cache_digest(&canonical);
        let db_path = self.cache.join("db").join(&ident);

        // Authenticate the URL, if necessary.
        let remote = if let Some(credentials) = GIT_STORE.get(&canonical) {
            Cow::Owned(credentials.apply(self.git.repository.clone()))
        } else {
            Cow::Borrowed(&self.git.repository)
        };

        let remote = GitRemote::new(&remote);
        let (db, actual_rev, task) = match (self.git.precise, remote.db_at(&db_path).ok()) {
            // If we have a locked revision, and we have a preexisting database
            // which has that revision, then no update needs to happen.
            (Some(rev), Some(mut db)) if db.contains(rev.into()) => {
                tracing::debug!(
                    "Using existing Git source `{}` pointed at `{}`",
                    self.git.repository,
                    rev
                );
                // The regular fetch path is skipped here, so LFS objects
                // wouldn't otherwise be (re)validated. Do it explicitly when
                // the caller asked for LFS, so a cached DB without LFS blobs
                // still ends up populated.
                if self.lfs == Some(true) {
                    db.ensure_lfs(remote.url().as_str(), rev.into());
                }
                (db, rev, None)
            }

            // ... otherwise we use this state to update the git database. Note
            // that we still check for being offline here, for example in the
            // situation that we have a locked revision but the database
            // doesn't have it.
            (locked_rev, db) => {
                tracing::debug!("Updating Git source `{}`", self.git.repository);

                // Report the checkout operation to the reporter.
                let task = self.reporter.as_ref().map(|reporter| {
                    reporter.on_checkout_start(remote.url(), self.git.reference.as_rev())
                });

                let (db, actual_rev) = remote.checkout(
                    &db_path,
                    db,
                    &self.git.reference,
                    locked_rev.map(GitOid::from),
                    &self.client,
                    self.lfs,
                )?;

                (db, GitSha::from(actual_rev), task)
            }
        };

        // Don’t use the full hash, in order to contribute less to reaching the
        // path length limit on Windows.
        let short_id = db.to_short_id(actual_rev.into())?;

        // Check out `actual_rev` from the database to a scoped location on the
        // filesystem. This will use hard links and such to ideally make the
        // checkout operation here pretty fast.
        let checkout_path = self
            .cache
            .join("checkouts")
            .join(&ident)
            .join(short_id.as_str());

        tracing::debug!(
            "Copying git revision `{}` to path `{}`",
            actual_rev,
            checkout_path.display()
        );
        db.copy_to(actual_rev.into(), &checkout_path, self.lfs)?;

        // Report the checkout operation to the reporter.
        if let Some(task) = task
            && let Some(reporter) = self.reporter.as_ref()
        {
            reporter.on_checkout_complete(remote.url(), short_id.as_str(), task);
        }

        tracing::trace!("Finished fetching Git source `{}`", self.git.repository);

        Ok(Fetch {
            repository: RepositoryReference {
                url: canonical,
                reference: self.git.reference.clone(),
            },
            commit: actual_rev,
            path: checkout_path,
            lfs_ready: db.lfs_ready(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Fetch {
    /// The [`RepositoryReference`] reference that was fetched.
    repository: RepositoryReference,

    /// The precise git checkout
    commit: GitSha,

    /// The path to the checked-out repository.
    path: PathBuf,

    /// Outcome of the most recent `git lfs fsck` for this source:
    /// `Some(true)` means LFS objects validated, `Some(false)` means LFS
    /// was requested but `git-lfs` is missing or `fsck` reported problems,
    /// and `None` means LFS was never requested.
    lfs_ready: Option<bool>,
}

impl Fetch {
    pub fn repository(&self) -> &RepositoryReference {
        &self.repository
    }

    pub fn commit(&self) -> GitSha {
        self.commit
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn into_path(self) -> PathBuf {
        self.path
    }

    /// Returns the LFS validation state for this fetch. See
    /// [`Fetch::lfs_ready`] field docs for the meaning of each variant.
    pub fn lfs_ready(&self) -> Option<bool> {
        self.lfs_ready
    }
}

pub fn cache_digest(url: &RepositoryUrl) -> String {
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    let hash = hasher.finish();
    format!("{hash:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wraps env-var manipulation so concurrent tests in this module don't
    /// race on the process-global env. Cargo runs unit tests in parallel by
    /// default; serialising with a mutex avoids flaky behaviour when each
    /// test sets/unsets the same variable.
    fn with_env<R>(value: Option<&str>, body: impl FnOnce() -> R) -> R {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var(PIXI_GIT_LFS_ENV).ok();
        // SAFETY: tests are serialised by LOCK above.
        match value {
            Some(v) => unsafe { std::env::set_var(PIXI_GIT_LFS_ENV, v) },
            None => unsafe { std::env::remove_var(PIXI_GIT_LFS_ENV) },
        }
        let out = body();
        match previous {
            Some(v) => unsafe { std::env::set_var(PIXI_GIT_LFS_ENV, v) },
            None => unsafe { std::env::remove_var(PIXI_GIT_LFS_ENV) },
        }
        out
    }

    #[test]
    fn env_unset_is_none() {
        with_env(None, || assert_eq!(lfs_enabled_from_env(), None));
    }

    #[test]
    fn env_empty_is_none() {
        with_env(Some(""), || assert_eq!(lfs_enabled_from_env(), None));
        with_env(Some("   "), || assert_eq!(lfs_enabled_from_env(), None));
    }

    #[test]
    fn env_truthy_is_some_true() {
        for v in ["1", "true", "TRUE", "yes", "YES", "on", "ON"] {
            with_env(Some(v), || {
                assert_eq!(lfs_enabled_from_env(), Some(true), "value={v}")
            });
        }
    }

    #[test]
    fn env_falsy_is_some_false() {
        for v in ["0", "false", "FALSE", "no", "NO", "off", "OFF"] {
            with_env(Some(v), || {
                assert_eq!(lfs_enabled_from_env(), Some(false), "value={v}")
            });
        }
    }
}
