//! Upstream HTTP client used by the reverse proxy.
//!
//! Wraps `hyper-util`'s `legacy::Client` and exposes `forward()` which sends
//! a boxed-body request to a backend and returns the raw hyper response.
//!
//! Four `Client` instances are kept, one per `(scheme, protocol)` combination:
//! `hyper-util`'s legacy `Client::builder().http2_only(bool)` bakes the
//! protocol into the client at build time — it can't be switched per request —
//! so the cheapest way to support all four combinations is to keep four
//! pre-built clients and dispatch based on `Backend.protocol` and
//! `Backend.scheme`. Idle pools are essentially free when unused.

use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::{TokioExecutor, TokioTimer};
use std::time::Duration;

/// Body type carried through the proxy pipeline.
///
/// Both hyper request bodies and axum request bodies are converted to this
/// boxed type before hitting the upstream client so all four client instances
/// share one concrete `Body` type. `UnsyncBoxBody` is used because axum's
/// `Body` is `Send` but not `Sync`, and `hyper-util`'s legacy `Client` only
/// requires `Send + 'static` on the body.
pub type ProxyBody = UnsyncBoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

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

/// Upstream HTTP client. Holds four `legacy::Client` instances — one per
/// `(scheme, protocol)` combination — and dispatches `forward()` to the right
/// client based on `Backend` config.
pub struct UpstreamClient {
    h1_plain: Client<HttpConnector, ProxyBody>,
    h1_tls: Client<HttpsConnector<HttpConnector>, ProxyBody>,
    h2_plain: Client<HttpConnector, ProxyBody>,
    h2_tls: Client<HttpsConnector<HttpConnector>, ProxyBody>,
}

impl UpstreamClient {
    /// # Panics
    /// Panics if the system's native root store cannot be loaded.
    #[must_use]
    pub fn new() -> Self {
        let idle = Duration::from_mins(1);
        let ka = Duration::from_secs(30);

        let plain_conn = HttpConnector::new();

        let h1_plain = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(idle)
            .build::<_, ProxyBody>(plain_conn.clone());
        let h2_plain = Client::builder(TokioExecutor::new())
            .http2_only(true)
            .timer(TokioTimer::new())
            .http2_keep_alive_interval(Some(ka))
            .pool_idle_timeout(idle)
            .build::<_, ProxyBody>(plain_conn);

        let tls_h1_conn = HttpsConnectorBuilder::new()
            .with_native_roots()
            .expect("system roots")
            .https_or_http()
            .enable_http1()
            .build();
        let tls_h2_conn = HttpsConnectorBuilder::new()
            .with_native_roots()
            .expect("system roots")
            .https_or_http()
            .enable_http2()
            .build();
        let h1_tls = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(idle)
            .build(tls_h1_conn);
        let h2_tls = Client::builder(TokioExecutor::new())
            .http2_only(true)
            .timer(TokioTimer::new())
            .http2_keep_alive_interval(Some(ka))
            .pool_idle_timeout(idle)
            .build(tls_h2_conn);

        Self {
            h1_plain,
            h1_tls,
            h2_plain,
            h2_tls,
        }
    }

    /// Forward `req` to `backend`, dispatching to the correct client based on
    /// `backend.protocol` and `backend.scheme`.
    ///
    /// # Errors
    /// Returns [`ProxyError::Request`] if the underlying hyper client fails
    /// (connect refused, protocol error, remote reset, etc.).
    pub async fn forward(
        &self,
        backend: &super::Backend,
        req: Request<ProxyBody>,
    ) -> Result<Response<Incoming>, ProxyError> {
        use super::{BackendProtocol, BackendScheme};
        match (backend.scheme, backend.protocol) {
            (BackendScheme::Http, BackendProtocol::Http1) => self.h1_plain.request(req).await,
            (BackendScheme::Https, BackendProtocol::Http1) => self.h1_tls.request(req).await,
            (BackendScheme::Http, BackendProtocol::Http2) => self.h2_plain.request(req).await,
            (BackendScheme::Https, BackendProtocol::Http2) => self.h2_tls.request(req).await,
        }
        .map_err(ProxyError::from)
    }

    /// Legacy shim used by Task 1.7 tests. Removed once tests migrate to `forward`.
    ///
    /// # Errors
    /// Returns [`ProxyError::Request`] if the underlying hyper client fails.
    #[cfg(test)]
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
