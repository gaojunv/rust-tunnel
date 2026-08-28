//! 集成测试：Trojan 域名配置持久化、443 共享模式 e2e、自签名回退。
//!
//! run_server 不启动 Trojan listener，共享模式 e2e 直接调用底层函数：
//! ReverseProxyState::reconcile_http_listener + trojan_runtime::apply_trojan_config。

#[path = "common/mod.rs"]
mod common;

use common::api_client::ApiClient;
use common::wait_until;
use reqwest::StatusCode;
use rust_tunnel_server::auth::AuthConfig;
use rust_tunnel_server::dynamic_config::{DynamicConfig, TrojanDynamicConfig};
use rust_tunnel_server::{api, control_plane as control, Database, ServerConfig};
use std::net::TcpListener as StdTcpListener;
use std::time::Duration;
use tempfile::TempDir;
use tokio::task::JoinHandle;

fn reserve_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .expect("reserve port")
        .local_addr()
        .expect("local_addr")
        .port()
}

fn test_config(db_path: &str, api_port: u16) -> ServerConfig {
    let control_port = reserve_port();
    ServerConfig {
        control_addr: format!("127.0.0.1:{control_port}"),
        api_addr: format!("127.0.0.1:{api_port}"),
        admin_password: None,
        jwt_secret: Some("test-jwt-secret-do-not-use-in-prod".to_string()),
        client_auth_token: None,
        tls: false,
        tls_cert: "./data/tls/cert.pem".to_string(),
        tls_key: "./data/tls/key.pem".to_string(),
        log: "warn".to_string(),
        db_path: db_path.to_string(),
        client_dist_dir: "./client".to_string(),
        ss_enabled: false,
        ss_port: None,
        ss_cipher: None,
        ss_password: None,
        trojan_enabled: false,
        trojan_port: None,
        trojan_password: None,
        trojan_fallback: "127.0.0.1:80".to_string(),
        dns_enabled: false,
        dns_bind: "127.0.0.1:0".to_string(),
        dns_tunnel_domain: "tunnel.local".to_string(),
        dns_mesh_domain: "mesh.local".to_string(),
        reverse_proxy_enabled: false,
        reverse_proxy_max_connections: 10000,
        reverse_proxy_connection_timeout: 30,
        reverse_proxy_buffer_size: 8192,
        api_tls: false,
        api_domain: None,
        acme_enabled: false,
        acme_server_url: "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
        acme_cert_dir: "./data/certs".to_string(),
        acme_auto_renew: true,
        acme_renewal_check_interval: 24,
        acme_renewal_days_before_expiry: 30,
        acme_email: None,
        acme_tos_agreed: false,
    }
}

struct RunningServer {
    db: Database,
    dynamic_config: DynamicConfig,
    api_base: String,
    task: JoinHandle<()>,
}

async fn start_server(config: &ServerConfig) -> RunningServer {
    let db = Database::new(&config.db_path).await.expect("db new");
    let dynamic_config = DynamicConfig::load_or_seed(&db, config).await;
    let state = control::ServerState::with_db(db.clone());
    state.set_dynamic_config(dynamic_config.clone()).await;
    let auth_config = AuthConfig::new(config.admin_password.clone(), config.jwt_secret.clone());

    let api_addr = config.api_addr.clone();
    let task = tokio::spawn(async move {
        let _ = api::run_api_server(api_addr, state, auth_config, None).await;
    });

    let api_base = format!("http://{}", config.api_addr);
    let health_base = api_base.clone();
    wait_until("api health", || {
        let base = health_base.clone();
        async move {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_millis(200))
                .build()
                .ok()?;
            let resp = client.get(format!("{base}/api/health")).send().await.ok()?;
            if resp.status().is_success() {
                Some(())
            } else {
                None
            }
        }
    })
    .await
    .expect("api never became healthy");

    RunningServer {
        db,
        dynamic_config,
        api_base,
        task,
    }
}

async fn stop_server(server: RunningServer) {
    server.task.abort();
    drop(server.db);
}

