use rust_tunnel::common::{
    create_server_config, init_logging_with_layer, init_logging_with_level, load_or_generate_cert,
    TunnelResult,
};
use rust_tunnel::server::acme::CertificateProvider;
use rust_tunnel::server::logs::LogLayer;
use rust_tunnel::server::{api, auth, control, listener, Database, ServerConfig};
use std::sync::Arc;
use tokio::sync::watch;

#[tokio::main]
async fn main() -> TunnelResult<()> {
    let config = ServerConfig::load().map_err(std::io::Error::other)?;
    tracing::info!("Starting rust-tunnel server on {}", config.control_addr);

    // Initialize database
    tracing::info!("Initializing database at {}", config.db_path);
    let db = Database::new(&config.db_path).await?;

    // Create shared state with database
    let mut state = control::ServerState::with_db(db.clone());

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

        state.set_acme_client(Arc::new(client), acme_config);
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
        if let Some(ref acme_client) = state.acme_client {
            cert_manager
                .set_acme_client(acme_client.clone())
                .await;
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

        state.set_cert_manager(cert_manager);
        tracing::info!("Certificate manager initialized (cert_dir: {})", config.acme_cert_dir);

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

    // Split config for control and API servers
    let control_config = config.clone();
    let api_addr = config.api_addr.clone();
    let control_state = state.clone();
    let api_state = state.clone();

    // Spawn control server
    tokio::spawn(async move {
        if let Err(e) = control::run_server(control_config, control_state).await {
            tracing::error!("Control server error: {}", e);
        }
    });

    // Start Shadowsocks listener if enabled
    if config.ss_enabled {
        let ss_port = config
            .ss_port
            .expect("ss_port should be set when ss_enabled is true");
        let ss_cipher = config
            .ss_cipher
            .expect("ss_cipher should be set when ss_enabled is true");
        let ss_password = config
            .ss_password
            .expect("ss_password should be set when ss_enabled is true");

        tracing::info!(
            "Starting Shadowsocks listener on port {}, cipher {}",
            ss_port,
            ss_cipher
        );

        let state_clone = state.clone();
        tokio::spawn(async move {
            if let Err(e) =
                listener::start_shadowsocks_listener(state_clone, ss_port, ss_cipher, ss_password)
                    .await
            {
                tracing::error!("Shadowsocks listener failed: {}", e);
            }
        });
    }

    // Start Trojan listener if enabled
    if config.trojan_enabled {
        let trojan_port = config
            .trojan_port
            .expect("trojan_port should be set when trojan_enabled is true");
        let trojan_password = config
            .trojan_password
            .expect("trojan_password should be set when trojan_enabled is true");
        let trojan_fallback = config.trojan_fallback.clone();

        tracing::info!(
            "Starting Trojan TLS listener on port {}, fallback {}",
            trojan_port,
            trojan_fallback
        );

        // Trojan requires TLS - load or generate certificates
        let cert_pair = load_or_generate_cert(&config.tls_cert, &config.tls_key).map_err(|e| {
            std::io::Error::other(format!("Failed to load TLS certificates for Trojan: {}", e))
        })?;
        let tls_config = create_server_config(cert_pair).map_err(|e| {
            std::io::Error::other(format!("Failed to create TLS config for Trojan: {}", e))
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
                            tracing::info!("Cert event channel closed, stopping TLS config updater");
                            break;
                        }
                    }
                }
            });
        }

        let state_clone = state.clone();
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
                tracing::error!("Trojan listener failed: {}", e);
            }
        });
    }

    // Spawn API server
    tokio::spawn(async move {
        if let Err(e) = api::run_api_server(api_addr, api_state, auth_config, None).await {
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
