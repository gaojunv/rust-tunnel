//! Server-side spawner: negotiates process spawn on the client over the control channel.

use std::time::Duration;

use crate::server::control_plane::client_registry::ClientRegistry;

/// 按 agent 类型生成启动命令。agent_path 为 None 时依赖 PATH 查找。
pub fn agent_command(
    agent_type: &str,
    agent_path: Option<&str>,
) -> Result<(String, Vec<String>), String> {
    let path = agent_path.unwrap_or(match agent_type {
        "gemini" => "gemini",
        "claude-code" => "claude-code-acp",
        "opencode" => "opencode",
        other => return Err(format!("unsupported agent type: {other}")),
    });
    let args = match agent_type {
        "gemini" => vec!["--experimental-acp".to_string()],
        "claude-code" => vec![],
        "opencode" => vec!["--acp".to_string()],
        other => return Err(format!("unsupported agent type: {other}")),
    };
    Ok((path.to_string(), args))
}

/// 组装 agent 进程的环境变量（LLM 回环代理地址注入）
pub fn agent_env(port: u16) -> Vec<(String, String)> {
    let base = format!("http://127.0.0.1:{port}");
    vec![
        ("OPENAI_BASE_URL".into(), format!("{base}/v1")),
        ("OPENAI_API_KEY".into(), "tunnel-injected".into()), // 占位，服务端真注入
        ("ANTHROPIC_BASE_URL".into(), base),
        ("ANTHROPIC_AUTH_TOKEN".into(), "tunnel-injected".into()),
    ]
}

#[derive(Clone)]
pub struct AgentSpawner {
    registry: ClientRegistry,
}

impl AgentSpawner {
    pub fn new(registry: ClientRegistry) -> Self {
        Self { registry }
    }

    /// 启动客户端内嵌 LLM 代理，返回回环端口。
    pub async fn start_llm_proxy(
        &self,
        client_id: &str,
        session_id: &str,
        timeout: Duration,
    ) -> Result<u16, String> {
        let msg = self
            .registry
            .spawn_negotiate(
                client_id,
                session_id,
                crate::common::ControlMessage::AgentLlmProxyStart {
                    session_id: session_id.to_string(),
                },
                timeout,
            )
            .await
            .map_err(|e| format!("llm proxy start failed: {e}"))?;
        match msg {
            crate::common::ControlMessage::AgentLlmProxyReady { port, .. } if port > 0 => Ok(port),
            crate::common::ControlMessage::AgentLlmProxyReady { .. } => {
                Err("llm proxy failed to bind".into())
            }
            other => Err(format!("unexpected response: {other:?}")),
        }
    }

    /// spawn agent 进程，env 注入 LLM 代理地址。
    // 8 个参数：每个语义单一，拆 struct 反而绕（brief 指定签名，仿 agent_exec 处理）。
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_agent(
        &self,
        client_id: &str,
        session_id: &str,
        agent_type: &str,
        agent_path: Option<&str>,
        port: u16,
        cwd: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        let (command, args) = agent_command(agent_type, agent_path)?;
        let msg = self
            .registry
            .spawn_negotiate(
                client_id,
                session_id,
                crate::common::ControlMessage::AgentSpawnRequest {
                    session_id: session_id.to_string(),
                    command,
                    args,
                    env: agent_env(port),
                    cwd: Some(cwd.to_string()),
                },
                timeout,
            )
            .await
            .map_err(|e| format!("agent spawn failed: {e}"))?;
        match msg {
            crate::common::ControlMessage::AgentSpawnResponse { success: true, .. } => Ok(()),
            crate::common::ControlMessage::AgentSpawnResponse { error, .. } => {
                Err(error.unwrap_or_else(|| "unknown spawn error".into()))
            }
            other => Err(format!("unexpected response: {other:?}")),
        }
    }

    /// 取某客户端控制通道的 sender（AcpBridge 回发 AgentLlmProxyChunk 用）。
    /// 客户端离线返回 None。
    pub async fn client_control_sender(
        &self,
        client_id: &str,
    ) -> Option<tokio::sync::mpsc::Sender<crate::common::ControlMessage>> {
        self.registry
            .get(client_id)
            .await
            .map(|entry| entry.control_sender.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    use crate::common::ControlMessage;
    use crate::server::db::Database;

    #[test]
    fn test_agent_command_gemini() {
        let (cmd, args) = agent_command("gemini", None).unwrap();
        assert_eq!(cmd, "gemini");
        assert_eq!(args, vec!["--experimental-acp"]);
    }

    #[test]
    fn test_agent_command_custom_path() {
        let (cmd, _) = agent_command("gemini", Some("/opt/gemini/bin/gemini")).unwrap();
        assert_eq!(cmd, "/opt/gemini/bin/gemini");
    }

    #[test]
    fn test_agent_command_unsupported() {
        assert!(agent_command("cursor", None).is_err());
    }

    #[test]
    fn test_agent_env() {
        let env = agent_env(45678);
        let base = env.iter().find(|(k, _)| k == "OPENAI_BASE_URL").unwrap();
        assert_eq!(base.1, "http://127.0.0.1:45678/v1");
    }

    /// 构造一个注册了模拟客户端 + 自动应答协商请求的 registry。
    /// `respond` 闭包把收到的请求转成响应消息。
    async fn mock_registry<F>(respond: F) -> ClientRegistry
    where
        F: FnOnce(ControlMessage) -> ControlMessage + Send + 'static,
    {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let registry = ClientRegistry::new(db);
        let (tx, mut rx) = mpsc::channel(32);
        registry
            .register("nas", None, None, "secret", tx)
            .await
            .unwrap();
        let registry2 = registry.clone();
        tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            let sid = match &req {
                ControlMessage::AgentSpawnRequest { session_id, .. } => session_id.clone(),
                ControlMessage::AgentLlmProxyStart { session_id } => session_id.clone(),
                other => panic!("unexpected request: {other:?}"),
            };
            registry2.resolve_spawn_pending(&sid, respond(req)).await;
        });
        registry
    }

