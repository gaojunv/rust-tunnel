use crate::dns::zone::DnsZone;
use rust_tunnel_common::DnsRecord;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Central DNS registry that ties tunnel ports and mesh services to DNS records.
/// Thread-safe wrapper around DnsZone.
#[derive(Clone)]
pub struct DnsRegistry {
    zone: Arc<Mutex<DnsZone>>,
    /// Server's public IP for tunnel A records
    server_ip: Arc<Mutex<String>>,
    tunnel_domain: String,
    mesh_domain: String,
}

impl DnsRegistry {
    /// 创建 DNS 注册表。
    #[must_use]
    pub fn new(server_ip: String, tunnel_domain: String, mesh_domain: String) -> Self {
        Self {
            zone: Arc::new(Mutex::new(DnsZone::new())),
            server_ip: Arc::new(Mutex::new(server_ip)),
            tunnel_domain,
            mesh_domain,
        }
    }

    /// Register a tunnel port with a custom DNS name
    pub async fn register_tunnel(&self, dns_name: &str, port: u16, protocol: Option<&str>) {
        let a_name = format!("{dns_name}.{}", self.tunnel_domain);
        let ip = self.server_ip.lock().await.clone();
        let mut zone = self.zone.lock().await;

        // Remove existing records for this name to prevent duplicates on reconnect
        zone.remove_records(&a_name);

        zone.add_record(DnsRecord::TunnelA {
            name: a_name.clone(),
            target_ip: ip.clone(),
            port,
        });

        if let Some(proto) = protocol {
            let srv_name = format!("_{proto}._tcp.{dns_name}.{}", self.tunnel_domain);
            zone.remove_records(&srv_name);
            zone.add_record(DnsRecord::TunnelSrv {
                name: srv_name,
                target: a_name,
                port,
            });
        }
    }

    /// Auto-register tunnel port with default name "port-{port}"
    /// Returns the generated FQDN
    pub async fn register_tunnel_default(&self, port: u16, protocol: Option<&str>) -> String {
        let dns_name = format!("port-{port}");
        self.register_tunnel(&dns_name, port, protocol).await;
        format!("{dns_name}.{}", self.tunnel_domain)
    }

    /// Unregister all records for a tunnel by DNS name and port
    pub async fn unregister_tunnel(&self, dns_name: &str, port: u16) {
        let mut zone = self.zone.lock().await;
        let a_name = format!("{dns_name}.{}", self.tunnel_domain);
        zone.remove_records(&a_name);
        // Also clean up default name for this port
        let default_name = format!("port-{port}.{}", self.tunnel_domain);
        if default_name != a_name {
            zone.remove_records(&default_name);
        }
    }

    /// Register a mesh service as DNS record
    pub async fn register_mesh_service(
        &self,
        mesh_id: &str,
        service_name: &str,
        protocol: &str,
        target_ip: &str,
        port: u16,
    ) {
        let name = format!("{service_name}.{mesh_id}.{}", self.mesh_domain);
        let srv_name = format!(
            "_{protocol}._tcp.{service_name}.{mesh_id}.{}",
            self.mesh_domain
        );
        let mut zone = self.zone.lock().await;

        // Remove existing records to prevent duplicates on reconnect
        zone.remove_records(&name);
        zone.remove_records(&srv_name);

        zone.add_record(DnsRecord::MeshA {
            name: name.clone(),
            target_ip: target_ip.to_string(),
        });

        zone.add_record(DnsRecord::MeshSrv {
            name: srv_name,
            target: name,
            port,
        });
    }

    /// Unregister all mesh services for a client
    pub async fn unregister_mesh_client(&self, mesh_id: &str) {
        let mut zone = self.zone.lock().await;
        let mesh_domain = self.mesh_domain.clone();
        let mid = mesh_id.to_string();
        zone.remove_by_predicate(move |r| match r {
            DnsRecord::MeshA { name, .. } | DnsRecord::MeshSrv { name, .. } => {
                name.contains(&mid) && name.ends_with(&mesh_domain)
            }
            _ => false,
        });
    }

    /// Query A records for a name
    pub async fn query_a(&self, name: &str) -> Vec<String> {
        self.zone.lock().await.get_a_records(name)
    }

    /// Query SRV records for a name
    pub async fn query_srv(&self, name: &str) -> Vec<(String, u16)> {
        self.zone.lock().await.get_srv_records(name)
    }

    /// List all DNS records
    pub async fn list_records(&self) -> Vec<DnsRecord> {
        self.zone.lock().await.list_all()
    }

    /// Add a manual DNS record (from API)
    pub async fn add_manual_record(&self, record: DnsRecord) {
        self.zone.lock().await.add_record(record);
    }

    /// Remove a DNS record by name (from API)
    pub async fn remove_record(&self, name: &str) -> usize {
        self.zone.lock().await.remove_records(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_register_tunnel() {
        let registry = DnsRegistry::new(
            "10.0.0.1".into(),
            "tunnel.local".into(),
            "mesh.local".into(),
        );
        registry.register_tunnel("webapp", 9000, Some("http")).await;

        let a = registry.query_a("webapp.tunnel.local").await;
        assert_eq!(a, vec!["10.0.0.1"]);

        let srv = registry.query_srv("_http._tcp.webapp.tunnel.local").await;
        assert_eq!(srv.len(), 1);
        assert_eq!(srv[0].1, 9000);
    }

    #[tokio::test]
    async fn test_register_tunnel_default() {
        let registry = DnsRegistry::new(
            "10.0.0.1".into(),
            "tunnel.local".into(),
            "mesh.local".into(),
        );
        let name = registry.register_tunnel_default(8080, None).await;
        assert_eq!(name, "port-8080.tunnel.local");

        let a = registry.query_a("port-8080.tunnel.local").await;
        assert_eq!(a, vec!["10.0.0.1"]);
    }

    #[tokio::test]
    async fn test_unregister_tunnel() {
        let registry = DnsRegistry::new(
            "10.0.0.1".into(),
            "tunnel.local".into(),
            "mesh.local".into(),
        );
        registry.register_tunnel("webapp", 9000, None).await;
        assert_eq!(registry.query_a("webapp.tunnel.local").await.len(), 1);

        registry.unregister_tunnel("webapp", 9000).await;
        assert!(registry.query_a("webapp.tunnel.local").await.is_empty());
    }

    #[tokio::test]
    async fn test_register_mesh_service() {
        let registry = DnsRegistry::new(
            "10.0.0.1".into(),
            "tunnel.local".into(),
            "mesh.local".into(),
        );
        registry
            .register_mesh_service("mynet", "db", "mysql", "192.168.1.100", 3306)
            .await;

        let a = registry.query_a("db.mynet.mesh.local").await;
        assert_eq!(a, vec!["192.168.1.100"]);
    }

    #[tokio::test]
    async fn test_unregister_mesh_client() {
        let registry = DnsRegistry::new(
            "10.0.0.1".into(),
            "tunnel.local".into(),
            "mesh.local".into(),
        );
        registry
            .register_mesh_service("mynet", "db", "mysql", "192.168.1.100", 3306)
            .await;
        registry
            .register_mesh_service("mynet", "api", "http", "192.168.1.100", 8080)
            .await;

        registry.unregister_mesh_client("mynet").await;
        assert!(registry.query_a("db.mynet.mesh.local").await.is_empty());
        assert!(registry.query_a("api.mynet.mesh.local").await.is_empty());
    }
}
