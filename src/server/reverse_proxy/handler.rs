//! Unified HTTP proxy request handler.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use http_body_util::BodyExt;
use hyper::Request;
use tracing::error;

use super::router::RouteTable;
use super::upstream::{ProxyBody, ProxyError, UpstreamClient};
use super::{
    record_traffic, Backend, BackendKind, BackendProtocol, ReverseProxyState, TrafficPending,
};
use crate::server::stats::{EntityType, StatsCollector};

use hyper::body::{Body as HttpBody, Bytes};
use hyper::header::{HeaderMap, HeaderValue};

/// Per-rule connection counter shared with `ReverseProxyState`.
pub type ConnectionCounts = Arc<tokio::sync::Mutex<HashMap<String, u64>>>;

/// Where the handler pulls its routing decision from.
///
/// The unified handler routes every request through a `RouteTable` snapshot.
/// Callers that only serve a single rule (legacy per-rule listener) build a
/// single-rule `RouteTable` and pass it in as `Shared`.
#[derive(Clone)]
pub struct RouteSource(pub Arc<ArcSwap<RouteTable>>);

/// State injected into the axum Router.
///
/// The third element is `Arc<ReverseProxyState>` which provides access to
/// `connection_counts`, `traffic_pending`, and the `ClientConnector` for
/// client-kind backends.
pub type ProxyState = (RouteSource, Arc<UpstreamClient>, Arc<ReverseProxyState>);

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

/// Walk `err.source()` chain to build a string like:
///   "outer message | caused by: middle | caused by: inner"
///
/// Useful when the top-level error variant hides the real cause several
/// layers down (e.g. hyper-util's `SendRequest` wrapping `hyper::Error`).
fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![err.to_string()];
    let mut cursor: Option<&(dyn std::error::Error + 'static)> = err.source();
    while let Some(e) = cursor {
        parts.push(format!("caused by: {e}"));
        cursor = e.source();
    }
    parts.join(" | ")
}

/// Resolve the target backend for a request. Returns the matched rule id and
/// the selected backend, or `None` when no route matches (caller should reply
/// 404).
///
/// Both listener types (legacy per-rule and shared) go through this — legacy
/// listeners just build a one-rule table. `RouteTable::match_http_request`
/// already honors `rule.enabled`, longest-prefix matching, and the route's
/// configured load balancing algorithm.
async fn resolve_backend(
    source: &RouteSource,
    host: &str,
    path: &str,
) -> Option<(String, Backend)> {
    let snap = source.0.load();
    snap.match_http_request(host, path)
        .await
        .map(|(rule, _, backend)| (rule.id.clone(), backend.clone()))
}

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

