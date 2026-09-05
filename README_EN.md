# Rust Tunnel

[简体中文](README.md) | **English**

A Rust-based client-server NAT traversal and edge proxy platform with a React/TypeScript management UI. The server runs on the public internet and forwards traffic to intranet clients over an encrypted control channel. It also ships built-in Shadowsocks / Trojan proxies, a reverse proxy (with both direct and tunnel backends), embedded DNS / mesh service discovery, automatic ACME certificates, SQLite persistence, real-time observability, an LLM gateway (with a RAG knowledge base), and an AI Agent workbench (ACP as the primary path, with in-tunnel tool execution).

## Features

- **NAT traversal**: encrypted control channel (TLS on by default, self-signed Ed25519), client registration and tunnel multiplexing (`ClientConnector → ClientRegistry.open_tunnel → ClientTunnelStream`)
- **Reverse proxy**: rule-based routing with `Direct` (direct connect) and `Client` (tunnel) backends behind a unified `Connector` abstraction, supporting TCP/HTTP and SNI dispatch
- **Proxy protocols**: built-in Shadowsocks (`shadowsocks-rust`, AES-256-GCM / ChaCha20-Poly1305) and Trojan (TLS required, SHA-224 authentication, fallback camouflage)
- **Network infrastructure**: embedded authoritative DNS (`*.tunnel.local` / `*.mesh.local`), mesh service discovery, PKI/ACME auto-renewal, API TLS
- **Observability**: heartbeat-based RTT / packet loss / throughput, 0–100 quality score with alert thresholds; per-minute traffic buckets (retained 24h) plus aggregated stats; structured logs (tracing Layer → in-memory + SQLite, with pagination / filtering / SSE)
- **LLM gateway**: OpenAI / Anthropic / Responses protocol endpoints (`POST /v1/chat/completions` and `POST /v1/responses`), provider / model / api-key / usage management, compat tool-call rewriting, and per-model `extra_config` declaring the upstream `responses` protocol
- **Unified knowledge container**: multi-format extraction (PDF/Word/Excel/PPT → Markdown) → Markdown chunking → remote embedding → dual indexing with qdrant-edge vectors + page graph; background ingestion, reindexing, retrieval preview, and cross-source search
- **AI Agent workbench**: WebSocket turn streaming, ACP primary path (`AgentSpawn` / `AgentLlmProxy` spawn processes on the client over the control channel, stdio pump, idle reaper) plus a runner fallback path; in-tunnel tool execution (shell / read / write / patch / list / search / git / code_outline / read_symbol / task, etc.), an approval matrix, multi-role sub-agents, context compaction, and automatic session titles
- **Management UI**: Axum + `rust-embed` embedded frontend, JWT authentication, artifact archive downloads (client / wiki-desktop)
- **Desktop client**: `crates/client-gui` tray client (winit + tray-icon + eframe/egui, four tabs: Connection / Logs / Settings / About)
- **Wiki desktop**: packaged with Tauri 2 (`wiki-desktop-ui` + `wiki-core` / `wiki-serve`), Markdown + full-text search + graph

## Architecture

### Cargo Workspace

The root meta-crate `rust-tunnel` hosts only the `tests/` e2e tests and contains no implementation code; the implementation is spread across 13 crates:

| crate | description |
|---|---|
| `crates/common` | protocol / TLS / error / logging / mesh + `DEFAULT_PTY_PORT` |
| `crates/client` | client lib + bin (control channel, tunnel shuttle, config) |
| `crates/client-gui` | desktop tray client (winit / tray-icon / eframe) |
| `crates/server` | server assembly (control_plane / protocols / persistence / mgmt / llm / pki / net / agent / config) |
| `crates/agent` | agent domain (runner / tools / executor / approval / session / title / compact / sse / spawner / acp_bridge / acp_events / llm_bridge / roles) |
| `crates/llm` | LLM gateway (openai / anthropic / responses adapters, provider / model / key, usage, model_groups) |
| `crates/rag` | RAG knowledge base (extractor / chunker / embedder / store / retriever / ingest) |
| `crates/persistence` | SQLite access layer |
| `crates/pki` | certificates and ACME |
| `crates/protocols` | SS / Trojan / reverse proxy protocol implementations |
| `crates/stats` | statistics collection and persistence |
| `crates/wiki-core` / `wiki-serve` | wiki core and service |

