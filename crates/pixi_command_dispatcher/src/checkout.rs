//! Compute engine keys for source checkout operations.

use std::{future::Future, sync::Arc};

use derive_more::Display;
use pixi_compute_engine::{ComputeCtx, Key};
use pixi_git::{GitError, GitUrl, resolver::RepositoryReference, source::Fetch as GitFetch};
use pixi_path::AbsPathBuf;
use pixi_spec::UrlSpec;
use pixi_url::UrlError;
use url::Url;

use crate::{
    command_dispatcher::url::UrlCheckout,
    compute_data::{
        HasCacheDirs, HasDownloadClient, HasGitCheckoutSemaphore, HasGitResolver, HasReporter,
        HasUrlCheckoutSemaphore, HasUrlResolver,
    },
    reporter::{
        GitCheckoutId, GitCheckoutReporter, Reporter, ReporterContext, UrlCheckoutId,
        UrlCheckoutReporter,
    },
    reporter_context::current_reporter_context,
    reporter_lifecycle::{Active, LifecycleKind, ReporterLifecycle},
};

/// `LifecycleKind` for git checkouts.
struct GitReporterLifecycle;

impl LifecycleKind for GitReporterLifecycle {
    type Reporter<'r> = dyn GitCheckoutReporter + 'r;
    type Id = GitCheckoutId;
    type Env = RepositoryReference;

    fn queue<'r>(
        reporter: Option<&'r dyn Reporter>,
        parent: Option<ReporterContext>,
        env: &Self::Env,
    ) -> Option<Active<'r, Self::Reporter<'r>, Self::Id>> {
        reporter
            .and_then(|r| r.as_git_reporter())
            .map(|r| Active {
                reporter: r,
                id: r.on_queued(parent, env),
            })
    }

    fn on_started<'r>(active: &Active<'r, Self::Reporter<'r>, Self::Id>) {
        active.reporter.on_started(active.id);
    }

    fn on_finished<'r>(active: Active<'r, Self::Reporter<'r>, Self::Id>) {
        active.reporter.on_finished(active.id);
    }
}

/// `LifecycleKind` for url checkouts.
struct UrlReporterLifecycle;

impl LifecycleKind for UrlReporterLifecycle {
    type Reporter<'r> = dyn UrlCheckoutReporter + 'r;
    type Id = UrlCheckoutId;
    type Env = Url;

    fn queue<'r>(
        reporter: Option<&'r dyn Reporter>,
        parent: Option<ReporterContext>,
        env: &Self::Env,
    ) -> Option<Active<'r, Self::Reporter<'r>, Self::Id>> {
        reporter
            .and_then(|r| r.as_url_reporter())
            .map(|r| Active {
                reporter: r,
                id: r.on_queued(parent, env),
            })
    }

    fn on_started<'r>(active: &Active<'r, Self::Reporter<'r>, Self::Id>) {
        active.reporter.on_started(active.id);
    }

    fn on_finished<'r>(active: Active<'r, Self::Reporter<'r>, Self::Id>) {
        active.reporter.on_finished(active.id);
    }
}

/// Dedup key for a git checkout. Keyed on [`RepositoryReference`]
/// (normalized URL + reference, no `precise`) so callers that differ
/// only in whether they pre-resolved the commit still dedup.
#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("{}@{}", _0.url.as_url(), _0.reference)]
pub(crate) struct CheckoutGit(RepositoryReference);

impl CheckoutGit {
    pub(crate) fn new(git_url: &GitUrl) -> Self {
        Self(RepositoryReference::from(git_url))
    }
}

impl Key for CheckoutGit {
    type Value = Arc<Result<GitFetch, GitError>>;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        let data = ctx.global_data();
        let resolver = data.git_resolver().clone();
        let client = data.download_client().clone();
        let cache_dir = data.cache_dirs().git();
        let semaphore = data.git_checkout_semaphore().clone();
        let reporter = data.reporter().map(Arc::as_ref);

        let lifecycle = ReporterLifecycle::<GitReporterLifecycle>::queued(
            reporter,
            current_reporter_context(),
            &self.0,
        );

        let _permit = semaphore
            .acquire()
            .await
            .expect("git checkout semaphore is never closed");
        let _lifecycle = lifecycle.start();

        // `from_reference` auto-fills `precise` from a full-commit
        // reference, so the resolver skips ref-resolution when it can.
        let git_url =
            GitUrl::from_reference(self.0.url.clone().into_url(), self.0.reference.clone());

        Arc::new(
            resolver
                .fetch(git_url, client, cache_dir.into_std_path_buf(), None)
                .await,
        )
    }
}

#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("{_0}")]
pub(crate) struct CheckoutUrl(pub UrlSpec);

impl Key for CheckoutUrl {
    type Value = Arc<Result<UrlCheckout, UrlError>>;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        let data = ctx.global_data();
        let resolver = data.url_resolver().clone();
        let client = data.download_client().clone();
        let cache_dir = data.cache_dirs().url();
        let semaphore = data.url_checkout_semaphore().clone();
        let reporter = data.reporter().map(Arc::as_ref);

        let lifecycle = ReporterLifecycle::<UrlReporterLifecycle>::queued(
            reporter,
            current_reporter_context(),
            &self.0.url,
        );

        let _permit = semaphore
            .acquire()
            .await
            .expect("url checkout semaphore is never closed");
        let _lifecycle = lifecycle.start();

        Arc::new(
            resolver
                .fetch(self.0.clone(), client, cache_dir.into_std_path_buf(), None)
                .await
                .map(|fetch| UrlCheckout {
                    pinned_url: fetch.pinned().clone(),
                    dir: AbsPathBuf::new(fetch.path())
                        .expect("url fetch does not return absolute path")
                        .into_assume_dir(),
                }),
        )
    }
}

#[allow(dead_code)]
pub(crate) trait CheckoutExt {
    fn checkout_git(
        &mut self,
        git_url: &GitUrl,
    ) -> impl Future<Output = Result<GitFetch, GitError>>;

    fn checkout_url(
        &mut self,
        url_spec: &UrlSpec,
    ) -> impl Future<Output = Result<UrlCheckout, UrlError>>;
}

impl CheckoutExt for ComputeCtx {
    async fn checkout_git(&mut self, git_url: &GitUrl) -> Result<GitFetch, GitError> {
        self.compute(&CheckoutGit::new(git_url))
            .await
            .as_ref()
            .clone()
    }

    async fn checkout_url(&mut self, url_spec: &UrlSpec) -> Result<UrlCheckout, UrlError> {
        self.compute(&CheckoutUrl(url_spec.clone()))
            .await
            .as_ref()
            .clone()
    }
}
