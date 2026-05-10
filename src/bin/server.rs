use rust_tunnel::common::{init_logging_with_level, TunnelResult};
use rust_tunnel::server::{ServerConfig, control, api, auth, listener, Database};

#[tokio::main]
async fn main() -> TunnelResult<()> {
    let config = ServerConfig::load()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    init_logging_with_level(&config.log);
    tracing::info!("Starting rust-tunnel server on {}", config.control_addr);

    // Initialize database
    tracing::info!("Initializing database at {}", config.db_path);
    let db = Database::new(&config.db_path).await?;

    // Create shared state with database
    let state = control::ServerState::with_db(db.clone());

    // Load historical data from database
    if let Err(e) = state.traffic_store.load_from_db().await {
        tracing::warn!("Failed to load historical traffic data from database: {}", e);
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
    let auth_config = auth::AuthConfig::new(
        config.admin_password.clone(),
        config.jwt_secret.clone(),
    );

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
        let ss_port = config.ss_port.expect("ss_port should be set when ss_enabled is true");
        let ss_cipher = config.ss_cipher.expect("ss_cipher should be set when ss_enabled is true");
        let ss_password = config.ss_password.expect("ss_password should be set when ss_enabled is true");

        tracing::info!("Starting Shadowsocks listener on port {}, cipher {}", ss_port, ss_cipher);

        let state_clone = state.clone();
        tokio::spawn(async move {
            if let Err(e) = listener::start_shadowsocks_listener(state_clone, ss_port, ss_cipher, ss_password).await {
                tracing::error!("Shadowsocks listener failed: {}", e);
            }
        });
    }

    // Spawn API server
    tokio::spawn(async move {
        if let Err(e) = api::run_api_server(api_addr, api_state, auth_config).await {
            tracing::error!("API server error: {}", e);
        }
    });

    // Wait for Ctrl+C
    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutting down...");

    Ok(())
}
