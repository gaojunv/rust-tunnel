//! AI agent workbench: server-side agent loop, tool execution over tunnel.

pub mod compact;
pub mod executor;
pub mod runner;
pub mod session;
pub mod sse;
pub mod tools;

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::server::client_registry::ClientRegistry;
use crate::server::db::Database;

/// Shared agent state, hung on `ServerState`.
#[derive(Clone)]
pub struct AgentState {
    pub registry: ClientRegistry,
    pub db: Database,
    /// Per-workspace serialization of command execution (git state safety)
    workspace_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Per-session serialization of agent turns：同一 session 的多个 WS 连接
    /// （多标签页/重连叠旧连接未断）各自跑 turn 会并发写库、消息交错、
    /// tool_call_id 失配，回合级互斥防止上下文被写坏。
    session_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl AgentState {
    pub fn new(registry: ClientRegistry, db: Database) -> Self {
        Self {
            registry,
            db,
            workspace_locks: Arc::new(Mutex::new(HashMap::new())),
            session_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get (or create) the execution mutex for a workspace.
    pub async fn workspace_lock(&self, workspace_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.workspace_locks.lock().await;
        locks
            .entry(workspace_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Get (or create) the turn mutex for a session.
    pub async fn session_lock(&self, session_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.session_locks.lock().await;
        locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}