#[tokio::test(flavor = "multi_thread")]
async fn trojan_domain_and_port_persist_across_restart() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let tempdir = TempDir::new().expect("tempdir");
        let db_path = tempdir.path().join("rust-tunnel.db");
        let db_path = db_path.to_string_lossy().to_string();

        let mut config = test_config(&db_path, reserve_port());
        config.trojan_enabled = true;
        config.trojan_port = Some(reserve_port());
        config.trojan_password = Some("initial-password".to_string());

        let server = start_server(&config).await;
        let api_client = ApiClient::new(&server.api_base);

        // 经 API 修改 domain/port（enabled:false，避免测试环境 bind 特权端口）
        let new_port = reserve_port();
        let (status, body) = api_client
            .post_json(
                "/api/trojan",
                serde_json::json!({
                    "enabled": false,
                    "port": new_port,
                    "password": "updated-password",
                    "fallback": "127.0.0.1:8080",
                    "domain": "Trojan.Example.com",
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "update trojan, body={body:?}");

        // GET 立即返回新值（域名已归一化为小写）
        let (get_status, get_body) = api_client.get_json("/api/trojan").await;
        assert_eq!(get_status, StatusCode::OK);
        assert_eq!(get_body["domain"], "trojan.example.com", "GET 应返回新域名");
        assert_eq!(get_body["port"], new_port);

        // 同库重启后保持
        stop_server(server).await;
        config.api_addr = format!("127.0.0.1:{}", reserve_port());
        let restarted = start_server(&config).await;
        let trojan = restarted
            .dynamic_config
            .trojan
            .as_ref()
            .expect("trojan config after restart");
        assert_eq!(trojan.port, new_port);
        assert_eq!(trojan.domain, "trojan.example.com", "重启后域名应持久化");
        assert_eq!(trojan.password, "updated-password");
        stop_server(restarted).await;
    })
    .await;
    result.expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn trojan_password_optional_keeps_existing() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let tempdir = TempDir::new().expect("tempdir");
        let db_path = tempdir.path().join("rust-tunnel.db");
        let db_path = db_path.to_string_lossy().to_string();

        let mut config = test_config(&db_path, reserve_port());
        config.trojan_enabled = true;
        config.trojan_port = Some(reserve_port());
        config.trojan_password = Some("keep-me".to_string());

        let server = start_server(&config).await;
        let api_client = ApiClient::new(&server.api_base);

        // 不带 password 字段 → 保留原密码
        let (status, body) = api_client
            .post_json(
                "/api/trojan",
                serde_json::json!({
                    "enabled": false,
                    "port": reserve_port(),
                    "domain": "t.example.com",
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "body={body:?}");

        // 以 DB 为准核对（dynamic_config 是启动时快照）
        let all = server.db.load_trojan_configs().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].password, "keep-me", "留空 password 应保留原密码");
        assert_eq!(all[0].domain, "t.example.com");

        // 非法域名 → 400
        let (status, _) = api_client
            .post_json(
                "/api/trojan",
                serde_json::json!({
                    "enabled": false,
                    "port": reserve_port(),
                    "domain": "*.bad.com",
                }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        stop_server(server).await;
    })
    .await;
    result.expect("test timed out");
}

// ---------------------------------------------------------------------------
// 共享模式 e2e 与自签名回退：直接驱动底层函数（run_server 不启动 Trojan）
// ---------------------------------------------------------------------------

use rust_tunnel_server::acme::{CertEntry, CertSource, CertificateManager};
use rust_tunnel_server::reverse_proxy::{
    Backend, BackendKind, BackendProtocol, BackendScheme, LoadBalancing, ProxyRule, ProxyTlsConfig,
    Route, RuleType,
};
use rust_tunnel_server::trojan::sha224_hex;
use rust_tunnel_server::trojan_runtime;
use std::sync::Arc;

/// 造一张自签名证书加入 cert_manager。
async fn add_test_cert(mgr: &Arc<CertificateManager>, domain: &str) {
    use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
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

/// 启动 TCP echo 服务器，返回端口。
async fn start_echo() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let (mut r, mut w) = tokio::io::split(stream);
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    port
}