/// Wrap a body so each data frame's length is accumulated into the matched
/// rule's pending traffic (`is_in = true` counts bytes_in — client to
/// backend; otherwise bytes_out — backend to client). Recording is live: the
/// periodic flush task picks the deltas up without waiting for the body to end.
///
/// 同一份字节同时写入统一统计采集器（StatsCollector，proxy 桶）。
fn count_body<B>(
    body: B,
    pending: TrafficPending,
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
                record_traffic(&pending, &rule_id, n, 0, 0);
                stats.record_bytes(EntityType::Proxy, &rule_id, n, 0);
            } else {
                record_traffic(&pending, &rule_id, 0, n, 0);
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
fn build_upstream_request(
    req: Request<Body>,
    backend: &Backend,
) -> Result<Request<ProxyBody>, ProxyError> {
    let (mut parts, body) = req.into_parts();
    let pq = parts
        .uri
        .path_and_query()
        .map(hyper::http::uri::PathAndQuery::as_str)
        .unwrap_or("/");
    use super::{BackendProtocol, BackendScheme};
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

/// Convert the hyper response returned by `UpstreamClient` back to an axum
/// `Response<Body>`. Streams the body without buffering, counting response
/// bytes into the rule's traffic accumulator.
fn build_downstream_response(
    resp: hyper::Response<hyper::body::Incoming>,
    pending: TrafficPending,
    stats: StatsCollector,
    rule_id: String,
) -> Response {
    let (mut parts, incoming) = resp.into_parts();
    // Preserve Upgrade/Connection on a 101 so the caller can complete the
    // WebSocket handshake — otherwise the browser sees the switch but no
    // negotiated upgrade tokens and treats it as a protocol error.
    let preserve_upgrade = parts.status == StatusCode::SWITCHING_PROTOCOLS;
    parts.headers = strip_hop_by_hop(&parts.headers, preserve_upgrade);
    let axum_body = Body::new(count_body(incoming, pending, stats, rule_id, false));
    Response::from_parts(parts, axum_body)
}

fn error_response(err: &ProxyError) -> Response {
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

/// Strip the port (if any) from a Host header value.
///
/// Handles bracketed IPv6 literals per RFC 7230 §5.4 (`[::1]:8080` → `::1`).
fn host_without_port(raw: &str) -> &str {
    if let Some(rest) = raw.strip_prefix('[') {
        // Bracketed IPv6 literal; the closing ']' terminates the host.
        rest.split(']').next().unwrap_or(rest)
    } else {
        // Plain hostname or IPv4; first ':' terminates the host.
        raw.split(':').next().unwrap_or(raw)
    }
}

/// Unified axum handler that replaces both the legacy per-rule handler and
/// the shared-listener handler.
pub async fn handle_proxy_request_unified(
    State((source, upstream, proxy_state)): State<ProxyState>,
    mut req: Request<Body>,
) -> Response {
    let connection_counts = proxy_state.connection_counts.clone();
    let traffic_pending = proxy_state.traffic_pending.clone();
    let stats = proxy_state.stats_collector.clone();
    // h2 requests carry the authority in `:authority` (surfaced as Uri::host());
    // h1 requests may put it in either the URI (absolute-form) or a Host header.
    let host = req
        .uri()
        .host()
        .map(str::to_string)
        .or_else(|| {
            req.headers()
                .get("host")
                .and_then(|h| h.to_str().ok())
                .map(|s| host_without_port(s).to_string())
        })
        .unwrap_or_default();
    let path = req.uri().path().to_string();

    // Diagnostics captured up-front so we can include them in error logs
    // regardless of where the flow fails.
    let method = req.method().clone();
    let incoming_version = req.version();
    let incoming_header_count = req.headers().len();

    let Some((rule_id, backend)) = resolve_backend(&source, &host, &path).await else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from(format!("No route for host '{host}'")))
            .unwrap();
    };

    // Track connection for stats
    {
        let mut counts = connection_counts.lock().await;
        *counts.entry(rule_id.clone()).or_insert(0) += 1;
    }
    // Count this request as one connection in the traffic accumulator
    record_traffic(&traffic_pending, &rule_id, 0, 0, 1);
    // 统一统计：proxy 桶活跃连接 +1
    stats.incr_conns(EntityType::Proxy, &rule_id);

    // Decrement on scope exit (deferred via clone)
    let rule_id_for_decrement = rule_id.clone();
    let counts_for_decrement = connection_counts.clone();

    // Client backend fork: bypass the shared UpstreamClient and dial
    // through the ClientConnector tunnel.
    if backend.kind == BackendKind::Client {
        // HTTP/2 not supported for client backends
        if backend.protocol != BackendProtocol::Http1 {
            let mut counts = counts_for_decrement.lock().await;
            if let Some(c) = counts.get_mut(&rule_id_for_decrement) {
                if *c > 0 {
                    *c -= 1;
                }
            }
            stats.decr_conns(EntityType::Proxy, &rule_id_for_decrement);
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("HTTP/2 to client backend not yet supported"))
                .unwrap();
        }
        return handle_client_backend(
            proxy_state,
            req,
            backend,
            rule_id_for_decrement,
            traffic_pending.clone(),
        )
        .await;
    }

    // WebSocket upgrade path: capture the downstream OnUpgrade future BEFORE
    // `req` is consumed by `build_upstream_request` — hyper's upgrade handle
    // is one-shot per request and must be taken from the original Request.
    let downstream_upgrade = if is_websocket_upgrade(req.headers()) {
        Some(hyper::upgrade::on(&mut req))
    } else {
        None
    };

    // Wrap the request body to count bytes sent to the backend (bytes_in)
    let req = {
        let (parts, body) = req.into_parts();
        let counted = Body::new(count_body(
            body,
            traffic_pending.clone(),
            stats.clone(),
            rule_id.clone(),
            true,
        ));
        Request::from_parts(parts, counted)
    };

    let upstream_req = match build_upstream_request(req, &backend) {
        Ok(r) => r,
        Err(e) => {
            // Decrement on build error
            let mut counts = counts_for_decrement.lock().await;
            if let Some(c) = counts.get_mut(&rule_id_for_decrement) {
                if *c > 0 {
                    *c -= 1;
                }
            }
            stats.decr_conns(EntityType::Proxy, &rule_id_for_decrement);
            return error_response(&e);
        }
    };

    let ws_potential = downstream_upgrade.is_some();

    match upstream.forward(&backend, upstream_req).await {
        Ok(mut resp) => {
            let is_ws = ws_potential && resp.status() == StatusCode::SWITCHING_PROTOCOLS;
            if let Some(client_upgrade) = downstream_upgrade {
                if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
                    // Take the upstream upgrade handle now, before `resp` is
                    // consumed by `build_downstream_response` — same one-shot
                    // constraint as the downstream side.
                    let upstream_upgrade = hyper::upgrade::on(&mut resp);
                    let rid = rule_id_for_decrement.clone();
                    let cc = counts_for_decrement.clone();
                    let tp = traffic_pending.clone();
                    let sc = stats.clone();
                    tokio::spawn(async move {
                        let (client_up, server_up) =
                            match tokio::try_join!(client_upgrade, upstream_upgrade) {
                                Ok(pair) => pair,
                                Err(e) => {
                                    tracing::debug!("ws upgrade join failed: {e}");
                                    // Decrement on WS join failure
                                    let mut counts = cc.lock().await;
                                    if let Some(c) = counts.get_mut(&rid) {
                                        if *c > 0 {
                                            *c -= 1;
                                        }
                                    }
                                    sc.decr_conns(EntityType::Proxy, &rid);
                                    return;
                                }
                            };
                        // hyper 1.x's `Upgraded` implements hyper's own
                        // Read/Write traits; wrap with TokioIo so it satisfies
                        // tokio's AsyncRead + AsyncWrite for copy_bidirectional.
                        let mut client_io = hyper_util::rt::TokioIo::new(client_up);
                        let mut server_io = hyper_util::rt::TokioIo::new(server_up);
                        match tokio::io::copy_bidirectional(&mut client_io, &mut server_io).await {
                            // (client→backend, backend→client) = (bytes_in, bytes_out)
                            Ok((to_backend, to_client)) => {
                                record_traffic(&tp, &rid, to_backend, to_client, 0);
                                sc.record_bytes(EntityType::Proxy, &rid, to_backend, to_client);
                            }
                            Err(e) => {
                                tracing::debug!("ws bidirectional copy ended: {e}");
                            }
                        }
                        // Decrement on WS tunnel close
                        let mut counts = cc.lock().await;
                        if let Some(c) = counts.get_mut(&rid) {
                            if *c > 0 {
                                *c -= 1;
                            }
                        }
                        sc.decr_conns(EntityType::Proxy, &rid);
                    });
                }
            }
            if !is_ws {
                // Decrement for regular (non-WS) responses
                let mut counts = counts_for_decrement.lock().await;
                if let Some(c) = counts.get_mut(&rule_id_for_decrement) {
                    if *c > 0 {
                        *c -= 1;
                    }
                }
                stats.decr_conns(EntityType::Proxy, &rule_id_for_decrement);
            }
            build_downstream_response(
                resp,
                traffic_pending.clone(),
                stats.clone(),
                rule_id.clone(),
            )
        }
        Err(e) => {
            // Decrement on upstream error
            let mut counts = counts_for_decrement.lock().await;
            if let Some(c) = counts.get_mut(&rule_id_for_decrement) {
                if *c > 0 {
                    *c -= 1;
                }
            }
            stats.decr_conns(EntityType::Proxy, &rule_id_for_decrement);
            error!(
                error = %error_chain(&e),
                backend = %backend.addr,
                scheme = ?backend.scheme,
                protocol = ?backend.protocol,
                method = %method,
                path = %path,
                incoming_version = ?incoming_version,
                header_count = incoming_header_count,
                "upstream request failed"
            );
            error_response(&e)
        }
    }
}

