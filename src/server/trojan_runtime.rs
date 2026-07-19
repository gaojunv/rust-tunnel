//! Trojan 启动/重启统一入口。
//!
//! 三处调用方共用 `apply_trojan_config`：
//! - `src/bin/server.rs` 启动路径
//! - API `POST /api/trojan` 更新路径
//! - 反代规则增删后的 `sync_trojan_mode` 联动
//!
//! 职责：证书解析（ACME 精确 → 一层通配 → 自签名回退并告警）、
//! 共享/独立监听模式判定与切换、ACME 证书热更新订阅。

use std::sync::Arc;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::common::error::TunnelResult;
use crate::common::tls::{create_server_config, load_or_generate_cert};
use crate::server::acme::manager::{CertEvent, CertificateManager};
use crate::server::acme::provider::CertCoverage;
use crate::server::acme::CertificateProvider;
use crate::server::control::ServerState;
use crate::server::dynamic_config::TrojanDynamicConfig;
use crate::server::reverse_proxy::sni_resolver::wildcard_for;
use crate::server::reverse_proxy::TrojanSniEntry;

/// Trojan 证书来源（与 API 响应 `cert_source` 字段一一对应）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrojanCertSource {
    AcmeExact,
    AcmeWildcard,
    SelfSigned,
}

impl TrojanCertSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AcmeExact => "acme_exact",
            Self::AcmeWildcard => "acme_wildcard",
            Self::SelfSigned => "self_signed",
        }
    }
}

/// 解析 Trojan 使用的 TLS 配置：ACME 精确/通配优先，未命中回退自签名并告警。
pub async fn resolve_trojan_tls(
    cert_manager: Option<&Arc<CertificateManager>>,
    domain: &str,
    tls_cert_path: &str,
    tls_key_path: &str,
) -> TunnelResult<(Arc<rustls::ServerConfig>, TrojanCertSource)> {
    if !domain.is_empty() {
        match cert_manager {
            Some(mgr) => match mgr.get_tls_server_config_covering(domain).await {
                Some((cfg, CertCoverage::Exact)) => {
                    info!("Trojan 使用 ACME 证书（精确匹配 {}）", domain);
                    return Ok((cfg, TrojanCertSource::AcmeExact));
                }
                Some((cfg, CertCoverage::Wildcard(pattern))) => {
                    info!("Trojan 复用 ACME 通配符证书 {} 覆盖 {}", pattern, domain);
                    return Ok((cfg, TrojanCertSource::AcmeWildcard));
                }
                None => {
                    warn!(
                        "Trojan 域名 {} 无匹配 ACME 证书（含通配符），回退自签名证书",
                        domain
                    );
                }
            },
            None => {
                warn!(
                    "cert_manager 未初始化，Trojan 域名 {} 回退自签名证书",
                    domain
                );
            }
        }
    }
    let pair = load_or_generate_cert(tls_cert_path, tls_key_path)?;
    let config = create_server_config(pair)?;
    Ok((config, TrojanCertSource::SelfSigned))
}

