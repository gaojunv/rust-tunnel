//! MCP-over-HTTP 最小端点：只暴露 remember 工具，供 ACP agent（claude-code-acp 等
//! 进程跑在内网客户端）经控制通道回环到服务端调用。hand-rolled JSON-RPC 子集
//! （不引入 MCP SDK 依赖），仅 `rag` feature 编译（`agent/mod.rs` 挂载处门控）。
//!
//! 兼容性锚点（MCP SDK streamableHttp 1.30.0 / claude-code-acp）：
//! - notification（无 `id`，如 `notifications/initialized`）→ 202 空 body；
//! - GET/DELETE → 405（由调用方处理，本 handler 只收 POST body）；
//! - content-type 由调用方写 `application/json`；
//! - `initialize` 固定回 `protocolVersion "2025-03-26"`（全 SDK 版本支持）；
//! - 无状态：不签发也不校验 `Mcp-Session-Id`。
//!
//! token 校验 / 路径路由由 ACP 桥（`acp_bridge`）负责；本模块只做 JSON-RPC 层，
//! 分发规则见 [`handle_request`]。

use super::memory::remember::remember_execute;
use super::memory::{remember_tool_schema, MemoryState, REMEMBER_TOOL_DESCRIPTION};

/// MCP HTTP 响应。调用方负责写 `content-type: application/json`（202 空 body 时
/// 无所谓）。status 未含 `Content-Type`，由外层回环代理补全。
pub struct McpHttpResponse {
    pub status: u16,
    pub data: Vec<u8>,
}

/// 分发一条 MCP JSON-RPC 请求。`body` 为 POST 原文；`memory` 为记忆体运行时，
/// `client_id` / `workspace_id` / `session_id` 是 remember 的落库坐标。
pub async fn handle_request(
    memory: &MemoryState,
    client_id: &str,
    workspace_id: &str,
    session_id: &str,
    body: &[u8],
) -> McpHttpResponse {
    let req: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "mcp request body is not valid JSON");
            return parse_error();
        }
    };
    // JSON-RPC batch（顶层数组）不支持，只处理单条请求。选 200 + JSON-RPC 错误体：
    // 语义是"请求已受理、内容非法"，与 400 等效，SDK 看到 error 即报错。
    if req.is_array() {
        return jsonrpc_error(&serde_json::Value::Null, -32600, "Invalid Request");
    }
    // 无 `id` → notification：受理成功，202 空 body（JSON-RPC 约定不响应）。
    let Some(id) = req.get("id") else {
        return McpHttpResponse {
            status: 202,
            data: Vec::new(),
        };
    };
    let Some(method) = req.get("method").and_then(serde_json::Value::as_str) else {
        return jsonrpc_error(id, -32601, "Method not found");
    };
    match method {
        // protocolVersion 固定 "2025-03-26"：MCP SDK 各版本均接受（高于不支持）。
        "initialize" => ok(
            id,
            &serde_json::json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {
                    "name": "rust-tunnel-memory",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        ),
        "ping" => ok(id, &serde_json::json!({})),
        "tools/list" => ok(
            id,
            &serde_json::json!({
                "tools": [{
                    "name": "remember",
                    "description": REMEMBER_TOOL_DESCRIPTION,
                    "inputSchema": remember_tool_schema(),
                }],
            }),
        ),
        "tools/call" => {
            let Some(params) = req.get("params").and_then(serde_json::Value::as_object) else {
                return jsonrpc_error(id, -32602, "Invalid params");
            };
            let name = params
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            // 参数不完整（缺 arguments）与未知工具同样按 Invalid params 处理。
            let Some(arguments) = params.get("arguments") else {
                return jsonrpc_error(id, -32602, "Invalid params");
            };
            if name != "remember" {
                return jsonrpc_error(id, -32602, "Invalid params");
            }
            // 原样序列化 arguments（`{"content": "..."}`）→ remember_execute 自行解析。
            let args_json = arguments.to_string();
            let result =
                remember_execute(memory, client_id, workspace_id, session_id, &args_json).await;
            match result {
                Ok(text) => ok(
                    id,
                    &serde_json::json!({
                        "content": [{"type": "text", "text": text}],
                    }),
                ),
                // 错误喂回模型（isError=true），工具调用不中断会话。
                Err(e) => ok(
                    id,
                    &serde_json::json!({
                        "content": [{"type": "text", "text": e}],
                        "isError": true,
                    }),
                ),
            }
        }
        _ => jsonrpc_error(id, -32601, "Method not found"),
    }
}

/// 200 + JSON-RPC `result`（id 原样回显，数字/字符串皆可）。
fn ok(id: &serde_json::Value, result: &serde_json::Value) -> McpHttpResponse {
    McpHttpResponse {
        status: 200,
        data: serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
        .unwrap_or_default(),
    }
}

/// 200 + JSON-RPC `error`（id 原样回显；解析失败场景由 [`parse_error`] 单独处理）。
fn jsonrpc_error(id: &serde_json::Value, code: i64, message: &str) -> McpHttpResponse {
    McpHttpResponse {
        status: 200,
        data: serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }))
        .unwrap_or_default(),
    }
}

/// 400 + -32700 parse error（body 无法解析，无 id 可取 → id=null）。
fn parse_error() -> McpHttpResponse {
    McpHttpResponse {
        status: 400,
        data: br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#
            .to_vec(),
    }
}

#[cfg(all(test, feature = "rag"))]
mod tests {
    use super::*;
    use crate::db::Database;

