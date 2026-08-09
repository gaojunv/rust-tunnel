use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;

use super::super::upstream::ProxyError;
use super::upstream_request::{count_body, strip_hop_by_hop};
use crate::stats::StatsCollector;

/// Walk `err.source()` chain to build a string like:
///   "outer message | caused by: middle | caused by: inner"
///
/// Useful when the top-level error variant hides the real cause several
/// layers down (e.g. hyper-util's `SendRequest` wrapping `hyper::Error`).
pub(super) fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![err.to_string()];
    let mut cursor: Option<&(dyn std::error::Error + 'static)> = err.source();
    while let Some(e) = cursor {
        parts.push(format!("caused by: {e}"));
        cursor = e.source();
    }
    parts.join(" | ")
}

/// Convert the hyper response returned by `UpstreamClient` back to an axum
/// `Response<Body>`. Streams the body without buffering, counting response
/// bytes into the stats collector.
pub(super) fn build_downstream_response(
    resp: hyper::Response<hyper::body::Incoming>,
    stats: StatsCollector,
    rule_id: String,
) -> Response {
    let (mut parts, incoming) = resp.into_parts();
    // Preserve Upgrade/Connection on a 101 so the caller can complete the
    // WebSocket handshake — otherwise the browser sees the switch but no
    // negotiated upgrade tokens and treats it as a protocol error.
    let preserve_upgrade = parts.status == StatusCode::SWITCHING_PROTOCOLS;
    parts.headers = strip_hop_by_hop(&parts.headers, preserve_upgrade);
    let axum_body = Body::new(count_body(incoming, stats, rule_id, false));
    Response::from_parts(parts, axum_body)
}

pub(super) fn error_response(err: &ProxyError) -> Response {
    let (status, body) = match err {
        ProxyError::BadBackendAddr(_) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{err}")),
        ProxyError::Connect(_) | ProxyError::Request(_) | ProxyError::Body(_) => {
            (StatusCode::BAD_GATEWAY, format!("{err}"))
        }
    };
    Response::builder()
        .status(status)
        .body(Body::from(body))
        .unwrap()
}
