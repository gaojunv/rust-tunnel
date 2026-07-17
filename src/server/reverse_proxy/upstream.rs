//! Upstream HTTP client used by the reverse proxy.
//!
//! Wraps `hyper-util`'s `legacy::Client` and exposes `forward()` which sends
//! a boxed-body request to a backend and returns the raw hyper response.

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use std::time::Duration;

/// Body type carried through the proxy pipeline.
///
/// Both hyper request bodies and axum request bodies are converted to this
/// boxed type before hitting the upstream client so all four client instances
/// share one concrete `Body` type.
pub type ProxyBody = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("invalid backend address: {0}")]
    BadBackendAddr(String),
    #[error("upstream connect failed: {0}")]
    Connect(String),
    #[error("upstream request failed: {0}")]
    Request(#[from] hyper_util::client::legacy::Error),
    #[error("body forward error: {0}")]
    Body(String),
}

/// Upstream HTTP client. PR 1 only carries `h1_plain`; PR 2 adds the other
/// three quadrants.
pub struct UpstreamClient {
    h1_plain: Client<HttpConnector, ProxyBody>,
}

impl UpstreamClient {
    #[must_use]
    pub fn new() -> Self {
        let h1_plain = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(60))
            .build::<_, ProxyBody>(HttpConnector::new());
        Self { h1_plain }
    }

    /// Forward `req` to the given plain-HTTP/1 backend.
    pub async fn forward_h1_plain(
        &self,
        req: Request<ProxyBody>,
    ) -> Result<Response<Incoming>, ProxyError> {
        self.h1_plain.request(req).await.map_err(ProxyError::from)
    }
}

impl Default for UpstreamClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_client_constructs() {
        let _c = UpstreamClient::new();
    }
}
