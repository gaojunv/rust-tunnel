use rust_tunnel::common::{
    create_server_config, init_logging_with_layer, init_logging_with_level, load_or_generate_cert, TunnelResult,
};
use rust_tunnel::server::{api, auth, control, listener, Database, ServerConfig};
use rust_tunnel::server::logs::LogLayer;

#[tokio::main]
async fn main() -> TunnelResult<()> {
    let config = ServerConfig::load().map_err(std::io::Error::other)?;
    tracing::info!("Starting rust-tunnel server on {}", config.control_addr);

    // Initialize database
    tracing::info!("Initializing database at {}", config.db_path);
    let db = Database::new(&config.db_path).await?;

    // Create shared state with database
    let state = control::ServerState::with_db(db.clone());

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
        let tls_acceptor = tokio_rustls::TlsAcceptor::from(tls_config);

        let state_clone = state.clone();
        tokio::spawn(async move {
            if let Err(e) = listener::start_trojan_listener(
                state_clone,
                trojan_port,
                trojan_password,
                trojan_fallback,
                tls_acceptor,
            )
            .await
            {
                tracing::error!("Trojan listener failed: {}", e);
            }
        });
    }

    // Spawn API server
    tokio::spawn(async move {
        if let Err(e) = api::run_api_server(api_addr, api_state, auth_config).await {
            tracing::error!("API server error: {}", e);
        }
    });

    // Start periodic DB flush for traffic data (every 30 seconds)
    state.traffic_store.start_flush_task();

    // Start periodic cleanup of old log entries (every hour, removes 7+ day old logs)
    let db_for_cleanup = db.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            let seven_days_ago = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_micros() as i64)
                .unwrap_or(0)
                - 7 * 24 * 3600 * 1_000_000i64;
            if let Err(e) = db_for_cleanup.cleanup_old_logs(seven_days_ago).await {
                tracing::warn!("Failed to cleanup old logs: {}", e);
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
