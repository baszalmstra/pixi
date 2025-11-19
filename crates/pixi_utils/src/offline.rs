use rattler_redaction::Redact;
use reqwest_middleware::Middleware;
use url::Url;

#[derive(Debug, thiserror::Error)]
#[error("Network access is disabled, could not find a cache response for: '{url}'")]
pub struct OfflineError {
    pub url: Url,
}

pub struct OfflineMiddleware;

#[async_trait::async_trait]
impl Middleware for OfflineMiddleware {
    async fn handle(
        &self,
        req: reqwest::Request,
        _extensions: &mut http::Extensions,
        _next: reqwest_middleware::Next<'_>,
    ) -> reqwest_middleware::Result<reqwest::Response> {
        Err(reqwest_middleware::Error::Middleware(
            OfflineError {
                url: req.url().clone().redact(),
            }
            .into(),
        ))
    }
}