    /// 构造开启且 embedding 可达的 MemoryState（复用 memory 模块的 mock server）。
    async fn memory_with_embedding() -> (Database, MemoryState) {
        let base = crate::memory::mock_embedding_server(8).await;
        crate::memory::test_memory_with_embedding(&base).await
    }

    /// 未配 embedding 的 MemoryState（initialize/ping/tools/list 等不触 remember
    /// 的用例用；`_dir` 保持存活至测试结束，避免 store Drop flush 写已删目录）。
    async fn plain_memory() -> (Database, MemoryState) {
        let db = Database::new(":memory:").await.unwrap();
        let (_dir, store) = crate::memory::test_store();
        let memory = MemoryState::new(
            db.clone(),
            store,
            None,
            crate::llm::LlmState::new(None, None),
        );
        (db, memory)
    }

    #[tokio::test]
    async fn initialize_returns_protocol_and_server_info() {
        let (_db, memory) = plain_memory().await;
        let resp = handle_request(
            &memory,
            "c1",
            "w1",
            "s1",
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
        )
        .await;
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.data).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1, "数字 id 原样回显");
        assert_eq!(v["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(v["result"]["serverInfo"]["name"], "rust-tunnel-memory");
        assert!(!v["result"]["serverInfo"]["version"]
            .as_str()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn notification_returns_202_empty() {
        let (_db, memory) = plain_memory().await;
        let resp = handle_request(
            &memory,
            "c1",
            "w1",
            "s1",
            br#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
        )
        .await;
        assert_eq!(resp.status, 202);
        assert!(resp.data.is_empty(), "notification 不响应 JSON");
    }

    #[tokio::test]
    async fn ping_returns_empty_result() {
        let (_db, memory) = plain_memory().await;
        let resp = handle_request(
            &memory,
            "c1",
            "w1",
            "s1",
            br#"{"jsonrpc":"2.0","id":"abc","method":"ping"}"#,
        )
        .await;
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.data).unwrap();
        assert_eq!(v["id"], "abc", "字符串 id 原样回显");
        assert_eq!(v["result"], serde_json::json!({}));
    }

    #[tokio::test]
    async fn tools_list_exposes_remember() {
        let (_db, memory) = plain_memory().await;
        let resp = handle_request(
            &memory,
            "c1",
            "w1",
            "s1",
            br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        )
        .await;
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.data).unwrap();
        let tools = v["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "remember");
        assert_eq!(
            tools[0]["inputSchema"]["required"],
            serde_json::json!(["content"])
        );
        assert_eq!(tools[0]["description"], REMEMBER_TOOL_DESCRIPTION);
    }

    #[tokio::test]
    async fn tools_call_remember_saves_record() {
        let (db, memory) = memory_with_embedding().await;
        let resp = handle_request(
            &memory,
            "c1",
            "w1",
            "s1",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"remember","arguments":{"content":"用户偏好中文注释","scope":"workspace","tags":["rust"]}}}"#
                .as_bytes(),
        )
        .await;
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.data).unwrap();
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("memory saved (id="), "text: {text}");
        assert!(v["result"].get("isError").is_none(), "成功不置 isError");
        let all = db
            .memory_list(None, None, None, None, None, None, 100, 0)
            .await
            .unwrap();
        assert_eq!(all.len(), 1, "DB 应有一条记忆");
        assert_eq!(all[0].content, "用户偏好中文注释");
        assert_eq!(all[0].source_trigger, "remember");
    }

    #[tokio::test]
    async fn tools_call_bad_args_returns_is_error() {
        let (_db, memory) = memory_with_embedding().await;
        let resp = handle_request(
            &memory,
            "c1",
            "w1",
            "s1",
            br#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"remember","arguments":{"content":"  "}}}"#,
        )
        .await;
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.data).unwrap();
        assert_eq!(v["result"]["isError"], true);
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("non-empty"), "text: {text}");
    }

    #[tokio::test]
    async fn tools_call_unknown_tool_is_invalid_params() {
        let (_db, memory) = plain_memory().await;
        let resp = handle_request(
            &memory,
            "c1",
            "w1",
            "s1",
            br#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}"#,
        )
        .await;
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.data).unwrap();
        assert_eq!(v["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn unknown_method_is_method_not_found() {
        let (_db, memory) = plain_memory().await;
        let resp = handle_request(
            &memory,
            "c1",
            "w1",
            "s1",
            br#"{"jsonrpc":"2.0","id":6,"method":"prompts/list"}"#,
        )
        .await;
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.data).unwrap();
        assert_eq!(v["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn bad_json_is_parse_error() {
        let (_db, memory) = plain_memory().await;
        let resp = handle_request(&memory, "c1", "w1", "s1", b"not json").await;
        assert_eq!(resp.status, 400);
        let v: serde_json::Value = serde_json::from_slice(&resp.data).unwrap();
        assert_eq!(v["error"]["code"], -32700);
        assert_eq!(v["id"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn batch_array_is_invalid_request() {
        let (_db, memory) = plain_memory().await;
        let resp = handle_request(
            &memory,
            "c1",
            "w1",
            "s1",
            br#"[{"jsonrpc":"2.0","id":7,"method":"ping"}]"#,
        )
        .await;
        assert_eq!(resp.status, 200, "batch 走 200 + JSON-RPC 错误体");
        let v: serde_json::Value = serde_json::from_slice(&resp.data).unwrap();
        assert_eq!(v["error"]["code"], -32600);
    }
}
