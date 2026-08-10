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
use tokio::sync::{watch, RwLock};
use tracing::{info, warn};

use crate::acme::manager::{CertEvent, CertificateManager};
use crate::acme::provider::CertCoverage;
use crate::control::{ServerState, TrojanRuntimeStatus};
use crate::dynamic_config::TrojanDynamicConfig;
use crate::reverse_proxy::sni_resolver::wildcard_for;
use crate::reverse_proxy::TrojanSniEntry;
use rust_tunnel_common::error::TunnelResult;
use rust_tunnel_common::tls::{create_server_config, load_or_generate_cert};

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
/// 重新走证书解析——命中 ACME 证书则把新 TLS 配置推入 watch channel 并把
/// `cert_source` 更新为实际来源（自签名模式由此热升级到 ACME，续期场景
/// 也会重新解析保持正确来源）；未命中则保持现状不动作。
pub fn spawn_trojan_cert_reload(
    cert_manager: Arc<CertificateManager>,
    trojan_domain: String,
    tls_config_tx: watch::Sender<Arc<rustls::ServerConfig>>,
    mut abort_rx: watch::Receiver<bool>,
    trojan_runtime: Arc<RwLock<TrojanRuntimeStatus>>,
) -> tokio::task::JoinHandle<()> {
    // 在 spawn 前订阅，避免任务调度延迟导致丢失订阅前瞬间的签发事件
    let mut cert_rx = cert_manager.subscribe();
    tokio::spawn(async move {
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
                                if let Some((cfg, coverage)) = cert_manager
                                    .get_tls_server_config_covering(&trojan_domain)
                                    .await
                                {
                                    let source = match coverage {
                                        CertCoverage::Exact => TrojanCertSource::AcmeExact,
                                        CertCoverage::Wildcard(_) => TrojanCertSource::AcmeWildcard,
                                    };
                                    info!(
                                        "Trojan TLS 配置热更新（证书 {}，来源 {}）",
                                        d,
                                        source.as_str()
                                    );
                                    let _ = tls_config_tx.send(cfg);
                                    trojan_runtime.write().await.cert_source =
                                        Some(source.as_str().to_string());
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

    // 2.5 独立监听回退前检测端口占用：目标端口仍被反代共享监听器占用时
    // bind 必失败，按规格"端口被占则停止并告警"，避免运行状态误报"运行中"
    let port_occupied_by = if shared_listen_addr.is_none() {
        state
            .proxy_state
            .shared_listener_addr_for_port(cfg.port)
            .await
    } else {
        None
    };
    if let Some(addr) = port_occupied_by {
        warn!(
            "Trojan 端口 {} 仍被反代共享监听器 {} 占用，无法回退独立监听，Trojan 已停止",
            cfg.port, addr
        );
        *state.trojan_runtime.write().await = Default::default();
        return Ok(());
    }

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

    // 4. 证书热更新订阅：domain 非空即订阅（自签名模式也可经此热升级到 ACME）
    if !domain_lc.is_empty() {
        if let Some(mgr) = state.cert_manager.clone() {
            spawn_trojan_cert_reload(
                mgr,
                domain_lc.clone(),
                tls_config_tx,
                abort_rx.clone(),
                state.trojan_runtime.clone(),
            );
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
                if let Err(e) = crate::listener::start_trojan_listener_with_abort(
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
            crate::acme::CertEntry {
                cert_pem: cert.pem(),
                key_pem: kp.serialize_pem(),
                chain_pem: None,
                expires_at: None,
                source: crate::acme::CertSource::Manual,
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

    /// 修复①：自签名模式（domain 非空、启动时无 ACME 证书）也订阅证书事件，
    /// Issued 覆盖 trojan 域名后热升级：cert_source 变为 acme_exact。
    #[tokio::test]
    async fn apply_self_signed_hot_upgrades_to_acme_on_issued() {
        let temp = tempfile::TempDir::new().unwrap();
        let mgr = Arc::new(CertificateManager::new(temp.path().to_str().unwrap()));
        let mut state = ServerState::new();
        state.cert_manager = Some(mgr.clone());
        state.tls_cert_path = temp.path().join("c.pem").to_str().unwrap().to_string();
        state.tls_key_path = temp.path().join("k.pem").to_str().unwrap().to_string();

        let cfg = TrojanDynamicConfig {
            enabled: true,
            port: 0, // 独立监听绑定随机端口，避免端口冲突
            password: "p".to_string(),
            fallback: "127.0.0.1:80".to_string(),
            domain: "trojan.example.com".to_string(),
        };
        apply_trojan_config(&state, &cfg).await.unwrap();
        // 启动时无 ACME 证书 → 自签名回退
        assert_eq!(
            state.trojan_runtime.read().await.cert_source.as_deref(),
            Some("self_signed")
        );

        // 模拟 ACME 签发（add_certificate 广播 CertEvent::Issued）
        add_test_cert(&mgr, "trojan.example.com").await;

        // 热升级：watch 推送新配置（同一分支内）并把 cert_source 更新为 acme_exact
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if state.trojan_runtime.read().await.cert_source.as_deref() == Some("acme_exact") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("自签名模式未热升级到 ACME 证书");
    }

    /// 修复①：直接验证 spawn_trojan_cert_reload —— Issued 后 watch 收到新 TLS
    /// 配置且 cert_source 更新；续期（再次 Issued）仍能正确推送。
    #[tokio::test]
    async fn cert_reload_pushes_new_config_and_updates_source() {
        let temp = tempfile::TempDir::new().unwrap();
        let mgr = Arc::new(CertificateManager::new(temp.path().to_str().unwrap()));
        // 初始为自签名 TLS 配置
        let pair = load_or_generate_cert(
            temp.path().join("c.pem").to_str().unwrap(),
            temp.path().join("k.pem").to_str().unwrap(),
        )
        .unwrap();
        let initial = create_server_config(pair).unwrap();
        let (tx, mut rx) = watch::channel(initial);
        let (_abort_tx, abort_rx) = watch::channel(false);
        let runtime = Arc::new(RwLock::new(TrojanRuntimeStatus {
            cert_source: Some("self_signed".to_string()),
            shared: false,
        }));
        spawn_trojan_cert_reload(
            mgr.clone(),
            "trojan.example.com".to_string(),
            tx,
            abort_rx,
            runtime.clone(),
        );

        // 模拟 ACME 签发事件 → watch 收到新配置
        add_test_cert(&mgr, "trojan.example.com").await;
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.changed())
            .await
            .expect("watch 未收到热升级后的 TLS 配置")
            .unwrap();
        // cert_source 在 send 之后写入，轮询等待避免竞态
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if runtime.read().await.cert_source.as_deref() == Some("acme_exact") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cert_source 未更新为 acme_exact");

        // 续期场景：再次签发同域证书仍能收到新配置、来源保持正确
        add_test_cert(&mgr, "trojan.example.com").await;
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.changed())
            .await
            .expect("续期事件未推送新配置")
            .unwrap();
        assert_eq!(
            runtime.read().await.cert_source.as_deref(),
            Some("acme_exact")
        );
    }

    /// 修复②：独立监听回退时目标端口仍被反代共享监听器占用 →
    /// 不 spawn 独立 listener、清空运行状态（相当于停止）。
    #[tokio::test]
    async fn apply_independent_fallback_stops_when_port_occupied() {
        use crate::reverse_proxy::{
            Backend, BackendKind, BackendProtocol, BackendScheme, LoadBalancing, ProxyRule, Route,
            RuleType,
        };

        let state = ServerState::new();

        // 反代在该端口保留共享监听器（模拟规则从 TLS 降级为 HTTP 后 listener 仍绑定端口）
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let listen_addr = format!("127.0.0.1:{port}");
        let rule = ProxyRule {
            id: "r1".into(),
            name: "r1".into(),
            rule_type: RuleType::Http,
            listen: listen_addr.clone(),
            domains: vec!["a.example.com".into()],
            routes: vec![Route {
                path: "/".into(),
                backends: vec![Backend {
                    kind: BackendKind::Direct,
                    addr: "127.0.0.1:9".into(),
                    client_name: None,
                    weight: 100,
                    protocol: BackendProtocol::Http1,
                    scheme: BackendScheme::Http,
                }],
                load_balancing: LoadBalancing::RoundRobin,
            }],
            tls: None, // 未启用 TLS → trojan 无法共享，需回退独立监听
            enabled: true,
            created_at: None,
            cert_status: None,
        };
        state
            .proxy_state
            .rules
            .lock()
            .await
            .insert("r1".into(), rule);
        state
            .proxy_state
            .reconcile_http_listener(&listen_addr)
            .await
            .unwrap();
        assert!(state
            .proxy_state
            .shared_listeners
            .lock()
            .await
            .contains_key(&listen_addr));

        // 模拟 trojan 原处于共享模式运行中
        {
            let mut rt = state.trojan_runtime.write().await;
            rt.cert_source = Some("acme_exact".to_string());
            rt.shared = true;
        }

        let cfg = TrojanDynamicConfig {
            enabled: true,
            port,
            password: "p".to_string(),
            fallback: "127.0.0.1:80".to_string(),
            domain: "trojan.example.com".to_string(),
        };
        apply_trojan_config(&state, &cfg).await.unwrap();

        // 端口被占用：运行状态清空，abort handle 未重建（未 spawn 独立 listener）
        {
            let rt = state.trojan_runtime.read().await;
            assert!(rt.cert_source.is_none());
            assert!(!rt.shared);
        }
        assert!(state.trojan_listener_abort.read().await.is_none());
    }
}
