//! In-process server+client harness. Each `TestHarness::spawn` yields a fully
//! isolated server (fresh tempdir, random ports) that lives until drop.

use rust_tunnel_client::config::ClientConfig;
use rust_tunnel_client::control::run_client;
use rust_tunnel_server::auth::AuthConfig;
use rust_tunnel_server::reverse_proxy::{
    tcp_proxy::TcpProxy, Backend, BackendKind, BackendProtocol, BackendScheme, ReverseProxyState,
};
use rust_tunnel_server::{api, control_plane as control, Database, ServerConfig};
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use tempfile::TempDir;
use tokio::task::JoinHandle;

use super::api_client::ApiClient;
use super::retry::wait_until;

/// Options for spawning a test server. Defaults are TLS-off, no auth.
#[derive(Default, Clone)]
pub struct HarnessOpts {
    pub tls: bool,
    pub admin_password: Option<String>,
    /// Password used by test clients to authenticate with the server.
    pub client_password: Option<String>,
    /// Default client name used by `spawn_client`.
    pub client_name: Option<String>,
    /// Number of tunneled ports the harness should pre-reserve for tests.
    pub exposed_port_count: usize,
}

pub struct TestHarness {
    _tempdir: TempDir,
    pub control_addr: SocketAddr,
    pub api_base: String,
    pub exposed_ports: Vec<u16>,
    pub admin_password: Option<String>,
    pub client_password: Option<String>,
    pub tls: bool,
    /// Clone of the server's `ReverseProxyState`, for starting tunnel listeners.
    pub proxy_state: ReverseProxyState,
    /// Clone of the server's `ServerState`, for starting SS/Trojan listeners
    /// that share the same `StatsCollector` as the API.
    pub server_state: control::ServerState,
    server_task: JoinHandle<()>,
    api_task: JoinHandle<()>,
    client_tasks: Vec<JoinHandle<()>>,
}

/// Reserve N ephemeral TCP ports on 127.0.0.1 by binding and dropping listeners.
/// There is a small TOCTOU window, but each test uses a fresh set of ports so
/// contention only matters when the CI runner is under massive parallel load.
fn reserve_ports(n: usize) -> Vec<u16> {
    (0..n)
        .map(|_| {
            let l = StdTcpListener::bind("127.0.0.1:0").expect("reserve port");
            l.local_addr().expect("local_addr").port()
        })
        .collect()
}

