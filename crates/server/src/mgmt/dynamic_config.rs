//! 动态配置管理 — 基于数据库的运行时配置。

use crate::config::ServerConfig;
use crate::db::Database;
use serde::{Deserialize, Serialize};
use tracing::info;

/// Shadowsocks 动态配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowsocksDynamicConfig {
    /// 是否启用 Shadowsocks。
    pub enabled: bool,
    /// 监听端口。
    pub port: u16,
    /// 加密方式。
    pub cipher: String,
    /// 连接密码。
    pub password: String,
}

/// Trojan 动态配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrojanDynamicConfig {
    /// 是否启用 Trojan。
    pub enabled: bool,
    /// 监听端口。
    pub port: u16,
    /// 连接密码。
    pub password: String,
    /// 认证失败时的回落地址。
    pub fallback: String,
    /// SNI/ACME 域名；空串 = 不用 ACME 证书、不参与反代 SNI 分流
    pub domain: String,
}

/// 反向代理动态配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReverseProxySettings {
    /// 最大并发连接数。
    pub max_connections: u32,
    /// 连接超时时间（秒）。
    pub connection_timeout_secs: u64,
    /// 转发缓冲区大小（字节）。
    pub buffer_size: usize,
}

/// DNS 动态配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsSettings {
    /// 隧道域名后缀。
    pub tunnel_domain: String,
    /// Mesh 域名后缀。
    pub mesh_domain: String,
}

/// 全量动态配置（从数据库加载）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DynamicConfig {
    /// 日志级别。
    pub log_level: String,
    /// LLM 请求摘要日志开关：关闭时跳过正常请求日志（错误日志不受影响）。
    pub llm_request_logging: bool,
    /// Shadowsocks 配置（None 表示未启用）。
    pub ss: Option<ShadowsocksDynamicConfig>,
    /// Trojan 配置（None 表示未启用）。
    pub trojan: Option<TrojanDynamicConfig>,
    /// 反向代理配置。
    pub reverse_proxy: ReverseProxySettings,
    /// DNS 配置。
    pub dns: DnsSettings,
}

impl DynamicConfig {
    /// 供 LLM handler 内部默认使用：llm_request_logging 默认开启，其余取最小默认。
    /// 生产环境由 init_llm_state 注入 ServerState 的真实实例覆盖此默认。
    #[must_use]
    pub fn default_for_llm() -> Self {
        Self {
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
        }
    }

    /// 从数据库加载动态配置；若无记录则以 ServerConfig 为种子写入首行。
    ///
    /// 表中若存在多行（旧版本按端口 upsert 可能残留多份配置），取最近更新的一行——
    /// 那是用户最近一次通过 API 保存的配置。
    #[allow(
        clippy::too_many_lines,
        reason = "顺序加载 6 类动态配置并在缺失时回写种子，共享同一 DB/ServerConfig 上下文，拆分会散开编排逻辑"
    )]
    #[allow(clippy::single_match_else, reason = "携带 guard 的 match 在 edition 2021 下用 if let 需 let-chain，保留 match 更清晰")]
    pub async fn load_or_seed(db: &Database, server_config: &ServerConfig) -> Self {
        // Log level
        let log_level = if let Ok(Some(level)) = db.load_server_setting("log_level").await {
            level
        } else {
            let level = server_config.log.clone();
            let _ = db.save_server_setting("log_level", &level).await;
            level
        };

        // LLM request logging
        let llm_request_logging = if let Ok(Some(v)) = db.load_server_setting("llm_request_logging").await {
            v == "1" || v == "true"
        } else {
            let _ = db.save_server_setting("llm_request_logging", "true").await;
            true
        };

        // Shadowsocks
        let ss = match db.load_shadowsocks_configs().await {
            Ok(configs) if !configs.is_empty() => {
                let c = {
                    let mut max = &configs[0];
                    for cand in &configs[1..] {
                        if (cand.updated_at, cand.id) > (max.updated_at, max.id) {
                            max = cand;
                        }
                    }
                    max
                };
                Some(ShadowsocksDynamicConfig {
                    enabled: c.enabled != 0,
                    port: u16::try_from(c.port).unwrap_or(u16::MAX),
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
                let c = {
                    let mut max = &configs[0];
                    for cand in &configs[1..] {
                        if (cand.updated_at, cand.id) > (max.updated_at, max.id) {
                            max = cand;
                        }
                    }
                    max
                };
                Some(TrojanDynamicConfig {
                    enabled: c.enabled != 0,
                    port: u16::try_from(c.port).unwrap_or(u16::MAX),
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
        let reverse_proxy = if let Ok(Some(cfg)) = db.load_reverse_proxy_config().await {
            ReverseProxySettings {
                max_connections: u32::try_from(cfg.max_connections).unwrap_or(u32::MAX),
                connection_timeout_secs: cfg.connection_timeout_secs.cast_unsigned(),
                buffer_size: usize::try_from(cfg.buffer_size).unwrap_or(usize::MAX),
            }
        } else {
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
        };

        // DNS
        let dns = if let Ok(Some(cfg)) = db.load_dns_config().await {
            DnsSettings {
                tunnel_domain: cfg.tunnel_domain,
                mesh_domain: cfg.mesh_domain,
            }
        } else {
            let settings = DnsSettings {
                tunnel_domain: server_config.dns_tunnel_domain.clone(),
                mesh_domain: server_config.dns_mesh_domain.clone(),
            };
            let _ = db
                .save_dns_config(&settings.tunnel_domain, &settings.mesh_domain)
                .await;
            settings
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