/// 启动返回固定响应的 HTTP mock backend，返回端口。
async fn start_http_backend(body: &'static str) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    port
}

/// 构造 Trojan 请求头（与 trojan_test.rs 的 build_trojan_header 同格式）。
fn build_trojan_header(password: &str, target_ip: [u8; 4], target_port: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(sha224_hex(password).as_bytes());
    buf.extend_from_slice(b"\r\n");
    buf.push(0x01); // CONNECT
    buf.push(0x01); // ATYP = IPv4
    buf.extend_from_slice(&target_ip);
    buf.extend_from_slice(&target_port.to_be_bytes());
    buf.extend_from_slice(b"\r\n");
    buf
}

/// 用指定 SNI 建立 TLS 连接（insecure 模式），发送 trojan 头 + 数据，读回 echo。
async fn trojan_roundtrip(
    server_port: u16,
    sni: &str,
    password: &str,
    echo_port: u16,
    data: &[u8],
) -> Option<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let config = rust_tunnel_common::create_insecure_client_config().ok()?;
    let connector = tokio_rustls::TlsConnector::from(config);
    let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{server_port}"))
        .await
        .ok()?;
    let server_name = rustls::pki_types::ServerName::try_from(sni.to_string()).ok()?;
    let mut tls = connector.connect(server_name, stream).await.ok()?;
    tls.write_all(&build_trojan_header(password, [127, 0, 0, 1], echo_port))
        .await
        .ok()?;
    tls.write_all(data).await.ok()?;
    let mut resp = vec![0u8; data.len()];
    tls.read_exact(&mut resp).await.ok()?;
    Some(resp)
}

