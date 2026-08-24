//! Server-side spawner: negotiates process spawn on the client over the control channel.

use std::time::Duration;


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
        "opencode" => vec!["acp".to_string()],
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
/// `model` 是服务端解析出的有效模型引用（`resolve_effective_model`）。claude-code/
/// gemini 不使用它（模型经 ACP set_config_option 注入）；opencode 不读
/// `OPENAI_BASE_URL`/`OPENCODE_MODEL`（其 provider SDK 实例化时 baseURL 显式取
/// `options.baseURL ?? model.api.url`，覆盖了 AI SDK 的 env fallback；flag 层也无
/// `OPENCODE_MODEL`），故 opencode 分支改用 `OPENCODE_CONFIG_CONTENT` 注入自定义
/// provider `rust-tunnel`（baseURL 指向回环代理），把 LLM 流量引入隧道——见
/// [`opencode_config_content`]。
///
/// `available_models` 是服务端启用的网关模型 id 列表（仅 opencode 分支用于
/// provider.models 枚举），其余 agent 传空切片。
pub fn agent_env(
    agent_type: &str,
    port: u16,
    model: Option<&str>,
    available_models: &[String],
    tier_envs: &[(String, String)],
) -> Vec<(String, String)> {
    let base = format!("http://127.0.0.1:{port}");
    let mut env = vec![
        ("OPENAI_BASE_URL".into(), format!("{base}/v1")),
        ("OPENAI_API_KEY".into(), "tunnel-injected".into()), // 占位，服务端真注入
        ("ANTHROPIC_BASE_URL".into(), base),
        ("ANTHROPIC_AUTH_TOKEN".into(), "tunnel-injected".into()),
        ("ANTHROPIC_API_KEY".into(), "tunnel-injected".into()),
    ];
    if agent_type == "opencode" {
        if let Some(content) = opencode_config_content(port, model, available_models) {
            env.push(("OPENCODE_CONFIG_CONTENT".into(), content));
        }
    }
    if agent_type == "claude-code" {
        for (k, v) in tier_envs {
            env.push((k.clone(), v.clone()));
        }
    }
    env
}

/// 构造 opencode 内联配置 JSON（经 `OPENCODE_CONFIG_CONTENT` 注入，local 级、后
/// 合并，deep merge 不破坏用户其它配置）。
///
/// 注册自定义 provider `rust-tunnel`（npm `@ai-sdk/openai-compatible`，opencode
/// 内置打包）：`options.baseURL` 指向回环代理 `http://127.0.0.1:{port}/v1`，
/// `options.apiKey` 占位 `"tunnel-injected"`（llm_bridge 服务端真注入认证）。
/// `provider.models` 枚举服务端启用的网关模型（key 即 modelID，value 为
/// `{"name": "<model>"}`；opencode 侧模型名只影响展示，实际请求的 model 由服务端
/// llm_bridge 按 session 配置重写）。`enabled_providers` 白名单 `["rust-tunnel"]`
/// 防止 opencode 用本机凭据/env placeholder key 选中其它 provider 直连外网。
///
/// `default_model` 为非空白字符串时设置顶层 `model`/`small_model` 为
/// `rust-tunnel/<default>`（small_model 用于标题生成等后台任务；白名单后必须显式
/// 给）；为 None/空白则不设这两个键（白名单下 opencode 自选好歹可用的模型），
/// 但 provider 仍注入。`models` 为空且 `default_model` 为 None 时返回 None
/// （无注入意义）。
fn opencode_config_content(
    port: u16,
    default_model: Option<&str>,
    models: &[String],
) -> Option<String> {
    let mut models_map = serde_json::Map::new();
    // 去重：available_models + default_model（若有）合并，保持出现顺序。
    let mut seen = std::collections::HashSet::new();
    let default_model = default_model.filter(|s| !s.trim().is_empty());
    let mut names: Vec<&str> = models
        .iter()
        .map(String::as_str)
        .filter(|m| seen.insert((*m).to_string()))
        .collect();
    if let Some(dm) = default_model {
        if seen.insert(dm.to_string()) {
            names.push(dm);
        }
    }
    for name in &names {
        models_map.insert(
            (*name).to_string(),
            serde_json::json!({"name": name}),
        );
    }
    if models_map.is_empty() && default_model.is_none() {
        return None;
    }
    let mut config = serde_json::Map::new();
    let provider = serde_json::json!({
        "npm": "@ai-sdk/openai-compatible",
        "options": {
            "baseURL": format!("http://127.0.0.1:{port}/v1"),
            "apiKey": "tunnel-injected",
        },
        "models": serde_json::Value::Object(models_map),
    });
    config.insert(
        "provider".to_string(),
        serde_json::json!({ "rust-tunnel": provider }),
    );

    if let Some(dm) = default_model {
        config.insert("model".to_string(), format!("rust-tunnel/{dm}").into());
        config.insert(
            "small_model".to_string(),
            format!("rust-tunnel/{dm}").into(),
        );
    }
    config.insert(
        "enabled_providers".to_string(),
        serde_json::json!(["rust-tunnel"]),
    );
    serde_json::to_string(&serde_json::Value::Object(config)).ok()
}

