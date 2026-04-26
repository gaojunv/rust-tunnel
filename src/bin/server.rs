use clap::Parser;
use rust_tunnel::common::{init_logging_with_level, TunnelResult};
use rust_tunnel::server::{ServerConfig, control, api, auth, Database};

#[tokio::main]
async fn main() -> TunnelResult<()> {
    let config = ServerConfig::parse();
    init_logging_with_level(&config.log);
    tracing::info!("Starting rust-tunnel server on {}", config.control_addr);

    // Initialize database
    tracing::info!("Initializing database at {}", config.db_path);
    let db = Database::new(&config.db_path).await?;

    // Create shared state with database
    let state = control::ServerState::with_db(db.clone());

    // Load historical data from database
    if let Err(e) = state.traffic_store.load_from_db().await {
        tracing::warn!("Failed to load historical data from database: {}", e);
    } else {
        tracing::info!("Loaded historical traffic data from database");
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
