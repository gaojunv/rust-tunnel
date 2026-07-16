use rust_tunnel::common::{
    create_server_config, init_logging_with_layer, init_logging_with_level, load_or_generate_cert,
    TunnelResult,
};
use rust_tunnel::server::acme::CertificateProvider;
use rust_tunnel::server::logs::LogLayer;
use rust_tunnel::server::{api, auth, control, listener, Database, ServerConfig};
use std::sync::Arc;
use tokio::sync::watch;

async fn disable_conflicting_rules_on_port(
    proxy_state: &rust_tunnel::server::reverse_proxy::ReverseProxyState,
    listen_addr: &str,
    err: &rust_tunnel::server::reverse_proxy::error::ReconcileError,
) {
    use rust_tunnel::server::reverse_proxy::error::ReconcileError as E;
    use rust_tunnel::server::reverse_proxy::{ProxyRule, RuleType};

    // Pick a set of rule ids to disable based on the failure mode.
    let rule_ids_to_disable: Vec<String> = {
        let rules = proxy_state.rules.lock().await;
        let mut on_port: Vec<&ProxyRule> = rules
            .values()
            .filter(|r| {
                r.enabled && r.rule_type == RuleType::Http && r.listen == listen_addr
            })
            .collect();
        on_port.sort_by(|a, b| a.created_at.cmp(&b.created_at));

        match err {
            E::DomainConflict { .. } => {
                // Keep the oldest rule per domain, disable duplicates.
                let mut seen_domains: std::collections::HashSet<String> = Default::default();
                let mut victims: Vec<String> = Vec::new();
                for r in &on_port {
                    let clashes = r.domains.iter().any(|d| !seen_domains.insert(d.clone()));
                    if clashes {
                        victims.push(r.id.clone());
                    }
                }
                victims
            }
            E::TlsMismatch { .. } => {
                // Majority wins. Ties: whichever the oldest rule chose.
                let tls_on = on_port.iter().filter(|r| r.tls.as_ref().is_some_and(|t| t.enabled)).count();
                let tls_off = on_port.len() - tls_on;
                let keep_tls = if tls_on == tls_off {
                    on_port.first().is_some_and(|r| r.tls.as_ref().is_some_and(|t| t.enabled))
                } else {
                    tls_on > tls_off
                };
                on_port.iter()
                    .filter(|r| r.tls.as_ref().is_some_and(|t| t.enabled) != keep_tls)
                    .map(|r| r.id.clone())
                    .collect()
            }
            E::NoCertManager { .. } => {
                // Cert manager missing but TLS requested → disable every
                // TLS-enabled rule so the port at least serves plain-HTTP rules.
                on_port.iter()
                    .filter(|r| r.tls.as_ref().is_some_and(|t| t.enabled))
                    .map(|r| r.id.clone())
                    .collect()
            }
            E::BindFailed { .. } => {
                // Bind failure is an environment issue (port already in use,
                // permission denied). Disabling rules will not help — leave
                // them alone and let the operator fix the port.
                Vec::new()
            }
        }
    };

    for id in rule_ids_to_disable {
        let rule_opt = {
            let mut rules = proxy_state.rules.lock().await;
            if let Some(r) = rules.get_mut(&id) {
                r.enabled = false;
                Some(r.clone())
            } else {
                None
            }
        };
        if let Some(r) = rule_opt {
            if let Err(e) = proxy_state.save_rule(&r).await {
                tracing::error!("Failed to disable conflicting rule {}: {}", id, e);
            } else {
                tracing::warn!(
                    "Disabled rule {} on port {} to resolve reconcile conflict",
                    id, listen_addr
                );
            }
        }
    }
}

