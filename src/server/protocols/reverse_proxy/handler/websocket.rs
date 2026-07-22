use hyper::header::{HeaderMap, HeaderValue};

/// Return true if the given headers announce a WebSocket-style upgrade
/// (i.e. carry `Upgrade: websocket`, case-insensitive).
///
/// Used both on the request side (to decide whether to preserve
/// hop-by-hop headers when forwarding) and on the response side (to
/// spot the 101 that must be forwarded verbatim).
#[must_use]
pub fn is_websocket_upgrade(headers: &HeaderMap<HeaderValue>) -> bool {
    headers
        .get(hyper::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.eq_ignore_ascii_case("websocket"))
}