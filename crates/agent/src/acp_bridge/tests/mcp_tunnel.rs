#![cfg(feature = "rag")]

use super::helpers::*;

#[cfg(feature = "rag")]
#[tokio::test]
async fn test_mcp_tunnel_valid_token_returns_initialize() {
    // 有效 token + 合法 initialize → 200 done=true，body 为 MCP initialize 响应。
    let (mut bridge, mut rx) = mcp_tunnel_env(Some("tok-123"), true).await;
    let base = crate::memory::mock_embedding_server(8).await;
    let (_mdb, memory) = crate::memory::test_memory_with_embedding(&base).await;
    bridge = bridge.with_memory(memory);
    let (_rid, status, done, data) = send_mcp_request(
        &bridge,
        &mut rx,
        "/mcp/tok-123",
        br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#
            .to_vec(),
    )
    .await;
    assert_eq!(status, 200);
    assert!(done, "MCP 响应必须单 chunk done=true");
    let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 1);
    assert_eq!(v["result"]["protocolVersion"], "2025-03-26");
}

#[cfg(feature = "rag")]
#[tokio::test]
async fn test_mcp_tunnel_invalid_token_returns_404() {
    // token 不匹配 → 404 + JSON-RPC error（-32001），done=true。
    let (bridge, mut rx) = mcp_tunnel_env(Some("real-token"), false).await;
    let (_rid, status, done, data) = send_mcp_request(
        &bridge,
        &mut rx,
        "/mcp/wrong-token",
        br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#.to_vec(),
    )
    .await;
    assert_eq!(status, 404);
    assert!(done);
    let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
    assert_eq!(v["error"]["code"], -32001);
    assert_eq!(v["error"]["message"], "invalid mcp token");
}

#[cfg(feature = "rag")]
#[tokio::test]
async fn test_mcp_tunnel_revoked_token_returns_404() {
    // 会话条目被移除（kill/重拉/reaper）→ token 即吊销 → 404。
    let (bridge, mut rx) = mcp_tunnel_env(None, false).await;
    let (_rid, status, done, data) = send_mcp_request(
        &bridge,
        &mut rx,
        "/mcp/stale-token",
        br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#.to_vec(),
    )
    .await;
    assert_eq!(status, 404);
    assert!(done);
    let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
    assert_eq!(v["error"]["code"], -32001);
}

#[cfg(feature = "rag")]
#[tokio::test]
async fn test_mcp_tunnel_empty_body_returns_405() {
    // 空 body（旧客户端 GET SSE 探测等非 POST）→ 405（MCP SDK 视 405 为
    // 静默成功，不阻断 initialize）。
    let (bridge, mut rx) = mcp_tunnel_env(Some("tok"), false).await;
    let (_rid, status, done, data) =
        send_mcp_request(&bridge, &mut rx, "/mcp/tok", Vec::new()).await;
    assert_eq!(status, 405);
    assert!(done);
    assert!(data.is_empty());
}

// ── stdio pump ─────────────────────────────────────────────
