use clap::{Parser, ArgAction};
use std::net::SocketAddr;

/// Client endpoint for rust-tunnel intranet penetration tool
#[derive(Parser, Debug, Clone)]
pub struct ClientConfig {
    /// Server address to connect (e.g., server-ip:8080)
    #[clap(long)]
    pub server: String,

    /// Forward rule: REMOTE_PORT:LOCAL_HOST:LOCAL_PORT
    /// Example: 8080:localhost:80
    #[clap(long = "forward", action = ArgAction::Append)]
    pub forwards: Vec<String>,

    /// Log level (trace, debug, info, warn, error)
    #[clap(long, default_value = "info")]
    pub log: String,
}

#[derive(Debug, Clone)]
pub struct ForwardRule {
    pub remote_port: u16,
    pub local_addr: SocketAddr,
}

impl ClientConfig {
    pub fn parse_forwards(&self) -> Result<Vec<ForwardRule>, String> {
        let mut rules = Vec::new();
        for forward in &self.forwards {
            let parts: Vec<&str> = forward.split(':').collect();
            if parts.len() != 3 {
                return Err(format!("Invalid forward format: '{}', expected: REMOTE_PORT:LOCAL_HOST:LOCAL_PORT", forward));
            }
            let remote_port = parts[0].parse::<u16>()
                .map_err(|e| format!("Invalid remote port: {}", e))?;
            let local_host = parts[1];
            let local_port = parts[2].parse::<u16>()
                .map_err(|e| format!("Invalid local port: {}", e))?;
            let local_addr = format!("{}:{}", local_host, local_port)
                .parse::<SocketAddr>()
                .map_err(|e| format!("Invalid local address: {}", e))?;
            rules.push(ForwardRule {
                remote_port,
                local_addr,
            });
        }
        Ok(rules)
    }
}
