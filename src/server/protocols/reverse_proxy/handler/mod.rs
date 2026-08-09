//! Unified HTTP proxy request handler.

mod client_backend;
mod downstream_response;
mod upstream_request;
mod websocket;

use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use hyper::Request;
use tracing::error;

use super::router::RouteTable;
use super::upstream::UpstreamClient;
use super::{BackendKind, BackendProtocol, ReverseProxyState};
use crate::server::stats::EntityType;

use client_backend::{handle_client_backend, host_without_port, resolve_backend};
use downstream_response::{build_downstream_response, error_chain, error_response};
use upstream_request::{build_upstream_request, count_body};
use websocket::is_websocket_upgrade;

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
/// the `StatsCollector` and the `ClientConnector` for client-kind backends.
pub type ProxyState = (RouteSource, Arc<UpstreamClient>, Arc<ReverseProxyState>);

/// Unified axum handler that replaces both the legacy per-rule handler and
/// the shared-listener handler.
pub async fn handle_proxy_request_unified(
    State((source, upstream, proxy_state)): State<ProxyState>,
    mut req: Request<Body>,
) -> Response {
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
    stats.incr_conns(EntityType::Proxy, &rule_id);

    // Decrement on scope exit (deferred via clone)
    let rule_id_for_decrement = rule_id.clone();

    // Client backend fork: bypass the shared UpstreamClient and dial
    // through the ClientConnector tunnel.
    if backend.kind == BackendKind::Client {
        // HTTP/2 not supported for client backends
        if backend.protocol != BackendProtocol::Http1 {
            stats.decr_conns(EntityType::Proxy, &rule_id_for_decrement);
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("HTTP/2 to client backend not yet supported"))
                .unwrap();
        }
        return handle_client_backend(proxy_state, req, backend, rule_id_for_decrement).await;
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
        let counted = Body::new(count_body(body, stats.clone(), rule_id.clone(), true));
        Request::from_parts(parts, counted)
    };

    let upstream_req = match build_upstream_request(req, &backend) {
        Ok(r) => r,
        Err(e) => {
            // Decrement on build error
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
                    let sc = stats.clone();
                    tokio::spawn(async move {
                        let (client_up, server_up) =
                            match tokio::try_join!(client_upgrade, upstream_upgrade) {
                                Ok(pair) => pair,
                                Err(e) => {
                                    tracing::debug!("ws upgrade join failed: {e}");
                                    // Decrement on WS join failure
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
                                sc.record_bytes(EntityType::Proxy, &rid, to_backend, to_client);
                            }
                            Err(e) => {
                                tracing::debug!("ws bidirectional copy ended: {e}");
                            }
                        }
                        // Decrement on WS tunnel close
                        sc.decr_conns(EntityType::Proxy, &rid);
                    });
                }
            }
            if !is_ws {
                // Decrement for regular (non-WS) responses
                stats.decr_conns(EntityType::Proxy, &rule_id_for_decrement);
            }
            build_downstream_response(resp, stats.clone(), rule_id.clone())
        }
        Err(e) => {
            // Decrement on upstream error
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

#[cfg(test)]
mod tests {
    use super::upstream_request::strip_hop_by_hop;
    use super::*;
    use http_body_util::BodyExt;
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

        use rust_tunnel_common::ControlMessage;
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
        let stats = proxy_state.stats_collector.clone();

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

        let summary = stats.get_summary();
        assert_eq!(
            summary.proxy.total_bytes_in, 3,
            "request body bytes counted"
        );
        assert_eq!(
            summary.proxy.total_bytes_out, 11,
            "response body bytes counted"
        );
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

#[cfg(test)]
mod http2_tests {
    use super::*;

    use std::collections::HashSet;
    use std::net::SocketAddr;

    use axum::routing::any;
    use axum::Router;
    use http_body_util::BodyExt;
    use hyper::body::Bytes;
    use hyper_util::rt::TokioExecutor;
    use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
    use rustls::pki_types::ServerName;
    use tokio::net::TcpListener;
    use tokio_rustls::rustls;

    use crate::server::acme::{CertEntry, CertSource, CertificateManager};
    use crate::server::reverse_proxy::shared_listener::SharedListener;
    use crate::server::reverse_proxy::{
        Backend, BackendScheme, LoadBalancing, ProxyRule, ProxyTlsConfig, Route, RuleType,
    };

    /// Register a self-signed cert for the given domain in the manager.
    async fn seed_cert(mgr: &Arc<CertificateManager>, domain: &str) {
        let kp = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let params = CertificateParams::new(vec![domain.to_string()]).unwrap();
        let cert = params.self_signed(&kp).unwrap();
        mgr.add_certificate(
            domain,
            CertEntry {
                cert_pem: cert.pem(),
                key_pem: kp.serialize_pem(),
                chain_pem: None,
                expires_at: None,
                source: CertSource::Manual,
            },
        )
        .await
        .unwrap();
    }

    /// Trust anything client-side; the test uses a self-signed cert.
    fn insecure_client_config_with_alpn() -> Arc<rustls::ClientConfig> {
        #[derive(Debug)]
        struct NoVerify;
        impl rustls::client::danger::ServerCertVerifier for NoVerify {
            fn verify_server_cert(
                &self,
                _: &rustls::pki_types::CertificateDer<'_>,
                _: &[rustls::pki_types::CertificateDer<'_>],
                _: &rustls::pki_types::ServerName<'_>,
                _: &[u8],
                _: rustls::pki_types::UnixTime,
            ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            fn verify_tls12_signature(
                &self,
                _: &[u8],
                _: &rustls::pki_types::CertificateDer<'_>,
                _: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
            {
                Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
            }
            fn verify_tls13_signature(
                &self,
                _: &[u8],
                _: &rustls::pki_types::CertificateDer<'_>,
                _: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
            {
                Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
            }
            fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
                vec![
                    rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
                    rustls::SignatureScheme::RSA_PSS_SHA256,
                    rustls::SignatureScheme::ED25519,
                ]
            }
        }
        let mut c = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth();
        c.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Arc::new(c)
    }

    /// Reserve a free localhost port by binding and dropping.
    async fn free_port() -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        a
    }

    #[tokio::test]
    async fn downstream_h2_over_tls() {
        // 1. h1 backend that returns a fixed body.
        let backend_app = Router::new().route("/", any(|| async { "hello from h1 backend" }));
        let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr: SocketAddr = backend_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(backend_listener, backend_app).await.unwrap();
        });

        // 2. TLS-terminated shared listener with ALPN advertised.
        let temp_dir = tempfile::TempDir::new().unwrap();
        let mgr = Arc::new(CertificateManager::new(temp_dir.path().to_str().unwrap()));
        seed_cert(&mgr, "test.local").await;

        let listen_addr = free_port().await;
        let rule = ProxyRule {
            id: "r1".into(),
            name: "r1".into(),
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
            tls: Some(ProxyTlsConfig {
                enabled: true,
                acme: false,
                domain: Some("test.local".into()),
            }),
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        let table = RouteTable::from_rules(vec![rule]);

        let _listener = SharedListener::spawn(
            listen_addr.to_string(),
            true,
            table,
            Some(mgr.clone()),
            HashSet::from(["r1".to_string()]),
            Arc::new(ReverseProxyState::new()),
        )
        .await
        .expect("shared listener spawn");

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // 3. Dial with SNI = test.local via raw tokio-rustls + hyper::client::conn::http2 handshake.
        let tcp = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let connector = tokio_rustls::TlsConnector::from(insecure_client_config_with_alpn());
        let sni = ServerName::try_from("test.local").unwrap();
        let tls = connector.connect(sni, tcp).await.unwrap();
        let alpn = tls.get_ref().1.alpn_protocol().map(|p| p.to_vec());
        assert_eq!(
            alpn.as_deref(),
            Some(b"h2".as_slice()),
            "ALPN must negotiate h2"
        );

        let io = hyper_util::rt::TokioIo::new(tls);
        let (mut sender, conn) = hyper::client::conn::http2::handshake::<
            _,
            _,
            http_body_util::Empty<Bytes>,
        >(TokioExecutor::new(), io)
        .await
        .unwrap();
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let req = Request::builder()
            .method("GET")
            .uri("https://test.local/")
            .header("host", "test.local")
            .body(http_body_util::Empty::<Bytes>::new())
            .unwrap();
        let resp = sender.send_request(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let (_, body) = resp.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        assert_eq!(bytes, "hello from h1 backend");
    }

    #[tokio::test]
    async fn upstream_h2c() {
        use http_body_util::Full;
        use hyper::server::conn::http2 as h2_server;
        use hyper_util::client::legacy::{connect::HttpConnector, Client};

        // 1. h2c backend (hyper's http2 server, no TLS).
        let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr: SocketAddr = backend_listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = backend_listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let io = hyper_util::rt::TokioIo::new(stream);
                    let svc = hyper::service::service_fn(|_req| async {
                        Ok::<_, std::convert::Infallible>(hyper::Response::new(Full::new(
                            Bytes::from("h2c ok"),
                        )))
                    });
                    let _ = h2_server::Builder::new(TokioExecutor::new())
                        .serve_connection(io, svc)
                        .await;
                });
            }
        });

        // 2. Plain-HTTP proxy rule that upstreams to the h2c backend via Http2 + Http.
        let temp_dir = tempfile::TempDir::new().unwrap();
        let mgr = Arc::new(CertificateManager::new(temp_dir.path().to_str().unwrap()));

        let listen_addr = free_port().await;
        let rule = ProxyRule {
            id: "r_h2c".into(),
            name: "r_h2c".into(),
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
        let _listener = SharedListener::spawn(
            listen_addr.to_string(),
            false,
            table,
            Some(mgr),
            HashSet::from(["r_h2c".to_string()]),
            Arc::new(ReverseProxyState::new()),
        )
        .await
        .expect("shared listener spawn");

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // 3. h1 client -> gateway -> h2c backend.
        let client: Client<HttpConnector, http_body_util::Empty<Bytes>> =
            Client::builder(TokioExecutor::new()).build(HttpConnector::new());
        let uri: hyper::Uri = format!("http://{listen_addr}/").parse().unwrap();
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("host", "test.local")
            .body(http_body_util::Empty::<Bytes>::new())
            .unwrap();
        let resp = client.request(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(bytes, "h2c ok");
    }

    #[tokio::test]
    async fn upstream_connect_failure_returns_502() {
        use hyper_util::client::legacy::{connect::HttpConnector, Client};

        // Point the rule at a reserved/closed port -- connect will refuse quickly.
        let temp_dir = tempfile::TempDir::new().unwrap();
        let mgr = Arc::new(CertificateManager::new(temp_dir.path().to_str().unwrap()));

        let listen_addr = free_port().await;
        let rule = ProxyRule {
            id: "r_fail".into(),
            name: "r_fail".into(),
            rule_type: RuleType::Http,
            listen: listen_addr.to_string(),
            domains: vec!["test.local".into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Direct,
                    // Port 1 is well-known-reserved and effectively always closed on localhost.
                    addr: "127.0.0.1:1".to_string(),
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
        let _listener = SharedListener::spawn(
            listen_addr.to_string(),
            false,
            table,
            Some(mgr),
            HashSet::from(["r_fail".to_string()]),
            Arc::new(ReverseProxyState::new()),
        )
        .await
        .expect("shared listener spawn");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let client: Client<HttpConnector, http_body_util::Empty<Bytes>> =
            Client::builder(TokioExecutor::new()).build(HttpConnector::new());
        let uri: hyper::Uri = format!("http://{listen_addr}/").parse().unwrap();
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("host", "test.local")
            .body(http_body_util::Empty::<Bytes>::new())
            .unwrap();
        let resp = client.request(req).await.unwrap();
        assert_eq!(resp.status(), 502, "connect refused must produce 502");
    }
}
