//! Registry of currently-connected clients, keyed by name.
//!
//! See docs/superpowers/specs/2026-07-18-web-managed-client-routing-design.md §2.6

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use tracing::{debug, warn};

use crate::common::ControlMessage;
use crate::server::db::Database;

pub type ControlSender = mpsc::Sender<ControlMessage>;

/// Outcome of an `OpenTunnel` dial, delivered via a one-shot channel from the
/// main control loop to whoever asked (`ClientConnector` in a later task).
#[derive(Debug)]
pub enum TunnelOpenOutcome {
    Ok,
    Failed(String),
}

/// Per-tunnel state on the server side, indexed by `connection_id`.
///
/// The `open_result` one-shot is populated when the control loop receives a
/// `TunnelOpenResult` message; the reader task consumes it via `take()`.
#[derive(Debug)]
pub struct ActiveTunnelConnection {
    /// Server main loop pushes inbound `Data` payloads here; the
    /// `ClientTunnelStream` consumes them via its `Receiver` half.
    pub inbound: mpsc::Sender<Vec<u8>>,
    /// Filled once when `TunnelOpenResult` arrives. `take()` before use.
    pub open_result: Option<oneshot::Sender<TunnelOpenOutcome>>,
}

#[derive(Debug)]
pub enum RegisterError {
    AuthFailed,
    DbError(sqlx::Error),
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AuthFailed => write!(f, "authentication failed"),
            Self::DbError(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for RegisterError {}

#[derive(Debug)]
pub struct ClientEntry {
    pub name: String,
    pub hostname: Option<String>,
    pub client_version: Option<String>,
    pub control_sender: ControlSender,
    pub connected_at: DateTime<Utc>,
    /// 最后一次心跳时间戳（微秒），用于超时检测（当前 Task 未使用，Task 12 心跳循环会更新）
    pub last_ping_micros: AtomicU64,
    /// Active tunnels for this client (cid -> per-conn state). Populated by
    /// the connector code in a later task; guarded by a tokio mutex so it can
    /// be held across awaits during connection setup / teardown.
    pub active_connections: Mutex<HashMap<u64, ActiveTunnelConnection>>,
    /// Pending agent exec requests, keyed by request_id; resolved by
    /// `deliver_agent_response` when the client's AgentExecResponse arrives.
    pub agent_pending: Mutex<HashMap<String, oneshot::Sender<crate::common::AgentResult>>>,
}

/// Global registry of online clients, keyed by name. Cloneable: internal state
/// is `Arc`-shared.
#[derive(Clone)]
pub struct ClientRegistry {
    entries: Arc<RwLock<HashMap<String, Arc<ClientEntry>>>>,
    db: Database,
    /// 统一统计采集器（None = 不记录，测试默认）
    stats_collector: Option<crate::server::stats::StatsCollector>,
}

impl ClientRegistry {
    pub fn new(db: Database) -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            db,
            stats_collector: None,
        }
    }

    /// 挂上统一统计采集器；之后的 open_tunnel 会按 client_name 记录
    /// 连接数与流量。
    pub fn set_stats_collector(&mut self, collector: crate::server::stats::StatsCollector) {
        self.stats_collector = Some(collector);
    }