Dependencies flow one way: `common ← client`, `common ← server`. `rag` is a non-default feature (gating qdrant-edge and the vector-index side).

### Core Data Flow

1. The client registers over the TLS control channel with `Register{protocol_version:2, name, password, version}`
2. Pipe routing is defined by reverse-proxy rules managed via the Web UI (`kind=Direct` connects directly to the outside; `kind=Client` goes through a tunnel into the intranet)
3. `ClientConnector → ClientRegistry.open_tunnel → ClientTunnelStream` builds an `AsyncRead/Write` stream over the control channel
4. The reverse proxy's TCP/HTTP handlers invoke the unified `Connector` trait without caring about the backend type

## Installing Rust

**macOS / Linux:**
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env
```
**Windows:** download and run [rustup-init.exe](https://rustup.rs/)

The frontend requires Node.js (installation via nvm is recommended).

## Building

```bash
cargo build                      # debug build (qdrant-edge excluded by default)
cargo build -p rust-tunnel-server --features rag              # full build with RAG
cargo build -p rust-tunnel-server --features rag,embed-frontend  # with embedded frontend (release form)
cargo check
cargo clippy -p rust-tunnel-server
```

The embedded frontend artifacts are packed into the binary from `frontend-dist/` (gitignored) via `rust-embed`:

```bash
cd frontend && npm install && npm run build
rm -rf ../frontend-dist && cp -r dist ../frontend-dist
```

## Usage

### Server

```bash
# Basic (TLS on by default; self-signed certificate auto-generated)
cargo run -p rust-tunnel-server --features rag -- --bind 0.0.0.0:8080

# Custom config and ports
./rust-tunnel-server --config /path/to/config.toml --bind 0.0.0.0:8080 --api-bind 0.0.0.0:3000

# Enable Shadowsocks / Trojan / DNS / reverse proxy / ACME (also configurable in the Web UI or TOML)
./rust-tunnel-server --ss-enabled true --ss-port 8388 --ss-cipher aes-256-gcm --ss-password <pwd> \
  --trojan-enabled true --trojan-port 443 --trojan-password <pwd> --trojan-fallback 127.0.0.1:80 \
  --dns-enabled true --dns-bind 0.0.0.0:53
