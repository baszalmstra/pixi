use pixi_compute_engine::ComputeError;
use pixi_git::{GitError, GitUrl, git::GitReference, source::Fetch};
use pixi_path::AbsPathBuf;
use pixi_record::{PinnedGitCheckout, PinnedGitSpec, PinnedSourceSpec};
use pixi_spec::GitSpec;

use crate::{
    CommandDispatcher, CommandDispatcherError, SourceCheckout, SourceCheckoutError,
    checkout::CheckoutGit,
};

impl CommandDispatcher {
    /// Check out the git repository associated with the given spec.
    pub async fn pin_and_checkout_git(
        &self,
        git_spec: GitSpec,
    ) -> Result<SourceCheckout, CommandDispatcherError<SourceCheckoutError>> {
        // Determine the git url, including the reference
        let git_reference = git_spec
            .rev
            .clone()
            .map(|rev| rev.into())
            .unwrap_or(GitReference::DefaultBranch);
        let pinned_git_reference = git_spec.rev.unwrap_or_default();

        let git_url = GitUrl::try_from(git_spec.git)
            .map_err(GitError::from)
            .map_err(SourceCheckoutError::GitError)
            .map_err(CommandDispatcherError::Failed)?
            .with_reference(git_reference.clone());

        // Fetch the git url via the compute engine
        let fetch = self
            .checkout_git_url(git_url)
            .await
            .map_err(|err| err.map(SourceCheckoutError::from))?;

        // Determine the pinned spec from the fetch
        let pinned = PinnedGitSpec {
            git: fetch.repository().url.clone().into_url(),
            source: PinnedGitCheckout {
                commit: fetch.commit(),
                subdirectory: git_spec.subdirectory.clone(),
                reference: pinned_git_reference,
            },
        };

        Self::fetch_to_checkout(fetch, pinned)
    }

    /// Check out a particular git repository via the compute engine.
    pub async fn checkout_git_url(
        &self,
        git_url: GitUrl,
    ) -> Result<Fetch, CommandDispatcherError<GitError>> {
        let result = self
            .engine
            .compute(&CheckoutGit::new(&git_url))
            .await
            .map_err(|e| match e {
                ComputeError::Cycle(_) | ComputeError::Canceled => {
                    CommandDispatcherError::Cancelled
                }
            })?;

        match result.as_ref() {
            Ok(fetch) => Ok(fetch.clone()),
            Err(e) => Err(CommandDispatcherError::Failed(e.clone())),
        }
    }

    /// Checkout a pinned git repository.
    pub async fn checkout_pinned_git(
        &self,
        git_spec: PinnedGitSpec,
    ) -> Result<SourceCheckout, CommandDispatcherError<SourceCheckoutError>> {
        let git_url = GitUrl::from_commit(
            git_spec.git.clone(),
            git_spec.source.reference.clone().into(),
            git_spec.source.commit,
        );
        // Fetch the git url via the compute engine
        let fetch = self
            .checkout_git_url(git_url)
            .await
            .map_err(|err| err.map(SourceCheckoutError::from))?;

        Self::fetch_to_checkout(fetch, git_spec)
    }

    #[allow(clippy::result_large_err)]
    fn fetch_to_checkout(
        fetch: Fetch,
        pinned: PinnedGitSpec,
    ) -> Result<SourceCheckout, CommandDispatcherError<SourceCheckoutError>> {
        let root_dir = AbsPathBuf::new(fetch.into_path())
            .expect("git checkout returned a relative path")
            .into_assume_dir();

        let path = if !pinned.source.subdirectory.is_empty() {
            root_dir
                .join(pinned.source.subdirectory.as_path())
                .into_assume_dir()
        } else {
            root_dir
        };

        Ok(SourceCheckout {
            path: path.into(),
            pinned: PinnedSourceSpec::Git(pinned),
        })
    }
}