    /// Register a client. On success returns the entry; if a same-name entry
    /// is already online it is kicked with `Disconnect{"replaced"}`.
    ///
    /// # Errors
    /// - `RegisterError::AuthFailed` — password does not match the stored token.
    /// - `RegisterError::DbError` — underlying sqlx failure while loading/persisting.
    pub async fn register(
        &self,
        name: &str,
        hostname: Option<String>,
        client_version: Option<String>,
        password: &str,
        sender: ControlSender,
    ) -> Result<Arc<ClientEntry>, RegisterError> {
        // 1. Auth
        let stored = self
            .db
            .load_server_auth()
            .await
            .map_err(RegisterError::DbError)?;
        match stored {
            Some(token) if token == password => {}
            _ => return Err(RegisterError::AuthFailed),
        }

        // 2. Kick old same-name entry (best-effort: send Disconnect + drop entry).
        {
            let mut entries = self.entries.write().await;
            if let Some(old) = entries.remove(name) {
                let old_sender = old.control_sender.clone();
                tokio::spawn(async move {
                    let _ = old_sender
                        .send(ControlMessage::Disconnect {
                            reason: "replaced".into(),
                        })
                        .await;
                });
            }
        }

        // 3. Persist row (UPSERT).
        self.db
            .upsert_client(name, hostname.as_deref())
            .await
            .map_err(RegisterError::DbError)?;

        // 4. Insert new entry.
        let entry = Arc::new(ClientEntry {
            name: name.to_string(),
            hostname,
            client_version,
            control_sender: sender,
            connected_at: Utc::now(),
            last_ping_micros: AtomicU64::new(0),
            active_connections: Mutex::new(HashMap::new()),
            agent_pending: Mutex::new(HashMap::new()),
        });
        self.entries
            .write()
            .await
            .insert(name.to_string(), entry.clone());
        debug!("client '{}' registered", name);
        Ok(entry)
    }

    #[must_use]
    pub async fn get(&self, name: &str) -> Option<Arc<ClientEntry>> {
        self.entries.read().await.get(name).cloned()
    }

    #[must_use]
    pub async fn list_online(&self) -> Vec<Arc<ClientEntry>> {
        self.entries.read().await.values().cloned().collect()
    }

    /// Drop a client entry from the registry and send `Disconnect { reason }`
    /// on its control channel. Best-effort: if the channel is already closed
    /// the send silently fails.
    pub async fn disconnect(&self, name: &str, reason: &str) {
        let removed = self.entries.write().await.remove(name);
        if let Some(entry) = removed {
            let reason = reason.to_string();
            let sender = entry.control_sender.clone();
            let _ = sender.send(ControlMessage::Disconnect { reason }).await;
            debug!("client '{}' disconnected", name);
        } else {
            warn!("disconnect() called for unknown client '{}'", name);
        }
    }

    #[must_use]
    pub fn db(&self) -> &Database {
        &self.db
    }

    /// Initiate a tunneled dial from a specific client to `target_addr`.
    /// Returns a duplex stream ready for the reverse-proxy handler to read/write.
    /// Fails if the client is offline or the client's dial fails within 5 s.
    ///
    /// # Errors
    /// - `NotConnected` — client is not online
    /// - `BrokenPipe` — control channel closed while sending OpenTunnel
    /// - `TimedOut` — client did not respond within 5 seconds
    /// - `ConnectionRefused` — client's dial to target_addr failed
    pub async fn open_tunnel(
        &self,
        client_name: &str,
        target_addr: &str,
    ) -> std::io::Result<crate::server::tunnel_stream::ClientTunnelStream> {
        use std::io::{Error, ErrorKind};
        use std::time::Duration;

        let entry = self.get(client_name).await.ok_or_else(|| {
            Error::new(
                ErrorKind::NotConnected,
                format!("client '{client_name}' offline"),
            )
        })?;

        // Random cid; collision is astronomically unlikely for u64.
        let cid: u64 = rand::random();

        let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
        let (open_tx, open_rx) = tokio::sync::oneshot::channel();
        {
            let mut conns = entry.active_connections.lock().await;
            conns.insert(
                cid,
                ActiveTunnelConnection {
                    inbound: inbound_tx,
                    open_result: Some(open_tx),
                },
            );
        }

        // Send OpenTunnel
        entry
            .control_sender
            .send(ControlMessage::OpenTunnel {
                connection_id: cid,
                target_addr: target_addr.to_string(),
            })
            .await
            .map_err(|_| Error::new(ErrorKind::BrokenPipe, "control channel closed"))?;

        // Await result with 5s timeout
        let outcome = tokio::time::timeout(Duration::from_secs(5), open_rx).await;

        // Handle timeout separately (needs async cleanup)
        let outcome = match outcome {
            Ok(inner) => inner,
            Err(_elapsed) => {
                // Clean up connection entry on timeout
                let mut conns = entry.active_connections.lock().await;
                conns.remove(&cid);
                return Err(Error::new(
                    ErrorKind::TimedOut,
                    "OpenTunnel timed out after 5s",
                ));
            }
        };

        // Handle oneshot cancellation
        let outcome = match outcome {
            Ok(v) => v,
            Err(_) => {
                return Err(Error::new(
                    ErrorKind::BrokenPipe,
                    "open_result channel dropped",
                ));
            }
        };

        match outcome {
            TunnelOpenOutcome::Ok => {
                let stream = crate::server::tunnel_stream::ClientTunnelStream::new(
                    cid,
                    entry.control_sender.clone(),
                    inbound_rx,
                );
                // 挂上统计：tunnel 打开计数 + 后续双向流量按 client_name 累计
                let stream = match &self.stats_collector {
                    Some(collector) => stream.with_stats(collector.clone(), client_name),
                    None => stream,
                };
                Ok(stream)
            }
            TunnelOpenOutcome::Failed(err) => {
                // Clean up connection entry on failure
                let mut conns = entry.active_connections.lock().await;
                conns.remove(&cid);
                Err(Error::new(
                    ErrorKind::ConnectionRefused,
                    format!("client dial failed: {err}"),
                ))
            }
        }
    }

