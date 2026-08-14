//! Embedded LLM loopback proxy: receives agent's LLM API requests on 127.0.0.1,
//! forwards them over the control channel to the server's LLM gateway.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use rust_tunnel_common::ControlMessage;

/// 单次 LLM 代理请求 body 上限：控制通道协议 1MB 硬上限（`read_from_stream`
/// 超限即报 `Message too large` 并断开整个控制连接）。900KB 给 bincode 序列化
/// 头留足余量；超限的请求在客户端本地直接回 HTTP 413，绝不把大消息发上
/// 控制通道（否则 agent 的 LLM 调用会把控制连接打掉，连带全部 ACP 会话）。
const MAX_LLM_PROXY_BODY: usize = 900 * 1024;

/// request_id -> 等待响应 chunk 的 HTTP 连接发送端
pub type PendingMap = Arc<Mutex<HashMap<String, mpsc::Sender<ControlMessage>>>>;

pub fn new_pending_map() -> PendingMap {
    Arc::new(Mutex::new(HashMap::new()))
}

/// 把服务端回传的 chunk 路由给等待中的 HTTP 连接。返回 false 表示无此请求。
pub async fn route_chunk(pending: &PendingMap, chunk: &ControlMessage) -> bool {
    let ControlMessage::AgentLlmProxyChunk { request_id, .. } = chunk else {
        return false;
    };
    let tx = pending.lock().await.get(request_id).cloned();
    match tx {
        Some(tx) => {
            let done = matches!(chunk, ControlMessage::AgentLlmProxyChunk { done: true, .. });
            let _ = tx.send(chunk.clone()).await;
            if done {
                pending.lock().await.remove(request_id);
            }
            true
        }
        None => false,
    }
}

/// 启动回环 HTTP 代理，返回绑定端口与 kill 信号发送端。
/// 每个进入的请求经 control_tx 发 AgentLlmProxyRequest，响应从 pending 收集后写回。
/// 调用方持有 kill_tx：发 `()` 即让 accept 循环退出（listener drop 释放端口）。
pub async fn serve(
    session_id: String,
    control_tx: mpsc::Sender<ControlMessage>,
    pending: PendingMap,
) -> std::io::Result<(u16, tokio::sync::oneshot::Sender<()>)> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let (kill_tx, mut kill_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut kill_rx => break, // kill 信号：结束监听，listener drop 释放端口
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else {
                        break;
                    };
                    let sid = session_id.clone();
                    let tx = control_tx.clone();
                    let pend = pending.clone();
                    tokio::spawn(handle_conn(stream, sid, tx, pend));
                }
            }
        }
    });
    Ok((port, kill_tx))
}

