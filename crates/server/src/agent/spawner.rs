//! Server-side spawner: negotiates process spawn on the client over the control channel.

use std::time::Duration;

use crate::control_plane::client_registry::ClientRegistry;

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

/// 组装 agent 进程的环境变量（LLM 回环代理地址 + 按 agent 类型的模型注入）。
///
/// `base` 是客户端内嵌 LLM 回环代理地址。所有 agent 都注入 OpenAI/Anthropic
/// 双协议入口（`OPENAI_BASE_URL` + `/v1`、`ANTHROPIC_BASE_URL`），agent 按自己的
/// 协议偏好路由到服务端 LLM 网关。`ANTHROPIC_API_KEY` 是 Anthropic 协议通用的 key
/// 变量——claude-code-acp 认 `ANTHROPIC_AUTH_TOKEN`，而 opencode 只认 `API_KEY`，
/// 两者都注入、各取所需，互无副作用。
///
/// `model` 是服务端解析出的有效模型引用（`resolve_effective_model`）；opencode
/// 读取 `OPENCODE_MODEL` 作为默认模型（best-effort——不支持的 agent 忽略未知
/// 环境变量；实际请求的 model 仍由 llm_bridge 按服务端配置覆盖，此处只保证
/// opencode 进程启动时有模型可用、UI 显示一致）。
pub fn agent_env(agent_type: &str, port: u16, model: Option<&str>) -> Vec<(String, String)> {
    let base = format!("http://127.0.0.1:{port}");
    let mut env = vec![
        ("OPENAI_BASE_URL".into(), format!("{base}/v1")),
        ("OPENAI_API_KEY".into(), "tunnel-injected".into()), // 占位，服务端真注入
        ("ANTHROPIC_BASE_URL".into(), base),
        ("ANTHROPIC_AUTH_TOKEN".into(), "tunnel-injected".into()),
        ("ANTHROPIC_API_KEY".into(), "tunnel-injected".into()),
    ];
    if agent_type == "opencode" {
        if let Some(m) = model.filter(|s| !s.trim().is_empty()) {
            env.push(("OPENCODE_MODEL".into(), m.to_string()));
        }
    }
    env
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
                rust_tunnel_common::ControlMessage::AgentLlmProxyStart {
                    session_id: session_id.to_string(),
                },
                timeout,
            )
            .await
            .map_err(|e| format!("llm proxy start failed: {e}"))?;
        match msg {
            rust_tunnel_common::ControlMessage::AgentLlmProxyReady { port, .. } if port > 0 => {
                Ok(port)
            }
            rust_tunnel_common::ControlMessage::AgentLlmProxyReady { .. } => {
                Err("llm proxy failed to bind".into())
            }
            other => Err(format!("unexpected response: {other:?}")),
        }
    }

    /// spawn agent 进程，env 注入 LLM 代理地址。
    /// `model` 是服务端解析出的有效模型引用（opencode 经 `OPENCODE_MODEL` 注入）。
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
        model: Option<&str>,
    ) -> Result<(), String> {
        let (command, args) = agent_command(agent_type, agent_path)?;
        let msg = self
            .registry
            .spawn_negotiate(
                client_id,
                session_id,
                rust_tunnel_common::ControlMessage::AgentSpawnRequest {
                    session_id: session_id.to_string(),
                    command,
                    args,
                    env: agent_env(agent_type, port, model),
                    cwd: Some(cwd.to_string()),
                },
                timeout,
            )
            .await
            .map_err(|e| format!("agent spawn failed: {e}"))?;
        match msg {
            rust_tunnel_common::ControlMessage::AgentSpawnResponse { success: true, .. } => Ok(()),
            rust_tunnel_common::ControlMessage::AgentSpawnResponse { error, .. } => {
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
    ) -> Option<tokio::sync::mpsc::Sender<rust_tunnel_common::ControlMessage>> {
        self.registry
            .get(client_id)
            .await
            .map(|entry| entry.control_sender.clone())
    }

    /// 经控制通道在客户端执行一条 agent 命令（ACP `fs/read_text_file` /
    /// `fs/write_text_file` 转发用；runner 路径走 `executor::exec_on_client`）。
    #[allow(clippy::too_many_arguments)]
    pub async fn agent_exec(
        &self,
        client_id: &str,
        request_id: &str,
        session_id: &str,
        root_path: &str,
        docker_container: Option<&str>,
        command: rust_tunnel_common::AgentCommand,
        timeout: Duration,
    ) -> std::io::Result<rust_tunnel_common::AgentResult> {
        self.registry
            .agent_exec(
                client_id,
                request_id,
                session_id,
                root_path,
                docker_container,
                command,
                timeout,
            )
            .await
    }

    /// 向客户端下发 `AgentExecCancel`（ACP 路径取消/杀进程用）。
    ///
    /// ACP 会话能 spawn 成功即意味着客户端 ≥ 0.4.0（agent spawn 协议与
    /// `AgentExecCancel` 同批引入），无需再按版本门控；客户端离线静默返回 false。
    pub async fn send_agent_cancel(&self, client_id: &str, request_id: &str) -> bool {
        self.registry.send_agent_cancel(client_id, request_id).await
    }

    /// 请求客户端停止某 session 的内嵌 LLM 回环代理（释放回环监听端口）。
    /// 客户端离线或发送失败返回 false（无害）。
    pub async fn stop_llm_proxy(&self, client_id: &str, session_id: &str) -> bool {
        self.registry
            .send_control(
                client_id,
                rust_tunnel_common::ControlMessage::AgentLlmProxyStop {
                    session_id: session_id.to_string(),
                },
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    use crate::db::Database;
    use rust_tunnel_common::ControlMessage;

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
    fn test_agent_command_opencode() {
        let (cmd, args) = agent_command("opencode", None).unwrap();
        assert_eq!(cmd, "opencode");
        assert_eq!(args, vec!["--acp"]);
    }

    #[test]
    fn test_agent_env_common_injects_both_anthropic_key_vars() {
        // claude-code-acp 认 AUTH_TOKEN；opencode 认 API_KEY——两者都注入，
        // 各自按需读取，互无副作用。
        let env = agent_env("claude-code", 45678, None);
        let base = env.iter().find(|(k, _)| k == "OPENAI_BASE_URL").unwrap();
        assert_eq!(base.1, "http://127.0.0.1:45678/v1");
        assert!(
            env.iter().any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == "tunnel-injected"),
            "ANTHROPIC_API_KEY must be injected: {env:?}"
        );
        assert!(
            env.iter().any(|(k, v)| k == "ANTHROPIC_AUTH_TOKEN" && v == "tunnel-injected"),
            "ANTHROPIC_AUTH_TOKEN must be injected: {env:?}"
        );
        // 非 opencode agent 不注入 OPENCODE_MODEL
        assert!(
            !env.iter().any(|(k, _)| k == "OPENCODE_MODEL"),
            "OPENCODE_MODEL must only be injected for opencode: {env:?}"
        );
    }

    #[test]
    fn test_agent_env_opencode_injects_model() {
        let env = agent_env("opencode", 45678, Some("gpt-4o"));
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "OPENCODE_MODEL")
                .map(|(_, v)| v.as_str()),
            Some("gpt-4o"),
            "opencode model should be injected: {env:?}"
        );
        // 空/None 模型不注入
        let env_none = agent_env("opencode", 45678, None);
        assert!(
            !env_none.iter().any(|(k, _)| k == "OPENCODE_MODEL"),
            "no model -> no OPENCODE_MODEL: {env_none:?}"
        );
        let env_blank = agent_env("opencode", 45678, Some("   "));
        assert!(
            !env_blank.iter().any(|(k, _)| k == "OPENCODE_MODEL"),
            "blank model -> no OPENCODE_MODEL: {env_blank:?}"
        );
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
            .spawn_agent(
                "nas",
                "sess-1",
                "gemini",
                None,
                45678,
                "/workspace",
                Duration::from_secs(2),
                None,
            )
            .await
            .expect("spawn should succeed");
    }

    #[tokio::test]
    async fn test_spawn_agent_opencode_success() {
        // opencode 路径：命令 `opencode --acp`，env 注入 OPENCODE_MODEL。
        let registry = mock_registry(|req| match req {
            ControlMessage::AgentSpawnRequest {
                session_id,
                command,
                args,
                env,
                cwd,
            } => {
                assert_eq!(command, "opencode");
                assert_eq!(args, vec!["--acp"]);
                assert_eq!(cwd.as_deref(), Some("/workspace"));
                assert_eq!(
                    env.iter()
                        .find(|(k, _)| k == "OPENCODE_MODEL")
                        .map(|(_, v)| v.as_str()),
                    Some("gpt-4o"),
                    "OPENCODE_MODEL should be injected: {env:?}"
                );
                assert!(
                    env.iter().any(|(k, _)| k == "ANTHROPIC_API_KEY"),
                    "ANTHROPIC_API_KEY should be injected: {env:?}"
                );
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
            .spawn_agent(
                "nas",
                "sess-1",
                "opencode",
                None,
                45678,
                "/workspace",
                Duration::from_secs(2),
                Some("gpt-4o"),
            )
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
            .spawn_agent(
                "nas",
                "sess-1",
                "gemini",
                None,
                45678,
                "/workspace",
                Duration::from_secs(2),
                None,
            )
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
            .spawn_agent(
                "nas",
                "sess-1",
                "cursor",
                None,
                45678,
                "/workspace",
                Duration::from_secs(1),
                None,
            )
            .await
            .expect_err("unsupported agent type should error locally");
        assert!(err.contains("unsupported agent type"), "err: {err}");
    }
}
