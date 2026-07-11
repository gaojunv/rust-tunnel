//! In-process server+client harness. Each `TestHarness::spawn` yields a fully
//! isolated server (fresh tempdir, random ports) that lives until drop.

use rust_tunnel::client::config::{ClientConfig, ForwardRule};
use rust_tunnel::client::control::run_client;
use rust_tunnel::server::auth::AuthConfig;
use rust_tunnel::server::{api, control, Database, ServerConfig};
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
    pub client_auth_token: Option<String>,
    /// Number of tunneled ports the harness should pre-reserve for tests.
    pub exposed_port_count: usize,
}

pub struct TestHarness {
    _tempdir: TempDir,
    pub control_addr: SocketAddr,
    pub api_base: String,
    pub exposed_ports: Vec<u16>,
    pub admin_password: Option<String>,
    pub client_auth_token: Option<String>,
    pub tls: bool,
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
            client_auth_token: opts.client_auth_token.clone(),
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
        };

        let db = Database::new(&config.db_path).await.expect("db new");
        let state = control::ServerState::with_db(db);
        let auth_config = AuthConfig::new(config.admin_password.clone(), config.jwt_secret.clone());

        let control_state = state.clone();
        let control_config = config.clone();
        let server_task = tokio::spawn(async move {
            let _ = control::run_server(control_config, control_state).await;
        });

        let api_state = state.clone();
        let api_addr_clone = api_addr.clone();
        let api_task = tokio::spawn(async move {
            let _ = api::run_api_server(api_addr_clone, api_state, auth_config).await;
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
            client_auth_token: opts.client_auth_token,
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
    pub fn spawn_client(&mut self, forwards: Vec<ForwardRule>) -> tokio::task::AbortHandle {
        let client_config = ClientConfig {
            server: self.control_addr.to_string(),
            forwards: forwards
                .iter()
                .map(|f| format!("{}:{}", f.remote_port, f.local_addr))
                .collect(),
            mesh: None,
            mesh_name: None,
            mesh_services: Vec::new(),
            auth_token: self.client_auth_token.clone(),
            tls: self.tls,
            tls_server_name: Some("localhost".to_string()),
            tls_insecure: true,
            log: "warn".to_string(),
        };

        let handle = tokio::spawn(async move {
            let _ = run_client(client_config, forwards).await;
        });
        let abort = handle.abort_handle();
        self.client_tasks.push(handle);
        abort
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
                let n = body.as_array().map(|a| a.len()).unwrap_or(0);
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