fn tls_http_rule(listen: &str, domain: &str, backend_port: u16) -> ProxyRule {
    ProxyRule {
        id: "r1".into(),
        name: "r1".into(),
        rule_type: RuleType::Http,
        listen: listen.into(),
        domains: vec![domain.into()],
        routes: vec![Route {
            path: "/".into(),
            backends: vec![Backend {
                kind: BackendKind::Direct,
                addr: format!("127.0.0.1:{backend_port}"),
                client_name: None,
                weight: 100,
                protocol: BackendProtocol::Http1,
                scheme: BackendScheme::Http,
            }],
            load_balancing: LoadBalancing::RoundRobin,
        }],
        tls: Some(ProxyTlsConfig {
            enabled: true,
            acme: false,
            domain: Some(domain.into()),
        }),
        enabled: true,
        created_at: None,
        cert_status: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_mode_e2e_sni_dispatch() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let tempdir = TempDir::new().unwrap();
        let db = Database::new(tempdir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let mut state = control::ServerState::with_db(db);

        // cert_manager：trojan 域名 + 反代域名各一张证书
        let cert_dir = tempdir.path().join("certs");
        let cm = Arc::new(CertificateManager::new(cert_dir.to_str().unwrap()));
        add_test_cert(&cm, "trojan.test.local").await;
        add_test_cert(&cm, "proxy.test.local").await;
        state.set_cert_manager(cm.clone());
        state.proxy_state.set_cert_manager(cm);
        state.tls.cert_path = tempdir.path().join("tls/cert.pem").to_string_lossy().into();
        state.tls.key_path = tempdir.path().join("tls/key.pem").to_string_lossy().into();

        let echo_port = start_echo().await;
        let backend_port = start_http_backend("hello-from-backend").await;

        // 反代 TLS HTTP 规则与 trojan 同端口
        let port = reserve_port();
        let listen_addr = format!("127.0.0.1:{port}");
        state.proxy_state.rules.lock().await.insert(
            "r1".into(),
            tls_http_rule(&listen_addr, "proxy.test.local", backend_port),
        );
        state
            .proxy_state
            .reconcile_http_listener(&listen_addr)
            .await
            .unwrap();

        // 应用 trojan 配置 → 应进入共享模式
        let tj = TrojanDynamicConfig {
            enabled: true,
            port,
            password: "secret".to_string(),
            fallback: "127.0.0.1:1".to_string(),
            domain: "trojan.test.local".to_string(),
        };
        trojan_runtime::apply_trojan_config(&state, &tj)
            .await
            .unwrap();
        {
            let rt = state.proxy_ports.trojan_runtime.read().await;
            assert!(rt.shared, "应进入共享模式");
            assert_eq!(rt.cert_source.as_deref(), Some("acme_exact"));
        }

        // SNI = trojan 域名 → trojan 握手 + echo 代理（wait_until 处理监听器就绪）
        let echo_data = b"ping-through-trojan";
        let resp = wait_until("trojan handshake over shared port", || async {
            trojan_roundtrip(port, "trojan.test.local", "secret", echo_port, echo_data).await
        })
        .await
        .expect("trojan roundtrip via shared port");
        assert_eq!(resp, echo_data);

        // SNI = 反代域名 → 走 HTTP Host 路由到 mock backend
        let http_client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .resolve(
                "proxy.test.local",
                std::net::SocketAddr::new(
                    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                    port,
                ),
            )
            .build()
            .unwrap();
        let body = wait_until("https via shared port", || {
            let c = http_client.clone();
            let url = format!("https://proxy.test.local:{port}/x");
            async move {
                let resp = c.get(&url).send().await.ok()?;
                if resp.status().is_success() {
                    resp.text().await.ok()
                } else {
                    None
                }
            }
        })
        .await
        .expect("https via shared port");
        assert_eq!(body, "hello-from-backend");

        // 清理：停 trojan 分流 + 移除反代规则停监听器
        trojan_runtime::apply_trojan_config(
            &state,
            &TrojanDynamicConfig {
                enabled: false,
                port,
                password: String::new(),
                fallback: String::new(),
                domain: String::new(),
            },
        )
        .await
        .unwrap();
        state.proxy_state.rules.lock().await.clear();
        state
            .proxy_state
            .reconcile_http_listener(&listen_addr)
            .await
            .unwrap();
    })
    .await;
    result.expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn standalone_mode_self_signed_fallback() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let tempdir = TempDir::new().unwrap();
        let db = Database::new(tempdir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let mut state = control::ServerState::with_db(db);

        // cert_manager 为空（无任何证书）→ 自签名回退
        let cert_dir = tempdir.path().join("certs");
        let cm = Arc::new(CertificateManager::new(cert_dir.to_str().unwrap()));
        state.set_cert_manager(cm.clone());
        state.proxy_state.set_cert_manager(cm);
        state.tls.cert_path = tempdir.path().join("tls/cert.pem").to_string_lossy().into();
        state.tls.key_path = tempdir.path().join("tls/key.pem").to_string_lossy().into();

        let echo_port = start_echo().await;
        let port = reserve_port();
        let tj = TrojanDynamicConfig {
            enabled: true,
            port,
            password: "secret".to_string(),
            fallback: "127.0.0.1:1".to_string(),
            domain: "none.example.org".to_string(),
        };
        trojan_runtime::apply_trojan_config(&state, &tj)
            .await
            .unwrap();
        {
            let rt = state.proxy_ports.trojan_runtime.read().await;
            assert!(!rt.shared, "无反代规则应为独立监听");
            assert_eq!(
                rt.cert_source.as_deref(),
                Some("self_signed"),
                "无匹配 ACME 证书应回退自签名"
            );
        }

        // 独立监听器可用：trojan 握手 + echo
        let data = b"standalone-self-signed";
        let resp = wait_until("trojan standalone roundtrip", || async {
            trojan_roundtrip(port, "none.example.org", "secret", echo_port, data).await
        })
        .await
        .expect("standalone trojan roundtrip");
        assert_eq!(resp, data);

        // 清理
        if let Some(tx) = state.proxy_ports.trojan_listener_abort.write().await.take() {
            let _ = tx.send(true);
        };
    })
    .await;
    result.expect("test timed out");
}
