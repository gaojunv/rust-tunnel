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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_forwards_single() {
        let config = ClientConfig {
            server: "localhost:8080".into(),
            forwards: vec!["8080:localhost:80".into()],
            log: "info".into(),
        };

        let rules = config.parse_forwards().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].remote_port, 8080);
        assert_eq!(rules[0].local_addr, "localhost:80");
    }

    #[test]
    fn test_parse_forwards_multiple() {
        let config = ClientConfig {
            server: "localhost:8080".into(),
            forwards: vec![
                "8080:localhost:80".into(),
                "9000:127.0.0.1:3000".into(),
            ],
            log: "info".into(),
        };

        let rules = config.parse_forwards().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].remote_port, 8080);
        assert_eq!(rules[0].local_addr, "localhost:80");
        assert_eq!(rules[1].remote_port, 9000);
        assert_eq!(rules[1].local_addr, "127.0.0.1:3000");
    }

    #[test]
    fn test_parse_forwards_empty() {
        let config = ClientConfig {
            server: "localhost:8080".into(),
            forwards: vec![],
            log: "info".into(),
        };

        let rules = config.parse_forwards().unwrap();
        assert!(rules.is_empty());
    }

    #[test]
    fn test_parse_forwards_ipv6() {
        let config = ClientConfig {
            server: "localhost:8080".into(),
            forwards: vec!["8080:::1:80".into()],
            log: "info".into(),
        };

        let rules = config.parse_forwards().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].remote_port, 8080);
        assert_eq!(rules[0].local_addr, "::1:80");
    }

    #[test]
    fn test_parse_forwards_invalid_format() {
        let config = ClientConfig {
            server: "localhost:8080".into(),
            forwards: vec!["invalid".into()],
            log: "info".into(),
        };

        let result = config.parse_forwards();
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_forwards_invalid_remote_port() {
        let config = ClientConfig {
            server: "localhost:8080".into(),
            forwards: vec!["not_a_port:localhost:80".into()],
            log: "info".into(),
        };

        let result = config.parse_forwards();
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_forwards_invalid_local_port() {
        let config = ClientConfig {
            server: "localhost:8080".into(),
            forwards: vec!["8080:localhost:not_a_port".into()],
            log: "info".into(),
        };

        let result = config.parse_forwards();
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_forwards_port_out_of_range() {
        let config = ClientConfig {
            server: "localhost:8080".into(),
            forwards: vec!["70000:localhost:80".into()], // 70000 > 65535
            log: "info".into(),
        };

        let result = config.parse_forwards();
        assert!(result.is_err());
    }

    #[test]
    fn test_client_config_clone() {
        let config = ClientConfig {
            server: "localhost:8080".into(),
            forwards: vec!["8080:localhost:80".into()],
            log: "debug".into(),
        };

        let cloned = config.clone();
        assert_eq!(config.server, cloned.server);
        assert_eq!(config.forwards, cloned.forwards);
        assert_eq!(config.log, cloned.log);
    }

    #[test]
    fn test_forward_rule_clone() {
        let rule = ForwardRule {
            remote_port: 8080,
            local_addr: "localhost:80".into(),
        };

        let cloned = rule.clone();
        assert_eq!(rule.remote_port, cloned.remote_port);
        assert_eq!(rule.local_addr, cloned.local_addr);
    }
}