impl TestHarness {
    /// Spawn a fresh server and wait until `/api/health` returns 200.
    pub async fn spawn(opts: HarnessOpts) -> Self {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let data_dir = tempdir.path().to_path_buf();
        std::fs::create_dir_all(data_dir.join("tls")).expect("tls dir");

        let mut ports = reserve_ports(2 + opts.exposed_port_count);
        let control_port = ports.remove(0);
        let api_port = ports.remove(0);
        let exposed_ports: Vec<u16> = ports;

        let control_addr: SocketAddr = format!("127.0.0.1:{control_port}").parse().unwrap();
        let api_addr = format!("127.0.0.1:{api_port}");
        let api_base = format!("http://{api_addr}");

        let db_path = data_dir.join("rust-tunnel.db");
        let cert_path = data_dir.join("tls/cert.pem");
        let key_path = data_dir.join("tls/key.pem");

        let config = ServerConfig {
            control_addr: control_addr.to_string(),
            api_addr: api_addr.clone(),
            admin_password: opts.admin_password.clone(),
            jwt_secret: Some("test-jwt-secret-do-not-use-in-prod".to_string()),
            client_auth_token: opts.client_password.clone(),
            tls: opts.tls,
            tls_cert: cert_path.to_string_lossy().to_string(),
            tls_key: key_path.to_string_lossy().to_string(),
            log: "warn".to_string(),
            db_path: db_path.to_string_lossy().to_string(),
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
            // Reverse Proxy defaults
            reverse_proxy_enabled: false,
            reverse_proxy_max_connections: 10000,
            reverse_proxy_connection_timeout: 30,
            reverse_proxy_buffer_size: 8192,
            // API TLS defaults
            api_tls: false,
            api_domain: None,
            // ACME defaults
            acme_enabled: false,
            acme_server_url: "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            acme_cert_dir: "./data/certs".to_string(),
            acme_auto_renew: true,
            acme_renewal_check_interval: 24,
            acme_renewal_days_before_expiry: 30,
            acme_email: None,
            acme_tos_agreed: false,
        };

        let db = Database::new(&config.db_path).await.expect("db new");
        // rag 注入块需要可变绑定；非 rag 构建下 mut 无消费者 → allow。
        #[cfg_attr(not(feature = "rag"), allow(unused_mut))]
        let mut state = control::ServerState::with_db(db);
        // Seed a server_auth row so clients can authenticate.
        let client_pw = opts
            .client_password
            .clone()
            .unwrap_or_else(|| "test-password".to_string());
        state
            .db()
            .expect("db")
            .save_server_auth(&client_pw)
            .await
            .expect("seed server_auth");
        // Wire up the ClientConnector so reverse-proxy rules can reach tunnel clients.
        state.wire_up_client_connector().await;
        // Initialize LLM gateway state (mirrors src/bin/server.rs startup):
        // no gateway rule exists yet → disabled; fixed test master key enables
        // provider API key encryption at rest.
        state
            .init_llm_state(
                state.db().cloned(),
                Some([42u8; 32]),
                // RAG 向量数据根目录：与生产一致，用 DB 所在目录（VectorStore 内部再拼 rag/<kb_id>）
                std::path::Path::new(&config.db_path)
                    .parent()
                    .unwrap_or(std::path::Path::new(".")),
                state.dynamic_config.clone(),
            )
            .await;
        // AI 记忆体运行时注入（镜像 src/bin/server.rs 生产路径）：克隆
        // `LlmState.rag_store` 同一实例构造 MemoryState（严禁二次 VectorStore::new
        // ——双 EdgeShard 对同一目录各自 flush 会竞态 panic），经 with_memory 挂到
        // AgentState（顺带注入 ACP 桥）。非 rag 构建 cfg 掉，行为与旧 harness 一致。
        #[cfg(feature = "rag")]
        {
            let llm = state.llm_state.read().await.clone();
            if let Some(llm) = llm {
                let memory = rust_tunnel_server::agent::memory::MemoryState::new(
                    state.db().cloned().expect("db present"),
                    llm.rag_store.clone(),
                    llm.cipher.clone(),
                    (*llm).clone(),
                );
                let wiki = rust_tunnel_server::agent::wiki::WikiState::new(
                    state.db().cloned().expect("db present"),
                    (*llm).clone(),
                );
                if let Some(agent) = state.agent_state.take() {
                    state.agent_state = Some(agent.with_memory(memory).with_wiki(wiki));
                }
            }
        }
        let proxy_state = state.proxy_state.clone();

        let auth_config = AuthConfig::new(config.admin_password.clone(), config.jwt_secret.clone());

        let control_state = state.clone();
        let control_config = config.clone();
        let server_task = tokio::spawn(async move {
            let _ = control::run_server(control_config, control_state, None).await;
        });

        let api_state = state.clone();
        let api_addr_clone = api_addr.clone();
        let api_task = tokio::spawn(async move {
            let _ = api::run_api_server(api_addr_clone, api_state, auth_config, None).await;
        });

        // Wait for API health so tests know the server is ready.
        let health_base = api_base.clone();
        let health_result = wait_until("api health", || {
            let base = health_base.clone();
            async move {
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_millis(200))
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
        .await;

        if health_result.is_err() {
            // Surface whichever background task died, so a bind failure
            // doesn't masquerade as a slow start.
            let server_finished = server_task.is_finished();
            let api_finished = api_task.is_finished();
            panic!(
                "api never became healthy (server_task.is_finished={server_finished}, api_task.is_finished={api_finished})"
            );
        }

        Self {
            _tempdir: tempdir,
            control_addr,
            api_base,
            exposed_ports,
            admin_password: opts.admin_password,
            client_password: opts.client_password,
            proxy_state,
            server_state: state,
            tls: opts.tls,
            server_task,
            api_task,
            client_tasks: Vec::new(),
        }
    }

    /// Spawn a rust-tunnel client that connects back to this harness.
    /// Returns an `AbortHandle` that the caller can use to kill the client
    /// mid-test (e.g. to test reconnect). The harness itself also holds an
    /// abort handle for cleanup on drop.
    ///
    /// In the v2 protocol, clients do not specify forward rules — they just
    /// register with a name + password. Tunnel listeners are set up separately
    /// via `start_tcp_tunnel`.
    pub fn spawn_client(&mut self, client_name: Option<&str>) -> tokio::task::AbortHandle {
        self.spawn_client_with_agent(client_name, false)
    }

    /// Spawn a client with the agent executor enabled (`enable_agent = true`),
    /// so the server can run `ClientRegistry::agent_exec` commands against it
    /// over the real control channel.
    pub fn spawn_agent_client(&mut self, client_name: Option<&str>) -> tokio::task::AbortHandle {
        self.spawn_client_with_agent(client_name, true)
    }

    fn spawn_client_with_agent(
        &mut self,
        client_name: Option<&str>,
        enable_agent: bool,
    ) -> tokio::task::AbortHandle {
        let name = client_name
            .map(|s| s.to_string())
            .unwrap_or_else(|| "test-client".to_string());
        let password = self
            .client_password
            .clone()
            .unwrap_or_else(|| "test-password".to_string());
        let client_config = ClientConfig {
            server: self.control_addr.to_string(),
            name: Some(name),
            password,
            mesh: None,
            mesh_name: None,
            mesh_services: Vec::new(),
            tls: self.tls,
            tls_server_name: Some("localhost".to_string()),
            tls_insecure: true,
            enable_agent,
            agent_pty_port: rust_tunnel_client::pty::DEFAULT_PTY_PORT,
            log: "warn".to_string(),
        };

        let handle = tokio::spawn(async move {
            let _ = run_client(client_config).await;
        });
        let abort = handle.abort_handle();
        self.client_tasks.push(handle);
        abort
    }

    /// Start a TCP tunnel listener on `listen_port` that forwards to
    /// `target_addr` (a host:port reachable from the named client).
    ///
    /// Uses the server's `TcpProxy` + `ClientConnector` so traffic flows
    /// through the control-channel tunnel.
    pub async fn start_tcp_tunnel(&self, listen_port: u16, target_addr: &str, client_name: &str) {
        let backend = Backend {
            kind: BackendKind::Client,
            addr: target_addr.to_string(),
            client_name: Some(client_name.to_string()),
            weight: 100,
            protocol: BackendProtocol::Http1,
            scheme: BackendScheme::Http,
        };
        let tcp_proxy = TcpProxy::new(self.proxy_state.clone());
        let addr: std::net::SocketAddr = format!("127.0.0.1:{listen_port}")
            .parse()
            .expect("parse listen addr");
        tcp_proxy
            .start(addr, backend, format!("test-tunnel-{listen_port}"))
            .await
            .expect("start tcp tunnel");
    }

    /// Convenience: assert that at least `at_least` clients are registered via /api/clients.
    /// `api` must already be authenticated if the harness has a password.
    pub async fn wait_client_count(&self, api: &ApiClient, at_least: usize) -> Result<(), String> {
        // Track the last non-2xx status so an unauthenticated caller against
        // a password-protected harness gets a diagnostic message instead of
        // a generic "client registered" timeout.
        let last_status = std::sync::Arc::new(std::sync::Mutex::new(None::<reqwest::StatusCode>));
        let last_status_clone = last_status.clone();
        let result = wait_until("client registered", || {
            let last_status = last_status_clone.clone();
            async move {
                let (status, body) = api.get_json("/api/clients").await;
                if !status.is_success() {
                    *last_status.lock().unwrap() = Some(status);
                    return None;
                }
                let n = body
                    .get("clients")
                    .and_then(|c| c.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                if n >= at_least {
                    Some(())
                } else {
                    None
                }
            }
        })
        .await;

        match result {
            Ok(v) => Ok(v),
            Err(msg) => {
                let seen = *last_status.lock().unwrap();
                if let Some(st) = seen {
                    Err(format!(
                        "{msg} (last /api/clients status was {st}; did you forget to login?)"
                    ))
                } else {
                    Err(msg)
                }
            }
        }
    }

    pub fn api_client(&self) -> ApiClient {
        ApiClient::new(self.api_base.clone())
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        for t in self.client_tasks.drain(..) {
            t.abort();
        }
        self.server_task.abort();
        self.api_task.abort();
    }
}