/// Client backend handler: dials through the ClientConnector tunnel,
/// performs an HTTP/1.1 handshake, sends the rewritten request, and
/// returns the response.
///
/// This path is used when `backend.kind == Client`. It bypasses the
/// shared `UpstreamClient` and opens a fresh `ClientTunnelStream` for
/// each request (no connection pooling in this first version).
///
/// WebSocket upgrades are supported: on a 101 the upgraded downstream and
/// upstream IOs are bridged with `copy_bidirectional`, raw bytes flowing
/// through the control-channel tunnel.
async fn handle_client_backend(
    state: Arc<ReverseProxyState>,
    req: Request<Body>,
    backend: Backend,
    rule_id: String,
    traffic_pending: TrafficPending,
) -> Response {
    use hyper::client::conn::http1;
    use hyper_util::rt::TokioIo;

    // 统一统计采集器（与 ReverseProxyState 共享）
    let stats = state.stats_collector.clone();

    // Capture the downstream OnUpgrade future BEFORE `req` is consumed —
    // hyper's upgrade handle is one-shot per request. The handle is stored
    // in the request extensions, so it survives `into_parts` below.
    let mut req = req;
    let downstream_upgrade = if is_websocket_upgrade(req.headers()) {
        Some(hyper::upgrade::on(&mut req))
    } else {
        None
    };

    // Wrap request body to count bytes_in
    let (parts, body) = req.into_parts();
    let counted = Body::new(count_body(
        body,
        traffic_pending.clone(),
        stats.clone(),
        rule_id.clone(),
        true,
    ));
    let req = Request::from_parts(parts, counted);

    // Dial via ClientConnector
    let connector = match state.connector_for(&backend).await {
        Ok(c) => c,
        Err(e) => {
            state.decrement_connections(&rule_id).await;
            stats.decr_conns(EntityType::Proxy, &rule_id);
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!("connector unavailable: {e}")))
                .unwrap();
        }
    };
    let stream = match connector.connect(&backend).await {
        Ok(s) => s,
        Err(e) => {
            state.decrement_connections(&rule_id).await;
            stats.decr_conns(EntityType::Proxy, &rule_id);
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!("client backend dial failed: {e}")))
                .unwrap();
        }
    };
    let io = TokioIo::new(stream);

    // HTTP/1.1 handshake
    let (mut sender, conn) = match http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .handshake(io)
        .await
    {
        Ok(pair) => pair,
        Err(e) => {
            state.decrement_connections(&rule_id).await;
            stats.decr_conns(EntityType::Proxy, &rule_id);
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!("http1 handshake failed: {e}")))
                .unwrap();
        }
    };
    let ws_potential = downstream_upgrade.is_some();
    if ws_potential {
        // `with_upgrades` keeps the Upgraded IO alive after the 101 so the
        // raw byte stream can be bridged below.
        tokio::spawn(async move {
            if let Err(e) = conn.with_upgrades().await {
                tracing::debug!("client backend conn ended: {e}");
            }
        });
    } else {
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::debug!("client backend conn ended: {e}");
            }
        });
    }

    // Rewrite the request URI to relative form for the tunneled backend
    let mut upstream_req = match build_upstream_request(req, &backend) {
        Ok(r) => r,
        Err(e) => {
            state.decrement_connections(&rule_id).await;
            stats.decr_conns(EntityType::Proxy, &rule_id);
            return error_response(&e);
        }
    };

    // For direct http1::handshake (not through a proxy), hyper's Client expects
    // the URI in origin-form (e.g. "/path?query"), not absolute-form. Rewrite.
    {
        let pq = upstream_req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/");
        *upstream_req.uri_mut() = pq.parse().unwrap_or_else(|_| upstream_req.uri().clone());
    }

    match sender.send_request(upstream_req).await {
        Ok(mut resp) => {
            let is_ws = ws_potential && resp.status() == StatusCode::SWITCHING_PROTOCOLS;
            if is_ws {
                // Take the upstream upgrade handle before `resp` is consumed
                // by `build_downstream_response` — same one-shot constraint
                // as the downstream side.
                let upstream_upgrade = hyper::upgrade::on(&mut resp);
                let client_upgrade = downstream_upgrade.expect("checked above");
                let rid = rule_id.clone();
                let cc = state.connection_counts.clone();
                let tp = traffic_pending.clone();
                let sc = stats.clone();
                tokio::spawn(async move {
                    let (client_up, server_up) =
                        match tokio::try_join!(client_upgrade, upstream_upgrade) {
                            Ok(pair) => pair,
                            Err(e) => {
                                tracing::debug!("ws upgrade join failed: {e}");
                                let mut counts = cc.lock().await;
                                if let Some(c) = counts.get_mut(&rid) {
                                    if *c > 0 {
                                        *c -= 1;
                                    }
                                }
                                sc.decr_conns(EntityType::Proxy, &rid);
                                return;
                            }
                        };
                    // hyper 1.x's `Upgraded` implements hyper's own Read/Write
                    // traits; wrap with TokioIo so it satisfies tokio's
                    // AsyncRead + AsyncWrite for copy_bidirectional.
                    let mut client_io = TokioIo::new(client_up);
                    let mut server_io = TokioIo::new(server_up);
                    match tokio::io::copy_bidirectional(&mut client_io, &mut server_io).await {
                        // (client→backend, backend→client) = (bytes_in, bytes_out)
                        Ok((to_backend, to_client)) => {
                            record_traffic(&tp, &rid, to_backend, to_client, 0);
                            sc.record_bytes(EntityType::Proxy, &rid, to_backend, to_client);
                        }
                        Err(e) => {
                            tracing::debug!("ws bidirectional copy ended: {e}");
                        }
                    }
                    let mut counts = cc.lock().await;
                    if let Some(c) = counts.get_mut(&rid) {
                        if *c > 0 {
                            *c -= 1;
                        }
                    }
                    sc.decr_conns(EntityType::Proxy, &rid);
                });
            }
            if !is_ws {
                state.decrement_connections(&rule_id).await;
                stats.decr_conns(EntityType::Proxy, &rule_id);
            }
            build_downstream_response(resp, traffic_pending, stats.clone(), rule_id)
        }
        Err(e) => {
            state.decrement_connections(&rule_id).await;
            stats.decr_conns(EntityType::Proxy, &rule_id);
            Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!("upstream request failed: {e}")))
                .unwrap()
        }
    }
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
        let out = strip_hop_by_hop(&h, false);
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
        let out = strip_hop_by_hop(&h, false);
        assert!(out.get("x-custom-hop").is_none());
        assert!(out.get("connection").is_none());
        assert!(out.get("upgrade").is_none());
        assert_eq!(out.get("x-keep"), Some(&hv("ok")));
    }

    #[test]
    fn te_is_hop_by_hop() {
        let mut h = HeaderMap::new();
        h.insert("TE", hv("trailers"));
        let out = strip_hop_by_hop(&h, false);
        assert!(out.get("te").is_none());
    }

    /// With `preserve_upgrade=true`, `Connection` and `Upgrade` are kept
    /// verbatim while other hop-by-hop tokens listed in `Connection` still
    /// get their headers stripped (e.g. `Keep-Alive`).
    #[test]
    fn preserves_upgrade_and_connection_on_ws_upgrade() {
        let mut h = HeaderMap::new();
        h.insert("Connection", hv("keep-alive, Upgrade"));
        h.insert("Upgrade", hv("websocket"));
        h.insert("Keep-Alive", hv("timeout=5"));
        h.insert("Sec-WebSocket-Key", hv("dGhlIHNhbXBsZSBub25jZQ=="));
        let out = strip_hop_by_hop(&h, true);
        assert_eq!(out.get("connection"), Some(&hv("keep-alive, Upgrade")));
        assert_eq!(out.get("upgrade"), Some(&hv("websocket")));
        assert!(
            out.get("keep-alive").is_none(),
            "keep-alive is still hop-by-hop even during a WS upgrade"
        );
        assert_eq!(
            out.get("sec-websocket-key"),
            Some(&hv("dGhlIHNhbXBsZSBub25jZQ=="))
        );
    }

    #[test]
    fn host_without_port_strips_ipv4_port() {
        assert_eq!(host_without_port("example.com:8080"), "example.com");
        assert_eq!(host_without_port("example.com"), "example.com");
        assert_eq!(host_without_port("10.0.0.1:80"), "10.0.0.1");
    }

    #[test]
    fn host_without_port_handles_ipv6_literal() {
        assert_eq!(host_without_port("[::1]:8080"), "::1");
        assert_eq!(host_without_port("[fe80::1]"), "fe80::1");
        assert_eq!(host_without_port("[::1]"), "::1");
    }

    #[tokio::test]
    async fn stream_response_does_not_buffer() {
        use std::net::SocketAddr;
        use std::sync::Arc;
        use std::time::Duration;

        use arc_swap::ArcSwap;
        use axum::body::Body;
        use axum::routing::get;
        use axum::Router;
        use futures_util::stream;
        use hyper::body::Bytes;
        use tokio::net::TcpListener;
        use tokio::time::Instant;

        use crate::server::reverse_proxy::router::RouteTable;
        use crate::server::reverse_proxy::upstream::UpstreamClient;
        use crate::server::reverse_proxy::{
            Backend, BackendKind, BackendProtocol, BackendScheme, LoadBalancing, ProxyRule, Route,
            RuleType,
        };

        // 1. Backend that trickles 8 chunks of 1 KiB with 50 ms between them.
        let backend_app = Router::new().route(
            "/",
            get(|| async {
                let s = stream::unfold(0u32, |i| async move {
                    if i >= 8 {
                        return None;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let chunk: Result<Bytes, std::io::Error> = Ok(Bytes::from(vec![b'x'; 1024]));
                    Some((chunk, i + 1))
                });
                Body::from_stream(s)
            }),
        );
        let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr: SocketAddr = backend_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(backend_listener, backend_app).await.unwrap();
        });

        // 2. Proxy: single-rule RouteTable pointing at the backend.
        let rule = ProxyRule {
            id: "r1".into(),
            name: "r1".into(),
            rule_type: RuleType::Http,
            listen: "127.0.0.1:0".into(),
            domains: vec!["test.local".into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Direct,
                    addr: backend_addr.to_string(),
                    client_name: None,
                    weight: 100,
                    protocol: BackendProtocol::Http1,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::default(),
            }],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        let table = RouteTable::from_rules(vec![rule]);
        let source = RouteSource(Arc::new(ArcSwap::from_pointee(table)));
        let upstream = Arc::new(UpstreamClient::new());
        let proxy_state = Arc::new(ReverseProxyState::new());

        // 3. Build the axum Router with the unified handler and bind it.
        let app = Router::new()
            .fallback(axum::routing::any(handle_proxy_request_unified))
            .with_state((source, upstream, proxy_state));
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr: SocketAddr = proxy_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(proxy_listener, app).await.unwrap();
        });

        // 4. Send a request via reqwest with Host: test.local.
        let client = reqwest::Client::builder().build().unwrap();
        let started = Instant::now();
        let resp = client
            .get(format!("http://{proxy_addr}/"))
            .header("host", "test.local")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // The first chunk should arrive well before the full 8*50ms = 400 ms
        // it would take if the body were buffered.
        use futures_util::StreamExt;
        let mut body_stream = resp.bytes_stream();
        let first = body_stream.next().await.unwrap().unwrap();
        let ttfb = started.elapsed();
        assert!(!first.is_empty());
        assert!(
            ttfb < Duration::from_millis(300),
            "first byte took {ttfb:?} — body appears to have been buffered"
        );
    }

    #[tokio::test]
    async fn hop_by_hop_headers_are_stripped_on_upstream() {
        use std::net::SocketAddr;
        use std::sync::Arc;

        use arc_swap::ArcSwap;
        use axum::extract::Request as AxumRequest;
        use axum::routing::any;
        use axum::Router;
        use tokio::net::TcpListener;
        use tokio::sync::mpsc;

        use crate::server::reverse_proxy::router::RouteTable;
        use crate::server::reverse_proxy::upstream::UpstreamClient;
        use crate::server::reverse_proxy::{
            Backend, BackendKind, BackendProtocol, BackendScheme, LoadBalancing, ProxyRule, Route,
            RuleType,
        };

        let (tx, mut rx) = mpsc::unbounded_channel::<hyper::HeaderMap>();
        let tx_clone = tx.clone();
        let backend_app = Router::new().route(
            "/",
            any(move |req: AxumRequest| {
                let tx = tx_clone.clone();
                async move {
                    let _ = tx.send(req.headers().clone());
                    "ok"
                }
            }),
        );
        let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr: SocketAddr = backend_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(backend_listener, backend_app).await.unwrap();
        });

        let rule = ProxyRule {
            id: "r_hop".into(),
            name: "r_hop".into(),
            rule_type: RuleType::Http,
            listen: "127.0.0.1:0".into(),
            domains: vec!["test.local".into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Direct,
                    addr: backend_addr.to_string(),
                    client_name: None,
                    weight: 100,
                    protocol: BackendProtocol::Http1,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::default(),
            }],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        let table = RouteTable::from_rules(vec![rule]);
        let source = RouteSource(Arc::new(ArcSwap::from_pointee(table)));
        let upstream = Arc::new(UpstreamClient::new());
        let proxy_state = Arc::new(ReverseProxyState::new());

        let app = Router::new()
            .fallback(any(handle_proxy_request_unified))
            .with_state((source, upstream, proxy_state));
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr: SocketAddr = proxy_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(proxy_listener, app).await.unwrap();
        });

        let client = reqwest::Client::builder().build().unwrap();
        let resp = client
            .get(format!("http://{proxy_addr}/"))
            .header("host", "test.local")
            .header("connection", "keep-alive, x-custom-hop")
            .header("keep-alive", "timeout=5")
            .header("x-custom-hop", "should-be-stripped")
            .header("x-keep", "keep-me")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let received = rx.recv().await.unwrap();
        assert!(
            received.get("connection").is_none(),
            "connection should be stripped"
        );
        assert!(
            received.get("keep-alive").is_none(),
            "keep-alive should be stripped"
        );
        assert!(
            received.get("x-custom-hop").is_none(),
            "x-custom-hop from Connection should be stripped"
        );
        assert_eq!(received.get("x-keep").unwrap(), "keep-me");
    }

    /// Regression: the proxy must rewrite the Host header to the backend
    /// address. Forwarding the public domain verbatim makes Host-sensitive
    /// upstreams (Django ALLOWED_HOSTS, webpack-dev-server, virtual-host
    /// routers) reject the request — typically with a 400.
    #[tokio::test]
    async fn host_header_is_rewritten_to_backend_addr() {
        use std::net::SocketAddr;
        use std::sync::Arc;

        use arc_swap::ArcSwap;
        use axum::extract::Request as AxumRequest;
        use axum::routing::any;
        use axum::Router;
        use tokio::net::TcpListener;
        use tokio::sync::mpsc;

        use crate::server::reverse_proxy::router::RouteTable;
        use crate::server::reverse_proxy::upstream::UpstreamClient;
        use crate::server::reverse_proxy::{
            Backend, BackendKind, BackendProtocol, BackendScheme, LoadBalancing, ProxyRule, Route,
            RuleType,
        };

        let (tx, mut rx) = mpsc::unbounded_channel::<Option<String>>();
        let tx_clone = tx.clone();
        let backend_app = Router::new().route(
            "/",
            any(move |req: AxumRequest| {
                let tx = tx_clone.clone();
                async move {
                    let host = req
                        .headers()
                        .get("host")
                        .and_then(|h| h.to_str().ok())
                        .map(str::to_string);
                    let _ = tx.send(host);
                    "ok"
                }
            }),
        );
        let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr: SocketAddr = backend_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(backend_listener, backend_app).await.unwrap();
        });

        let rule = ProxyRule {
            id: "r_host".into(),
            name: "r_host".into(),
            rule_type: RuleType::Http,
            listen: "127.0.0.1:0".into(),
            domains: vec!["test.local".into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Direct,
                    addr: backend_addr.to_string(),
                    client_name: None,
                    weight: 100,
                    protocol: BackendProtocol::Http1,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::default(),
            }],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        let table = RouteTable::from_rules(vec![rule]);
        let source = RouteSource(Arc::new(ArcSwap::from_pointee(table)));
        let upstream = Arc::new(UpstreamClient::new());
        let proxy_state = Arc::new(ReverseProxyState::new());

        let app = Router::new()
            .fallback(any(handle_proxy_request_unified))
            .with_state((source, upstream, proxy_state));
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr: SocketAddr = proxy_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(proxy_listener, app).await.unwrap();
        });

        let client = reqwest::Client::builder().build().unwrap();
        let resp = client
            .get(format!("http://{proxy_addr}/"))
            .header("host", "test.local")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let received = rx.recv().await.unwrap();
        assert_eq!(
            received.as_deref(),
            Some(backend_addr.to_string().as_str()),
            "backend must see Host rewritten to its own addr, not the public domain"
        );
    }

    /// Regression: a real HTTP/2 request has host in `:authority` (surfaced via
    /// `Uri::host()`), NOT in a `Host` header. If we only look at the `host`
    /// header, browsers over h2 get "No route for host ''".
    #[tokio::test]
    async fn handler_reads_host_from_uri_authority_when_no_host_header() {
        use std::net::SocketAddr;
        use std::sync::Arc;

        use arc_swap::ArcSwap;
        use axum::body::Body;
        use axum::extract::State;
        use axum::http::Request as AxumRequest;

        use crate::server::reverse_proxy::router::RouteTable;
        use crate::server::reverse_proxy::upstream::UpstreamClient;
        use crate::server::reverse_proxy::{
            Backend, BackendKind, BackendProtocol, BackendScheme, LoadBalancing, ProxyRule, Route,
            RuleType,
        };

        // Backend that returns 200 with a known body.
        let backend_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr: SocketAddr = backend_listener.local_addr().unwrap();
        let backend_app = axum::Router::new().route("/", axum::routing::any(|| async { "ok" }));
        tokio::spawn(async move {
            axum::serve(backend_listener, backend_app).await.unwrap();
        });

        let rule = ProxyRule {
            id: "r".into(),
            name: "r".into(),
            rule_type: RuleType::Http,
            listen: "127.0.0.1:0".into(),
            domains: vec!["test.local".into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Direct,
                    addr: backend_addr.to_string(),
                    client_name: None,
                    weight: 100,
                    protocol: BackendProtocol::Http1,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::default(),
            }],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        let table = RouteTable::from_rules(vec![rule]);
        let source = RouteSource(Arc::new(ArcSwap::from_pointee(table)));
        let upstream = Arc::new(UpstreamClient::new());
        let proxy_state = Arc::new(ReverseProxyState::new());

        // Build a request the way hyper delivers an h2 request:
        // - authority set on the URI (from :authority pseudo-header)
        // - NO Host header
        // - Version HTTP_2
        let req: AxumRequest<Body> = AxumRequest::builder()
            .method("GET")
            .uri("http://test.local/")
            .version(hyper::Version::HTTP_2)
            .body(Body::empty())
            .unwrap();

        let state = State((source, upstream, proxy_state));
        let resp = handle_proxy_request_unified(state, req).await;

        assert_eq!(
            resp.status(),
            200,
            "handler must resolve host from URI authority when Host header is absent"
        );
    }

    /// End-to-end: client → proxy → backend WebSocket upgrade + bidirectional
    /// byte echo through the tunnel. Uses raw TCP + a hand-written HTTP/1.1
    /// upgrade handshake so the test doesn't pull in tokio-tungstenite.
    #[tokio::test]
    async fn websocket_upgrade_proxies_end_to_end() {
        use std::collections::HashSet;
        use std::net::SocketAddr;

        use base64::Engine;
        use sha1::{Digest, Sha1};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        use crate::server::acme::CertificateManager;
        use crate::server::reverse_proxy::router::RouteTable;
        use crate::server::reverse_proxy::shared_listener::SharedListener;
        use crate::server::reverse_proxy::{
            Backend, BackendKind, BackendProtocol, BackendScheme, LoadBalancing, ProxyRule, Route,
            RuleType,
        };

        const WS_MAGIC: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

        // 1. Backend: raw TCP. Accept a WS upgrade, then echo bytes.
        let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr: SocketAddr = backend_listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = backend_listener.accept().await.unwrap();
                tokio::spawn(async move {
                    // Read request until we see \r\n\r\n
                    let mut buf = Vec::with_capacity(4096);
                    let mut chunk = [0u8; 1024];
                    loop {
                        let n = sock.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&buf);
                    // Extract Sec-WebSocket-Key (case-insensitive header name).
                    let key_line = text
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("sec-websocket-key:"))
                        .expect("no ws key");
                    let key = key_line.split_once(':').unwrap().1.trim();
                    let mut h = Sha1::new();
                    h.update(key.as_bytes());
                    h.update(WS_MAGIC.as_bytes());
                    let accept = base64::engine::general_purpose::STANDARD.encode(h.finalize());
                    let resp = format!(
                        "HTTP/1.1 101 Switching Protocols\r\n\
                         Upgrade: websocket\r\n\
                         Connection: Upgrade\r\n\
                         Sec-WebSocket-Accept: {accept}\r\n\r\n"
                    );
                    sock.write_all(resp.as_bytes()).await.unwrap();
                    // Byte-for-byte echo.
                    let mut echo = [0u8; 4096];
                    loop {
                        let n = match sock.read(&mut echo).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        if sock.write_all(&echo[..n]).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });

        // 2. Build a single-rule shared listener pointing at the backend.
        let temp_dir = tempfile::TempDir::new().unwrap();
        let mgr = Arc::new(CertificateManager::new(temp_dir.path().to_str().unwrap()));

        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr: SocketAddr = probe.local_addr().unwrap();
        drop(probe);

        let rule = ProxyRule {
            id: "r_ws".into(),
            name: "r_ws".into(),
            rule_type: RuleType::Http,
            listen: listen_addr.to_string(),
            domains: vec!["test.local".into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Direct,
                    addr: backend_addr.to_string(),
                    client_name: None,
                    weight: 100,
                    protocol: BackendProtocol::Http1,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::default(),
            }],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        let table = RouteTable::from_rules(vec![rule]);
        let proxy_state = Arc::new(ReverseProxyState::new());
        let _listener = SharedListener::spawn(
            listen_addr.to_string(),
            false,
            table,
            Some(mgr),
            HashSet::from(["r_ws".to_string()]),
            proxy_state,
        )
        .await
        .expect("shared listener spawn");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // 3. Send HTTP/1.1 WS upgrade through the proxy.
        let mut client = TcpStream::connect(listen_addr).await.unwrap();
        let req = "GET / HTTP/1.1\r\n\
                   Host: test.local\r\n\
                   Upgrade: websocket\r\n\
                   Connection: Upgrade\r\n\
                   Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                   Sec-WebSocket-Version: 13\r\n\r\n";
        client.write_all(req.as_bytes()).await.unwrap();

        // 4. Read response headers up to \r\n\r\n.
        let mut resp_buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 1024];
        let header_end = loop {
            let n =
                tokio::time::timeout(std::time::Duration::from_secs(5), client.read(&mut chunk))
                    .await
                    .expect("timed out waiting for 101 headers")
                    .unwrap();
            assert!(n > 0, "connection closed before 101 completed");
            resp_buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = resp_buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = std::str::from_utf8(&resp_buf[..header_end]).unwrap();
        let status_line = head.lines().next().unwrap();
        assert!(
            status_line.contains("101"),
            "expected 101 Switching Protocols, got: {status_line}"
        );
        let lower = head.to_ascii_lowercase();
        assert!(
            lower.contains("upgrade: websocket"),
            "missing Upgrade: websocket on 101 response; head={head}"
        );
        assert!(
            lower.contains("connection: upgrade"),
            "missing Connection: Upgrade on 101 response; head={head}"
        );
        assert!(
            lower.contains("sec-websocket-accept:"),
            "missing Sec-WebSocket-Accept on 101 response; head={head}"
        );

        // 5. After 101, send a raw payload and read it back through the tunnel.
        // Any leftover post-header bytes (unlikely) go first.
        let leftover = resp_buf[header_end..].to_vec();
        let payload: [u8; 8] = [0x81, 0x82, 0xde, 0xad, 0xbe, 0xef, 0x00, 0x11];
        client.write_all(&payload).await.unwrap();

        let mut got = leftover;
        while got.len() < payload.len() {
            let mut buf = [0u8; 64];
            let n = tokio::time::timeout(std::time::Duration::from_secs(5), client.read(&mut buf))
                .await
                .expect("timed out reading echo through tunnel")
                .unwrap();
            assert!(n > 0, "tunnel closed before echoing bytes");
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(
            &got[..payload.len()],
            &payload,
            "echoed bytes must match through the upgrade tunnel"
        );
    }

    /// End-to-end: WebSocket upgrade through a **client-kind** backend —
    /// downstream → proxy → control-channel tunnel → (fake) client → WS echo.
    /// The fake client lives at the registry message level: it answers
    /// OpenTunnel, completes the WS handshake on the first Data frame, then
    /// echoes raw bytes.
    #[tokio::test]
    async fn websocket_upgrade_to_client_backend_end_to_end() {
        use std::collections::HashSet;
        use std::net::SocketAddr;
        use std::sync::Arc;

        use arc_swap::ArcSwap;
        use axum::routing::any;
        use axum::Router;
        use base64::Engine;
        use sha1::{Digest, Sha1};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        use crate::common::ControlMessage;
        use crate::server::client_registry::{ClientRegistry, TunnelOpenOutcome};
        use crate::server::db::Database;
        use crate::server::reverse_proxy::connector::ClientConnector;
        use crate::server::reverse_proxy::router::RouteTable;
        use crate::server::reverse_proxy::upstream::UpstreamClient;
        use crate::server::reverse_proxy::{
            Backend, BackendKind, BackendProtocol, BackendScheme, LoadBalancing, ProxyRule, Route,
            RuleType,
        };

        const WS_MAGIC: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

        // 1. Fake tunnel client registered in a ClientRegistry.
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("pw").await.unwrap();
        let registry = ClientRegistry::new(db);
        let (client_tx, mut client_rx) = tokio::sync::mpsc::channel(64);
        let entry = registry
            .register("ws-client", None, None, "pw", client_tx)
            .await
            .unwrap();

        let entry_for_client = entry.clone();
        tokio::spawn(async move {
            let mut upgraded: HashSet<u64> = HashSet::new();
            while let Some(msg) = client_rx.recv().await {
                match msg {
                    ControlMessage::OpenTunnel { connection_id, .. } => {
                        let mut conns = entry_for_client.active_connections.lock().await;
                        if let Some(active) = conns.get_mut(&connection_id) {
                            if let Some(tx) = active.open_result.take() {
                                let _ = tx.send(TunnelOpenOutcome::Ok);
                            }
                        }
                    }
                    ControlMessage::Data {
                        connection_id,
                        data,
                    } => {
                        let reply = if upgraded.contains(&connection_id) {
                            Some(data) // raw echo after the handshake
                        } else {
                            // First frame: HTTP upgrade request — answer 101.
                            let text = String::from_utf8_lossy(&data);
                            let key = text
                                .lines()
                                .find(|l| l.to_ascii_lowercase().starts_with("sec-websocket-key:"))
                                .and_then(|l| l.split_once(':'))
                                .map(|(_, v)| v.trim().to_string())
                                .expect("no ws key in upgrade request");
                            let mut h = Sha1::new();
                            h.update(key.as_bytes());
                            h.update(WS_MAGIC.as_bytes());
                            let accept =
                                base64::engine::general_purpose::STANDARD.encode(h.finalize());
                            upgraded.insert(connection_id);
                            Some(
                                format!(
                                    "HTTP/1.1 101 Switching Protocols\r\n\
                                     Upgrade: websocket\r\n\
                                     Connection: Upgrade\r\n\
                                     Sec-WebSocket-Accept: {accept}\r\n\r\n"
                                )
                                .into_bytes(),
                            )
                        };
                        if let Some(bytes) = reply {
                            let conns = entry_for_client.active_connections.lock().await;
                            if let Some(active) = conns.get(&connection_id) {
                                let _ = active.inbound.send(bytes).await;
                            }
                        }
                    }
                    _ => {}
                }
            }
        });

        // 2. Proxy with a client-kind backend.
        let rule = ProxyRule {
            id: "r_ws_client".into(),
            name: "r_ws_client".into(),
            rule_type: RuleType::Http,
            listen: "127.0.0.1:0".into(),
            domains: vec!["test.local".into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Client,
                    addr: "127.0.0.1:9999".into(),
                    client_name: Some("ws-client".into()),
                    weight: 100,
                    protocol: BackendProtocol::Http1,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::default(),
            }],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        let table = RouteTable::from_rules(vec![rule]);
        let source = RouteSource(Arc::new(ArcSwap::from_pointee(table)));
        let upstream = Arc::new(UpstreamClient::new());
        let proxy_state = Arc::new(ReverseProxyState::new());
        proxy_state
            .set_client_connector(Arc::new(ClientConnector::new(registry)))
            .await;

        let app = Router::new()
            .fallback(any(handle_proxy_request_unified))
            .with_state((source, upstream, proxy_state));
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr: SocketAddr = proxy_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(proxy_listener, app).await.unwrap();
        });

        // 3. Raw WS handshake through the proxy.
        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        let req = "GET /chat HTTP/1.1\r\n\
                   Host: test.local\r\n\
                   Upgrade: websocket\r\n\
                   Connection: Upgrade\r\n\
                   Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                   Sec-WebSocket-Version: 13\r\n\r\n";
        client.write_all(req.as_bytes()).await.unwrap();

        let mut resp_buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 1024];
        let header_end = loop {
            let n =
                tokio::time::timeout(std::time::Duration::from_secs(5), client.read(&mut chunk))
                    .await
                    .expect("timed out waiting for 101 headers")
                    .unwrap();
            assert!(n > 0, "connection closed before 101 completed");
            resp_buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = resp_buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = std::str::from_utf8(&resp_buf[..header_end]).unwrap();
        let status_line = head.lines().next().unwrap();
        assert!(
            status_line.contains("101"),
            "expected 101 Switching Protocols through client backend, got: {status_line}"
        );

        // 4. Post-upgrade bytes flow both ways through the tunnel.
        let leftover = resp_buf[header_end..].to_vec();
        let payload: [u8; 8] = [0x81, 0x82, 0xde, 0xad, 0xbe, 0xef, 0x00, 0x11];
        client.write_all(&payload).await.unwrap();

        let mut got = leftover;
        while got.len() < payload.len() {
            let mut buf = [0u8; 64];
            let n = tokio::time::timeout(std::time::Duration::from_secs(5), client.read(&mut buf))
                .await
                .expect("timed out reading echo through client tunnel")
                .unwrap();
            assert!(n > 0, "tunnel closed before echoing bytes");
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(
            &got[..payload.len()],
            &payload,
            "echoed bytes must match through the client-backend upgrade tunnel"
        );
    }

    /// h1 fallback: when URI is relative (no authority) and only Host header
    /// carries the value, it must still be honored.
    #[tokio::test]
    async fn handler_falls_back_to_host_header_for_relative_uri() {
        use std::net::SocketAddr;
        use std::sync::Arc;

        use arc_swap::ArcSwap;
        use axum::body::Body;
        use axum::extract::State;
        use axum::http::Request as AxumRequest;

        use crate::server::reverse_proxy::router::RouteTable;
        use crate::server::reverse_proxy::upstream::UpstreamClient;
        use crate::server::reverse_proxy::{
            Backend, BackendKind, BackendProtocol, BackendScheme, LoadBalancing, ProxyRule, Route,
            RuleType,
        };

        let backend_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr: SocketAddr = backend_listener.local_addr().unwrap();
        let backend_app = axum::Router::new().route("/", axum::routing::any(|| async { "ok" }));
        tokio::spawn(async move {
            axum::serve(backend_listener, backend_app).await.unwrap();
        });

        let rule = ProxyRule {
            id: "r".into(),
            name: "r".into(),
            rule_type: RuleType::Http,
            listen: "127.0.0.1:0".into(),
            domains: vec!["test.local".into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Direct,
                    addr: backend_addr.to_string(),
                    client_name: None,
                    weight: 100,
                    protocol: BackendProtocol::Http1,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::default(),
            }],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        let table = RouteTable::from_rules(vec![rule]);
        let source = RouteSource(Arc::new(ArcSwap::from_pointee(table)));
        let upstream = Arc::new(UpstreamClient::new());
        let proxy_state = Arc::new(ReverseProxyState::new());

        // h1-style: URI is just "/", host lives in the Host header.
        let req: AxumRequest<Body> = AxumRequest::builder()
            .method("GET")
            .uri("/")
            .header("host", "test.local:8080")
            .body(Body::empty())
            .unwrap();

        let state = State((source, upstream, proxy_state));
        let resp = handle_proxy_request_unified(state, req).await;

        assert_eq!(resp.status(), 200);
    }

    /// Traffic accounting: a proxied request must record one connection and
    /// the request/response body bytes into the rule's pending accumulator.
    #[tokio::test]
    async fn traffic_is_recorded_for_proxied_request() {
        use std::net::SocketAddr;
        use std::sync::Arc;

        use arc_swap::ArcSwap;
        use axum::body::Body;
        use axum::extract::State;
        use axum::http::Request as AxumRequest;

        use crate::server::reverse_proxy::router::RouteTable;
        use crate::server::reverse_proxy::upstream::UpstreamClient;
        use crate::server::reverse_proxy::{
            Backend, BackendKind, BackendProtocol, BackendScheme, LoadBalancing, ProxyRule, Route,
            RuleType,
        };

        let backend_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr: SocketAddr = backend_listener.local_addr().unwrap();
        let backend_app =
            axum::Router::new().route("/", axum::routing::any(|| async { "hello world" }));
        tokio::spawn(async move {
            axum::serve(backend_listener, backend_app).await.unwrap();
        });

        let rule = ProxyRule {
            id: "r1".into(),
            name: "r1".into(),
            rule_type: RuleType::Http,
            listen: "127.0.0.1:0".into(),
            domains: vec!["test.local".into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Direct,
                    addr: backend_addr.to_string(),
                    client_name: None,
                    weight: 100,
                    protocol: BackendProtocol::Http1,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::default(),
            }],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        let table = RouteTable::from_rules(vec![rule]);
        let source = RouteSource(Arc::new(ArcSwap::from_pointee(table)));
        let upstream = Arc::new(UpstreamClient::new());
        let proxy_state = Arc::new(ReverseProxyState::new());
        let traffic_pending = proxy_state.traffic_pending.clone();

        let req: AxumRequest<Body> = AxumRequest::builder()
            .method("POST")
            .uri("/")
            .header("host", "test.local")
            .body(Body::from("abc"))
            .unwrap();

        let state = State((source, upstream, proxy_state));
        let resp = handle_proxy_request_unified(state, req).await;
        assert_eq!(resp.status(), 200);

        // Consume the response body so its bytes are counted
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"hello world");

        let map = traffic_pending.lock().unwrap();
        let delta = map.get("r1").expect("traffic recorded for rule r1");
        assert_eq!(delta.connections, 1);
        assert_eq!(delta.bytes_in, 3, "request body bytes counted");
        assert_eq!(delta.bytes_out, 11, "response body bytes counted");
    }

    // ---- Client backend tests ----

    /// Client backend with HTTP/2 protocol → 502.
    #[tokio::test]
    async fn client_backend_http2_returns_502() {
        use arc_swap::ArcSwap;
        use axum::body::Body;
        use axum::extract::State;
        use axum::http::Request as AxumRequest;

        use crate::server::reverse_proxy::router::RouteTable;
        use crate::server::reverse_proxy::upstream::UpstreamClient;
        use crate::server::reverse_proxy::{
            Backend, BackendKind, BackendProtocol, BackendScheme, LoadBalancing, ProxyRule, Route,
            RuleType,
        };

        // Build a rule with a client-kind backend using HTTP/2
        let rule = ProxyRule {
            id: "r1".into(),
            name: "r1".into(),
            rule_type: RuleType::Http,
            listen: "127.0.0.1:0".into(),
            domains: vec!["test.local".into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Client,
                    addr: "localhost:80".into(),
                    client_name: Some("home-nas".into()),
                    weight: 100,
                    protocol: BackendProtocol::Http2,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::default(),
            }],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        let table = RouteTable::from_rules(vec![rule]);
        let source = RouteSource(Arc::new(ArcSwap::from_pointee(table)));
        let upstream = Arc::new(UpstreamClient::new());
        let proxy_state = Arc::new(ReverseProxyState::new());

        let req: AxumRequest<Body> = AxumRequest::builder()
            .method("GET")
            .uri("/")
            .header("host", "test.local")
            .body(Body::empty())
            .unwrap();

        let state = State((source, upstream, proxy_state));
        let resp = handle_proxy_request_unified(state, req).await;
        assert_eq!(resp.status(), 502);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("HTTP/2 to client backend not yet supported"),
            "body: {text}"
        );
    }

    /// Client backend with no ClientConnector registered → 502.
    /// (WebSocket upgrades to client backends are supported — see
    /// `websocket_upgrade_to_client_backend_end_to_end`.)
    #[tokio::test]
    async fn client_backend_offline_returns_502() {
        use arc_swap::ArcSwap;
        use axum::body::Body;
        use axum::extract::State;
        use axum::http::Request as AxumRequest;

        use crate::server::reverse_proxy::router::RouteTable;
        use crate::server::reverse_proxy::upstream::UpstreamClient;
        use crate::server::reverse_proxy::{
            Backend, BackendKind, BackendProtocol, BackendScheme, LoadBalancing, ProxyRule, Route,
            RuleType,
        };

        // Build a rule with a client-kind backend (no ClientConnector registered)
        let rule = ProxyRule {
            id: "r1".into(),
            name: "r1".into(),
            rule_type: RuleType::Http,
            listen: "127.0.0.1:0".into(),
            domains: vec!["test.local".into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Client,
                    addr: "localhost:80".into(),
                    client_name: Some("home-nas".into()),
                    weight: 100,
                    protocol: BackendProtocol::Http1,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::default(),
            }],
            tls: None,
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        let table = RouteTable::from_rules(vec![rule]);
        let source = RouteSource(Arc::new(ArcSwap::from_pointee(table)));
        let upstream = Arc::new(UpstreamClient::new());
        let proxy_state = Arc::new(ReverseProxyState::new());

        let req: AxumRequest<Body> = AxumRequest::builder()
            .method("GET")
            .uri("/")
            .header("host", "test.local")
            .body(Body::empty())
            .unwrap();

        let state = State((source, upstream, proxy_state));
        let resp = handle_proxy_request_unified(state, req).await;
        assert_eq!(resp.status(), 502);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("client backend"),
            "body should mention client backend, got: {text}"
        );
    }
}
