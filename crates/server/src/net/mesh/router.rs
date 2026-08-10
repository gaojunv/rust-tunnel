use rust_tunnel_common::MeshRoute;
use std::collections::HashMap;

/// Mesh routing table tracking all mesh networks and their members.
pub struct MeshRouter {
    /// mesh_id -> (client_name -> MeshRoute)
    networks: HashMap<String, HashMap<String, MeshRoute>>,
}

impl Default for MeshRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl MeshRouter {
    pub fn new() -> Self {
        Self {
            networks: HashMap::new(),
        }
    }

    /// Join a client to a mesh network
    pub fn join(&mut self, mesh_id: &str, client_name: &str) {
        self.networks
            .entry(mesh_id.to_string())
            .or_default()
            .entry(client_name.to_string())
            .or_insert_with(|| MeshRoute {
                client_name: client_name.to_string(),
                public_addr: None,
                p2p_available: false,
                services: Vec::new(),
            });
    }

    /// Remove a client from a mesh network. Returns true if the client was a member.
    pub fn leave(&mut self, mesh_id: &str, client_name: &str) -> bool {
        if let Some(members) = self.networks.get_mut(mesh_id) {
            let removed = members.remove(client_name).is_some();
            if members.is_empty() {
                self.networks.remove(mesh_id);
            }
            return removed;
        }
        false
    }

    /// Update a client's public address
    pub fn update_address(&mut self, mesh_id: &str, client_name: &str, addr: String) -> bool {
        if let Some(members) = self.networks.get_mut(mesh_id) {
            if let Some(route) = members.get_mut(client_name) {
                route.public_addr = Some(addr);
                return true;
            }
        }
        false
    }

    /// Set P2P availability for a client
    pub fn set_p2p_available(&mut self, mesh_id: &str, client_name: &str, available: bool) {
        if let Some(members) = self.networks.get_mut(mesh_id) {
            if let Some(route) = members.get_mut(client_name) {
                route.p2p_available = available;
            }
        }
    }

    /// Register services for a client in a mesh
    pub fn register_services(
        &mut self,
        mesh_id: &str,
        client_name: &str,
        services: Vec<rust_tunnel_common::MeshService>,
    ) {
        if let Some(members) = self.networks.get_mut(mesh_id) {
            if let Some(route) = members.get_mut(client_name) {
                route.services = services;
            }
        }
    }

    /// Get all members of a mesh
    pub fn get_members(&self, mesh_id: &str) -> Vec<&MeshRoute> {
        self.networks
            .get(mesh_id)
            .map(|m| m.values().collect())
            .unwrap_or_default()
    }

    /// Find which meshes a client belongs to
    pub fn get_client_meshes(&self, client_name: &str) -> Vec<String> {
        self.networks
            .iter()
            .filter(|(_, members)| members.contains_key(client_name))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Look up a specific client in a mesh
    pub fn get_member(&self, mesh_id: &str, client_name: &str) -> Option<&MeshRoute> {
        self.networks.get(mesh_id)?.get(client_name)
    }

    /// Remove a client from all meshes. Returns list of affected mesh IDs.
    pub fn remove_client(&mut self, client_name: &str) -> Vec<String> {
        let affected: Vec<String> = self.get_client_meshes(client_name);
        for mesh_id in &affected.clone() {
            self.leave(mesh_id, client_name);
        }
        affected
    }

    /// List all mesh network IDs
    pub fn list_networks(&self) -> Vec<String> {
        self.networks.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_join_and_leave() {
        let mut router = MeshRouter::new();
        router.join("mesh1", "client-a");
        assert_eq!(router.get_members("mesh1").len(), 1);

        router.leave("mesh1", "client-a");
        assert_eq!(router.get_members("mesh1").len(), 0);
    }

    #[test]
    fn test_leave_nonexistent() {
        let mut router = MeshRouter::new();
        assert!(!router.leave("mesh1", "nobody"));
    }

    #[test]
    fn test_update_address() {
        let mut router = MeshRouter::new();
        router.join("mesh1", "client-a");
        assert!(router.update_address("mesh1", "client-a", "1.2.3.4:12345".into()));
        let member = router.get_member("mesh1", "client-a").unwrap();
        assert_eq!(member.public_addr, Some("1.2.3.4:12345".into()));
    }

    #[test]
    fn test_remove_client_from_all() {
        let mut router = MeshRouter::new();
        router.join("mesh1", "client-a");
        router.join("mesh2", "client-a");
        router.join("mesh2", "client-b");

        let affected = router.remove_client("client-a");
        assert_eq!(affected.len(), 2);
        assert_eq!(router.get_members("mesh1").len(), 0);
        assert_eq!(router.get_members("mesh2").len(), 1);
    }

    #[test]
    fn test_list_networks() {
        let mut router = MeshRouter::new();
        router.join("mesh1", "client-a");
        router.join("mesh2", "client-b");
        let nets = router.list_networks();
        assert_eq!(nets.len(), 2);
    }

    #[test]
    fn test_get_client_meshes() {
        let mut router = MeshRouter::new();
        router.join("mesh1", "client-a");
        router.join("mesh2", "client-a");
        let meshes = router.get_client_meshes("client-a");
        assert_eq!(meshes.len(), 2);
    }

    #[test]
    fn test_register_services() {
        let mut router = MeshRouter::new();
        router.join("mesh1", "client-a");
        router.register_services(
            "mesh1",
            "client-a",
            vec![rust_tunnel_common::MeshService {
                name: "db".into(),
                protocol: "mysql".into(),
                local_addr: "localhost:3306".into(),
            }],
        );
        let member = router.get_member("mesh1", "client-a").unwrap();
        assert_eq!(member.services.len(), 1);
        assert_eq!(member.services[0].name, "db");
    }
}