    #[tokio::test]
    async fn test_start_llm_proxy_success() {
        let registry = mock_registry(|_req| ControlMessage::AgentLlmProxyReady {
            session_id: "sess-1".into(),
            port: 45678,
        })
        .await;
        let spawner = AgentSpawner::new(registry);
        let port = spawner
            .start_llm_proxy("nas", "sess-1", Duration::from_secs(2))
            .await
            .expect("llm proxy should start");
        assert_eq!(port, 45678);
    }

    #[tokio::test]
    async fn test_start_llm_proxy_bind_failure() {
        let registry = mock_registry(|_req| ControlMessage::AgentLlmProxyReady {
            session_id: "sess-1".into(),
            port: 0,
        })
        .await;
        let spawner = AgentSpawner::new(registry);
        let err = spawner
            .start_llm_proxy("nas", "sess-1", Duration::from_secs(2))
            .await
            .expect_err("port 0 should be a failure");
        assert!(err.contains("failed to bind"), "err: {err}");
    }

    #[tokio::test]
    async fn test_start_llm_proxy_unexpected_response() {
        let registry = mock_registry(|_req| ControlMessage::AgentSpawnResponse {
            session_id: "sess-1".into(),
            success: true,
            error: None,
        })
        .await;
        let spawner = AgentSpawner::new(registry);
        let err = spawner
            .start_llm_proxy("nas", "sess-1", Duration::from_secs(2))
            .await
            .expect_err("wrong response type should error");
        assert!(err.contains("unexpected response"), "err: {err}");
    }

    #[tokio::test]
    async fn test_start_llm_proxy_offline_client() {
        let db = Database::new(":memory:").await.unwrap();
        let registry = ClientRegistry::new(db);
        let spawner = AgentSpawner::new(registry);
        let err = spawner
            .start_llm_proxy("ghost", "sess-1", Duration::from_millis(100))
            .await
            .expect_err("offline client should error");
        assert!(err.contains("llm proxy start failed"), "err: {err}");
    }

    #[tokio::test]
    async fn test_spawn_agent_success() {
        let registry = mock_registry(|req| match req {
            ControlMessage::AgentSpawnRequest {
                session_id,
                command,
                args,
                env,
                cwd,
            } => {
                assert_eq!(command, "gemini");
                assert_eq!(args, vec!["--experimental-acp"]);
                assert_eq!(cwd.as_deref(), Some("/workspace"));
                // env 注入 LLM 代理地址
                let base = env
                    .iter()
                    .find(|(k, _)| k == "OPENAI_BASE_URL")
                    .expect("OPENAI_BASE_URL injected");
                assert_eq!(base.1, "http://127.0.0.1:45678/v1");
                ControlMessage::AgentSpawnResponse {
                    session_id,
                    success: true,
                    error: None,
                }
            }
            other => panic!("unexpected request: {other:?}"),
        })
        .await;
        let spawner = AgentSpawner::new(registry);
        spawner
            .spawn_agent("nas", "sess-1", "gemini", None, 45678, "/workspace", Duration::from_secs(2))
            .await
            .expect("spawn should succeed");
    }

    #[tokio::test]
    async fn test_spawn_agent_client_error() {
        let registry = mock_registry(|_req| ControlMessage::AgentSpawnResponse {
            session_id: "sess-1".into(),
            success: false,
            error: Some("binary not found".into()),
        })
        .await;
        let spawner = AgentSpawner::new(registry);
        let err = spawner
            .spawn_agent("nas", "sess-1", "gemini", None, 45678, "/workspace", Duration::from_secs(2))
            .await
            .expect_err("client error should propagate");
        assert_eq!(err, "binary not found");
    }

    #[tokio::test]
    async fn test_spawn_agent_unsupported_type() {
        let db = Database::new(":memory:").await.unwrap();
        let registry = ClientRegistry::new(db);
        let spawner = AgentSpawner::new(registry);
        let err = spawner
            .spawn_agent("nas", "sess-1", "cursor", None, 45678, "/workspace", Duration::from_secs(1))
            .await
            .expect_err("unsupported agent type should error locally");
        assert!(err.contains("unsupported agent type"), "err: {err}");
    }
}
