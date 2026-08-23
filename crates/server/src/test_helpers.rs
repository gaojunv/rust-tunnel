//! 测试辅助设施：统一 `Database` 构造、fixture 种子、mock 上游。
//!
//! 所有函数仅在 `#[cfg(test)]` 下编译。目标是消除 6+ 处重复 fixture
//! （`fresh_db()`、`seed_session()`、`test_agent_state()` 等），
//! 并为批次 3+ 拆 crate 后的测试迁移提供统一入口。

use crate::db::Database;

/// 内存 SQLite 数据库（最常用，零文件系统依赖）。
pub async fn in_memory_db() -> Database {
    Database::new(":memory:")
        .await
        .expect("in-memory db")
}

/// 文件-backed 数据库 + 临时目录（需跨连接持久化的测试用）。
/// 返回 `(Database, TempDir)`——`TempDir` 随测试作用域 drop 自动清理。
pub async fn file_db() -> (Database, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let path = tmp.path().join("test.db");
    let db = Database::new(path.to_str().expect("utf8 path"))
        .await
        .expect("file db");
    (db, tmp)
}

/// 种子：创建 workspace + session，返回 session id。
/// 满足 `agent_sessions` → `agent_workspaces` 的外键约束。
pub async fn seed_workspace_and_session(db: &Database) -> String {
    db.agent_create_workspace(
        "ws-test", "test-ws", "test-client", "host", "/tmp",
        None, None, "", None, None, None, None,
    )
    .await
    .expect("seed workspace");
    db.agent_create_session("sess-test", "ws-test", None, None)
        .await
        .expect("seed session");
    "sess-test".to_string()
}

/// 种子：仅创建 workspace（不建 session），返回 workspace id。
pub async fn seed_workspace(db: &Database) -> String {
    db.agent_create_workspace(
        "ws-test", "test-ws", "test-client", "host", "/tmp",
        None, None, "", None, None, None, None,
    )
    .await
    .expect("seed workspace");
    "ws-test".to_string()
}

/// 本地回环 TCP mock 上游（LLM 网关测试用）。
///
/// 返回 `(base_url, JoinHandle)`——`base_url` 可直接用于 `ProviderConfig.base_url`。
/// `handler` 每个连接调用一次，接收原始请求字节，返回原始响应字节。
pub async fn mock_upstream(
    handler: impl Fn(Vec<u8>) -> Vec<u8> + Send + 'static,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let port = listener.local_addr().expect("local addr").port();
    let handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = Vec::with_capacity(4096);
            let mut tmp = [0u8; 4096];
            loop {
                match tokio::io::AsyncReadExt::read(&mut stream, &mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            // 简单等待完整 header；body 按需读取
                            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                let headers = String::from_utf8_lossy(&buf[..pos]);
                                if let Some(cl) = headers
                                    .lines()
                                    .find(|l| l.to_lowercase().starts_with("content-length:"))
                                    .and_then(|l| l.split(':').nth(1))
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                                {
                                    if buf.len() >= pos + 4 + cl {
                                        break;
                                    }
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            let response = handler(buf);
            let mut stream = stream;
            use tokio::io::AsyncWriteExt;
            let _ = stream.write_all(&response).await;
        }
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

/// 解析 mock 上游收到的原始 HTTP 请求 → (请求行, JSON body)。
/// 从 `llm/upstream.rs` 提炼的公共 helper。
pub fn parse_mock_request(buf: &[u8]) -> (String, serde_json::Value) {
    let text = String::from_utf8_lossy(buf);
    let mut parts = text.splitn(2, "\r\n\r\n");
    let header = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("");
    let request_line = header.lines().next().unwrap_or("").to_string();
    let json = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    (request_line, json)
}
