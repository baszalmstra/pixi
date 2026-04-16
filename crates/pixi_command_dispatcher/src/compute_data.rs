//! Extension traits on [`pixi_compute_engine::DataStore`] for ergonomic access
//! to pixi-specific shared resources from within [`Key::compute`] bodies.
//!
//! Values are registered at engine construction time by the
//! [`CommandDispatcherBuilder`](crate::CommandDispatcherBuilder) and are
//! immutable for the engine's lifetime.

use std::sync::Arc;

use pixi_compute_engine::DataStore;
use pixi_git::resolver::GitResolver;
use pixi_url::UrlResolver;
use rattler_networking::LazyClient;
use rattler_repodata_gateway::Gateway;
use tokio::sync::Semaphore;

use crate::{CacheDirs, command_dispatcher::CommandDispatcherData, reporter::Reporter};

/// Access the conda repodata gateway from global data.
pub trait HasGateway {
    fn gateway(&self) -> &Gateway;
}

impl HasGateway for DataStore {
    fn gateway(&self) -> &Gateway {
        &self.get::<Arc<CommandDispatcherData>>().gateway
    }
}

/// Access the git resolver from global data.
pub trait HasGitResolver {
    fn git_resolver(&self) -> &GitResolver;
}

impl HasGitResolver for DataStore {
    fn git_resolver(&self) -> &GitResolver {
        &self.get::<Arc<CommandDispatcherData>>().git_resolver
    }
}

/// Access the URL resolver from global data.
pub trait HasUrlResolver {
    fn url_resolver(&self) -> &UrlResolver;
}

impl HasUrlResolver for DataStore {
    fn url_resolver(&self) -> &UrlResolver {
        &self.get::<Arc<CommandDispatcherData>>().url_resolver
    }
}

/// Access the download client from global data.
pub trait HasDownloadClient {
    fn download_client(&self) -> &LazyClient;
}

impl HasDownloadClient for DataStore {
    fn download_client(&self) -> &LazyClient {
        &self.get::<Arc<CommandDispatcherData>>().download_client
    }
}

/// Access the cache directories from global data.
pub trait HasCacheDirs {
    fn cache_dirs(&self) -> &CacheDirs;
}

impl HasCacheDirs for DataStore {
    fn cache_dirs(&self) -> &CacheDirs {
        &self.get::<Arc<CommandDispatcherData>>().cache_dirs
    }
}

/// Access the optional dispatcher reporter from global data.
pub trait HasReporter {
    fn reporter(&self) -> Option<&Arc<dyn Reporter>>;
}

impl HasReporter for DataStore {
    fn reporter(&self) -> Option<&Arc<dyn Reporter>> {
        self.get::<Arc<CommandDispatcherData>>().reporter.as_ref()
    }
}

/// Access the semaphore bounding concurrent git checkouts.
pub trait HasGitCheckoutSemaphore {
    fn git_checkout_semaphore(&self) -> &Arc<Semaphore>;
}

impl HasGitCheckoutSemaphore for DataStore {
    fn git_checkout_semaphore(&self) -> &Arc<Semaphore> {
        &self
            .get::<Arc<CommandDispatcherData>>()
            .git_checkout_semaphore
    }
}

/// Access the semaphore bounding concurrent URL archive fetches.
pub trait HasUrlCheckoutSemaphore {
    fn url_checkout_semaphore(&self) -> &Arc<Semaphore>;
}

impl HasUrlCheckoutSemaphore for DataStore {
    fn url_checkout_semaphore(&self) -> &Arc<Semaphore> {
        &self
            .get::<Arc<CommandDispatcherData>>()
            .url_checkout_semaphore
    }
}
