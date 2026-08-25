use axum::body::Body;
use http_body_util::BodyExt;
use hyper::body::{Body as HttpBody, Bytes};
use hyper::header::{HeaderMap, HeaderValue};
use hyper::Request;

use super::super::upstream::{ProxyBody, ProxyError};
use super::super::{Backend, BackendProtocol, BackendScheme};
use super::websocket::is_websocket_upgrade;
use rust_tunnel_stats::{EntityType, StatsCollector};

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
///
/// When `preserve_upgrade` is `true`, the `Upgrade` and `Connection` headers
/// are retained verbatim so a WebSocket (or other Upgrade-based protocol)
/// handshake can pass through end-to-end (RFC 6455 §1.7, RFC 7230 §6.7).
/// Other hop-by-hop tokens listed in `Connection` (e.g. `keep-alive`) are
/// still stripped as separate headers.
#[must_use]
pub fn strip_hop_by_hop(
    headers: &HeaderMap<HeaderValue>,
    preserve_upgrade: bool,
) -> HeaderMap<HeaderValue> {
    let mut extra: Vec<String> = Vec::new();
    for conn in headers.get_all(hyper::header::CONNECTION) {
        if let Ok(s) = conn.to_str() {
            for tok in s.split(',') {
                let t = tok.trim();
                if t.is_empty() {
                    continue;
                }
                // On a WS upgrade, the "upgrade" token inside Connection is
                // the marker that keeps the tunnel alive — don't treat it
                // as a hop name to strip.
                if preserve_upgrade && t.eq_ignore_ascii_case("upgrade") {
                    continue;
                }
                extra.push(t.to_ascii_lowercase());
            }
        }
    }

    let mut out = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let n = name.as_str();
        if STATIC_HOP_BY_HOP.contains(&n) {
            if preserve_upgrade && (n == "connection" || n == "upgrade") {
                // fall through and retain
            } else {
                continue;
            }
        }
        if extra.iter().any(|h| h == n) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Wrap a body so each data frame's length is recorded into the unified
/// stats collector (`is_in = true` counts bytes_in — client to backend;
/// otherwise bytes_out — backend to client). Recording is live: the periodic
/// snapshot flush picks the deltas up without waiting for the body to end.
pub(super) fn count_body<B>(
    body: B,
    stats: StatsCollector,
    rule_id: String,
    is_in: bool,
) -> impl HttpBody<Data = Bytes, Error = B::Error> + Send
where
    B: HttpBody<Data = Bytes> + Send,
{
    body.map_frame(move |frame| {
        if let Some(data) = frame.data_ref() {
            let n = data.len() as u64;
            if is_in {
                stats.record_bytes(EntityType::Proxy, &rule_id, n, 0);
            } else {
                stats.record_bytes(EntityType::Proxy, &rule_id, 0, n);
            }
        }
        frame
    })
}

/// Build the outgoing upstream Request from the incoming axum Request.
///
/// - Rewrites URI to `http://<backend.addr><path?query>`
/// - Preserves method, headers (after hop-by-hop stripping)
/// - Boxes the body without buffering — streams through
pub(super) fn build_upstream_request(
    req: Request<Body>,
    backend: &Backend,
) -> Result<Request<ProxyBody>, ProxyError> {
    let (mut parts, body) = req.into_parts();
    let pq = parts
        .uri
        .path_and_query()
        .map_or("/", hyper::http::uri::PathAndQuery::as_str);
    let scheme = match backend.scheme {
        BackendScheme::Http => "http",
        BackendScheme::Https => "https",
    };
    let uri: hyper::Uri = format!("{scheme}://{}{}", backend.addr, pq)
        .parse()
        .map_err(|e| ProxyError::BadBackendAddr(format!("{e}")))?;
    parts.uri = uri;
    let preserve_upgrade = is_websocket_upgrade(&parts.headers);
    parts.headers = strip_hop_by_hop(&parts.headers, preserve_upgrade);
    // Rewrite Host to the backend authority (standard reverse-proxy behavior,
    // cf. nginx `proxy_set_header Host $proxy_host`). Forwarding the public
    // domain verbatim makes Host-sensitive upstreams (ALLOWED_HOSTS checks,
    // dev servers, virtual-host routers) reject the request, often with 400.
    if let Ok(host) = HeaderValue::from_str(&backend.addr) {
        parts.headers.insert(hyper::header::HOST, host);
    }
    // On WebSocket upgrades, keep Origin consistent with the rewritten Host:
    // WS servers commonly reject handshakes whose Origin host differs from
    // the request Host (e.g. gorilla/websocket's default CheckOrigin).
    // Only rewrite when an Origin header is actually present.
    if preserve_upgrade && parts.headers.contains_key(hyper::header::ORIGIN) {
        if let Ok(origin) = HeaderValue::from_str(&format!("{scheme}://{}", backend.addr)) {
            parts.headers.insert(hyper::header::ORIGIN, origin);
        }
    }
    // Normalize the request version to match the outgoing client's protocol.
    // The incoming downstream version may be HTTP/2 (from the h2 listener) or
    // HTTP/1.1; the upstream client is protocol-locked at build time, so we
    // must align the request version with the target client, otherwise
    // hyper-util returns UserUnsupportedVersion.
    parts.version = match backend.protocol {
        BackendProtocol::Http1 => hyper::Version::HTTP_11,
        BackendProtocol::Http2 => hyper::Version::HTTP_2,
    };

    let boxed: ProxyBody = body
        .map_err(|e| Box::<dyn std::error::Error + Send + Sync>::from(e.to_string()))
        .boxed_unsync();
    Ok(Request::from_parts(parts, boxed))
}