async fn handle_conn(
    stream: tokio::net::TcpStream,
    session_id: String,
    control_tx: mpsc::Sender<ControlMessage>,
    pending: PendingMap,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = stream;
    // 最小 HTTP/1.1 解析：读 headers 找 Content-Length，再读 body。
    // agent 的 LLM 请求是标准 JSON POST，够用。
    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];
    let header_end;
    loop {
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = find_header_end(&buf) {
                    header_end = pos;
                    break;
                }
                if buf.len() > 64 * 1024 {
                    return;
                } // 头太大，拒绝
            }
        }
    }
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let path = headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    let content_length: usize = headers
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(0);
    let body_start = header_end + 4; // \r\n\r\n
    while buf.len() < body_start + content_length {
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    let body = buf[body_start..body_start + content_length].to_vec();

    // 协议 1MB 上限：body 超限本地直接回 413，不让大消息进入控制通道
    // （服务端 read_from_stream 超限会断开控制连接，杀掉该客户端全部 ACP 会话）。
    if body.len() > MAX_LLM_PROXY_BODY {
        let msg = format!(
            "llm proxy request body too large: {} bytes (max: {MAX_LLM_PROXY_BODY})",
            body.len()
        );
        let head = format!(
            "HTTP/1.1 413 Payload Too Large\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{msg}",
            msg.len()
        );
        let _ = stream.write_all(head.as_bytes()).await;
        return;
    }

    let request_id = format!("{:032x}", rand::random::<u128>());
    let (resp_tx, mut resp_rx) = mpsc::channel(256);
    pending.lock().await.insert(request_id.clone(), resp_tx);
    if control_tx
        .send(ControlMessage::AgentLlmProxyRequest {
            request_id: request_id.clone(),
            session_id,
            path,
            body,
        })
        .await
        .is_err()
    {
        pending.lock().await.remove(&request_id);
        return;
    }

    // 收集响应：首个 chunk 定 status，之后流式写回（SSE 直通）
    let mut wrote_headers = false;
    while let Some(ControlMessage::AgentLlmProxyChunk {
        data, done, status, ..
    }) = resp_rx.recv().await
    {
        if !wrote_headers {
            let head = format!(
                "HTTP/1.1 {status} OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n"
            );
            if stream.write_all(head.as_bytes()).await.is_err() {
                break;
            }
            wrote_headers = true;
        }
        if !data.is_empty() {
            let chunk = format!("{:x}\r\n", data.len());
            if stream.write_all(chunk.as_bytes()).await.is_err()
                || stream.write_all(&data).await.is_err()
                || stream.write_all(b"\r\n").await.is_err()
            {
                break;
            }
        }
        if done {
            break;
        }
    }
    let _ = stream.write_all(b"0\r\n\r\n").await;
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_route_chunk_to_pending() {
        let pending = new_pending_map();
        let (tx, mut rx) = mpsc::channel(4);
        pending.lock().await.insert("r1".to_string(), tx);

        let chunk = ControlMessage::AgentLlmProxyChunk {
            request_id: "r1".into(),
            data: b"data: {}".to_vec(),
            done: false,
            status: 200,
        };
        assert!(route_chunk(&pending, &chunk).await);
        let got = rx.recv().await.unwrap();
        assert!(matches!(
            got,
            ControlMessage::AgentLlmProxyChunk { done: false, .. }
        ));

        // done chunk 路由后移除 pending
        let done_chunk = ControlMessage::AgentLlmProxyChunk {
            request_id: "r1".into(),
            data: vec![],
            done: true,
            status: 200,
        };
        assert!(route_chunk(&pending, &done_chunk).await);
        assert!(pending.lock().await.is_empty());
        // 已移除，再路由返回 false
        assert!(!route_chunk(&pending, &done_chunk).await);
    }

    #[tokio::test]
    async fn test_proxy_end_to_end() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let pending = new_pending_map();
        let (tx, mut rx) = mpsc::channel(32);
        let (port, kill_tx) = serve("sess-1".into(), tx, pending.clone())
            .await
            .unwrap();
        assert!(port > 0);

        // 模拟服务端：收到 request 后回两个 chunk + done
        let server = tokio::spawn(async move {
            let Some(ControlMessage::AgentLlmProxyRequest {
                request_id,
                path,
                body,
                ..
            }) = rx.recv().await
            else {
                panic!("expected proxy request");
            };
            assert_eq!(path, "/v1/chat/completions");
            assert_eq!(body, b"{\"model\":\"m\"}".to_vec());
            let mk = |data: Vec<u8>, done| ControlMessage::AgentLlmProxyChunk {
                request_id: request_id.clone(),
                data,
                done,
                status: 200,
            };
            route_chunk(&pending, &mk(b"data: one\n\n".to_vec(), false)).await;
            route_chunk(&pending, &mk(vec![], true)).await;
        });

        let mut conn = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        conn.write_all(
            b"POST /v1/chat/completions HTTP/1.1\r\nContent-Length: 13\r\n\r\n{\"model\":\"m\"}",
        )
        .await
        .unwrap();
        let mut resp = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            conn.read_to_end(&mut resp),
        )
        .await
        .unwrap()
        .unwrap();
        let text = String::from_utf8(resp).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK"));
        assert!(text.contains("data: one"));
        server.await.unwrap();

        // kill 后监听任务退出：回环端口应释放（connect 失败），防泄漏验证。
        kill_tx.send(()).unwrap();
        let mut port_closed = false;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err()
            {
                port_closed = true;
                break;
            }
        }
        assert!(port_closed, "listener should be released after kill");
    }

    #[test]
    fn test_find_header_end() {
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n\r\n"), Some(14));
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n"), None);
    }

    /// 回归：请求体超协议上限时必须本地回 413，且不向控制通道发
    /// AgentLlmProxyRequest（否则服务端 read_from_stream 超限断开整个控制连接，
    /// 连带全部 ACP 会话）。
    #[tokio::test]
    async fn test_proxy_rejects_oversized_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let pending = new_pending_map();
        let (tx, mut rx) = mpsc::channel(32);
        let (port, kill_tx) = serve("sess-1".into(), tx, pending.clone())
            .await
            .unwrap();

        let big = vec![b'x'; MAX_LLM_PROXY_BODY + 1];
        let mut conn = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let head = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            big.len()
        );
        conn.write_all(head.as_bytes()).await.unwrap();
        conn.write_all(&big).await.unwrap();

        let mut resp = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            conn.read_to_end(&mut resp),
        )
        .await
        .unwrap()
        .unwrap();
        let text = String::from_utf8(resp).unwrap();
        assert!(
            text.starts_with("HTTP/1.1 413"),
            "expected 413, got: {text}"
        );
        assert!(text.contains("too large"), "body should explain: {text}");

        // 控制通道不得收到该请求（body 被拦截在本地）
        let leaked = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .is_ok();
        assert!(!leaked, "oversized request must not reach the control channel");

        kill_tx.send(()).unwrap();
    }
}
