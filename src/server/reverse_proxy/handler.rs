//! Unified HTTP proxy request handler.

use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::Request;
use tracing::error;

use super::router::RouteTable;
use super::upstream::{ProxyBody, ProxyError, UpstreamClient};
use super::Backend;

use hyper::header::{HeaderMap, HeaderValue};

/// Where the handler pulls its routing decision from.
///
/// The unified handler routes every request through a `RouteTable` snapshot.
/// Callers that only serve a single rule (legacy per-rule listener) build a
/// single-rule `RouteTable` and pass it in as `Shared`.
#[derive(Clone)]
pub struct RouteSource(pub Arc<ArcSwap<RouteTable>>);

/// State injected into the axum Router.
pub type ProxyState = (RouteSource, Arc<UpstreamClient>);

/// Per RFC 7230 §6.1, these headers apply to the immediate connection only
/// and must be stripped by any intermediary.
const STATIC_HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// Return a copy of `headers` with hop-by-hop entries removed.
///
/// Also honors `Connection: X-Custom-Hop` by removing every header name listed
/// in a `Connection` value.
#[must_use]
pub fn strip_hop_by_hop(headers: &HeaderMap<HeaderValue>) -> HeaderMap<HeaderValue> {
    let mut extra: Vec<String> = Vec::new();
    for conn in headers.get_all(hyper::header::CONNECTION) {
        if let Ok(s) = conn.to_str() {
            for tok in s.split(',') {
                let t = tok.trim();
                if !t.is_empty() {
                    extra.push(t.to_ascii_lowercase());
                }
            }
        }
    }

    let mut out = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let n = name.as_str();
        if STATIC_HOP_BY_HOP.contains(&n) {
            continue;
        }
        if extra.iter().any(|h| h == n) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Resolve the target backend for a request. Returns `None` when no route
/// matches (caller should reply 404).
///
/// Both listener types (legacy per-rule and shared) go through this — legacy
/// listeners just build a one-rule table. `RouteTable::match_http_request`
/// already honors `rule.enabled`, longest-prefix matching, and the route's
/// configured load balancing algorithm.
async fn resolve_backend(source: &RouteSource, host: &str, path: &str) -> Option<Backend> {
    let snap = source.0.load();
    snap.match_http_request(host, path)
        .await
        .map(|(_, _, backend)| backend.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::{HeaderMap, HeaderValue};

    fn hv(s: &str) -> HeaderValue {
        HeaderValue::from_str(s).unwrap()
    }

    #[test]
    fn strips_static_hop_by_hop() {
        let mut h = HeaderMap::new();
        h.insert("Connection", hv("keep-alive"));
        h.insert("Keep-Alive", hv("timeout=5"));
        h.insert("Transfer-Encoding", hv("chunked"));
        h.insert("Content-Type", hv("application/json"));
        let out = strip_hop_by_hop(&h);
        assert!(out.get("connection").is_none());
        assert!(out.get("keep-alive").is_none());
        assert!(out.get("transfer-encoding").is_none());
        assert_eq!(out.get("content-type"), Some(&hv("application/json")));
    }

    #[test]
    fn strips_dynamic_connection_hops() {
        let mut h = HeaderMap::new();
        h.insert("Connection", hv("upgrade, X-Custom-Hop"));
        h.insert("X-Custom-Hop", hv("v"));
        h.insert("X-Keep", hv("ok"));
        let out = strip_hop_by_hop(&h);
        assert!(out.get("x-custom-hop").is_none());
        assert!(out.get("connection").is_none());
        assert!(out.get("upgrade").is_none());
        assert_eq!(out.get("x-keep"), Some(&hv("ok")));
    }

    #[test]
    fn te_is_hop_by_hop() {
        let mut h = HeaderMap::new();
        h.insert("TE", hv("trailers"));
        let out = strip_hop_by_hop(&h);
        assert!(out.get("te").is_none());
    }
}
