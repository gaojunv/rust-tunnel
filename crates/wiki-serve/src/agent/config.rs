// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![allow(clippy::missing_docs_in_private_items)]

//! Agent 设置与 `CODEX_HOME` 配置渲染.
//!
//! `AgentSettings` 持久化于 `{app_config_dir}/agent-settings.json`（0600）。
//! `CodexHome::ensure` 全量渲染 `{base_dir}/config.toml`（头注释 `# managed by wiki-desktop, do not edit`）。

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 认证模式.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMode {
    /// 网关模式（默认）：经 `model_provider = "tunnel"` + env `WIKI_TUNNEL_LLM_KEY`.
    #[default]
    #[serde(rename = "gateway")]
    Gateway,
    /// 直连 OpenAI（`model_provider = "openai"` + env `OPENAI_API_KEY`）.
    #[serde(rename = "openai-key")]
    OpenaiKey,
    /// ChatGPT 登录（不写密钥，依赖 codex 侧 `auth.json`）.
    #[serde(rename = "chatgpt")]
    ChatGpt,
}

/// 审批策略.
///
/// `Untrusted` 渲染为 codex 配置的 `untrusted`，前端 wire 值保持 `"auto"` 兼容.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    /// 每次请求审批（默认）.
    #[default]
    #[serde(rename = "on-request")]
    OnRequest,
    /// 仅不可信命令需审批（渲染为 codex 配置的 `untrusted`；前端 wire 值保持 `"auto"` 兼容）.
    #[serde(rename = "auto")]
    Untrusted,
    /// 从不批准.
    #[serde(rename = "never")]
    Never,
}

/// 沙箱模式.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// 仅 workspace 可写（默认）.
    #[default]
    #[serde(rename = "workspace-write")]
    WorkspaceWrite,
    /// 完全访问（危险）.
    #[serde(rename = "danger-full-access")]
    DangerFullAccess,
    /// 只读.
    #[serde(rename = "read-only")]
    ReadOnly,
}

/// Codex 自定义 provider 的 wire 协议（仅网关模式生效）.
///
/// codex 侧对应 `[model_providers.*]` 下的 `wire_api` 字段：
/// `responses` 请求 `POST {base_url}/responses`，`chat` 请求 `POST {base_url}/chat/completions`。
/// `OpenaiKey`/`ChatGpt` 模式使用 codex 内置 provider，wire 协议由 codex 决定，不渲染该字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum WireApi {
    /// OpenAI Responses API（默认，`/v1/responses`）.
    #[default]
    #[serde(rename = "responses")]
    Responses,
    /// Chat Completions API（`/v1/chat/completions`）.
    #[serde(rename = "chat")]
    Chat,
}

impl WireApi {
    /// codex config.toml 中的 `wire_api` 值.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::Chat => "chat",
        }
    }
}

fn default_approval() -> ApprovalPolicy {
    ApprovalPolicy::OnRequest
}

fn default_sandbox() -> SandboxMode {
    SandboxMode::WorkspaceWrite
}

/// Agent 设置（camelCase 序列化，与前端约定）.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSettings {
    /// 是否启用 agent.
    #[serde(default)]
    pub enabled: bool,
    /// 认证模式.
    #[serde(default)]
    pub auth_mode: AuthMode,
    /// 网关基址（如 `http://127.0.0.1:PORT` 或 `http://127.0.0.1:PORT/v1`，均可；codex 侧最终请求路径由 [`WireApi`] 决定
    /// —— `POST {base_url}/responses` 或 `POST {base_url}/chat/completions`，因此 `base_url` 已含 `v1` 与不含 `v1`
    /// 都会被正确拼接，假上游需同时监听两条路径）.
    #[serde(default)]
    pub gateway_base_url: Option<String>,
    /// 网关 API Key（敏感，不落 `config.toml`，仅经 env 注入）.
    #[serde(default)]
    pub gateway_api_key: Option<String>,
    /// OpenAI API Key（敏感，仅 env 注入）.
    #[serde(default)]
    pub openai_api_key: Option<String>,
    /// 模型名.
    #[serde(default)]
    pub model: Option<String>,
    /// 审批策略（默认 `on-request`）.
    #[serde(default = "default_approval")]
    pub approval_policy: ApprovalPolicy,
    /// 沙箱模式（默认 `workspace-write`）.
    #[serde(default = "default_sandbox")]
    pub sandbox_mode: SandboxMode,
    /// 二进制覆盖路径.
    #[serde(default)]
    pub codex_path_override: Option<String>,
    /// wire 协议（默认 `responses`；仅网关模式渲染进 config.toml）.
    #[serde(default)]
    pub wire_api: WireApi,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            auth_mode: AuthMode::Gateway,
            gateway_base_url: None,
            gateway_api_key: None,
            openai_api_key: None,
            model: None,
            approval_policy: ApprovalPolicy::OnRequest,
            sandbox_mode: SandboxMode::WorkspaceWrite,
            codex_path_override: None,
            wire_api: WireApi::Responses,
        }
    }
}

