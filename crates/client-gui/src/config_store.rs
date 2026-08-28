//! 配置持久化与钥匙串封装（GUI 侧）。

use std::path::PathBuf;

use rust_tunnel_client::ClientConfig;

/// 配置存储错误。
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// 文件写入失败。
    #[error("保存配置失败: {0}")]
    Save(String),
    /// 钥匙串读写失败。
    #[error("钥匙串错误: {0}")]
    Keyring(String),
}

/// 默认配置文件路径（与 `rust_tunnel_client::config::default_config_path` 一致，供 GUI 设置面板展示）。
#[must_use]
pub fn default_path() -> PathBuf {
    rust_tunnel_client::config::default_config_path()
}

/// 从默认路径加载配置；文件不存在时返回 `Ok(None)`。
#[must_use]
pub fn load_from_default_path() -> Option<ClientConfig> {
    let path = default_path();
    if !path.exists() {
        return None;
    }
    // 通过 CLI 解析的 from_cli 复用 TOML 合并：构造一个仅带 config_file 的 ClientCli
    let cli = rust_tunnel_client::config::ClientCli {
        config_file: Some(path.to_string_lossy().to_string()),
        server: None,
        name: None,
        password: None,
        tls: None,
        tls_server_name: None,
        tls_insecure: None,
        mesh: None,
        mesh_name: None,
        mesh_services: Vec::new(),
        enable_agent: false,
        agent_pty_port: None,
        log: None,
    };
    ClientConfig::from_cli(cli).ok()
}

/// 原子写入配置到默认路径。
///
/// # Errors
/// 当序列化或文件写入失败时返回 `Err`。
pub fn save_to_default_path(config: &ClientConfig) -> Result<(), StoreError> {
    let path = default_path();
    config
        .save_to_path(&path)
        .map_err(StoreError::Save)
}

/// 钥匙串 service 固定为 `rust-tunnel`。
const KEYRING_SERVICE: &str = "rust-tunnel";

fn keyring_account(server: &str, client_name: &str) -> String {
    format!("{server}/{client_name}")
}

/// 从系统钥匙串读取密码；不可用或不存在时返回 `None`（回退明文）。
#[must_use]
pub fn read_password_from_keyring(server: &str, client_name: &str) -> Option<String> {
    let account = keyring_account(server, client_name);
    let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &account) else {
        return None;
    };
    entry.get_password().ok()
}

/// 将密码写入钥匙串。
///
/// # Errors
/// 当钥匙串不可用或写入失败时返回 `Err`。
pub fn write_password_to_keyring(
    server: &str,
    client_name: &str,
    password: &str,
) -> Result<(), StoreError> {
    let account = keyring_account(server, client_name);
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, &account).map_err(|e| StoreError::Keyring(e.to_string()))?;
    entry
        .set_password(password)
        .map_err(|e| StoreError::Keyring(e.to_string()))
}

/// 从钥匙串删除条目（忽略不存在）。
#[allow(dead_code, reason = "设置面板清空密码时使用")]
pub fn delete_password_from_keyring(server: &str, client_name: &str) {
    let account = keyring_account(server, client_name);
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &account) {
        let _ = entry.delete_credential();
    }
}
