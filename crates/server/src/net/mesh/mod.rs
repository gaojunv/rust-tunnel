//! Mesh 网络管理：路由表、P2P 中继与客户端控制通道分发。

/// Mesh 中继。
pub mod relay;
/// Mesh 路由表。
pub mod router;
/// STUN 服务。
pub mod stun;

use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

use rust_tunnel_common::{ControlMessage, MeshMember, MeshRoute, MeshService};

use self::relay::MeshRelay;
use self::router::MeshRouter;

/// Mesh 网络中心管理器：聚合路由表、中继与客户端控制通道。
#[derive(Clone)]
pub struct MeshManager {
    /// 路由表。
    pub router: Arc<Mutex<MeshRouter>>,
    /// P2P 中继。
    pub relay: MeshRelay,
    /// client_name -> 控制消息发送端
    clients: Arc<Mutex<std::collections::HashMap<String, mpsc::Sender<ControlMessage>>>>,
}

impl Default for MeshManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MeshManager {
    /// 创建空 Mesh 管理器。
    #[must_use]
    pub fn new() -> Self {
        Self {
            router: Arc::new(Mutex::new(MeshRouter::new())),
            relay: MeshRelay::new(),
            clients: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// 注册客户端控制通道（同时注册到中继）。
    pub async fn register_client(&self, client_name: &str, tx: mpsc::Sender<ControlMessage>) {
        self.relay.register(client_name, tx.clone()).await;
        self.clients
            .lock()
            .await
            .insert(client_name.to_string(), tx);
    }

    /// 从所有 mesh 与中继中注销客户端。
    pub async fn unregister_client(&self, client_name: &str) {
        self.relay.unregister(client_name).await;
        self.clients.lock().await.remove(client_name);
        self.router.lock().await.remove_client(client_name);
    }

    /// 加入 mesh，返回更新后的成员列表（不含请求者由调用方过滤）。
    pub async fn join_mesh(&self, mesh_id: &str, client_name: &str) -> Vec<MeshMember> {
        self.router.lock().await.join(mesh_id, client_name);
        self.get_members_for(mesh_id, client_name).await
    }

    /// 离开 mesh，返回更新后的成员列表。
    pub async fn leave_mesh(&self, mesh_id: &str, client_name: &str) -> Vec<MeshMember> {
        self.router.lock().await.leave(mesh_id, client_name);
        self.get_members_for(mesh_id, client_name).await
    }

    /// 为 mesh 中的客户端注册服务。
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

    /// 构造广播用成员列表（包含请求者）。
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

    /// 向指定客户端发送控制消息，离线时返回 false。
    pub async fn send_to_client(&self, client_name: &str, msg: ControlMessage) -> bool {
        if let Some(tx) = self.clients.lock().await.get(client_name) {
            tx.send(msg).await.is_ok()
        } else {
            false
        }
    }

    /// 向 mesh 内所有客户端广播（可排除发送者）。
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

    /// 列出所有 mesh 及其成员（供 API 使用）。
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

    /// 查询单个 mesh 详情，不存在时返回 None。
    pub async fn get_mesh(&self, mesh_id: &str) -> Option<Vec<MeshRoute>> {
        let router = self.router.lock().await;
        if router.list_networks().contains(&mesh_id.to_string()) {
            Some(router.get_members(mesh_id).into_iter().cloned().collect())
        } else {
            None
        }
    }

    /// 在 mesh 中查找指定服务，返回 (路由, 服务)。
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
