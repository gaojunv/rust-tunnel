pub mod relay;
pub mod router;
pub mod stun;

use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

use crate::common::{ControlMessage, MeshMember, MeshRoute, MeshService};

use self::relay::MeshRelay;
use self::router::MeshRouter;

/// Central mesh manager: combines routing table + relay + per-client control channels
#[derive(Clone)]
pub struct MeshManager {
    pub router: Arc<Mutex<MeshRouter>>,
    pub relay: MeshRelay,
    /// client_name -> mpsc Sender for ControlMessage delivery
    clients: Arc<Mutex<std::collections::HashMap<String, mpsc::Sender<ControlMessage>>>>,
}

impl Default for MeshManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MeshManager {
    pub fn new() -> Self {
        Self {
            router: Arc::new(Mutex::new(MeshRouter::new())),
            relay: MeshRelay::new(),
            clients: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Register a client's control channel for mesh and relay communication
    pub async fn register_client(&self, client_name: &str, tx: mpsc::Sender<ControlMessage>) {
        self.relay.register(client_name, tx.clone()).await;
        self.clients
            .lock()
            .await
            .insert(client_name.to_string(), tx);
    }

    /// Unregister a client from all meshes and relay
    pub async fn unregister_client(&self, client_name: &str) {
        self.relay.unregister(client_name).await;
        self.clients.lock().await.remove(client_name);
        self.router.lock().await.remove_client(client_name);
    }

    /// Join a mesh. Returns the updated member list (excluding requester).
    pub async fn join_mesh(&self, mesh_id: &str, client_name: &str) -> Vec<MeshMember> {
        self.router.lock().await.join(mesh_id, client_name);
        self.get_members_for(mesh_id, client_name).await
    }

    /// Leave a mesh. Returns the updated member list.
    pub async fn leave_mesh(&self, mesh_id: &str, client_name: &str) -> Vec<MeshMember> {
        self.router.lock().await.leave(mesh_id, client_name);
        self.get_members_for(mesh_id, client_name).await
    }

    /// Register services for a client in a mesh
    pub async fn register_services(
        &self,
        mesh_id: &str,
        client_name: &str,
        services: Vec<MeshService>,
    ) {
        self.router
            .lock()
            .await
            .register_services(mesh_id, client_name, services);
    }

    /// Build member list for broadcast (all members including requester)
    async fn get_members_for(&self, mesh_id: &str, _exclude: &str) -> Vec<MeshMember> {
        let router = self.router.lock().await;
        router
            .get_members(mesh_id)
            .into_iter()
            .map(|r| MeshMember {
                client_name: r.client_name.clone(),
                public_addr: r.public_addr.clone(),
                online: true,
            })
            .collect()
    }

    /// Send message to a specific client by name
    pub async fn send_to_client(&self, client_name: &str, msg: ControlMessage) -> bool {
        if let Some(tx) = self.clients.lock().await.get(client_name) {
            tx.send(msg).await.is_ok()
        } else {
            false
        }
    }

    /// Broadcast to all clients in a mesh (excluding the sender)
    pub async fn broadcast_to_mesh(
        &self,
        mesh_id: &str,
        msg: ControlMessage,
        exclude: Option<&str>,
    ) {
        let router = self.router.lock().await;
        let clients = self.clients.lock().await;
        for member in router.get_members(mesh_id) {
            if let Some(exclude_name) = exclude {
                if member.client_name == exclude_name {
                    continue;
                }
            }
            if let Some(tx) = clients.get(&member.client_name) {
                let _ = tx.send(msg.clone()).await;
            }
        }
    }

    /// Get all mesh networks and their members (for API)
    pub async fn list_networks(&self) -> Vec<(String, Vec<MeshRoute>)> {
        let router = self.router.lock().await;
        router
            .list_networks()
            .into_iter()
            .map(|id| {
                let members = router.get_members(&id).into_iter().cloned().collect();
                (id, members)
            })
            .collect()
    }

    /// Get a specific mesh's details
    pub async fn get_mesh(&self, mesh_id: &str) -> Option<Vec<MeshRoute>> {
        let router = self.router.lock().await;
        if router.list_networks().contains(&mesh_id.to_string()) {
            Some(router.get_members(mesh_id).into_iter().cloned().collect())
        } else {
            None
        }
    }

    /// Look up a service in a mesh. Returns (route, service) if found.
    pub async fn lookup_service(
        &self,
        mesh_id: &str,
        service_name: &str,
    ) -> Option<(MeshRoute, MeshService)> {
        let router = self.router.lock().await;
        for member in router.get_members(mesh_id) {
            for svc in &member.services {
                if svc.name == service_name {
                    return Some((member.clone(), svc.clone()));
                }
            }
        }
        None
    }
}