impl AgentSettings {
    /// 从文件加载，不存在返回 `Default`.
    ///
    /// # Errors
    ///
    /// 读取或解析失败时返回 `std::io::Error`。
    pub fn load(path: &Path) -> std::io::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = std::fs::read_to_string(path)?;
        let s: Self = serde_json::from_str(&data).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(s)
    }

    /// 保存到文件（0600，父目录自动创建）.
    ///
    /// # Errors
    ///
    /// 序列化或写入失败时返回 `std::io::Error`。
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, data.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perm = std::fs::metadata(path)?.permissions();
            perm.set_mode(0o600);
            std::fs::set_permissions(path, perm)?;
        }
        Ok(())
    }

    /// 默认 settings 文件路径：`{app_config_dir}/agent-settings.json`.
    #[must_use]
    pub fn default_path(app_config_dir: &Path) -> PathBuf {
        app_config_dir.join("agent-settings.json")
    }

    /// 启动前校验（网关地址与所选认证模式的 key）.
    ///
    /// # Errors
    ///
    /// 校验失败时返回中文错误字符串。
    pub fn validate(&self) -> Result<(), String> {
        match self.auth_mode {
            AuthMode::Gateway => {
                let has_base = self
                    .gateway_base_url
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty());
                if !has_base {
                    return Err("网关模式需要在设置中填写网关地址".to_owned());
                }
                let has_key = self
                    .gateway_api_key
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty());
                if !has_key {
                    return Err("网关模式未配置 API Key，请打开设置".to_owned());
                }
            }
            AuthMode::OpenaiKey => {
                let has_key = self
                    .openai_api_key
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty());
                if !has_key {
                    return Err("OpenAI Key 模式未配置 API Key，请打开设置".to_owned());
                }
            }
            AuthMode::ChatGpt => {}
        }
        Ok(())
    }
}

/// `CODEX_HOME` 管理：渲染 `config.toml`.
pub struct CodexHome;

impl CodexHome {
    /// 全量渲染 `config.toml`（幂等，覆盖旧文件）.
    ///
    /// - 头注释 `# managed by wiki-desktop, do not edit`
    /// - 网关模式：`model_provider = "tunnel"`，`[model_providers.tunnel]` 指向 `gatewayBaseUrl`
    /// - `openai-key` 模式：`model_provider = "openai"`
    /// - `chatgpt` 模式：不写 provider 密钥相关项
    /// - 密钥不落盘，由上层经 env 注入
    ///
    /// # Errors
    ///
    /// 目录创建或写入失败时返回 `std::io::Error`。
    pub fn ensure(base_dir: &Path, settings: &AgentSettings) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(base_dir)?;
        let config_path = base_dir.join("config.toml");
        let mut out = String::new();
        out.push_str("# managed by wiki-desktop, do not edit\n");
        out.push_str("# 由 AgentSettings 全量渲染，密钥经环境变量注入\n\n");

