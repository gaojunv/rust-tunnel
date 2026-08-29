use clap::Parser;
use figment::{
    providers::{Format, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Client endpoint for rust-tunnel intranet penetration tool
#[derive(Parser, Debug, Clone)]
pub struct ClientCli {
    /// Path to configuration file (TOML format)
    #[clap(long = "config")]
    pub config_file: Option<String>,

    /// Server address to connect (e.g., server-ip:8080)
    #[clap(long)]
    pub server: Option<String>,

    /// Client name (default: system hostname)
    #[clap(long = "name")]
    pub name: Option<String>,

    /// Password (a.k.a. auth token) sent to the server
    #[clap(long = "password")]
    pub password: Option<String>,

    /// Enable TLS encryption for control channel (should match server setting)
    #[clap(long = "tls")]
    pub tls: Option<bool>,

    /// TLS server name for SNI (Server Name Indication)
    /// If not provided, the hostname from server address will be used
    #[clap(long = "tls-server-name")]
    pub tls_server_name: Option<String>,

    /// Skip TLS certificate verification (insecure, for self-signed certs)
    /// This enables TOFU (Trust On First Use) mode for self-signed certificates
    #[clap(long = "tls-insecure")]
    pub tls_insecure: Option<bool>,

    /// Mesh network ID to join
    #[clap(long = "mesh")]
    pub mesh: Option<String>,

    /// Mesh client display name in the mesh network
    #[clap(long = "mesh-name")]
    pub mesh_name: Option<String>,

    /// Mesh service definitions: NAME:PROTOCOL:LOCAL_ADDR
    /// Example: db:mysql:localhost:3306
    #[clap(long = "mesh-service", action = clap::ArgAction::Append)]
    pub mesh_services: Vec<String>,

    /// Enable agent executor: accept AgentExecRequest from server to run
    /// shell/file/git commands sandboxed in a workspace directory
    #[clap(long)]
    pub enable_agent: bool,

    /// Agent PTY service loopback port (default: 45631); only used with --enable-agent
    #[clap(long = "agent-pty-port")]
    pub agent_pty_port: Option<u16>,

    /// Log level (trace, debug, info, warn, error)
    #[clap(long)]
    pub log: Option<String>,
}

/// 从 TOML 配置文件解析的客户端配置（全部字段可选，未填则回落至默认值/环境变量/CLI）。
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct ClientConfigFile {
    /// 服务端地址（`host:port`）
    pub server: Option<String>,
    /// 客户端名称
    pub name: Option<String>,
    /// 认证口令
    pub password: Option<String>,
    /// 是否启用 TLS
    pub tls: Option<bool>,
    /// TLS SNI 名称
    pub tls_server_name: Option<String>,
    /// 是否跳过证书校验（TOFU 模式）
    pub tls_insecure: Option<bool>,
    /// Mesh 网络 ID
    pub mesh: Option<String>,
    /// Mesh 内显示名称
    pub mesh_name: Option<String>,
    /// Mesh 服务定义列表
    pub mesh_services: Option<Vec<String>>,
    /// 是否启用 agent 执行器
    pub enable_agent: Option<bool>,
    /// Agent PTY 回环端口
    pub agent_pty_port: Option<u16>,
    /// 日志级别
    pub log: Option<String>,
}

/// 归一化后的客户端配置（必填字段已校验，含默认值填充）。
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// 服务端地址（`host:port`），必填
    pub server: String,
    /// 客户端名称，未指定时回落至系统 hostname
    pub name: Option<String>,
    /// 认证口令，必填
    pub password: String,
    /// Mesh network ID to join
    pub mesh: Option<String>,
    /// Mesh client display name in the mesh network
    pub mesh_name: Option<String>,
    /// Mesh service definitions: NAME:PROTOCOL:LOCAL_ADDR
    pub mesh_services: Vec<String>,
    /// Enable TLS encryption for control channel
    pub tls: bool,
    /// TLS server name for SNI
    pub tls_server_name: Option<String>,
    /// Skip TLS certificate verification (insecure, for self-signed certs)
    pub tls_insecure: bool,
    /// Enable the agent executor (accept AgentExecRequest from server)
    pub enable_agent: bool,
    /// Agent PTY service loopback port
    pub agent_pty_port: u16,
    /// 日志级别字符串
    pub log: String,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            server: String::new(),
            name: None,
            password: String::new(),
            mesh: None,
            mesh_name: None,
            mesh_services: Vec::new(),
            tls: true, // TLS enabled by default for security
            tls_server_name: None,
            tls_insecure: true, // Accept self-signed certs by default (TOFU mode)
            enable_agent: false,
            agent_pty_port: crate::pty::DEFAULT_PTY_PORT,
            log: "info".to_string(),
        }
    }
}

