# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

rust-tunnel is a client-server intranet penetration tool written in Rust, with a React/TypeScript frontend for management. The server runs on a public network, exposing ports that forward traffic through an encrypted control channel to clients on an internal network.

## Architecture

### Backend (Rust)
- **Binaries**:
  - `src/bin/server.rs` - Server entry point; runs both control plane and API/frontend services
  - `src/bin/client.rs` - Client entry point; connects to server and manages local forwards
- **Core Modules**:
  - `src/common/` - Shared protocol and error handling
    - `protocol.rs` - Defines `ControlMessage` and message serialization (length‑prefixed bincode)
    - `error.rs` - `TunnelError` and `TunnelResult`
    - `logging.rs` - Logging initialization
  - `src/server/` - Server implementation
    - `control.rs` - Manages control connections and client registrations; includes `ServerState`
      - Supports multiple port registrations on a single control connection
      - Handles client cleanup on disconnect
      - Tracks active connections per port
    - `listener.rs` - Listens on exposed ports, notifies client of new connections
    - `proxy.rs` - Handles per-connection proxy traffic and traffic accounting
    - `api.rs` - Axum web API and `TrafficStore` for metrics
    - `auth.rs` - JWT‑based authentication for the web interface
    - `db.rs` - SQLite database for persistent traffic and client session storage
    - `config.rs` - Server configuration with Clap
  - `src/client/` - Client implementation
    - `control.rs` - Establishes control connection and manages local forwards
    - `proxy.rs` - Connects to local services and proxies traffic
    - `config.rs` - Client configuration with Clap

### Frontend (React + TypeScript + Vite)
- Located in `frontend/`
- Build output is served by the Rust server from `frontend-dist/` (gitignored)
- Uses Tailwind CSS, React Query, and Recharts
- Key components:
  - `Dashboard.tsx` - Main dashboard with metrics
  - `ClientList.tsx` - Connected clients table (groups clients by hostname)
  - `ClientDetail.tsx` - Client traffic details modal
  - `TrafficChart.tsx` - Real-time traffic charts
  - `Navbar.tsx` - Top navigation bar

### Database (SQLite)
- **Tables**:
  - `port_traffic` - Aggregate traffic statistics per port
  - `traffic_buckets` - Minute-level granular traffic data (last 24 hours retained)
  - `client_sessions` - Client connection/disconnection history with hostname tracking
- **Location**: Configurable via `--db-path` (default: `./data/rust-tunnel.db`)

## Common Development Commands

### Backend
```bash
cargo build                    # Debug build
cargo build --release          # Release build
cargo check                    # Fast compile check
cargo test                     # Run tests
cargo test -- --nocapture      # Run tests with output
cargo run --bin rust-tunnel-server -- --bind 0.0.0.0:8080
cargo run --bin rust-tunnel-client -- --server localhost:8080 --forward 9000:localhost:80
```

### Frontend
```bash
cd frontend
npm install                    # Install dependencies
npm run dev                    # Dev server (hot reload)
npm run build                  # Build to dist/
npm run lint                   # Lint with ESLint
```

### Deploy Frontend
```bash
cd frontend
npm run build
rm -rf ../frontend-dist
cp -r dist ../frontend-dist
```

## Server Configuration (Clap)
- `--bind` - Control plane listen address (default 0.0.0.0:8080)
- `--api-bind` - API/frontend listen address (default 0.0.0.0:3000)
- `--admin-password` - Web UI admin password (optional; enables auth)
- `--jwt-secret` - JWT signing secret (auto-generated if not provided)
- `--db-path` - SQLite database path (default: `./data/rust-tunnel.db`)
- `--log` - Log level (trace/debug/info/warn/error)

## Client Configuration (Clap)
- `--server` / `SERVER_ADDR` - Server control address (e.g., example.com:8080)
- `--forward` / `FORWARD` - Forward rules (repeatable); format `REMOTE_PORT:LOCAL_HOST:LOCAL_PORT` (supports IPv6)
- `--log` - Log level

## Code Patterns

- Async runtime: Tokio (full features)
- Error handling: `anyhow` for binaries, `thiserror` for library errors
- Serialization: `bincode` for control messages, `serde_json` for API
- Web framework: Axum
- State sharing: `Arc<Mutex<T>>`
- Database: sqlx with SQLite

## Recent Fixes & Improvements

### Database Persistence (April 2026)
- Added SQLite database for persistent traffic statistics
- Tables: `port_traffic` (aggregates), `traffic_buckets` (minute-level), `client_sessions` (history)
- Traffic data retained across server restarts
- Historical data loaded on server startup

### Configuration Refactoring (April 2026)
- Moved server config to `src/server/config.rs`
- Moved client config to `src/client/config.rs`
- Added proper IPv6 support in forward rule parsing
- Full test coverage for configuration modules

### Hostname Tracking (April 2026)
- Clients now report hostname to server
- `ControlMessage::Register` includes optional hostname field (backward compatible)
- Frontend groups clients by hostname with collapsible sections
- Database tracks client hostnames in session history

### Multi-port Registration (April 2026)
- Fixed issue where only the first port registration was processed
- Server now handles multiple `Register` messages on a single control connection
- All registered ports are tracked and cleaned up on disconnect

### Client Reconnection
- Server now properly cleans up old client registrations before accepting new ones
- Can disconnect and reconnect clients without "port already registered" errors
- `ServerState` removes old client before registering new one for the same port

### Connection Count Tracking
- Fixed frontend showing 0 connections for all clients
- `ServerState` now tracks which port each active connection belongs to
- API returns accurate connection counts per client

### Frontend Improvements
- Added `ClientDetail` modal showing detailed traffic per port
- Fixed logout redirect from `/login` to `/`
- Dashboard now shows real-time metrics and traffic charts
