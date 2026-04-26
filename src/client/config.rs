use clap::{Parser, ArgAction};

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
    pub local_addr: String,
}

impl ClientConfig {
    pub fn parse_forwards(&self) -> Result<Vec<ForwardRule>, String> {
        let mut rules = Vec::new();
        for forward in &self.forwards {
            // Use rsplitn to split from the end: REMOTE_PORT:LOCAL_HOST:LOCAL_PORT
            // This allows LOCAL_HOST to contain colons (IPv6)
            let mut parts = forward.rsplitn(2, ':');
            let local_port_str = parts.next().ok_or_else(||
                format!("Invalid forward format: '{}', expected: REMOTE_PORT:LOCAL_HOST:LOCAL_PORT", forward))?;
            let remaining = parts.next().ok_or_else(||
                format!("Invalid forward format: '{}', expected: REMOTE_PORT:LOCAL_HOST:LOCAL_PORT", forward))?;

            let mut parts2 = remaining.splitn(2, ':');
            let remote_port_str = parts2.next().unwrap();
            let local_host = parts2.next().ok_or_else(||
                format!("Invalid forward format: '{}', expected: REMOTE_PORT:LOCAL_HOST:LOCAL_PORT", forward))?;

            let remote_port = remote_port_str.parse::<u16>()
                .map_err(|e| format!("Invalid remote port: {}", e))?;
            let local_port = local_port_str.parse::<u16>()
                .map_err(|e| format!("Invalid local port: {}", e))?;
            let local_addr = format!("{}:{}", local_host, local_port);
            rules.push(ForwardRule {
                remote_port,
                local_addr,
            });
        }
        Ok(rules)
    }
}
