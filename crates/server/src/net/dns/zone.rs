use rust_tunnel_common::DnsRecord;
use std::collections::HashMap;

/// In-memory DNS zone for tunnel.local and mesh.local domains.
pub struct DnsZone {
    /// All records keyed by lowercase domain name
    records: HashMap<String, Vec<DnsRecord>>,
}

impl Default for DnsZone {
    fn default() -> Self {
        Self::new()
    }
}

impl DnsZone {
    /// 创建空 DNS 区域。
    #[must_use]
    pub fn new() -> Self {
        Self {
            records: HashMap::new(),
        }
    }

    /// 添加 DNS 记录。
    pub fn add_record(&mut self, record: DnsRecord) {
        let name = record.name().to_lowercase();
        self.records.entry(name).or_default().push(record);
    }

    /// Remove all records for a given name. Returns count removed.
    pub fn remove_records(&mut self, name: &str) -> usize {
        self.records
            .remove(&name.to_lowercase())
            .map_or(0, |v| v.len())
    }

    /// Remove records matching a predicate. Returns count removed.
    pub fn remove_by_predicate<F>(&mut self, predicate: F) -> usize
    where
        F: Fn(&DnsRecord) -> bool,
    {
        let mut count = 0;
        self.records.retain(|_, records| {
            let before = records.len();
            records.retain(|r| !predicate(r));
            count += before - records.len();
            !records.is_empty()
        });
        count
    }

    /// 获取指定名称的所有记录。
    #[must_use]
    pub fn get_records(&self, name: &str) -> Vec<&DnsRecord> {
        self.records
            .get(&name.to_lowercase())
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    }

    /// 获取 A 记录（返回 IP 地址列表）。
    #[must_use]
    pub fn get_a_records(&self, name: &str) -> Vec<String> {
        self.get_records(name)
            .iter()
            .filter_map(|r| match r {
                DnsRecord::TunnelA { target_ip, .. } | DnsRecord::MeshA { target_ip, .. } => Some(target_ip.clone()),
                _ => None,
            })
            .collect()
    }

    /// 获取 SRV 记录（返回目标与端口对）。
    #[must_use]
    pub fn get_srv_records(&self, name: &str) -> Vec<(String, u16)> {
        self.get_records(name)
            .iter()
            .filter_map(|r| match r {
                DnsRecord::TunnelSrv { target, port, .. } | DnsRecord::MeshSrv { target, port, .. } => Some((target.clone(), *port)),
                _ => None,
            })
            .collect()
    }

    /// 列出所有唯一记录名。
    #[must_use]
    pub fn list_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.records.keys().cloned().collect();
        names.sort();
        names
    }

    /// 列出所有记录（克隆返回）。
    #[must_use]
    pub fn list_all(&self) -> Vec<DnsRecord> {
        self.records
            .values()
            .flat_map(|v| v.iter().cloned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tunnel_a(name: &str, ip: &str, port: u16) -> DnsRecord {
        DnsRecord::TunnelA {
            name: name.to_string(),
            target_ip: ip.to_string(),
            port,
        }
    }

    #[test]
    fn test_add_and_get_record() {
        let mut zone = DnsZone::new();
        zone.add_record(make_tunnel_a("webapp.tunnel.local", "10.0.0.1", 9000));

        let records = zone.get_records("webapp.tunnel.local");
        assert_eq!(records.len(), 1);

        let a_records = zone.get_a_records("webapp.tunnel.local");
        assert_eq!(a_records, vec!["10.0.0.1"]);
    }

    #[test]
    fn test_remove_records() {
        let mut zone = DnsZone::new();
        zone.add_record(make_tunnel_a("webapp.tunnel.local", "10.0.0.1", 9000));
        let removed = zone.remove_records("webapp.tunnel.local");
        assert_eq!(removed, 1);
        assert!(zone.get_records("webapp.tunnel.local").is_empty());
    }

    #[test]
    fn test_case_insensitive() {
        let mut zone = DnsZone::new();
        zone.add_record(make_tunnel_a("Webapp.Tunnel.Local", "10.0.0.1", 9000));
        assert_eq!(zone.get_a_records("webapp.tunnel.local").len(), 1);
        assert_eq!(zone.get_a_records("WEBAPP.TUNNEL.LOCAL").len(), 1);
    }

    #[test]
    fn test_remove_by_predicate() {
        let mut zone = DnsZone::new();
        zone.add_record(make_tunnel_a("a.tunnel.local", "10.0.0.1", 9000));
        zone.add_record(make_tunnel_a("b.tunnel.local", "10.0.0.2", 9001));

        let removed = zone
            .remove_by_predicate(|r| matches!(r, DnsRecord::TunnelA { port, .. } if *port == 9000));
        assert_eq!(removed, 1);
        assert_eq!(zone.list_names().len(), 1);
    }

    #[test]
    fn test_list_all() {
        let mut zone = DnsZone::new();
        zone.add_record(make_tunnel_a("a.tunnel.local", "10.0.0.1", 9000));
        zone.add_record(make_tunnel_a("b.tunnel.local", "10.0.0.2", 9001));
        assert_eq!(zone.list_all().len(), 2);
    }
}