#[derive(Clone)]
pub struct AgentSpawner {
    registry: std::sync::Arc<dyn crate::TunnelExecutor>,
}

impl AgentSpawner {
    pub fn new(registry: std::sync::Arc<dyn crate::TunnelExecutor>) -> Self {
        Self { registry }
    }

    /// 查询客户端版本号（用于 WriteFile2/EditFile 等功能门控）。
    pub async fn client_version(&self, client_id: &str) -> Option<String> {
        self.registry
            .client_handle(client_id)
            .await
            .and_then(|h| h.client_version)
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
    /// `model` 是服务端解析出的有效模型引用（opencode 经 `OPENCODE_CONFIG_CONTENT`
    /// 注入；claude-code 不用它，仍走 ACP set_config_option）。
    /// `available_models` 是服务端启用的网关模型 id 列表（仅 opencode 分支用于
    /// provider.models 枚举），其余 agent 传空切片。
    // 9 个参数：每个语义单一，拆 struct 反而绕（brief 指定签名，仿 agent_exec 处理）。
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
        available_models: &[String],
        tier_envs: &[(String, String)],
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
                    env: agent_env(agent_type, port, model, available_models, tier_envs),
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
            .client_handle(client_id)
            .await
            .map(|h| h.control_sender)
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
    use std::sync::Arc;

    use crate::{ClientHandle, TunnelExecutor};
    use rust_tunnel_common::ControlMessage;

    /// 内存 mock：`spawn_negotiate` 用注入闭包即时应答；
    /// `client_handle`/`send_*` 按离线语义返回。
    #[derive(Default)]
    struct MockExecutor {
        responder: tokio::sync::Mutex<
            Option<Box<dyn FnOnce(ControlMessage) -> ControlMessage + Send>>,
        >,
    }

    #[async_trait::async_trait]
    impl TunnelExecutor for MockExecutor {
        async fn client_handle(&self, _client_id: &str) -> Option<ClientHandle> {
            None
        }
        async fn spawn_negotiate(
            &self,
            _client_id: &str,
            _session_id: &str,
            request: ControlMessage,
            _timeout: Duration,
        ) -> std::io::Result<ControlMessage> {
            let respond = self
                .responder
                .lock()
                .await
                .take()
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::NotConnected, "client offline")
                })?;
            Ok(respond(request))
        }
        async fn agent_exec(
            &self,
            _client_id: &str,
            _request_id: &str,
            _session_id: &str,
            _root_path: &str,
            _docker_container: Option<&str>,
            _command: rust_tunnel_common::AgentCommand,
            _timeout: Duration,
        ) -> std::io::Result<rust_tunnel_common::AgentResult> {
            Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "client offline"))
        }
        async fn send_agent_cancel(&self, _client_id: &str, _request_id: &str) -> bool {
            false
        }
        async fn send_control(
            &self,
            _client_id: &str,
            _msg: ControlMessage,
        ) -> bool {
            false
        }
        async fn open_tunnel(
            &self,
            _client_id: &str,
            _target_addr: &str,
        ) -> std::io::Result<crate::TunnelByteStream> {
            Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "client offline"))
        }
    }

    /// 构造一个自动应答协商请求的 executor。`respond` 闭包把请求转成响应。
    fn mock_executor<F>(respond: F) -> Arc<MockExecutor>
    where
        F: FnOnce(ControlMessage) -> ControlMessage + Send + 'static,
    {
        Arc::new(MockExecutor {
            responder: tokio::sync::Mutex::new(Some(Box::new(respond))),
        })
    }

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
    fn test_agent_env_claude_code_injects_tier_envs() {
        // claude-code + tier 映射 → ANTHROPIC_DEFAULT_*_MODEL / ANTHROPIC_SMALL_FAST_MODEL
        // 原样注入（值已由调用方 resolve 为网关模型引用）。
        let tier = vec![
            (
                "ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(),
                "model:opus-x".to_string(),
            ),
            (
                "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(),
                "model:haiku-y".to_string(),
            ),
            (
                "ANTHROPIC_SMALL_FAST_MODEL".to_string(),
                "model:haiku-y".to_string(),
            ),
        ];
        let env = agent_env("claude-code", 45678, None, &[], &tier);
        for (k, v) in &tier {
            assert!(
                env.iter().any(|(ek, ev)| ek == k && ev == v),
                "missing tier env {k}: {env:?}"
            );
        }
    }

    #[test]
    fn test_agent_env_claude_code_empty_tier_envs_no_injection() {
        // 未配置 tier 映射 → 不注入任何 tier env（行为与旧版一致）。
        let env = agent_env("claude-code", 45678, None, &[], &[]);
        assert!(
            !env.iter()
                .any(|(k, _)| k.starts_with("ANTHROPIC_DEFAULT_")
                    || k == "ANTHROPIC_SMALL_FAST_MODEL"),
            "no tier envs expected: {env:?}"
        );
    }

    #[test]
    fn test_agent_env_non_claude_ignores_tier_envs() {
        // 非 claude-code（opencode/gemini）即使调用方误传 tier_envs 也不注入。
        let tier = vec![(
            "ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(),
            "model:x".to_string(),
        )];
        for ty in ["opencode", "gemini", ""] {
            let env = agent_env(ty, 45678, None, &[], &tier);
            assert!(
                !env.iter().any(|(k, _)| k == "ANTHROPIC_DEFAULT_OPUS_MODEL"),
                "{ty} must not inject tier envs: {env:?}"
            );
        }
    }

    #[test]
    fn test_agent_command_opencode() {
        let (cmd, args) = agent_command("opencode", None).unwrap();
        assert_eq!(cmd, "opencode");
        assert_eq!(args, vec!["acp"]);
    }

    #[test]
    fn test_agent_env_common_injects_both_anthropic_key_vars() {
        // claude-code-acp 认 AUTH_TOKEN；opencode 认 API_KEY——两者都注入，
        // 各自按需读取，互无副作用。
        let env = agent_env("claude-code", 45678, None, &[], &[]);
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
        // 非 opencode agent 不注入 OPENCODE_CONFIG_CONTENT
        assert!(
            !env.iter().any(|(k, _)| k == "OPENCODE_CONFIG_CONTENT"),
            "OPENCODE_CONFIG_CONTENT must only be injected for opencode: {env:?}"
        );
    }

    #[test]
    fn test_agent_env_opencode_config_content() {
        // opencode + 模型 + available_models：注入配置，白名单 + provider.baseURL
        // + model/small_model + models map。
        let env = agent_env(
            "opencode",
            45678,
            Some("gpt-4o"),
            &["gpt-4o-mini".to_string(), "claude-3-5-sonnet".to_string()],
            &[],
        );
        let content = env
            .iter()
            .find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT")
            .map(|(_, v)| v.as_str())
            .expect("OPENCODE_CONFIG_CONTENT should be injected: {env:?}");
        let config: serde_json::Value =
            serde_json::from_str(content).expect("config should be valid JSON");
        // 顶层白名单
        assert_eq!(
            config["enabled_providers"],
            serde_json::json!(["rust-tunnel"]),
            "enabled_providers must whitelist rust-tunnel: {config}"
        );
        // model / small_model
        assert_eq!(config["model"], "rust-tunnel/gpt-4o");
        assert_eq!(config["small_model"], "rust-tunnel/gpt-4o");
        // provider baseURL 指向回环代理
        assert_eq!(
            config["provider"]["rust-tunnel"]["options"]["baseURL"],
            "http://127.0.0.1:45678/v1"
        );
        assert_eq!(
            config["provider"]["rust-tunnel"]["options"]["apiKey"],
            "tunnel-injected"
        );
        assert_eq!(
            config["provider"]["rust-tunnel"]["npm"],
            "@ai-sdk/openai-compatible"
        );
        // models map 含 available_models + default_model（去重）
        let models_map = config["provider"]["rust-tunnel"]["models"].as_object().unwrap();
        assert!(models_map.contains_key("gpt-4o-mini"));
        assert!(models_map.contains_key("claude-3-5-sonnet"));
        assert!(models_map.contains_key("gpt-4o"));
        assert_eq!(models_map["gpt-4o"], serde_json::json!({"name": "gpt-4o"}));
        assert_eq!(models_map.len(), 3);
        // default None：不注入 model 键但 provider 仍注入
        let env_none = agent_env("opencode", 45678, None, &["gpt-4o".to_string()], &[]);
        let content_none = env_none
            .iter()
            .find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT")
            .map(|(_, v)| v.as_str())
            .expect("provider should still be injected: {env_none:?}");
        let config_none: serde_json::Value =
            serde_json::from_str(content_none).expect("valid JSON");
        assert!(config_none.get("model").is_none(), "no model key: {config_none}");
        assert!(config_none.get("small_model").is_none(), "no small_model key");
        assert_eq!(
            config_none["provider"]["rust-tunnel"]["options"]["baseURL"],
            "http://127.0.0.1:45678/v1"
        );
        // 空/None 模型不注入
        let env_blank = agent_env("opencode", 45678, Some("   "), &["gpt-4o".to_string()], &[]);
        assert!(
            env_blank.iter().any(|(k, _)| k == "OPENCODE_CONFIG_CONTENT"),
            "blank model but available models -> still inject provider: {env_blank:?}"
        );
        // 空模型 + 空 available_models：无注入意义，返回 None → 不注入环境变量
        let env_empty = agent_env("opencode", 45678, None, &[], &[]);
        assert!(
            !env_empty.iter().any(|(k, _)| k == "OPENCODE_CONFIG_CONTENT"),
            "empty models + no default -> no injection: {env_empty:?}"
        );
    }

    #[tokio::test]
    async fn test_start_llm_proxy_success() {
        let registry = mock_executor(|_req| ControlMessage::AgentLlmProxyReady {
            session_id: "sess-1".into(),
            port: 45678,
        });
        let spawner = AgentSpawner::new(registry);
        let port = spawner
            .start_llm_proxy("nas", "sess-1", Duration::from_secs(2))
            .await
            .expect("llm proxy should start");
        assert_eq!(port, 45678);
    }

    #[tokio::test]
    async fn test_start_llm_proxy_bind_failure() {
        let registry = mock_executor(|_req| ControlMessage::AgentLlmProxyReady {
            session_id: "sess-1".into(),
            port: 0,
        });
        let spawner = AgentSpawner::new(registry);
        let err = spawner
            .start_llm_proxy("nas", "sess-1", Duration::from_secs(2))
            .await
            .expect_err("port 0 should be a failure");
        assert!(err.contains("failed to bind"), "err: {err}");
    }

    #[tokio::test]
    async fn test_start_llm_proxy_unexpected_response() {
        let registry = mock_executor(|_req| ControlMessage::AgentSpawnResponse {
            session_id: "sess-1".into(),
            success: true,
            error: None,
        });
        let spawner = AgentSpawner::new(registry);
        let err = spawner
            .start_llm_proxy("nas", "sess-1", Duration::from_secs(2))
            .await
            .expect_err("wrong response type should error");
        assert!(err.contains("unexpected response"), "err: {err}");
    }

    #[tokio::test]
    async fn test_start_llm_proxy_offline_client() {
        let spawner = AgentSpawner::new(Arc::new(MockExecutor::default()));
        let err = spawner
            .start_llm_proxy("ghost", "sess-1", Duration::from_millis(100))
            .await
            .expect_err("offline client should error");
        assert!(err.contains("llm proxy start failed"), "err: {err}");
    }

    #[tokio::test]
    async fn test_spawn_agent_success() {
        let registry = mock_executor(|req| match req {
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
        });
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
                &[],
                &[],
            )
            .await
            .expect("spawn should succeed");
    }

    #[tokio::test]
    async fn test_spawn_agent_opencode_success() {
        // opencode 路径：命令 `opencode acp`（子命令，非 `--acp`），env 注入
        // OPENCODE_CONFIG_CONTENT 携带自定义 provider baseURL 指向回环代理。
        let registry = mock_executor(|req| match req {
            ControlMessage::AgentSpawnRequest {
                session_id,
                command,
                args,
                env,
                cwd,
            } => {
                assert_eq!(command, "opencode");
                assert_eq!(args, vec!["acp"]);
                assert_eq!(cwd.as_deref(), Some("/workspace"));
                let content = env
                    .iter()
                    .find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT")
                    .map(|(_, v)| v.as_str())
                    .expect("OPENCODE_CONFIG_CONTENT should be injected: {env:?}");
                let config: serde_json::Value =
                    serde_json::from_str(content).expect("valid JSON");
                assert!(
                    config["provider"]["rust-tunnel"]["options"]["baseURL"]
                        .as_str()
                        .is_some_and(|u| u == "http://127.0.0.1:45678/v1"),
                    "config must route to loopback proxy: {config}"
                );
                assert_eq!(
                    config["enabled_providers"],
                    serde_json::json!(["rust-tunnel"])
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
        });
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
                &[],
                &[],
            )
            .await
            .expect("spawn should succeed");
    }

    #[tokio::test]
    async fn test_spawn_agent_client_error() {
        let registry = mock_executor(|_req| ControlMessage::AgentSpawnResponse {
            session_id: "sess-1".into(),
            success: false,
            error: Some("binary not found".into()),
        });
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
                &[],
                &[],
            )
            .await
            .expect_err("client error should propagate");
        assert_eq!(err, "binary not found");
    }

    #[tokio::test]
    async fn test_spawn_agent_unsupported_type() {
        let spawner = AgentSpawner::new(Arc::new(MockExecutor::default()));
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
                &[],
                &[],
            )
            .await
            .expect_err("unsupported agent type should error locally");
        assert!(err.contains("unsupported agent type"), "err: {err}");
    }
}