```

Common arguments (see `--help` for the full list):

- `--config <PATH>` — TOML config file
- `--bind <ADDR>` — control channel listener (default `0.0.0.0:8080`)
- `--api-bind <ADDR>` — API/Web listener (default `0.0.0.0:3000`)
- `--admin-password <PWD>` / `--jwt-secret <SECRET>` / `--client-auth-token <TOKEN>`
- `--tls / --tls-cert / --tls-key` — control channel TLS (on by default; `data/tls/*` auto-generated when unset)
- `--db-path <PATH>` — SQLite path (default `./data/rust-tunnel.db`, WAL mode)
- `--client-dist-dir / --wiki-dist-dir` — read-only archive directories (populated by CI, consumed by the Web downloads page)
- `--ss-enabled / --ss-port / --ss-cipher / --ss-password`
- `--trojan-enabled / --trojan-port / --trojan-password / --trojan-fallback`
- `--dns-enabled / --dns-bind / --dns-tunnel-domain / --dns-mesh-domain`
- `--reverse-proxy-enabled / --reverse-proxy-max-connections / --reverse-proxy-connection-timeout / --reverse-proxy-buffer-size`
- `--api-tls / --api-domain`
- `--acme-enabled / --acme-server-url / --acme-cert-dir / --acme-auto-renew / --acme-renewal-check-interval / --acme-renewal-days-before-expiry / --acme-email / --acme-tos-agreed`
- `--log <LEVEL>` — trace / debug / info / warn / error

> The release form uses `contrib/config.toml.template` as the deployment template (placeholders are rendered and validated in CI; see [CI/CD](#cicd)).

### Client

The client follows a zero-config paradigm and no longer uses `--forward`; forwarding is defined by the server's reverse-proxy rules, and the client only needs to register and carry tunnels.

```bash
# Basic
cargo run -p rust-tunnel-client -- --server example.com:8080 --password <client_token> --name home-nas

# TLS control (on by default; TOFU accepts self-signed certificates)
./rust-tunnel-client --server example.com:8080 --password <token> --tls true --tls-insecure true

# Mesh and Agent executor
./rust-tunnel-client --server example.com:8080 --password <token> --mesh home --mesh-name nas \
  --mesh-service db:mysql:localhost:3306 --enable-agent --agent-pty-port 45631

# Config file
./rust-tunnel-client --config /path/to/client.toml
```

Arguments: `--server <host:port>` (required), `--password <token>` (required), `--name <name>` (defaults to hostname), `--tls` / `--tls-server-name` / `--tls-insecure`, `--mesh` / `--mesh-name` / `--mesh-service` (repeatable), `--enable-agent` / `--agent-pty-port`, `--log`, `--config`.

See [`config/client.example.toml`](config/client.example.toml) for a `client.toml` example.

### Desktop Tray Client

```bash
cargo run -p rust-tunnel-client-gui
```

Native tray + eframe with four tabs (Connection / Logs / Settings / About). Configuration is stored in the platform-standard directory (macOS `~/Library/Application Support` / Windows `%APPDATA%` / Linux `~/.config`), with keyring and auto-start support.

### Wiki Desktop

Located in `wiki-desktop-ui` (Tauri 2). Artifacts are built by [`release-wiki-client.yml`](.github/workflows/release-wiki-client.yml) on `wiki-v*` tags as macOS `.dmg` and Windows `.msi` / `.exe` installers.

## Configuration

Three-level priority: **CLI > environment variables > TOML config file > defaults**.

- Server TOML reference: [`config/server.example.toml`](config/server.example.toml) (includes production / development / no-tls example sections)
- Client TOML reference: [`config/client.example.toml`](config/client.example.toml)
- Environment variable examples: [`.env.example`](.env.example)
- Deployment template: [`contrib/config.toml.template`](contrib/config.toml.template) (`release-server.yml` renders placeholders such as `${ADMIN_PASSWORD}` / `${CLIENT_AUTH_TOKEN}` / `${CLIENT_DIST_DIR}` / `${WIKI_DIST_DIR}` / `${SS_PASSWORD}` / `${TROJAN_PASSWORD}`)

Server environment variable mapping (excerpt): `CONTROL_ADDR` / `API_BIND` / `ADMIN_PASSWORD` / `JWT_SECRET` / `CLIENT_AUTH_TOKEN` / `TLS` / `TLS_CERT` / `TLS_KEY` / `LOG_LEVEL` / `DB_PATH` / `CLIENT_DIST_DIR` / `WIKI_DIST_DIR` / `DNS_ENABLED` / `DNS_BIND` / `DNS_TUNNEL_DOMAIN` / `DNS_MESH_DOMAIN` / `TROJAN_ENABLED` / `TROJAN_PORT` / `TROJAN_PASSWORD` / `TROJAN_FALLBACK`, etc. See the header comments of `config/server.example.toml` and `--help` for the full list.

Client environment variables: `SERVER_ADDR` / `PASSWORD` / `NAME` / `TLS` / `TLS_SERVER_NAME` / `TLS_INSECURE` / `MESH_ID` / `MESH_NAME` / `MESH_SERVICES` (comma-separated) / `LOG_LEVEL`.

Database (SQLite, WAL): `--db-path` (default `./data/rust-tunnel.db`). Tables: `port_traffic` / `traffic_buckets` / `client_sessions` / `connection_quality_history` / `shadowsocks_config` / `trojan_config` / `log_entries` / `clients` / `server_auth` / `knowledge_sources` / `knowledge_docs` / `knowledge_doc_index` / `knowledge_chunks` / `knowledge_pages` / `knowledge_page_edges` / `agent_workspaces` / `agent_sessions` / `agent_messages` / `agent_roles`, etc. Vector data lives under `<db_parent>/rag/<source_id>/`, and document originals under `<db_parent>/knowledge_docs/<source_id>/`; the vector index is compiled only with the `rag` feature.

## Web Management UI

After startup, visit `http://<server>:3000` (or the address given by `--api-bind`):

- Dashboard / Mesh / DNS / Clients (with details and kick-offline) / Shadowsocks / Trojan / Reverse Proxy / ACME / Logs / Settings
- LLM gateway (`LLMPage`) and unified knowledge container (`KnowledgePage`, dual vector + page indexing)
- AI Agent workbench (`AgentPage`: session list / message stream / approval popover / @file references / workspace management and ACP engine configuration)
- Downloads page (client binaries and Wiki desktop installers, shown in sections from read-only archive directories)

Tech stack: `react-router-dom v6` (`createBrowserRouter` + `ProtectedRoute`, route-level lazy loading), `@tanstack/react-query v5`, Vite (`/api` proxied to `localhost:3000`), `Tailwind CSS` / `Radix UI` / `Recharts` / `CodeMirror 6` / `xterm.js` / `streamdown`, etc. Shared components live in `frontend/src/components/shared/`, pages in `frontend/src/pages/`, types in `frontend/src/types/index.ts`, and the API client in `frontend/src/api/client.ts` (Axios + JWT interceptor).

## TLS & Security

- Control channel TLS is on by default; a self-signed Ed25519 certificate (PEM, 1-year validity) is auto-generated when unset; custom certificates are supported via `--tls-cert` / `--tls-key`
- The client accepts self-signed certificates via TOFU by default (`--tls-insecure true`); SNI can be specified with `--tls-server-name`
- API TLS and ACME (`instant-acme` + `hickory-proto`) support auto-renewal and port-80 redirection
- Web JWT authentication (enabled with `--admin-password`), client access tokens (`--client-auth-token` / `server_auth` table), and `?token=` query-parameter validation on download endpoints (because `<a download>` cannot carry headers)

## Observability

- Quality monitoring: heartbeat RTT / packet loss / throughput, 0–100 score, alert thresholds (warning at RTT ≥ 200ms or loss ≥ 5%, critical at RTT ≥ 500ms or loss ≥ 15%); history retained 60 minutes in memory and 24 hours in the DB
- Traffic: aggregated and per-minute buckets; the `stats` crate handles collection and persistence, pushed to frontend charts over SSE
- Logs: a custom tracing Layer writes to both an in-memory ring and SQLite; the API supports pagination / filtering / SSE

## LLM Gateway / Knowledge Base / Agent

- **LLM gateway**: `POST /v1/chat/completions` and `POST /v1/responses` (the latter converts bidirectionally via `responses.rs`), provider / model / api-key / usage / model-groups (multi-model failover and circuit breaking), `compat` tool-call rewriting
- **Knowledge base**: `extractor` (PDF/Word/Excel/PPT → Markdown, using `lopdf` / `zip` / `quick-xml`) → `chunker` → `embedder` (remote) → `store` (qdrant-edge shard) → `retriever` (retrieval injection) → `ingest` (background tasks); SSE event stream at `GET /api/knowledge/events?token=`
- **Agent**: WebSocket `GET /api/agent/ws` (including `notifications/ws` and `terminal/ws`), workspace / session / message persistence, per-workspace execution locks and per-session turn locks, context compaction and automatic titles, multi-role sub-agents (`agent_roles`, `mode=subagent|primary|all`, tool whitelists/blacklists, model overrides, `@role-name` switching)

## Development

### Backend

```bash
cargo check
cargo test -p rust-tunnel-common --lib
cargo test -p rust-tunnel-client --lib
cargo test -p rust-tunnel-server --lib            # without RAG, fast
cargo test -p rust-tunnel-server --lib --features rag  # with RAG
cargo test                                        # root e2e (rag included via dev-dep)
cargo test -j 2                                   # when memory-constrained (e2e compiles qdrant-edge, which is heavy)
cargo clippy -p rust-tunnel-server
cargo run -p rust-tunnel-server --bin checkdb     # SQLite diagnostics
```

### Frontend

```bash
cd frontend
npm install
npm run dev          # Vite HMR, /api → localhost:3000
npm run build        # tsc + Vite
npm run lint         # ESLint --max-warnings 0
npm test             # Vitest (jsdom)
```

### Build Cache

```bash
du -sh target
cargo clean -p rust-tunnel-server
cargo clean
```

Lint baseline: `clippy::pedantic = deny` (`doc_markdown = allow`); `unwrap_used` / `expect_used` / `panic` / `unwrap_in_result` = deny; `missing_docs = deny`.

## API Overview

See [`crates/server/src/mgmt/api/mod.rs`](crates/server/src/mgmt/api/mod.rs) for the complete route list.

- Public: `POST /api/login`, `GET /api/health`, `GET /api/knowledge/events` (SSE, `?token=`), `GET /api/stats/stream`, `GET /api/logs/stream`, `GET /api/agent/ws`, etc. Downloads: `GET /api/client-downloads/:version/:file`, `GET /api/wiki-downloads/:version/:file` (public, validated via `?token=`)
- Protected (JWT required when a password is set): `POST /api/logout`, `/api/clients`, `/api/server-auth`, `/api/stats/query`, `/api/stats/summary`, `/api/shadowsocks/*`, `/api/trojan/*`, `/api/mesh/*`, `/api/dns/*`, `/api/logs/*`, `/api/proxy/rules`, `/api/acme/*`, `/api/llm/*`, `/api/knowledge/*`, `/api/agent/*`, `/api/preferences`, `/api/settings`, etc.
- LLM: `/api/llm/gateway`, `/api/llm/providers`, `/api/llm/providers/:provider_id/models`, `/api/llm/models`, `/api/llm/api-keys`, `/api/llm/usage/*`, `/api/llm/model-groups/*`, plus `POST /v1/responses` and `POST /v1/chat/completions`
- Knowledge container: `/api/knowledge` CRUD, `/api/knowledge/:id/docs`, `/api/knowledge/:id/query` (retrieval preview), `/api/knowledge/:id/pages|graph|search`, `/api/knowledge/search`
- Agent: `/api/agent/workspaces`, `/api/agent/workspaces/:id/files|fs/*|git/*|github/*|sessions`, `/api/agent/sessions/:id` (including `/model`, `/role`, `/archive`, `/messages`, `/export`), `/api/agent/roles`, `/api/agent/default-model`

## CI/CD

GitHub Actions (see [`.github/workflows/`](.github/workflows/)):

- `release-server.yml` (manual): frontend build → `x86_64-unknown-linux-musl` static compilation (`--features rag,embed-frontend`) → config rendered from `contrib/config.toml.template` and validated → SCP binary + systemd unit + config → SSH restart
- `release-client.yml` (tag `v*` / manual): four-platform matrix build of client binaries, SCP'd to `${DEPLOY_PATH}/client/<tag>/`; `finalize-client` generates `SHA256SUMS` and updates the `latest` symlink
- `release-wiki-client.yml` (tag `wiki-v*` / manual): Tauri 2 installers (macOS aarch64/x86_64 `.dmg` + Windows `.msi` / NSIS `.exe`), renamed to `wiki-desktop-<os>-<arch>[-setup].<ext>` and SCP'd to `${DEPLOY_PATH}/wiki/<version>/` (directory name = tag without the `wiki-` prefix); `finalize-wiki` generates checksums and the `latest` symlink

Deployment uses systemd ([`contrib/rust-tunnel-server.service`](contrib/rust-tunnel-server.service)); both archive directories are exposed read-only to the Web downloads page via `client_dist_dir` / `wiki_dist_dir` as absolute paths.

## Dependencies

Backend (excerpt; see [`Cargo.toml`](Cargo.toml) for the full list): `tokio`, `axum` / `tower-http` / `hyper`, `sqlx` / `chrono` / `uuid`, `rustls` / `tokio-rustls` / `rcgen` / `webpki-roots`, `shadowsocks`, `qdrant-edge`, `tantivy` / `comrak` / `petgraph`, `portable-pty`, `agent-client-protocol` (`unstable_elicitation`), `hickory-proto` / `trust-dns-resolver` / `instant-acme`, etc. For the frontend, see [`frontend/package.json`](frontend/package.json).

## License

Dual-licensed under [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE); choose either.
