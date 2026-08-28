use clap::Parser;

/// Server endpoint for rust-tunnel intranet penetration tool
#[derive(Parser, Debug, Clone)]
pub struct ServerCli {
    /// Path to configuration file (TOML format)
    #[clap(long = "config")]
    pub config_file: Option<String>,

    /// Address to listen for control connections from clients
    /// Format: 0.0.0.0:8080
    #[clap(long = "bind")]
    pub control_addr: Option<String>,

    /// Address to listen for HTTP API connections
    /// Format: 0.0.0.0:3000
    #[clap(long = "api-bind")]
    pub api_addr: Option<String>,

    /// Password for web interface authentication (optional)
    #[clap(long = "admin-password")]
    pub admin_password: Option<String>,

    /// Secret key for JWT tokens (auto-generated if not provided)
    #[clap(long = "jwt-secret")]
    pub jwt_secret: Option<String>,

    /// Authentication token for client connections (optional but recommended)
    /// If set, clients must provide this token to register
    #[clap(long = "client-auth-token")]
    pub client_auth_token: Option<String>,

    /// Enable TLS encryption for control channel
    /// If true, clients must connect using TLS
    #[clap(long = "tls")]
    pub tls: Option<bool>,

    /// Path to TLS certificate file (PEM format)
    /// If not provided and TLS is enabled, a self-signed cert will be generated
    #[clap(long = "tls-cert")]
    pub tls_cert: Option<String>,

    /// Path to TLS private key file (PEM format)
    /// If not provided and TLS is enabled, a self-signed key will be generated
    #[clap(long = "tls-key")]
    pub tls_key: Option<String>,

    /// Log level (trace, debug, info, warn, error)
    #[clap(long)]
    pub log: Option<String>,

    /// Path to SQLite database file
    #[clap(long = "db-path")]
    pub db_path: Option<String>,

    /// Directory holding versioned client binaries (`<dir>/<tag>/…`), served by the web download page
    #[clap(long = "client-dist-dir")]
    pub client_dist_dir: Option<String>,

    /// Enable Shadowsocks proxy service
    #[clap(long = "ss-enabled")]
    pub ss_enabled: Option<bool>,

    /// Shadowsocks listen port
    #[clap(long = "ss-port")]
    pub ss_port: Option<u16>,

    /// Shadowsocks encryption method (aes-256-gcm, chacha20-ietf-poly1305)
    #[clap(long = "ss-cipher")]
    pub ss_cipher: Option<String>,

    /// Shadowsocks password
    #[clap(long = "ss-password")]
    pub ss_password: Option<String>,

    /// Enable Trojan proxy service
    #[clap(long = "trojan-enabled")]
    pub trojan_enabled: Option<bool>,

    /// Trojan listen port
    #[clap(long = "trojan-port")]
    pub trojan_port: Option<u16>,

    /// Trojan password
    #[clap(long = "trojan-password")]
    pub trojan_password: Option<String>,

    /// Trojan fallback address for non-Trojan traffic (default: 127.0.0.1:80)
    #[clap(long = "trojan-fallback")]
    pub trojan_fallback: Option<String>,

    /// Enable embedded DNS server
    #[clap(long = "dns-enabled")]
    pub dns_enabled: Option<bool>,

    /// DNS server bind address (default: 0.0.0.0:53)
    #[clap(long = "dns-bind")]
    pub dns_bind: Option<String>,

    /// Tunnel domain suffix (default: tunnel.local)
    #[clap(long = "dns-tunnel-domain")]
    pub dns_tunnel_domain: Option<String>,

    /// Mesh domain suffix (default: mesh.local)
    #[clap(long = "dns-mesh-domain")]
    pub dns_mesh_domain: Option<String>,

    // Reverse Proxy options
    /// Enable reverse proxy service
    #[clap(long = "reverse-proxy-enabled")]
    pub reverse_proxy_enabled: Option<bool>,

    /// Maximum connections for reverse proxy
    #[clap(long = "reverse-proxy-max-connections")]
    pub reverse_proxy_max_connections: Option<u32>,

    /// Connection timeout in seconds for reverse proxy
    #[clap(long = "reverse-proxy-connection-timeout")]
    pub reverse_proxy_connection_timeout: Option<u64>,

    /// Buffer size for reverse proxy
    #[clap(long = "reverse-proxy-buffer-size")]
    pub reverse_proxy_buffer_size: Option<usize>,

    // API TLS options
    /// Enable TLS for the API server (requires ACME or manual cert)
    #[clap(long = "api-tls")]
    pub api_tls: Option<bool>,

    /// Domain name for API server TLS certificate (ACME)
    #[clap(long = "api-domain")]
    pub api_domain: Option<String>,

    // ACME options
    /// Enable ACME certificate management
    #[clap(long = "acme-enabled")]
    pub acme_enabled: Option<bool>,

    /// ACME server URL
    #[clap(long = "acme-server-url")]
    pub acme_server_url: Option<String>,

    /// Certificate storage directory
    #[clap(long = "acme-cert-dir")]
    pub acme_cert_dir: Option<String>,

    /// Enable automatic certificate renewal
    #[clap(long = "acme-auto-renew")]
    pub acme_auto_renew: Option<bool>,

    /// Renewal check interval in hours
    #[clap(long = "acme-renewal-check-interval")]
    pub acme_renewal_check_interval: Option<u64>,

    /// Days before expiry to trigger renewal
    #[clap(long = "acme-renewal-days-before-expiry")]
    pub acme_renewal_days_before_expiry: Option<u64>,

    /// Contact email for ACME/Let's Encrypt
    #[clap(long = "acme-email")]
    pub acme_email: Option<String>,

    /// Agree to Let's Encrypt Terms of Service
    #[clap(long = "acme-tos-agreed")]
    pub acme_tos_agreed: Option<bool>,
}
