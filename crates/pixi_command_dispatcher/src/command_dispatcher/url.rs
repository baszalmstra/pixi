use pixi_compute_engine::ComputeError;
use pixi_path::AbsPresumedDirPathBuf;
use pixi_record::{PinnedSourceSpec, PinnedUrlSpec};
use pixi_spec::UrlSpec;
pub use pixi_url::UrlError;

use crate::{
    CommandDispatcher, CommandDispatcherError, SourceCheckout, SourceCheckoutError,
    checkout::CheckoutUrl,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlCheckout {
    pub pinned_url: PinnedUrlSpec,

    /// Directory which contains checkout.
    pub dir: AbsPresumedDirPathBuf,
}

impl UrlCheckout {
    pub fn into_path(self) -> AbsPresumedDirPathBuf {
        self.dir
    }
}

impl CommandDispatcher {
    /// Check out the url associated with the given spec.
    pub async fn pin_and_checkout_url(
        &self,
        url_spec: UrlSpec,
    ) -> Result<SourceCheckout, CommandDispatcherError<SourceCheckoutError>> {
        // Fetch the url via the compute engine
        let UrlCheckout { pinned_url, dir } = self
            .checkout_url(url_spec)
            .await
            .map_err(|err| err.map(SourceCheckoutError::from))?;

        Ok(SourceCheckout {
            path: dir.into(),
            pinned: PinnedSourceSpec::Url(pinned_url),
        })
    }

    /// Check out a particular url via the compute engine.
    pub async fn checkout_url(
        &self,
        url: UrlSpec,
    ) -> Result<UrlCheckout, CommandDispatcherError<UrlError>> {
        let result = self
            .engine
            .compute(&CheckoutUrl(url))
            .await
            .map_err(|e| match e {
                ComputeError::Cycle(_) | ComputeError::Canceled => {
                    CommandDispatcherError::Cancelled
                }
            })?;

        match result.as_ref() {
            Ok(checkout) => Ok(checkout.clone()),
            Err(e) => Err(CommandDispatcherError::Failed(e.clone())),
        }
    }

    /// Checkout a pinned url.
    pub async fn checkout_pinned_url(
        &self,
        pinned_url_spec: PinnedUrlSpec,
    ) -> Result<SourceCheckout, CommandDispatcherError<SourceCheckoutError>> {
        let url_spec = UrlSpec {
            url: pinned_url_spec.url.clone(),
            md5: pinned_url_spec.md5,
            sha256: Some(pinned_url_spec.sha256),
            subdirectory: pinned_url_spec.subdirectory.clone(),
        };
        // Fetch the url via the compute engine
        let fetch = self
            .checkout_url(url_spec)
            .await
            .map_err(|err| err.map(SourceCheckoutError::from))?;

        let path = if !pinned_url_spec.subdirectory.is_empty() {
            fetch
                .dir
                .join(pinned_url_spec.subdirectory.as_path())
                .into_assume_dir()
        } else {
            fetch.into_path()
        };

        Ok(SourceCheckout {
            path: path.into(),
            pinned: PinnedSourceSpec::Url(pinned_url_spec),
        })
    }
}
