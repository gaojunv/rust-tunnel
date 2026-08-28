//! 集成测试：通过 API 修改 Shadowsocks / Trojan 配置后，
//! 使用同一数据库重启服务器必须保持新配置（端口不回退到旧值）。
//!
//! 复现路径与 src/bin/server.rs 的启动流程一致：
//! `Database::new` → `DynamicConfig::load_or_seed` → `set_dynamic_config` → 启动 API。
//! “重启” = 停掉任务后用同一 db_path 与同一配置文件再走一遍该流程。

#[path = "common/mod.rs"]
mod common;

use common::api_client::ApiClient;
use common::wait_until;
use reqwest::StatusCode;
use rust_tunnel_server::auth::AuthConfig;
use rust_tunnel_server::dynamic_config::DynamicConfig;
use rust_tunnel_server::{api, control_plane as control, Database, ServerConfig};
use std::net::TcpListener as StdTcpListener;
use std::time::Duration;
use tempfile::TempDir;
use tokio::task::JoinHandle;

/// Reserve an ephemeral TCP port on 127.0.0.1 by binding and dropping a listener.
fn reserve_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .expect("reserve port")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// 与 tests/common/harness.rs 相同的 ServerConfig 默认值（TLS-off、无认证），
/// 只参数化 db_path / api 端口。
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

/// 一次“进程启动”：复刻 src/bin/server.rs 中与本测试相关的启动序列。
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

/// 模拟进程退出：停掉 API 任务并断开数据库连接。
async fn stop_server(server: RunningServer) {
    server.task.abort();
    drop(server.db);
}

#[tokio::test(flavor = "multi_thread")]
async fn shadowsocks_config_survives_restart() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let tempdir = TempDir::new().expect("tempdir");
        let db_path = tempdir.path().join("rust-tunnel.db");
        let db_path = db_path.to_string_lossy().to_string();

        // old_port < new_port：修复前 load 按 ORDER BY port 取第一行，必然捞回旧端口。
        let p1 = reserve_port();
        let p2 = reserve_port();
        let (old_port, new_port) = (p1.min(p2), p1.max(p2));
        assert_ne!(old_port, new_port);

        // 配置文件里是旧端口（首次启动会播种进 DB；用户改配置不会回写 TOML）
        let mut config = test_config(&db_path, reserve_port());
        config.ss_enabled = true;
        config.ss_port = Some(old_port);
        config.ss_cipher = Some("aes-256-gcm".to_string());
        config.ss_password = Some("initial-password".to_string());

        // 第一次启动：播种旧端口
        let server = start_server(&config).await;
        let seeded = server.dynamic_config.ss.as_ref().expect("ss seeded");
        assert_eq!(seeded.port, old_port, "首次启动应从配置文件播种旧端口");

        // 经 API 修改端口/密码
        let api_client = ApiClient::new(&server.api_base);
        let (status, body) = api_client
            .post_json(
                "/api/shadowsocks",
                serde_json::json!({
                    "enabled": true,
                    "port": new_port,
                    "cipher": "chacha20-ietf-poly1305",
                    "password": "updated-password",
                }),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "update shadowsocks should succeed, body={body:?}"
        );

        // 用同一 DB 重启（第二次启动换新的 API 端口，避免旧任务尚未释放监听）
        stop_server(server).await;
        config.api_addr = format!("127.0.0.1:{}", reserve_port());
        let restarted = start_server(&config).await;

        // 重启后必须保持 API 修改的新配置
        let ss = restarted
            .dynamic_config
            .ss
            .as_ref()
            .expect("ss config after restart");
        assert_eq!(ss.port, new_port, "重启后 SS 端口应保持新值而不是回退");
        assert_eq!(ss.cipher, "chacha20-ietf-poly1305");
        assert_eq!(ss.password, "updated-password");
        assert!(ss.enabled);
        stop_server(restarted).await;
    })
    .await;
    result.expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn trojan_config_survives_restart() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let tempdir = TempDir::new().expect("tempdir");
        let db_path = tempdir.path().join("rust-tunnel.db");
        let db_path = db_path.to_string_lossy().to_string();

        let p1 = reserve_port();
        let p2 = reserve_port();
        let (old_port, new_port) = (p1.min(p2), p1.max(p2));
        assert_ne!(old_port, new_port);

        let mut config = test_config(&db_path, reserve_port());
        config.trojan_enabled = true;
        config.trojan_port = Some(old_port);
        config.trojan_password = Some("initial-trojan-password".to_string());

        let server = start_server(&config).await;
        let seeded = server
            .dynamic_config
            .trojan
            .as_ref()
            .expect("trojan seeded");
        assert_eq!(seeded.port, old_port, "首次启动应从配置文件播种旧端口");

        // 经 API 修改端口。Trojan 启用监听需要证书（测试环境没有 cert_manager），
        // 这里以 enabled:false 提交——持久化路径与 enabled 无关。
        let api_client = ApiClient::new(&server.api_base);
        let (status, body) = api_client
            .post_json(
                "/api/trojan",
                serde_json::json!({
                    "enabled": false,
                    "port": new_port,
                    "password": "updated-trojan-password",
                    "fallback": "127.0.0.1:8080",
                }),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "update trojan should succeed, body={body:?}"
        );

        stop_server(server).await;
        config.api_addr = format!("127.0.0.1:{}", reserve_port());
        let restarted = start_server(&config).await;

        let trojan = restarted
            .dynamic_config
            .trojan
            .as_ref()
            .expect("trojan config after restart");
        assert_eq!(
            trojan.port, new_port,
            "重启后 Trojan 端口应保持新值而不是回退"
        );
        assert_eq!(trojan.password, "updated-trojan-password");
        stop_server(restarted).await;
    })
    .await;
    result.expect("test timed out");
}