impl ClientConfig {
    /// 从命令行与环境加载客户端配置（按三级优先级合并）。
    ///
    /// # Errors
    ///
    /// 当配置文件不存在或 TOML 解析失败时返回 `Err`；当必填字段 `server`/`password` 为空时也返回 `Err`。
    pub fn load() -> Result<Self, String> {
        let cli = ClientCli::parse();
        Self::from_cli(cli)
    }

    /// 按三级优先级（配置文件 → 环境变量 → CLI 参数）合并为归一化配置。
    ///
    /// # Errors
    ///
    /// 当指定的配置文件不存在或解析失败时返回 `Err`；当 `server` 或 `password` 最终为空时返回 `Err`。
    #[allow(
        clippy::too_many_lines,
        reason = "三级配置叠加（文件→环境变量→CLI）的顺序编排，含大量字段逐一覆盖分支，拆分无益"
    )]
    pub fn from_cli(cli: ClientCli) -> Result<Self, String> {
        let mut config = Self::default();

        // 1. Load from config file if specified
        if let Some(config_path) = &cli.config_file {
            if Path::new(config_path).exists() {
                let file_config: ClientConfigFile = Figment::new()
                    .merge(Toml::file(config_path))
                    .extract()
                    .map_err(|e| format!("Failed to parse config file: {e}"))?;

                if let Some(v) = file_config.server {
                    config.server = v;
                }
                if let Some(v) = file_config.name {
                    config.name = Some(v);
                }
                if let Some(v) = file_config.password {
                    config.password = v;
                }
                if let Some(v) = file_config.tls {
                    config.tls = v;
                }
                if let Some(v) = file_config.tls_server_name {
                    config.tls_server_name = Some(v);
                }
                if let Some(v) = file_config.tls_insecure {
                    config.tls_insecure = v;
                }
                if let Some(v) = file_config.mesh {
                    config.mesh = Some(v);
                }
                if let Some(v) = file_config.mesh_name {
                    config.mesh_name = Some(v);
                }
                if let Some(v) = file_config.mesh_services {
                    config.mesh_services = v;
                }
                if let Some(v) = file_config.log {
                    config.log = v;
                }
                if let Some(v) = file_config.enable_agent {
                    config.enable_agent = v;
                }
                if let Some(v) = file_config.agent_pty_port {
                    config.agent_pty_port = v;
                }
            } else {
                return Err(format!("Config file not found: {config_path}"));
            }
        }

        // 2. Load from environment variables (override file)
        if let Ok(v) = std::env::var("SERVER_ADDR") {
            config.server = v;
        }
        if let Ok(v) = std::env::var("NAME") {
            config.name = Some(v);
        }
        if let Ok(v) = std::env::var("PASSWORD") {
            config.password = v;
        }
        if let Ok(v) = std::env::var("TLS") {
            config.tls = v.to_lowercase() == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("TLS_SERVER_NAME") {
            config.tls_server_name = Some(v);
        }
        if let Ok(v) = std::env::var("TLS_INSECURE") {
            config.tls_insecure = v.to_lowercase() == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("LOG_LEVEL") {
            config.log = v;
        }
        if let Ok(v) = std::env::var("MESH_ID") {
            config.mesh = Some(v);
        }
        if let Ok(v) = std::env::var("MESH_NAME") {
            config.mesh_name = Some(v);
        }
        if let Ok(v) = std::env::var("MESH_SERVICES") {
            config.mesh_services = v.split(',').map(|s| s.trim().to_string()).collect();
        }

        // 3. Command line arguments (highest priority)
        if let Some(v) = cli.server {
            config.server = v;
        }
        if let Some(v) = cli.name {
            config.name = Some(v);
        }
        if let Some(v) = cli.password {
            config.password = v;
        }
        if let Some(v) = cli.tls {
            config.tls = v;
        }
        if let Some(v) = cli.tls_server_name {
            config.tls_server_name = Some(v);
        }
        if let Some(v) = cli.tls_insecure {
            config.tls_insecure = v;
        }
        if let Some(v) = cli.log {
            config.log = v;
        }
        if let Some(v) = cli.mesh {
            config.mesh = Some(v);
        }
        if let Some(v) = cli.mesh_name {
            config.mesh_name = Some(v);
        }
        if !cli.mesh_services.is_empty() {
            config.mesh_services = cli.mesh_services;
        }
        // CLI flag can only enable (there is no --disable-agent); a file-set
        // true value is preserved when the flag is absent.
        config.enable_agent = config.enable_agent || cli.enable_agent;
        if let Some(v) = cli.agent_pty_port {
            config.agent_pty_port = v;
        }

        // Validate required fields
        if config.server.is_empty() {
            return Err(
                "Server address is required. Use --server, SERVER_ADDR env, or set in config file"
                    .to_string(),
            );
        }
        if config.password.is_empty() {
            return Err(
                "Password is required. Use --password, PASSWORD env, or set in config file"
                    .to_string(),
            );
        }

        Ok(config)
    }

    /// 转换为可序列化的文件配置。
    #[must_use]
    pub fn to_file_config(&self) -> ClientConfigFile {
        ClientConfigFile {
            server: Some(self.server.clone()),
            name: self.name.clone(),
            password: Some(self.password.clone()),
            tls: Some(self.tls),
            tls_server_name: self.tls_server_name.clone(),
            tls_insecure: Some(self.tls_insecure),
            mesh: self.mesh.clone(),
            mesh_name: self.mesh_name.clone(),
            mesh_services: Some(self.mesh_services.clone()),
            enable_agent: Some(self.enable_agent),
            agent_pty_port: Some(self.agent_pty_port),
            log: Some(self.log.clone()),
        }
    }

    /// 原子写入 TOML 配置（临时文件 + rename，避免崩溃写半截）。
    ///
    /// # Errors
    /// 当序列化或文件写入失败时返回 `Err`。
    pub fn save_to_path(&self, path: &Path) -> Result<(), String> {
        let file_cfg = self.to_file_config();
        let toml_str = toml::to_string_pretty(&file_cfg)
            .map_err(|e| format!("Failed to serialize config: {e}"))?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create config dir: {e}"))?;
            }
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml_str).map_err(|e| format!("Failed to write config: {e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("Failed to rename config: {e}"))?;
        Ok(())
    }
}

/// 平台相关的默认配置文件路径：
///
/// - macOS: `~/Library/Application Support/rust-tunnel/client.toml`
/// - Windows: `%APPDATA%\rust-tunnel\client.toml`
/// - Linux: `$XDG_CONFIG_HOME/rust-tunnel/client.toml` 或 `~/.config/rust-tunnel/client.toml`
#[must_use]
pub fn default_config_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join("Library/Application Support/rust-tunnel/client.toml");
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata).join("rust-tunnel/client.toml");
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("rust-tunnel/client.toml");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".config/rust-tunnel/client.toml");
    }
    PathBuf::from("client.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ClientConfig::default();
        assert_eq!(config.server, "");
        assert!(config.password.is_empty());
        assert!(config.name.is_none());
        assert!(config.tls); // TLS enabled by default
        assert!(config.tls_insecure); // Accept self-signed certs by default
        assert_eq!(config.agent_pty_port, crate::pty::DEFAULT_PTY_PORT);
        assert_eq!(config.log, "info");
    }

    #[test]
    fn test_config_from_cli_no_file() {
        let cli = ClientCli {
            config_file: None,
            server: Some("localhost:8080".to_string()),
            name: Some("test-client".to_string()),
            password: Some("secret-token".to_string()),
            tls: Some(true),
            tls_server_name: Some("tunnel.example.com".to_string()),
            tls_insecure: Some(true),
            mesh: None,
            mesh_name: None,
            mesh_services: vec![],
            enable_agent: false,
            agent_pty_port: None,
            log: Some("debug".to_string()),
        };

        let config = ClientConfig::from_cli(cli).unwrap();
        assert_eq!(config.server, "localhost:8080");
        assert_eq!(config.name, Some("test-client".to_string()));
        assert_eq!(config.password, "secret-token");
        assert!(config.tls);
        assert_eq!(
            config.tls_server_name,
            Some("tunnel.example.com".to_string())
        );
        assert!(config.tls_insecure);
        assert_eq!(config.log, "debug");
    }

    #[test]
    fn test_enable_agent_flag() {
        let cli = ClientCli {
            config_file: None,
            server: Some("host:8080".into()),
            name: None,
            password: Some("pw".into()),
            tls: None,
            tls_server_name: None,
            tls_insecure: None,
            mesh: None,
            mesh_name: None,
            mesh_services: vec![],
            enable_agent: true,
            agent_pty_port: None,
            log: None,
        };
        let cfg = ClientConfig::from_cli(cli).unwrap();
        assert!(cfg.enable_agent);
    }

    #[test]
    fn test_enable_agent_default_false() {
        // Flag absent and no file → defaults to false
        let cli = ClientCli {
            config_file: None,
            server: Some("host:8080".into()),
            name: None,
            password: Some("pw".into()),
            tls: None,
            tls_server_name: None,
            tls_insecure: None,
            mesh: None,
            mesh_name: None,
            mesh_services: vec![],
            enable_agent: false,
            agent_pty_port: None,
            log: None,
        };
        let cfg = ClientConfig::from_cli(cli).unwrap();
        assert!(!cfg.enable_agent);
    }

    #[test]
    fn test_config_missing_server() {
        let cli = ClientCli {
            config_file: None,
            server: None,
            name: None,
            password: None,
            tls: None,
            tls_server_name: None,
            tls_insecure: None,
            mesh: None,
            mesh_name: None,
            mesh_services: vec![],
            enable_agent: false,
            agent_pty_port: None,
            log: None,
        };

        let result = ClientConfig::from_cli(cli);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Server address is required"));
    }

    #[test]
    fn test_password_required() {
        // Isolate from any PASSWORD env var set at the shell level
        let saved = std::env::var("PASSWORD").ok();
        if saved.is_some() {
            std::env::remove_var("PASSWORD");
        }

        let cli = ClientCli {
            config_file: None,
            server: Some("host:8080".into()),
            password: None,
            name: None,
            tls: None,
            tls_server_name: None,
            tls_insecure: None,
            mesh: None,
            mesh_name: None,
            mesh_services: vec![],
            enable_agent: false,
            agent_pty_port: None,
            log: None,
        };
        let err = ClientConfig::from_cli(cli).unwrap_err();
        assert!(err.contains("password"));

        // Restore env var if it was set
        if let Some(v) = saved {
            std::env::set_var("PASSWORD", v);
        }
    }

    #[test]
    fn test_name_defaults_to_none_in_from_cli() {
        // If name absent, from_cli leaves it None; run_client is where hostname
        // is resolved. Assert None here.
        let cli = ClientCli {
            config_file: None,
            server: Some("host:8080".into()),
            password: Some("pw".into()),
            name: None,
            tls: None,
            tls_server_name: None,
            tls_insecure: None,
            mesh: None,
            mesh_name: None,
            mesh_services: vec![],
            enable_agent: false,
            agent_pty_port: None,
            log: None,
        };
        let cfg = ClientConfig::from_cli(cli).unwrap();
        assert!(cfg.name.is_none());
        assert_eq!(cfg.password, "pw");
    }

    #[test]
    fn test_config_clone() {
        let config = ClientConfig {
            server: "localhost:8080".into(),
            name: Some("my-client".to_string()),
            password: "secret".to_string(),
            tls: true,
            tls_server_name: Some("test-server".to_string()),
            tls_insecure: true,
            mesh: Some("mesh-net".to_string()),
            mesh_name: Some("my-client".to_string()),
            mesh_services: vec!["db:mysql:localhost:3306".to_string()],
            enable_agent: true,
            agent_pty_port: 45631,
            log: "debug".into(),
        };

        let cloned = config.clone();
        assert_eq!(config.server, cloned.server);
        assert_eq!(config.name, cloned.name);
        assert_eq!(config.password, cloned.password);
        assert_eq!(config.tls, cloned.tls);
        assert_eq!(config.tls_server_name, cloned.tls_server_name);
        assert_eq!(config.tls_insecure, cloned.tls_insecure);
        assert_eq!(config.mesh, cloned.mesh);
        assert_eq!(config.mesh_name, cloned.mesh_name);
        assert_eq!(config.mesh_services, cloned.mesh_services);
        assert_eq!(config.enable_agent, cloned.enable_agent);
        assert_eq!(config.agent_pty_port, cloned.agent_pty_port);
        assert_eq!(config.log, cloned.log);
    }

    #[test]
    fn test_agent_pty_port_override() {
        // CLI --agent-pty-port 覆盖默认端口
        let cli = ClientCli {
            config_file: None,
            server: Some("host:8080".into()),
            name: None,
            password: Some("pw".into()),
            tls: None,
            tls_server_name: None,
            tls_insecure: None,
            mesh: None,
            mesh_name: None,
            mesh_services: vec![],
            enable_agent: true,
            agent_pty_port: Some(9999),
            log: None,
        };
        let cfg = ClientConfig::from_cli(cli).unwrap();
        assert_eq!(cfg.agent_pty_port, 9999);

        // 未指定时回落到默认端口
        let default_cfg = ClientConfig::default();
        assert_eq!(default_cfg.agent_pty_port, crate::pty::DEFAULT_PTY_PORT);
    }

    #[test]
    fn test_config_file_not_found() {
        let cli = ClientCli {
            config_file: Some("/nonexistent/config.toml".to_string()),
            server: Some("test".to_string()),
            name: None,
            password: None,
            tls: None,
            tls_server_name: None,
            tls_insecure: None,
            mesh: None,
            mesh_name: None,
            mesh_services: vec![],
            enable_agent: false,
            agent_pty_port: None,
            log: None,
        };

        let result = ClientConfig::from_cli(cli);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }
}