    /// Execute an agent command on a client over the control channel.
    ///
    /// # Errors
    /// - `NotConnected` — client offline
    /// - `BrokenPipe` — control channel closed while sending
    /// - `TimedOut` — no response within `timeout`
    pub async fn agent_exec(
        &self,
        client_name: &str,
        session_id: &str,
        command: crate::common::AgentCommand,
        timeout: std::time::Duration,
    ) -> std::io::Result<crate::common::AgentResult> {
        use std::io::{Error, ErrorKind};

        let entry = self.get(client_name).await.ok_or_else(|| {
            Error::new(
                ErrorKind::NotConnected,
                format!("client '{client_name}' offline"),
            )
        })?;

        let request_id = format!("{:032x}", rand::random::<u128>());
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = entry.agent_pending.lock().await;
            pending.insert(request_id.clone(), tx);
        }

        let send_result = entry
            .control_sender
            .send(ControlMessage::AgentExecRequest {
                session_id: session_id.to_string(),
                request_id: request_id.clone(),
                command,
            })
            .await;
        if send_result.is_err() {
            entry.agent_pending.lock().await.remove(&request_id);
            return Err(Error::new(ErrorKind::BrokenPipe, "control channel closed"));
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err(Error::new(ErrorKind::BrokenPipe, "response channel dropped")),
            Err(_) => {
                entry.agent_pending.lock().await.remove(&request_id);
                Err(Error::new(ErrorKind::TimedOut, "agent exec timed out"))
            }
        }
    }

    /// Route an AgentExecResponse from the control loop to the waiter.
    pub async fn deliver_agent_response(
        &self,
        client_name: &str,
        request_id: &str,
        result: crate::common::AgentResult,
    ) {
        if let Some(entry) = self.get(client_name).await {
            let tx = entry.agent_pending.lock().await.remove(request_id);
            if let Some(tx) = tx {
                let _ = tx.send(result);
            } else {
                debug!("agent response for unknown request_id {}", request_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::ControlMessage;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_register_first_time_success() {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let registry = ClientRegistry::new(db);

        let (tx, _rx) = mpsc::channel(32);
        let entry = registry
            .register(
                "home-nas",
                Some("nas.local".into()),
                Some("0.4.0".into()),
                "secret",
                tx,
            )
            .await
            .expect("register should succeed");
        assert_eq!(entry.name, "home-nas");
        assert_eq!(entry.hostname.as_deref(), Some("nas.local"));
        assert_eq!(entry.client_version.as_deref(), Some("0.4.0"));

        let online = registry.list_online().await;
        assert_eq!(online.len(), 1);
    }

    #[tokio::test]
    async fn test_register_wrong_password() {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let registry = ClientRegistry::new(db);

        let (tx, _rx) = mpsc::channel(32);
        let err = registry
            .register("home-nas", None, None, "wrong", tx)
            .await
            .expect_err("wrong password should fail");
        assert!(matches!(err, RegisterError::AuthFailed));
    }

    #[tokio::test]
    async fn test_register_no_stored_token_rejects() {
        // server_auth row absent → AuthFailed regardless of what client sends
        let db = Database::new(":memory:").await.unwrap();
        let registry = ClientRegistry::new(db);
        let (tx, _rx) = mpsc::channel(32);
        let err = registry
            .register("x", None, None, "anything", tx)
            .await
            .expect_err("no token → AuthFailed");
        assert!(matches!(err, RegisterError::AuthFailed));
    }

    #[tokio::test]
    async fn test_register_duplicate_kicks_old() {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let registry = ClientRegistry::new(db);

        let (tx1, mut rx1) = mpsc::channel(32);
        registry
            .register("home-nas", None, None, "secret", tx1)
            .await
            .unwrap();

        let (tx2, _rx2) = mpsc::channel(32);
        registry
            .register("home-nas", None, None, "secret", tx2)
            .await
            .unwrap();

        let received = tokio::time::timeout(std::time::Duration::from_millis(500), rx1.recv())
            .await
            .expect("timed out waiting for Disconnect on old sender")
            .expect("channel closed");
        match received {
            ControlMessage::Disconnect { reason } => assert_eq!(reason, "replaced"),
            other => panic!("expected Disconnect, got {other:?}"),
        }

        assert_eq!(registry.list_online().await.len(), 1);
    }

    #[tokio::test]
    async fn test_disconnect_removes_entry() {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let registry = ClientRegistry::new(db);

        let (tx, mut rx) = mpsc::channel(32);
        registry
            .register("home-nas", None, None, "secret", tx)
            .await
            .unwrap();
        registry.disconnect("home-nas", "kicked").await;

        let received = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        match received {
            ControlMessage::Disconnect { reason } => assert_eq!(reason, "kicked"),
            _ => panic!("wrong message"),
        }
        assert!(registry.get("home-nas").await.is_none());
    }

    #[tokio::test]
    async fn test_disconnect_unknown_client_is_noop() {
        let db = Database::new(":memory:").await.unwrap();
        let registry = ClientRegistry::new(db);
        // should not panic
        registry.disconnect("ghost", "kicked").await;
        assert!(registry.get("ghost").await.is_none());
    }

    #[tokio::test]
    async fn test_agent_exec_offline_client() {
        let db = Database::new(":memory:").await.unwrap();
        let registry = ClientRegistry::new(db);
        let result = registry
            .agent_exec(
                "ghost",
                "sess",
                crate::common::AgentCommand::GitStatus,
                std::time::Duration::from_secs(1),
            )
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::NotConnected);
    }

    #[tokio::test]
    async fn test_agent_exec_roundtrip() {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let registry = ClientRegistry::new(db);

        let (tx, mut rx) = mpsc::channel(32);
        registry
            .register("nas", None, None, "secret", tx)
            .await
            .unwrap();

        // 模拟客户端：收到 AgentExecRequest 后回 Response
        let registry2 = registry.clone();
        tokio::spawn(async move {
            let msg = rx.recv().await.unwrap();
            match msg {
                ControlMessage::AgentExecRequest {
                    session_id,
                    request_id,
                    ..
                } => {
                    registry2
                        .deliver_agent_response(
                            "nas",
                            &request_id,
                            crate::common::AgentResult::Success,
                        )
                        .await;
                    let _ = session_id;
                }
                other => panic!("expected AgentExecRequest, got {other:?}"),
            }
        });

        let result = registry
            .agent_exec(
                "nas",
                "sess",
                crate::common::AgentCommand::GitPush,
                std::time::Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert!(matches!(result, crate::common::AgentResult::Success));
    }

    #[tokio::test]
    async fn test_agent_exec_timeout() {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let registry = ClientRegistry::new(db);

        let (tx, _rx) = mpsc::channel(32); // 无人消费 → 永远等不到响应
        registry
            .register("nas", None, None, "secret", tx)
            .await
            .unwrap();

        let result = registry
            .agent_exec(
                "nas",
                "sess",
                crate::common::AgentCommand::GitPush,
                std::time::Duration::from_millis(100),
            )
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    }
}