#[tokio::main]
async fn main() -> TunnelResult<()> {
    let config = ServerConfig::load().map_err(std::io::Error::other)?;
    tracing::info!("Starting rust-tunnel server on {}", config.control_addr);

    // Initialize database
    tracing::info!("Initializing database at {}", config.db_path);
    let db = Database::new(&config.db_path).await?;

    // Create shared state with database
    let mut state = control::ServerState::with_db(db.clone());

    // Load or seed dynamic config from DB
    let dynamic_config = rust_tunnel::server::dynamic_config::DynamicConfig::load_or_seed(&db, &config).await;
    state.set_dynamic_config(dynamic_config).await;

    // Load persisted ACME and DNS provider configs from database
    if let Some(db_ref) = state.get_db() {
        if let Ok(Some(json)) = db_ref.load_server_setting("acme_config").await {
            if let Ok(acme_config) = serde_json::from_str::<control::AcmeFullConfig>(&json) {
                *state.acme_full_config.write().await = acme_config;
                tracing::info!("Loaded persisted ACME config from database");
            }
        }
        if let Ok(Some(json)) = db_ref.load_server_setting("dns_provider_config").await {
            if let Ok(dns_config) = serde_json::from_str::<rust_tunnel::server::acme::dns::DnsProviderConfig>(&json) {
                *state.dns_provider_config.write().await = Some(dns_config);
                tracing::info!("Loaded persisted DNS provider config from database");
            }
        }
    }

    // Set API TLS config on state (read-only, from config)
    state.api_tls = config.api_tls;
    state.api_domain = config.api_domain.clone();

    // Initialize logging with LogStore capture (after state creation so LogStore is available)
    let log_store = state.log_store.clone();
    if let Some(store) = log_store {
        init_logging_with_layer(&config.log, LogLayer::new(store));
    } else {
        init_logging_with_level(&config.log);
    }

    // Load historical data from database
    if let Err(e) = state.traffic_store.load_from_db().await {
        tracing::warn!(
            "Failed to load historical traffic data from database: {}",
            e
        );
    } else {
        tracing::info!("Loaded historical traffic data from database");
    }

    // Load quality history data from database
    if let Err(e) = state.quality_store.load_from_db().await {
        tracing::warn!("Failed to load quality history data from database: {}", e);
    } else {
        tracing::info!("Loaded quality history data from database");
    }

    // Create auth config
    let auth_config =
        auth::AuthConfig::new(config.admin_password.clone(), config.jwt_secret.clone());

    // Create DNS registry early so it's set on state before cloning for API server
    if config.dns_enabled {
        let dns_registry = rust_tunnel::server::DnsRegistry::new(
            "0.0.0.0".to_string(),
            config.dns_tunnel_domain.clone(),
            config.dns_mesh_domain.clone(),
        );

        // Set DNS registry on state so API can access it
        state.dns_registry = Some(dns_registry.clone());

        let dns_server = rust_tunnel::server::DnsServer::new(dns_registry, &config.dns_bind)
            .expect("Failed to create DNS server");

        tokio::spawn(async move {
            if let Err(e) = dns_server.run().await {
                tracing::error!("DNS server error: {}", e);
            }
        });

        tracing::info!("DNS server started on {}", config.dns_bind);
    }

    // Initialize ACME client if enabled
    if config.acme_enabled {
        let acme_state = rust_tunnel::server::acme::AcmeState::with_db(db.clone());
        let client = rust_tunnel::server::acme::client::AcmeClient::new(
            acme_state,
            config.acme_server_url.clone(),
            config.acme_cert_dir.clone(),
            config.acme_email.clone(),
        );

        if let Err(e) = client.initialize().await {
            tracing::warn!("Failed to initialize ACME client: {}", e);
        }

        let acme_config = control::AcmeConfigInfo {
            enabled: true,
            server_url: config.acme_server_url.clone(),
            cert_dir: config.acme_cert_dir.clone(),
        };

        state.set_acme_client(Arc::new(client), acme_config).await;
        tracing::info!(
            "ACME client initialized (server: {}, cert_dir: {})",
            config.acme_server_url,
            config.acme_cert_dir
        );

        // Initialize CertificateManager
        let cert_manager = Arc::new(
            rust_tunnel::server::acme::manager::CertificateManager::new(&config.acme_cert_dir),
        );

        // Load existing certificates from disk
        if let Err(e) = cert_manager.load_from_storage().await {
            tracing::warn!("Failed to load certificates from storage: {}", e);
        }

        // Set ACME client on the certificate manager
        {
            let acme_client_guard = state.acme_client.read().await;
            if let Some(ref acme_client) = *acme_client_guard {
                cert_manager
                    .set_acme_client(acme_client.clone())
                    .await;
            }
        }

        // Start renewal task if auto-renew is enabled
        if config.acme_auto_renew {
            let renewal_manager = cert_manager.clone();
            let interval = config.acme_renewal_check_interval;
            let days_before = config.acme_renewal_days_before_expiry;
            renewal_manager
                .start_renewal_task(interval, days_before);
            tracing::info!(
                "Certificate renewal task started (interval: {}h, days before expiry: {})",
                interval,
                days_before
            );
        }

        state.set_cert_manager(cert_manager.clone());
        tracing::info!("Certificate manager initialized (cert_dir: {})", config.acme_cert_dir);

        // Propagate the cert manager to ReverseProxyState so the shared
        // listener's SNI resolver and coverage queries have access to it.
        // ServerState::set_cert_manager (called above) only sets the top-level
        // cert_manager field; ReverseProxyState needs its own copy.
        if let Some(mgr) = state.cert_manager.clone() {
            state.proxy_state.set_cert_manager(mgr);
        }

        // Spawn cert-event reactor: refresh proxy rule cert_status on cert changes
        {
            let mut rx = cert_manager.subscribe();
            let proxy_state = state.proxy_state.clone();
            let mgr = cert_manager.clone();
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(rust_tunnel::server::acme::CertEvent::Issued { .. })
                        | Ok(rust_tunnel::server::acme::CertEvent::Renewed { .. })
                        | Ok(rust_tunnel::server::acme::CertEvent::Expired { .. }) => {
                            proxy_state.refresh_all_cert_status(&mgr).await;
                        }
                        Ok(rust_tunnel::server::acme::CertEvent::Error { .. }) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
            tracing::info!("cert-event reactor started");
        }

        // Populate full ACME config for API access
        {
            let mut full_config = state.acme_full_config.write().await;
            full_config.enabled = config.acme_enabled;
            full_config.server_url = config.acme_server_url.clone();
            full_config.email = config.acme_email.clone();
            full_config.cert_dir = config.acme_cert_dir.clone();
            full_config.auto_renew = config.acme_auto_renew;
            full_config.renewal_check_interval = config.acme_renewal_check_interval;
            full_config.renewal_days_before_expiry = config.acme_renewal_days_before_expiry;
            full_config.tos_agreed = config.acme_tos_agreed;
        }
    }

    // Load persisted proxy rules and reconcile HTTP listeners. Runs regardless
    // of whether ACME is enabled — plain-HTTP reverse-proxy rules must still
    // start on boot. If cert_manager is missing, TLS-enabled rules will be
    // disabled by disable_conflicting_rules_on_port's NoCertManager branch.
    if let Err(e) = state.proxy_state.load_from_db().await {
        tracing::warn!("Failed to load proxy rules from DB: {}", e);
    }
    {
        let addrs = state.proxy_state.distinct_http_listen_addrs().await;
        for addr in addrs {
            match state.proxy_state.reconcile_http_listener(&addr).await {
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!(
                        "Startup reconcile failed for {}: {}. Attempting fallback.",
                        addr, e
                    );
                    disable_conflicting_rules_on_port(&state.proxy_state, &addr, &e).await;
                    if let Err(e2) = state.proxy_state.reconcile_http_listener(&addr).await {
                        tracing::error!(
                            "Retry reconcile still failed for {}: {}. Port left offline.",
                            addr, e2
                        );
                    }
                }
            }
        }
    }

    // Split config for control and API servers
    let control_config = config.clone();
    let api_addr = config.api_addr.clone();
    let control_state = state.clone();
    let api_state = state.clone();

    // Create a watch channel for control channel TLS if TLS is enabled
    let control_tls_rx = if config.tls {
        let cert_pair = load_or_generate_cert(&config.tls_cert, &config.tls_key).map_err(|e| {
            std::io::Error::other(format!("Failed to load TLS certificates for control channel: {}", e))
        })?;
        let tls_config = create_server_config(cert_pair).map_err(|e| {
            std::io::Error::other(format!("Failed to create TLS config for control channel: {}", e))
        })?;

        let (tls_config_tx, tls_config_rx) = watch::channel(tls_config);

        // If a cert_manager exists, subscribe to cert events and update the watch channel
        if let Some(ref cert_manager) = state.cert_manager {
            let mut cert_rx = cert_manager.subscribe();
            let tx = tls_config_tx.clone();
            let cert_manager_clone = cert_manager.clone();
            tokio::spawn(async move {
                use rust_tunnel::server::acme::manager::CertEvent;
                loop {
                    match cert_rx.recv().await {
                        Ok(event) => match event {
                            CertEvent::Renewed { ref domain }
                            | CertEvent::Issued { ref domain } => {
                                tracing::info!(
                                    "Certificate event for control channel: {:?} for {}",
                                    event,
                                    domain
                                );
                                if let Some(new_config) =
                                    cert_manager_clone.get_tls_server_config(domain).await
                                {
                                    if let Err(e) = tx.send(new_config) {
                                        tracing::error!(
                                            "Failed to update control channel TLS config: {}",
                                            e
                                        );
                                    }
                                }
                            }
                            _ => {}
                        },
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("Cert event subscriber lagged by {} messages", n);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            tracing::info!(
                                "Cert event channel closed, stopping control channel TLS config updater"
                            );
                            break;
                        }
                    }
                }
            });
        }

        Some(tls_config_rx)
    } else {
        None
    };

    // Spawn control server
    tokio::spawn(async move {
        if let Err(e) = control::run_server(control_config, control_state, control_tls_rx).await {
            tracing::error!("Control server error: {}", e);
        }
    });

    // Start Shadowsocks if enabled in dynamic config
    {
        let dc = state.dynamic_config.read().await;
        if let Some(ref ss) = dc.ss {
            if ss.enabled {
                let state_clone = state.clone();
                let ss_port = ss.port;
                let ss_cipher = ss.cipher.clone();
                let ss_password = ss.password.clone();
                tracing::info!(
                    "Starting Shadowsocks listener on port {}, cipher {}",
                    ss_port,
                    ss_cipher
                );
                drop(dc); // Release the read lock before spawning
                tokio::spawn(async move {
                    if let Err(e) = listener::start_shadowsocks_listener(
                        state_clone, ss_port, ss_cipher, ss_password,
                    )
                    .await
                    {
                        tracing::error!("Shadowsocks listener error: {}", e);
                    }
                });
            }
        }
    }

    // Start Trojan if enabled in dynamic config
    {
        let dc = state.dynamic_config.read().await;
        if let Some(ref tj) = dc.trojan {
            if tj.enabled {
                let state_clone = state.clone();
                let trojan_port = tj.port;
                let trojan_password = tj.password.clone();
                let trojan_fallback = tj.fallback.clone();
                tracing::info!(
                    "Starting Trojan TLS listener on port {}, fallback {}",
                    trojan_port,
                    trojan_fallback
                );
                drop(dc); // Release the read lock before spawning

                // Trojan requires TLS - load or generate certificates
                let cert_pair =
                    load_or_generate_cert(&config.tls_cert, &config.tls_key).map_err(|e| {
                        std::io::Error::other(format!(
                            "Failed to load TLS certificates for Trojan: {}",
                            e
                        ))
                    })?;
                let tls_config = create_server_config(cert_pair).map_err(|e| {
                    std::io::Error::other(format!(
                        "Failed to create TLS config for Trojan: {}",
                        e
                    ))
                })?;

                // Create a watch channel for dynamic TLS config updates
                let (tls_config_tx, tls_config_rx) = watch::channel(tls_config);

                // If a cert_manager exists, subscribe to cert events and update the watch channel
                if let Some(ref cert_manager) = state.cert_manager {
                    let mut cert_rx = cert_manager.subscribe();
                    let tx = tls_config_tx.clone();
                    let cert_manager_clone = cert_manager.clone();
                    tokio::spawn(async move {
                        use rust_tunnel::server::acme::manager::CertEvent;
                        loop {
                            match cert_rx.recv().await {
                                Ok(event) => match event {
                                    CertEvent::Renewed { ref domain }
                                    | CertEvent::Issued { ref domain } => {
                                        tracing::info!(
                                            "Certificate event for Trojan listener: {:?} for {}",
                                            event,
                                            domain
                                        );
                                        // Try to get the updated TLS config from the cert manager
                                        if let Some(new_config) =
                                            cert_manager_clone.get_tls_server_config(domain).await
                                        {
                                            if let Err(e) = tx.send(new_config) {
                                                tracing::error!(
                                                    "Failed to update Trojan TLS config: {}",
                                                    e
                                                );
                                            }
                                        }
                                    }
                                    _ => {}
                                },
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                    tracing::warn!("Cert event subscriber lagged by {} messages", n);
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                    tracing::info!(
                                        "Cert event channel closed, stopping TLS config updater"
                                    );
                                    break;
                                }
                            }
                        }
                    });
                }

                tokio::spawn(async move {
                    if let Err(e) = listener::start_trojan_listener(
                        state_clone,
                        trojan_port,
                        trojan_password,
                        trojan_fallback,
                        tls_config_rx,
                    )
                    .await
                    {
                        tracing::error!("Trojan listener error: {}", e);
                    }
                });
            }
        }
    }

    // Spawn API server
    let api_tls_config = if config.api_tls {
        if let Some(ref cert_manager) = state.cert_manager {
            // Try to get certificate for API domain
            if let Some(ref domain) = config.api_domain {
                match cert_manager.get_tls_server_config(domain).await {
                    Some(cfg) => {
                        tracing::info!("API server TLS enabled with ACME certificate for domain: {}", domain);
                        Some(cfg)
                    }
                    None => {
                        tracing::warn!("No ACME certificate found for API domain '{}', API server will run without TLS", domain);
                        None
                    }
                }
            } else {
                tracing::warn!("API TLS enabled but no api_domain configured, API server will run without TLS");
                None
            }
        } else {
            tracing::warn!("API TLS enabled but ACME not configured, API server will run without TLS");
            None
        }
    } else {
        None
    };
    tokio::spawn(async move {
        if let Err(e) = api::run_api_server(api_addr, api_state, auth_config, api_tls_config).await {
            tracing::error!("API server error: {}", e);
        }
    });

    // Start periodic DB flush for traffic data (every 30 seconds)
    state.traffic_store.start_flush_task();

    // Start periodic quality sampling for Shadowsocks / Trojan ports (every 60 seconds)
    let quality_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            quality_state.sample_proxy_quality().await;
        }
    });

    // Start periodic cleanup of old data (every hour)
    let db_for_cleanup = db.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            // Remove logs older than 7 days
            let seven_days_ago = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_micros() as i64)
                .unwrap_or(0)
                - 7 * 24 * 3600 * 1_000_000i64;
            if let Err(e) = db_for_cleanup.cleanup_old_logs(seven_days_ago).await {
                tracing::warn!("Failed to cleanup old logs: {}", e);
            }
            // Remove quality history older than 24 hours
            let twenty_four_hours_ago = chrono::Utc::now() - chrono::Duration::hours(24);
            if let Err(e) = db_for_cleanup
                .cleanup_old_quality_history(twenty_four_hours_ago)
                .await
            {
                tracing::warn!("Failed to cleanup old quality history: {}", e);
            }
        }
    });

    // Wait for Ctrl+C
    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutting down...");

    // Final flush of traffic data to ensure persistence
    if let Err(e) = state.traffic_store.flush_to_db().await {
        tracing::warn!("Failed to flush traffic data during shutdown: {}", e);
    }

    Ok(())
}
