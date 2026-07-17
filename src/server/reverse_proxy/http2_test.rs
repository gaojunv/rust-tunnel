//! End-to-end HTTP/2 tests for the reverse proxy.
//!
//! - `downstream_h2_over_tls`: client (h2 over TLS + ALPN) → gateway → h1 backend
//! - `upstream_h2c`: client (h1) → gateway → h2c backend (prior-knowledge)

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::routing::any;
use axum::Router;
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::Request;
use hyper_util::rt::TokioExecutor;
use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use rustls::pki_types::ServerName;
use tokio::net::TcpListener;
use tokio_rustls::rustls;

use crate::server::acme::{CertEntry, CertSource, CertificateManager};
use crate::server::reverse_proxy::router::RouteTable;
use crate::server::reverse_proxy::shared_listener::SharedListener;
use crate::server::reverse_proxy::{
    Backend, BackendProtocol, BackendScheme, LoadBalancing, ProxyRule, ProxyTlsConfig, Route,
    RuleType,
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
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
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
                addr: backend_addr.to_string(),
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
    let (mut sender, conn) = hyper::client::conn::http2::handshake::<_, _, http_body_util::Empty<Bytes>>(
        TokioExecutor::new(),
        io,
    )
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
                addr: backend_addr.to_string(),
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
    )
    .await
    .expect("shared listener spawn");

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // 3. h1 client → gateway → h2c backend.
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

    // Point the rule at a reserved/closed port — connect will refuse quickly.
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
                // Port 1 is well-known-reserved and effectively always closed on localhost.
                addr: "127.0.0.1:1".to_string(),
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
