use std::sync::Arc;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use hyper::Request;

use super::super::{Backend, ReverseProxyState};
use super::downstream_response::error_response;
use super::upstream_request::{build_upstream_request, count_body};
use super::websocket::is_websocket_upgrade;
use super::RouteSource;
use rust_tunnel_stats::EntityType;

/// Resolve the target backend for a request. Returns the matched rule id and
/// the selected backend, or `None` when no route matches (caller should reply
/// 404).
///
/// Both listener types (legacy per-rule and shared) go through this — legacy
/// listeners just build a one-rule table. `RouteTable::match_http_request`
/// already honors `rule.enabled`, longest-prefix matching, and the route's
/// configured load balancing algorithm.
pub(super) async fn resolve_backend(
    source: &RouteSource,
    host: &str,
    path: &str,
) -> Option<(String, Backend)> {
    let snap = source.0.load();
    snap.match_http_request(host, path)
        .await
        .map(|(rule, _, backend)| (rule.id.clone(), backend.clone()))
}

/// Strip the port (if any) from a Host header value.
///
/// Handles bracketed IPv6 literals per RFC 7230 §5.4 (`[::1]:8080` → `::1`).
pub fn host_without_port(raw: &str) -> &str {
    if let Some(rest) = raw.strip_prefix('[') {
        // Bracketed IPv6 literal; the closing ']' terminates the host.
        rest.split(']').next().unwrap_or(rest)
    } else {
        // Plain hostname or IPv4; first ':' terminates the host.
        raw.split(':').next().unwrap_or(raw)
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
pub(super) async fn handle_client_backend(
    state: Arc<ReverseProxyState>,
    req: Request<Body>,
    backend: Backend,
    rule_id: String,
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
    let counted = Body::new(count_body(body, stats.clone(), rule_id.clone(), true));
    let req = Request::from_parts(parts, counted);

    // Dial via ClientConnector
    let connector = match state.connector_for(&backend).await {
        Ok(c) => c,
        Err(e) => {
            stats.decr_conns(EntityType::Proxy, &rule_id);
            let body = format!("connector unavailable: {e}");
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(body.clone()))
                .unwrap_or_else(|err| {
                    tracing::warn!("failed to build response: {err}");
                    Response::new(Body::from(body))
                });
        }
    };
    let stream = match connector.connect(&backend).await {
        Ok(s) => s,
        Err(e) => {
            stats.decr_conns(EntityType::Proxy, &rule_id);
            let body = format!("client backend dial failed: {e}");
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(body.clone()))
                .unwrap_or_else(|err| {
                    tracing::warn!("failed to build response: {err}");
                    Response::new(Body::from(body))
                });
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
            stats.decr_conns(EntityType::Proxy, &rule_id);
            let body = format!("http1 handshake failed: {e}");
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(body.clone()))
                .unwrap_or_else(|err| {
                    tracing::warn!("failed to build response: {err}");
                    Response::new(Body::from(body))
                });
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
            .map_or("/", axum::http::uri::PathAndQuery::as_str);
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
                let Some(client_upgrade) = downstream_upgrade else {
                    tracing::warn!("ws upgrade handle missing despite ws_potential");
                    stats.decr_conns(EntityType::Proxy, &rule_id);
                    return super::downstream_response::build_downstream_response(
                        resp,
                        stats.clone(),
                        rule_id,
                    );
                };
                let rid = rule_id.clone();
                let sc = stats.clone();
                tokio::spawn(async move {
                    let (client_up, server_up) =
                        match tokio::try_join!(client_upgrade, upstream_upgrade) {
                            Ok(pair) => pair,
                            Err(e) => {
                                tracing::debug!("ws upgrade join failed: {e}");
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
                            sc.record_bytes(EntityType::Proxy, &rid, to_backend, to_client);
                        }
                        Err(e) => {
                            tracing::debug!("ws bidirectional copy ended: {e}");
                        }
                    }
                    sc.decr_conns(EntityType::Proxy, &rid);
                });
            }
            if !is_ws {
                stats.decr_conns(EntityType::Proxy, &rule_id);
            }
            super::downstream_response::build_downstream_response(resp, stats.clone(), rule_id)
        }
        Err(e) => {
            stats.decr_conns(EntityType::Proxy, &rule_id);
            let body = format!("upstream request failed: {e}");
            Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(body.clone()))
                .unwrap_or_else(|err| {
                    tracing::warn!("failed to build response: {err}");
                    Response::new(Body::from(body))
                })
        }
    }
}
