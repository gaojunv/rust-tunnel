//! Unified HTTP proxy request handler.

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
    parts.headers = strip_hop_by_hop(&parts.headers);
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
/// `Response<Body>`. Streams the body without buffering.
fn build_downstream_response(resp: hyper::Response<hyper::body::Incoming>) -> Response {
    let (mut parts, incoming) = resp.into_parts();
    parts.headers = strip_hop_by_hop(&parts.headers);
    let axum_body = Body::new(incoming);
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
    State((source, upstream)): State<ProxyState>,
    req: Request<Body>,
) -> Response {
    let host = req
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .map(|s| host_without_port(s).to_string())
        .unwrap_or_default();
    let path = req.uri().path().to_string();

    let Some(backend) = resolve_backend(&source, &host, &path).await else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from(format!("No route for host '{host}'")))
            .unwrap();
    };

    let upstream_req = match build_upstream_request(req, &backend) {
        Ok(r) => r,
        Err(e) => return error_response(&e),
    };

    match upstream.forward(&backend, upstream_req).await {
        Ok(resp) => build_downstream_response(resp),
        Err(e) => {
            error!(
                error = %e,
                backend = %backend.addr,
                scheme = ?backend.scheme,
                protocol = ?backend.protocol,
                "upstream request failed"
            );
            error_response(&e)
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
            Backend, BackendProtocol, BackendScheme, LoadBalancing, ProxyRule, Route, RuleType,
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
                    addr: backend_addr.to_string(),
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

        // 3. Build the axum Router with the unified handler and bind it.
        let app = Router::new()
            .fallback(axum::routing::any(handle_proxy_request_unified))
            .with_state((source, upstream));
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
            Backend, BackendProtocol, BackendScheme, LoadBalancing, ProxyRule, Route, RuleType,
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
                    addr: backend_addr.to_string(),
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

        let app = Router::new()
            .fallback(any(handle_proxy_request_unified))
            .with_state((source, upstream));
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
}
