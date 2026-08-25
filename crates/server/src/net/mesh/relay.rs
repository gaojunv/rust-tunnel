use rust_tunnel_common::ControlMessage;
use rust_tunnel_common::TunnelError;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

/// Mesh 客户端间中继：经服务端转发双向数据。
#[derive(Clone)]
pub struct MeshRelay {
    /// client_name -> 控制消息发送端
    tunnels: Arc<Mutex<HashMap<String, mpsc::Sender<ControlMessage>>>>,
}

impl Default for MeshRelay {
    fn default() -> Self {
        Self::new()
    }
}

impl MeshRelay {
    /// 创建空的中继表。
    pub fn new() -> Self {
        Self {
            tunnels: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 注册客户端控制通道。
    pub async fn register(&self, client_name: &str, tx: mpsc::Sender<ControlMessage>) {
        let mut tunnels = self.tunnels.lock().await;
        tunnels.insert(client_name.to_string(), tx);
    }

    /// 注销客户端。
    pub async fn unregister(&self, client_name: &str) {
        let mut tunnels = self.tunnels.lock().await;
        tunnels.remove(client_name);
    }

    /// 将数据从 source 中继到 target。
    pub async fn relay_data(
        &self,
        source: &str,
        target: &str,
        data: Vec<u8>,
    ) -> Result<(), TunnelError> {
        let tunnels = self.tunnels.lock().await;
        let tx = tunnels
            .get(target)
            .ok_or_else(|| TunnelError::MeshRelay(format!("Target not found: {}", target)))?;

        let msg = ControlMessage::MeshRelay {
            target_client: source.to_string(),
            data,
        };

        tx.send(msg)
            .await
            .map_err(|_| TunnelError::MeshRelay("Failed to send relay message".to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_register_and_unregister() {
        let relay = MeshRelay::new();
        let (tx, _rx) = mpsc::channel::<ControlMessage>(16);

        relay.register("client-a", tx).await;
        {
            let tunnels = relay.tunnels.lock().await;
            assert!(tunnels.contains_key("client-a"));
        }

        relay.unregister("client-a").await;
        {
            let tunnels = relay.tunnels.lock().await;
            assert!(!tunnels.contains_key("client-a"));
        }
    }

    #[tokio::test]
    async fn test_relay_data_target_not_found() {
        let relay = MeshRelay::new();
        let result = relay
            .relay_data("client-a", "client-b", vec![1, 2, 3])
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_relay_data_success() {
        let relay = MeshRelay::new();
        let (tx, mut rx) = mpsc::channel::<ControlMessage>(16);

        relay.register("client-b", tx).await;
        relay
            .relay_data("client-a", "client-b", vec![1, 2, 3])
            .await
            .unwrap();

        let msg = rx.recv().await.unwrap();
        match msg {
            ControlMessage::MeshRelay {
                target_client,
                data,
            } => {
                assert_eq!(target_client, "client-a");
                assert_eq!(data, vec![1, 2, 3]);
            }
            _ => panic!("Unexpected message: {:?}", msg),
        }
    }
}