/// 订阅证书事件：当签发/续期的证书覆盖 trojan 域名（精确或通配）时，
/// 把新 TLS 配置推入 watch channel。仅 ACME 模式下调用。
pub fn spawn_trojan_cert_reload(
    cert_manager: Arc<CertificateManager>,
    trojan_domain: String,
    tls_config_tx: watch::Sender<Arc<rustls::ServerConfig>>,
    mut abort_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut cert_rx = cert_manager.subscribe();
        loop {
            tokio::select! {
                _ = abort_rx.changed() => {
                    info!("Trojan 证书热更新任务退出");
                    return;
                }
                event = cert_rx.recv() => {
                    match event {
                        Ok(CertEvent::Issued { domain }) | Ok(CertEvent::Renewed { domain }) => {
                            let d = domain.to_ascii_lowercase();
                            let covers = d == trojan_domain
                                || wildcard_for(&trojan_domain).as_deref() == Some(d.as_str());
                            if covers {
                                if let Some(cfg) = cert_manager.get_tls_server_config(&d).await {
                                    info!("Trojan TLS 配置热更新（证书 {}）", d);
                                    let _ = tls_config_tx.send(cfg);
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!("Trojan 证书事件订阅滞后 {} 条", n);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
            }
        }
    })
}

/// 应用一份 Trojan 配置：停旧 → 判定模式 → 解析证书 → 启动（共享注册或独立监听）。
///
/// 共享模式成立条件：`enabled && domain 非空 && trojan.port 上存在启用 TLS 的
/// 反代 HTTP 规则`。共享模式下 Trojan 不 bind 端口、不 `register_trojan`，
/// 只向 `ReverseProxyState` 注册 SNI 分流表项。
pub async fn apply_trojan_config(
    state: &ServerState,
    cfg: &TrojanDynamicConfig,
) -> Result<(), String> {
    // 1. 停掉现有实例（独立 listener 与证书热更新任务共享同一个 abort channel）
    {
        let mut abort = state.trojan_listener_abort.write().await;
        if let Some(tx) = abort.take() {
            let _ = tx.send(true);
        }
    }
    state.proxy_state.set_trojan_sni(None);
    // 等旧 listener 释放端口注册/绑定（沿用现有 API 更新路径的等待时长）
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    if !cfg.enabled {
        *state.trojan_runtime.write().await = Default::default();
        return Ok(());
    }

    // 2. 模式判定
    let shared_listen_addr = if cfg.domain.is_empty() {
        None
    } else {
        match state.proxy_state.http_listen_addr_for_port(cfg.port).await {
            Some((addr, true)) => Some(addr),
            _ => None,
        }
    };

    // 3. 证书解析（ACME 优先，回退自签名）
    let domain_lc = cfg.domain.to_ascii_lowercase();
    let (tls_config, source) = resolve_trojan_tls(
        state.cert_manager.as_ref(),
        &domain_lc,
        &state.tls_cert_path,
        &state.tls_key_path,
    )
    .await
    .map_err(|e| format!("Trojan TLS 配置解析失败: {e}"))?;

    let (tls_config_tx, tls_config_rx) = watch::channel(tls_config);
    let (abort_tx, abort_rx) = watch::channel(false);
    *state.trojan_listener_abort.write().await = Some(abort_tx);

    // 4. ACME 证书热更新订阅（自签名模式无需订阅）
    if !domain_lc.is_empty() && source != TrojanCertSource::SelfSigned {
        if let Some(mgr) = state.cert_manager.clone() {
            spawn_trojan_cert_reload(mgr, domain_lc.clone(), tls_config_tx, abort_rx.clone());
        }
    }

    // 5. 记录运行时状态（GET /api/trojan 读取）
    {
        let mut rt = state.trojan_runtime.write().await;
        rt.cert_source = Some(source.as_str().to_string());
        rt.shared = shared_listen_addr.is_some();
    }

    match shared_listen_addr {
        Some(addr) => {
            info!("Trojan 共享模式：SNI {} 复用反代监听 {}", domain_lc, addr);
            state.proxy_state.set_trojan_sni(Some(TrojanSniEntry {
                domain: domain_lc,
                listen_addr: addr.clone(),
                trojan_port: cfg.port,
                password: cfg.password.clone(),
                fallback: cfg.fallback.clone(),
                tls_config_rx,
                state: state.clone(),
            }));
            // 确保共享监听器已按当前规则 reconcile（幂等，规则无变化时为热替换快路径）
            if let Err(e) = state.proxy_state.reconcile_http_listener(&addr).await {
                warn!("Trojan 共享模式 reconcile 失败: {}", e);
            }
            Ok(())
        }
        None => {
            let state_clone = state.clone();
            let port = cfg.port;
            let password = cfg.password.clone();
            let fallback = cfg.fallback.clone();
            tokio::spawn(async move {
                if let Err(e) = crate::server::listener::start_trojan_listener_with_abort(
                    state_clone,
                    port,
                    password,
                    fallback,
                    tls_config_rx,
                    abort_rx,
                )
                .await
                {
                    tracing::error!("Trojan listener error: {}", e);
                }
            });
            Ok(())
        }
    }
}

/// 反代规则变更后调用：若 Trojan 期望模式与实际模式不一致则重新应用配置。
/// 覆盖两条边界规则：
/// - 共享 listener 被删除/禁用 → 回退独立监听（端口空闲时）
/// - 同端口新出现启用 TLS 的 HTTP 规则 → 切入共享模式
pub async fn sync_trojan_mode(state: &ServerState) {
    let cfg = {
        let dc = state.dynamic_config.read().await;
        match dc.trojan.clone() {
            Some(t) if t.enabled => t,
            _ => return,
        }
    };
    let desired_shared = !cfg.domain.is_empty()
        && matches!(
            state.proxy_state.http_listen_addr_for_port(cfg.port).await,
            Some((_, true))
        );
    let actual_shared = state.trojan_runtime.read().await.shared;
    if desired_shared != actual_shared {
        info!(
            "Trojan 监听模式切换: shared {} -> {}，重新应用配置",
            actual_shared, desired_shared
        );
        if let Err(e) = apply_trojan_config(state, &cfg).await {
            warn!("Trojan 模式切换失败: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn add_test_cert(mgr: &CertificateManager, domain: &str) {
        use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
        let kp = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let params = CertificateParams::new(vec![domain.to_string()]).unwrap();
        let cert = params.self_signed(&kp).unwrap();
        mgr.add_certificate(
            domain,
            crate::server::acme::CertEntry {
                cert_pem: cert.pem(),
                key_pem: kp.serialize_pem(),
                chain_pem: None,
                expires_at: None,
                source: crate::server::acme::CertSource::Manual,
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn resolve_prefers_acme_exact() {
        let temp = tempfile::TempDir::new().unwrap();
        let mgr = Arc::new(CertificateManager::new(temp.path().to_str().unwrap()));
        add_test_cert(&mgr, "trojan.example.com").await;

        let (_, source) = resolve_trojan_tls(
            Some(&mgr),
            "trojan.example.com",
            temp.path().join("c.pem").to_str().unwrap(),
            temp.path().join("k.pem").to_str().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(source, TrojanCertSource::AcmeExact);
        assert_eq!(source.as_str(), "acme_exact");
    }

    #[tokio::test]
    async fn resolve_falls_back_to_wildcard_then_self_signed() {
        let temp = tempfile::TempDir::new().unwrap();
        let mgr = Arc::new(CertificateManager::new(temp.path().to_str().unwrap()));
        add_test_cert(&mgr, "*.example.com").await;

        let (_, source) = resolve_trojan_tls(
            Some(&mgr),
            "trojan.example.com",
            temp.path().join("c.pem").to_str().unwrap(),
            temp.path().join("k.pem").to_str().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(source, TrojanCertSource::AcmeWildcard);

        // 无匹配证书 → 自签名回退（在 tempdir 生成，不触碰 ./data）
        let (_, source) = resolve_trojan_tls(
            Some(&mgr),
            "none.example.org",
            temp.path().join("c.pem").to_str().unwrap(),
            temp.path().join("k.pem").to_str().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(source, TrojanCertSource::SelfSigned);
        assert_eq!(source.as_str(), "self_signed");
    }

    #[tokio::test]
    async fn apply_trojan_config_disabled_clears_runtime() {
        let state = ServerState::new();
        let cfg = TrojanDynamicConfig {
            enabled: false,
            port: 443,
            password: "p".to_string(),
            fallback: "127.0.0.1:80".to_string(),
            domain: String::new(),
        };
        apply_trojan_config(&state, &cfg).await.unwrap();
        let rt = state.trojan_runtime.read().await;
        assert!(rt.cert_source.is_none());
        assert!(!rt.shared);
    }
}