        match settings.auth_mode {
            AuthMode::Gateway => {
                out.push_str("model_provider = \"tunnel\"\n");
                if let Some(model) = settings.model.as_deref().filter(|s| !s.trim().is_empty()) {
                    let esc = toml_escape(model);
                    let _ = writeln!(out, "model = \"{esc}\"");
                }
                out.push_str("\n[model_providers.tunnel]\n");
                // `name` 为 codex 0.153+ 新必填字段（对应 provider 显示名）；缺省则触发
                // `model_providers.tunnel: provider name must not be empty` 校验。
                out.push_str("name = \"tunnel\"\n");
                // 网关地址缺省时不再回退 OpenAI 官方地址（拿非 OpenAI key 打官方既错又危险），
                // 启动前由 `AgentSettings::validate` 拦截；此处二次兜底报错，杜绝静默默认。
                let base_url = settings
                    .gateway_base_url
                    .as_deref()
                    .filter(|u| !u.trim().is_empty())
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "网关模式需要在设置中填写网关地址",
                        )
                    })?;
                let esc_url = toml_escape(base_url);
                let _ = writeln!(out, "base_url = \"{esc_url}\"");
                out.push_str("env_key = \"WIKI_TUNNEL_LLM_KEY\"\n");
                let _ = writeln!(out, "wire_api = \"{}\"", settings.wire_api.as_str());
                // 审批与沙箱
                let approval = match settings.approval_policy {
                    ApprovalPolicy::OnRequest => "on-request",
                    ApprovalPolicy::Untrusted => "untrusted",
                    ApprovalPolicy::Never => "never",
                };
                let _ = writeln!(out, "\napproval_policy = \"{approval}\"");
                let sandbox = match settings.sandbox_mode {
                    SandboxMode::WorkspaceWrite => "workspace-write",
                    SandboxMode::DangerFullAccess => "danger-full-access",
                    SandboxMode::ReadOnly => "read-only",
                };
                let _ = writeln!(out, "sandbox_mode = \"{sandbox}\"");
            }
            AuthMode::OpenaiKey => {
                out.push_str("model_provider = \"openai\"\n");
                if let Some(model) = settings.model.as_deref().filter(|s| !s.trim().is_empty()) {
                    let esc = toml_escape(model);
                    let _ = writeln!(out, "model = \"{esc}\"");
                }
                let approval = match settings.approval_policy {
                    ApprovalPolicy::OnRequest => "on-request",
                    ApprovalPolicy::Untrusted => "untrusted",
                    ApprovalPolicy::Never => "never",
                };
                let _ = writeln!(out, "\napproval_policy = \"{approval}\"");
                let sandbox = match settings.sandbox_mode {
                    SandboxMode::WorkspaceWrite => "workspace-write",
                    SandboxMode::DangerFullAccess => "danger-full-access",
                    SandboxMode::ReadOnly => "read-only",
                };
                let _ = writeln!(out, "sandbox_mode = \"{sandbox}\"");
            }
            AuthMode::ChatGpt => {
                if let Some(model) = settings.model.as_deref().filter(|s| !s.trim().is_empty()) {
                    let esc = toml_escape(model);
                    let _ = writeln!(out, "model = \"{esc}\"");
                }
                let approval = match settings.approval_policy {
                    ApprovalPolicy::OnRequest => "on-request",
                    ApprovalPolicy::Untrusted => "untrusted",
                    ApprovalPolicy::Never => "never",
                };
                let _ = writeln!(out, "approval_policy = \"{approval}\"");
                let sandbox = match settings.sandbox_mode {
                    SandboxMode::WorkspaceWrite => "workspace-write",
                    SandboxMode::DangerFullAccess => "danger-full-access",
                    SandboxMode::ReadOnly => "read-only",
                };
                let _ = writeln!(out, "sandbox_mode = \"{sandbox}\"");
            }
        }

        std::fs::write(&config_path, out.as_bytes())?;
        Ok(config_path)
    }

    /// 组装子进程 env（密钥注入）.
    #[must_use]
    pub fn child_env(settings: &AgentSettings) -> HashMap<String, String> {
        let mut env = HashMap::new();
        match settings.auth_mode {
            AuthMode::Gateway => {
                if let Some(key) = settings.gateway_api_key.as_deref().filter(|s| !s.trim().is_empty()) {
                    env.insert("WIKI_TUNNEL_LLM_KEY".to_owned(), key.to_owned());
                }
            }
            AuthMode::OpenaiKey => {
                if let Some(key) = settings.openai_api_key.as_deref().filter(|s| !s.trim().is_empty()) {
                    env.insert("OPENAI_API_KEY".to_owned(), key.to_owned());
                }
            }
            AuthMode::ChatGpt => {}
        }
        env
    }

    /// `CODEX_HOME` 目录（`{app_config_dir}/codex-home`）.
    #[must_use]
    pub fn default_dir(app_config_dir: &Path) -> PathBuf {
        app_config_dir.join("codex-home")
    }
}

fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_roundtrip() {
        let s = AgentSettings::default();
        let json = serde_json::to_string(&s).expect("ser");
        let back: AgentSettings = serde_json::from_str(&json).expect("de");
        assert!(!back.enabled);
        assert_eq!(back.auth_mode, AuthMode::Gateway);
        assert_eq!(back.approval_policy, ApprovalPolicy::OnRequest);
        assert_eq!(back.sandbox_mode, SandboxMode::WorkspaceWrite);
    }

    #[test]
    fn camel_case_serialization() {
        let s = AgentSettings {
            enabled: true,
            auth_mode: AuthMode::OpenaiKey,
            gateway_base_url: Some("https://example.com".to_owned()),
            gateway_api_key: Some("gw-key".to_owned()),
            openai_api_key: Some("sk-xxx".to_owned()),
            model: Some("gpt-4o".to_owned()),
            approval_policy: ApprovalPolicy::Untrusted,
            sandbox_mode: SandboxMode::DangerFullAccess,
            codex_path_override: Some("/tmp/codex".to_owned()),
            wire_api: WireApi::Responses,
        };
        let v = serde_json::to_value(&s).expect("value");
        assert_eq!(v.get("enabled").and_then(serde_json::Value::as_bool), Some(true));
        assert_eq!(v.get("authMode").and_then(serde_json::Value::as_str), Some("openai-key"));
        assert_eq!(
            v.get("gatewayBaseUrl").and_then(serde_json::Value::as_str),
            Some("https://example.com")
        );
        assert_eq!(
            v.get("codexPathOverride").and_then(serde_json::Value::as_str),
            Some("/tmp/codex")
        );
        assert_eq!(v.get("approvalPolicy").and_then(serde_json::Value::as_str), Some("auto"));
        assert_eq!(
            v.get("sandboxMode").and_then(serde_json::Value::as_str),
            Some("danger-full-access")
        );
        // 反序列化
        let back: AgentSettings = serde_json::from_value(v).expect("de");
        assert_eq!(back.auth_mode, AuthMode::OpenaiKey);
    }

    #[test]
    fn config_toml_gateway_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("codex-home");
        let settings = AgentSettings {
            enabled: true,
            auth_mode: AuthMode::Gateway,
            gateway_base_url: Some("https://gw.example.com/v1".to_owned()),
            gateway_api_key: Some("secret-key".to_owned()),
            model: Some("gpt-4o".to_owned()),
            ..Default::default()
        };
        let path = CodexHome::ensure(&base, &settings).expect("ensure");
        let content = std::fs::read_to_string(&path).expect("read");
        assert!(content.starts_with("# managed by wiki-desktop"), "头注释缺失：{content}");
        assert!(content.contains("model_provider = \"tunnel\""), "网关 provider：{content}");
        assert!(content.contains("base_url = \"https://gw.example.com/v1\""), "base_url：{content}");
        assert!(content.contains("name = \"tunnel\""), "provider name：{content}");
        assert!(content.contains("WIKI_TUNNEL_LLM_KEY"), "env_key：{content}");
        assert!(content.contains("wire_api = \"responses\""), "wire_api：{content}");
        assert!(content.contains("model = \"gpt-4o\""), "model：{content}");
        assert!(!content.contains("secret-key"), "密钥不应落盘：{content}");
        assert!(content.contains("approval_policy"), "审批策略：{content}");
        assert!(content.contains("sandbox_mode"), "沙箱：{content}");
        // 回读 toml 合法性
        let parsed: toml::Value = content.parse().expect("toml parse");
        assert_eq!(
            parsed.get("model_provider").and_then(|v| v.as_str()),
            Some("tunnel")
        );
    }

    #[test]
    fn config_toml_gateway_mode_wire_api_chat() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("codex-home");
        let settings = AgentSettings {
            enabled: true,
            auth_mode: AuthMode::Gateway,
            gateway_base_url: Some("https://gw.example.com/v1".to_owned()),
            gateway_api_key: Some("secret-key".to_owned()),
            wire_api: WireApi::Chat,
            ..Default::default()
        };
        let path = CodexHome::ensure(&base, &settings).expect("ensure");
        let content = std::fs::read_to_string(&path).expect("read");
        assert!(content.contains("wire_api = \"chat\""), "wire_api：{content}");
        assert!(!content.contains("wire_api = \"responses\""), "wire_api 不应为 responses：{content}");
    }

    #[test]
    fn wire_api_backward_compat_default() {
        // 旧 settings 文件无 wireApi 字段，反序列化应回退 Responses
        let json = r#"{"enabled":true,"authMode":"gateway"}"#;
        let s: AgentSettings = serde_json::from_str(json).expect("de");
        assert_eq!(s.wire_api, WireApi::Responses);
        // 序列化应为 camelCase 的 wireApi
        let v = serde_json::to_value(&s).expect("ser");
        assert_eq!(v.get("wireApi").and_then(serde_json::Value::as_str), Some("responses"));
    }

    #[test]
    fn config_toml_openai_key_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("codex-home");
        let settings = AgentSettings {
            enabled: true,
            auth_mode: AuthMode::OpenaiKey,
            openai_api_key: Some("sk-xxx".to_owned()),
            model: Some("gpt-4o-mini".to_owned()),
            ..Default::default()
        };
        let path = CodexHome::ensure(&base, &settings).expect("ensure");
        let content = std::fs::read_to_string(&path).expect("read");
        assert!(content.contains("model_provider = \"openai\""), "openai provider：{content}");
        assert!(!content.contains("WIKI_TUNNEL_LLM_KEY"), "网关 key 不应出现：{content}");
        assert!(!content.contains("sk-xxx"), "openai key 不应落盘：{content}");
        let parsed: toml::Value = content.parse().expect("toml parse");
        assert_eq!(
            parsed.get("model_provider").and_then(|v| v.as_str()),
            Some("openai")
        );
    }

    #[test]
    fn config_toml_chatgpt_mode_no_provider_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("codex-home");
        let settings = AgentSettings {
            enabled: true,
            auth_mode: AuthMode::ChatGpt,
            ..Default::default()
        };
        let path = CodexHome::ensure(&base, &settings).expect("ensure");
        let content = std::fs::read_to_string(&path).expect("read");
        assert!(!content.contains("WIKI_TUNNEL_LLM_KEY"), "chatgpt 不应有 tunnel key：{content}");
        assert!(!content.contains("OPENAI_API_KEY"), "chatgpt 不应有 openai key：{content}");
        // chatgpt 模式不写 model_provider
        assert!(!content.contains("model_provider"), "chatgpt 不应写 model_provider：{content}");
    }

    #[test]
    fn config_toml_gateway_default_base_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("codex-home");
        let settings = AgentSettings {
            auth_mode: AuthMode::Gateway,
            gateway_base_url: None,
            ..Default::default()
        };
        let err = CodexHome::ensure(&base, &settings).expect_err("缺 gatewayBaseUrl 应报错");
        assert!(
            err.to_string().contains("网关模式需要在设置中填写网关地址"),
            "应提示网关地址缺失，实际：{err}"
        );
    }

    #[test]
    fn child_env_injection() {
        let gw = AgentSettings {
            auth_mode: AuthMode::Gateway,
            gateway_api_key: Some("gw-secret".to_owned()),
            ..Default::default()
        };
        let env = CodexHome::child_env(&gw);
        assert_eq!(env.get("WIKI_TUNNEL_LLM_KEY").map(String::as_str), Some("gw-secret"));
        assert!(!env.contains_key("OPENAI_API_KEY"));

        let oa = AgentSettings {
            auth_mode: AuthMode::OpenaiKey,
            openai_api_key: Some("sk-secret".to_owned()),
            ..Default::default()
        };
        let env2 = CodexHome::child_env(&oa);
        assert_eq!(env2.get("OPENAI_API_KEY").map(String::as_str), Some("sk-secret"));
        assert!(!env2.contains_key("WIKI_TUNNEL_LLM_KEY"));

        let chat = AgentSettings {
            auth_mode: AuthMode::ChatGpt,
            ..Default::default()
        };
        assert!(CodexHome::child_env(&chat).is_empty());
    }

    #[test]
    fn settings_persist_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent-settings.json");
        let original = AgentSettings {
            enabled: true,
            auth_mode: AuthMode::OpenaiKey,
            gateway_base_url: Some("https://gw.example.com".to_owned()),
            gateway_api_key: Some("gw-key".to_owned()),
            openai_api_key: Some("sk-123".to_owned()),
            model: Some("gpt-4o".to_owned()),
            approval_policy: ApprovalPolicy::Never,
            sandbox_mode: SandboxMode::ReadOnly,
            codex_path_override: Some("/usr/local/bin/codex".to_owned()),
            wire_api: WireApi::Chat,
        };
        original.save(&path).expect("save");
        let loaded = AgentSettings::load(&path).expect("load");
        assert_eq!(loaded, original);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "权限应为 0600，实际 {mode:o}");
        }
    }

    #[test]
    fn settings_load_missing_returns_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("missing.json");
        let s = AgentSettings::load(&path).expect("load missing");
        assert_eq!(s, AgentSettings::default());
    }

    #[test]
    fn config_ensure_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("codex-home");
        let s = AgentSettings {
            auth_mode: AuthMode::Gateway,
            gateway_base_url: Some("https://gw.example.com".to_owned()),
            gateway_api_key: Some("k".to_owned()),
            ..AgentSettings::default()
        };
        let p1 = CodexHome::ensure(&base, &s).expect("first");
        let c1 = std::fs::read_to_string(&p1).expect("read1");
        let p2 = CodexHome::ensure(&base, &s).expect("second");
        let c2 = std::fs::read_to_string(&p2).expect("read2");
        assert_eq!(c1, c2);
    }

    #[test]
    fn validate_gateway_modes() {
        // 缺 base_url
        let mut s = AgentSettings {
            auth_mode: AuthMode::Gateway,
            gateway_base_url: None,
            gateway_api_key: Some("k".to_owned()),
            ..AgentSettings::default()
        };
        let e = s.validate().expect_err("缺 base_url 应错");
        assert!(e.contains("网关模式需要在设置中填写网关地址"), "实际：{e}");

        // 空白 base_url 亦错
        s.gateway_base_url = Some("   ".to_owned());
        assert!(s.validate().is_err());

        // 缺 key
        s.gateway_base_url = Some("https://gw.example.com".to_owned());
        s.gateway_api_key = None;
        let e2 = s.validate().expect_err("缺 gateway key 应错");
        assert!(e2.contains("网关模式未配置 API Key"), "实际：{e2}");

        // 空白 key 亦错
        s.gateway_api_key = Some("  ".to_owned());
        assert!(s.validate().is_err());

        s.gateway_api_key = Some("tok".to_owned());
        assert!(s.validate().is_ok());

        // chatgpt 无需 key
        let chat = AgentSettings {
            auth_mode: AuthMode::ChatGpt,
            gateway_base_url: None,
            gateway_api_key: None,
            openai_api_key: None,
            ..AgentSettings::default()
        };
        assert!(chat.validate().is_ok());
    }

    #[test]
    fn validate_openai_key_modes() {
        let mut s = AgentSettings {
            auth_mode: AuthMode::OpenaiKey,
            openai_api_key: None,
            ..AgentSettings::default()
        };
        let e = s.validate().expect_err("缺 openai key 应错");
        assert!(e.contains("OpenAI Key 模式未配置 API Key"), "实际：{e}");
        s.openai_api_key = Some("   ".to_owned());
        assert!(s.validate().is_err());
        s.openai_api_key = Some("sk-xxx".to_owned());
        assert!(s.validate().is_ok());
    }
}
