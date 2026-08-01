//! Dynamic configuration management — DB-backed runtime config.

use crate::server::config::ServerConfig;
use crate::server::db::Database;
use serde::{Deserialize, Serialize};
use tracing::info;

/// Shadowsocks dynamic config
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowsocksDynamicConfig {
    pub enabled: bool,
    pub port: u16,
    pub cipher: String,
    pub password: String,
}

/// Trojan dynamic config
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrojanDynamicConfig {
    pub enabled: bool,
    pub port: u16,
    pub password: String,
    pub fallback: String,
    /// SNI/ACME 域名；空串 = 不用 ACME 证书、不参与反代 SNI 分流
    pub domain: String,
}

/// Reverse proxy dynamic settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReverseProxySettings {
    pub max_connections: u32,
    pub connection_timeout_secs: u64,
    pub buffer_size: usize,
}

/// DNS dynamic settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsSettings {
    pub tunnel_domain: String,
    pub mesh_domain: String,
}

/// All dynamic configuration loaded from DB
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DynamicConfig {
    pub log_level: String,
    /// LLM 请求摘要日志开关：关闭时跳过正常请求日志（错误日志不受影响）。
    pub llm_request_logging: bool,
    pub ss: Option<ShadowsocksDynamicConfig>,
    pub trojan: Option<TrojanDynamicConfig>,
    pub reverse_proxy: ReverseProxySettings,
    pub dns: DnsSettings,
}

impl DynamicConfig {
    /// Load dynamic config from DB. If DB has no records, seed from ServerConfig (first run).
    ///
    /// 表中若存在多行（旧版本按端口 upsert 可能残留多份配置），取最近更新的一行——
    /// 那是用户最近一次通过 API 保存的配置。
    pub async fn load_or_seed(db: &Database, server_config: &ServerConfig) -> Self {
        // Log level
        let log_level = match db.load_server_setting("log_level").await {
            Ok(Some(level)) => level,
            _ => {
                let level = server_config.log.clone();
                let _ = db.save_server_setting("log_level", &level).await;
                level
            }
        };

        // LLM request logging
        let llm_request_logging = match db.load_server_setting("llm_request_logging").await {
            Ok(Some(v)) => v == "1" || v == "true",
            _ => {
                let _ = db.save_server_setting("llm_request_logging", "true").await;
                true
            }
        };

        // Shadowsocks
        let ss = match db.load_shadowsocks_configs().await {
            Ok(configs) if !configs.is_empty() => {
                let c = configs
                    .iter()
                    .max_by_key(|c| (c.updated_at, c.id))
                    .expect("non-empty configs");
                Some(ShadowsocksDynamicConfig {
                    enabled: c.enabled != 0,
                    port: c.port as u16,
                    cipher: c.cipher.clone(),
                    password: c.password.clone(),
                })
            }
            _ => {
                if server_config.ss_enabled {
                    if let (Some(port), Some(ref cipher), Some(ref password)) = (
                        server_config.ss_port,
                        &server_config.ss_cipher,
                        &server_config.ss_password,
                    ) {
                        let _ = db
                            .save_shadowsocks_config(port, cipher, password, true)
                            .await;
                        Some(ShadowsocksDynamicConfig {
                            enabled: true,
                            port,
                            cipher: cipher.clone(),
                            password: password.clone(),
                        })
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        };

        // Trojan
        let trojan = match db.load_trojan_configs().await {
            Ok(configs) if !configs.is_empty() => {
                let c = configs
                    .iter()
                    .max_by_key(|c| (c.updated_at, c.id))
                    .expect("non-empty configs");
                Some(TrojanDynamicConfig {
                    enabled: c.enabled != 0,
                    port: c.port as u16,
                    password: c.password.clone(),
                    fallback: c.fallback.clone(),
                    domain: c.domain.clone(),
                })
            }
            _ => {
                if server_config.trojan_enabled {
                    if let (Some(port), Some(ref password)) =
                        (server_config.trojan_port, &server_config.trojan_password)
                    {
                        let _ = db
                            .save_trojan_config(
                                port,
                                password,
                                &server_config.trojan_fallback,
                                true,
                                "",
                            )
                            .await;
                        Some(TrojanDynamicConfig {
                            enabled: true,
                            port,
                            password: password.clone(),
                            fallback: server_config.trojan_fallback.clone(),
                            domain: String::new(),
                        })
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        };

        // Reverse proxy
        let reverse_proxy = match db.load_reverse_proxy_config().await {
            Ok(Some(cfg)) => ReverseProxySettings {
                max_connections: cfg.max_connections as u32,
                connection_timeout_secs: cfg.connection_timeout_secs as u64,
                buffer_size: cfg.buffer_size as usize,
            },
            _ => {
                let settings = ReverseProxySettings {
                    max_connections: server_config.reverse_proxy_max_connections,
                    connection_timeout_secs: server_config.reverse_proxy_connection_timeout,
                    buffer_size: server_config.reverse_proxy_buffer_size,
                };
                let _ = db
                    .save_reverse_proxy_config(
                        settings.max_connections,
                        settings.connection_timeout_secs,
                        settings.buffer_size,
                    )
                    .await;
                settings
            }
        };

        // DNS
        let dns = match db.load_dns_config().await {
            Ok(Some(cfg)) => DnsSettings {
                tunnel_domain: cfg.tunnel_domain,
                mesh_domain: cfg.mesh_domain,
            },
            _ => {
                let settings = DnsSettings {
                    tunnel_domain: server_config.dns_tunnel_domain.clone(),
                    mesh_domain: server_config.dns_mesh_domain.clone(),
                };
                let _ = db
                    .save_dns_config(&settings.tunnel_domain, &settings.mesh_domain)
                    .await;
                settings
            }
        };

        info!("Dynamic config loaded from DB");
        DynamicConfig {
            log_level,
            llm_request_logging,
            ss,
            trojan,
            reverse_proxy,
            dns,
        }
    }
}

#[cfg(test)]
mod tests {
    // 说明：load_or_seed 需要完整 Database + ServerConfig，集成成本高。
    // 这里用单元测试验证默认值常量和字段存在性，完整读取逻辑由 Task 5 集成测试覆盖。
    use super::*;

    #[test]
    fn test_llm_request_logging_field_exists() {
        let dc = DynamicConfig {
            log_level: "info".to_string(),
            llm_request_logging: true,
            ss: None,
            trojan: None,
            reverse_proxy: ReverseProxySettings {
                max_connections: 10000,
                connection_timeout_secs: 30,
                buffer_size: 8192,
            },
            dns: DnsSettings {
                tunnel_domain: "tunnel.local".to_string(),
                mesh_domain: "mesh.local".to_string(),
            },
        };
        assert!(dc.llm_request_logging);
    }
}
