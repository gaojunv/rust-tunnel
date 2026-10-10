use tokio::io::{AsyncReadExt, AsyncWrite};
use tokio::net::TcpStream;
use tracing::{debug, warn};

pub use crate::control::ClientState;
use rust_tunnel_common::{ControlMessage, TunnelError, TunnelResult};

/// 本地目标拨号超时：内核默认 SYN 重传可达数分钟，期间 pending 缓冲持续累积；
/// 10s 足够覆盖内网/回环场景，超时即快速失败并释放缓冲。
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 处理服务端的 `OpenTunnel` 请求：拨号本地目标、回传 `TunnelOpenResult`，随后双向转发数据。
///
/// # Errors
///
/// 当向服务端回传 `TunnelOpenResult` 失败或本地拨号失败且回传失败时返回 `Err`。
pub async fn handle_open_tunnel(
    state: ClientState,
    connection_id: u64,
    target_addr: String,
) -> TunnelResult<()> {
    // Connect to local target using async Tokio TcpStream (bounded by CONNECT_TIMEOUT)
    debug!("Connecting to target {}", target_addr);

    let connect_result =
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&target_addr)).await {
            Ok(res) => res,
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("connect to {target_addr} timed out after {CONNECT_TIMEOUT:?}"),
            )),
        };

    match connect_result {
        Ok(local_stream) => {
            debug!("Successfully connected to {}", target_addr);

            // Send success result
            state
                .control_sender
                .send(ControlMessage::TunnelOpenResult {
                    connection_id,
                    success: true,
                    error: None,
                })
                .await
                .map_err(|_| TunnelError::Protocol("failed to send TunnelOpenResult".into()))?;

            // Split stream: reading in this task, writing done by control loop
            let (mut local_reader, local_writer) = tokio::io::split(local_stream);
            // Box the writer for trait object
            let boxed_writer: Box<dyn AsyncWrite + Unpin + Send> = Box::new(local_writer);

            // Activate the pending connection (flushes any buffered data received
            // while we were connecting). If activation fails, the connection was
            // closed while we were connecting.
            if !state.activate_connection(connection_id, boxed_writer).await {
                debug!(
                    "Connection {} was closed while connecting to target",
                    connection_id
                );
                return Ok(());
            }

            let mut buf = vec![0u8; 8192];

            loop {
                match local_reader.read(&mut buf).await {
                    Ok(0) => {
                        debug!("Connection {} closed locally", connection_id);
                        break;
                    }
                    Ok(n) => {
                        // Send data from local to server via control channel
                        if let Err(e) = state
                            .control_sender
                            .send(ControlMessage::Data {
                                connection_id,
                                data: buf[..n].to_vec(),
                            })
                            .await
                        {
                            warn!(
                                "Failed to send data from connection {} to server: {}",
                                connection_id, e
                            );
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("Error reading from connection {}: {}", connection_id, e);
                        break;
                    }
                }
            }

            // Notify server connection is closed
            let _ = state
                .control_sender
                .send(ControlMessage::Close { connection_id })
                .await;

            // Remove from active connections
            state.remove_connection(connection_id).await;

            Ok(())
        }
        Err(e) => {
            warn!("Failed to connect to target {}: {}", target_addr, e);
            // Send failure result
            state
                .control_sender
                .send(ControlMessage::TunnelOpenResult {
                    connection_id,
                    success: false,
                    error: Some(e.to_string()),
                })
                .await
                .ok();
            state.remove_connection(connection_id).await;
            Err(TunnelError::Io(e))
        }
    }
}
