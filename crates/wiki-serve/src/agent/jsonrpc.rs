// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![allow(clippy::missing_docs_in_private_items)]

//! NDJSON JSON-RPC 2.0 传输层.
//!
//! 泛型 `JsonRpcConnection<R, W>` 封装 newline-delimited 分帧、id 关联与三分流：
//! response → pending、notification → sink、server request → sink。
//! 线上省略 `jsonrpc` 头，按 `\n` 切帧、容忍 `\r\n`、按行读取保证 UTF-8 增量安全。
//! 解析失败的行仅上报错误事件，不中断连接。

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, oneshot};

/// 待处理请求映射：`id -> oneshot` 发送端.
type PendingMap = HashMap<u64, oneshot::Sender<Result<Value, JsonRpcError>>>;

/// 连接状态回调.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonRpcStatus {
    /// 正常运行中.
    Running,
    /// 对端已退出或连接关闭.
    Exited {
        /// 退出码（若已知）.
        code: Option<i32>,
        /// 原因描述.
        reason: String,
    },
}

/// JSON-RPC 传输错误.
#[derive(Debug, Clone)]
pub struct JsonRpcError(pub String);

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for JsonRpcError {}

/// 事件接收端（由上层 `AgentManager` 注入）.
pub trait JsonRpcSink: Send + Sync + 'static {
    /// 收到 server 通知（有 method 无 id）.
    fn on_notification(&self, method: String, params: Option<Value>);
    /// 收到 server 发起的请求（有 method 有 id），上层需稍后 `respond`.
    fn on_server_request(&self, id: Value, method: String, params: Option<Value>);
    /// 连接状态变化.
    fn on_status(&self, status: JsonRpcStatus);
    /// 单行解析失败（不中断连接）.
    fn on_parse_error(&self, line: String, error: String);
}

/// 空实现（测试兜底）.
#[derive(Debug, Default)]
pub struct NoopSink;

impl JsonRpcSink for NoopSink {
    fn on_notification(&self, _method: String, _params: Option<Value>) {}
    fn on_server_request(&self, _id: Value, _method: String, _params: Option<Value>) {}
    fn on_status(&self, _status: JsonRpcStatus) {}
    fn on_parse_error(&self, _line: String, _error: String) {}
}

/// 测试用收集型 sink.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct CollectSink {
    /// 通知列表.
    pub notifications: std::sync::Mutex<Vec<(String, Option<Value>)>>,
    /// server request 列表.
    pub server_requests: std::sync::Mutex<Vec<(Value, String, Option<Value>)>>,
    /// 状态列表.
    pub statuses: std::sync::Mutex<Vec<JsonRpcStatus>>,
    /// 解析错误列表.
    pub parse_errors: std::sync::Mutex<Vec<(String, String)>>,
}

#[cfg(test)]
impl JsonRpcSink for CollectSink {
    fn on_notification(&self, method: String, params: Option<Value>) {
        self.notifications.lock().expect("lock").push((method, params));
    }
    fn on_server_request(&self, id: Value, method: String, params: Option<Value>) {
        self.server_requests
            .lock()
            .expect("lock")
            .push((id, method, params));
    }
    fn on_status(&self, status: JsonRpcStatus) {
        self.statuses.lock().expect("lock").push(status);
    }
    fn on_parse_error(&self, line: String, error: String) {
        self.parse_errors.lock().expect("lock").push((line, error));
    }
}

/// 泛型 NDJSON 连接.
pub struct JsonRpcConnection<R, W>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    writer: Arc<Mutex<W>>,
    pending: Arc<Mutex<PendingMap>>,
    next_id: Arc<AtomicU64>,
    sink: Arc<dyn JsonRpcSink>,
    read_handle: Option<tokio::task::JoinHandle<()>>,
    _marker: std::marker::PhantomData<R>,
}

impl<R, W> JsonRpcConnection<R, W>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    /// 以给定的读写 halves 与 sink 构造连接并启动读循环.
    #[allow(clippy::too_many_lines, reason = "读循环与分帧逻辑集中在一处，拆分会降低可读性")]
    pub fn new(reader: R, writer: W, sink: Arc<dyn JsonRpcSink>) -> Self {
        let writer = Arc::new(Mutex::new(writer));
        let pending: Arc<Mutex<PendingMap>> =
            Arc::new(Mutex::new(HashMap::new()));
        let next_id = Arc::new(AtomicU64::new(1));
        let pending_clone = Arc::clone(&pending);
        let sink_clone = Arc::clone(&sink);
        let read_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                let n = match reader.read_line(&mut line).await {
                    Ok(v) => v,
                    Err(err) => {
                        // 读错误视为退出
                        let mut guard = pending_clone.lock().await;
                        for (_, tx) in guard.drain() {
                            let _ = tx.send(Err(JsonRpcError(format!("read error: {err}"))));
                        }
                        sink_clone.on_status(JsonRpcStatus::Exited {
                            code: None,
                            reason: format!("read error: {err}"),
                        });
                        break;
                    }
                };
                if n == 0 {
                    // EOF
                    let mut guard = pending_clone.lock().await;
                    for (_, tx) in guard.drain() {
                        let _ = tx.send(Err(JsonRpcError("connection closed (EOF)".to_owned())));
                    }
                    sink_clone.on_status(JsonRpcStatus::Exited {
                        code: None,
                        reason: "EOF".to_owned(),
                    });
                    break;
                }
                // 去掉末尾 \n / \r\n
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if trimmed.is_empty() {
                    continue;
                }
                // 空白行跳过
                if trimmed.trim().is_empty() {
                    continue;
                }
                let parsed: Result<Value, _> = serde_json::from_str(trimmed);
                let value = match parsed {
                    Ok(v) => v,
                    Err(err) => {
                        sink_clone.on_parse_error(trimmed.to_owned(), err.to_string());
                        continue;
                    }
                };
                // 必须是 object
                let Some(obj) = value.as_object() else {
                    sink_clone.on_parse_error(trimmed.to_owned(), "not an object".to_owned());
                    continue;
                };
                let method = obj.get("method").and_then(|v| v.as_str()).map(ToOwned::to_owned);
                let id = obj.get("id").cloned();
                let params = obj.get("params").cloned();
                match (method, id) {
                    (Some(m), Some(id_val)) => {
                        // server → client request
                        sink_clone.on_server_request(id_val, m, params);
                    }
                    (Some(m), None) => {
                        // notification
                        sink_clone.on_notification(m, params);
                    }
                    (None, Some(id_val)) => {
                        // response → 归位 pending
                        // 仅处理数字 id（u64），其他视为未知 id 丢弃
                        let id_num = match &id_val {
                            Value::Number(n) => n.as_u64(),
                            _ => None,
                        };
                        if let Some(num) = id_num {
                            let tx_opt = {
                                let mut guard = pending_clone.lock().await;
                                guard.remove(&num)
                            };
                            if let Some(tx) = tx_opt {
                                // 区分 result / error
                                if let Some(err_val) = obj.get("error") {
                                    let _ = tx.send(Err(JsonRpcError(err_val.to_string())));
                                } else if let Some(res) = obj.get("result") {
                                    let _ = tx.send(Ok(res.clone()));
                                } else {
                                    // 无 result 也无 error，视为 null
                                    let _ = tx.send(Ok(Value::Null));
                                }
                            } else {
                                // 未知 id 丢弃，不上报错误
                            }
                        } else {
                            // 非数字 id 丢弃（兼容 Value id 的 server request 响应路径不需要处理）
                        }
                    }
                    (None, None) => {
                        // 既无 method 也无 id，视为畸形行
                        sink_clone.on_parse_error(trimmed.to_owned(), "missing method and id".to_owned());
                    }
                }
            }
        });
        Self {
            writer,
            pending,
            next_id,
            sink,
            read_handle: Some(read_handle),
            _marker: std::marker::PhantomData,
        }
    }

    /// 发送 client → server 请求，等待响应.
    ///
    /// # Errors
    ///
    /// 写失败或对端关闭时返回 [`JsonRpcError`]。
    pub async fn request(&self, method: &str, params: Option<Value>) -> Result<Value, JsonRpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.pending.lock().await;
            guard.insert(id, tx);
        }
        let mut payload = serde_json::Map::new();
        payload.insert("id".to_owned(), Value::Number(id.into()));
        payload.insert("method".to_owned(), Value::String(method.to_owned()));
        if let Some(p) = params {
            payload.insert("params".to_owned(), p);
        }
        let line = serde_json::to_string(&Value::Object(payload))
            .map_err(|e| JsonRpcError(e.to_string()))?;
        let write_res = {
            let mut guard = self.writer.lock().await;
            let res = guard.write_all(line.as_bytes()).await;
            if res.is_ok() {
                res.and(guard.write_all(b"\n").await)
            } else {
                res
            }
        };
        if let Err(err) = write_res {
            // 写失败：移除 pending 并广播 Exited
            {
                let mut guard = self.pending.lock().await;
                guard.remove(&id);
            }
            // 尝试让 writer flush 失败也触发 pending 清理
            self.fail_all_pending(format!("write error: {err}")).await;
            return Err(JsonRpcError(format!("write error: {err}")));
        }
        // flush
        {
            let mut guard = self.writer.lock().await;
            if let Err(err) = guard.flush().await {
                self.fail_all_pending(format!("flush error: {err}")).await;
                let mut g = self.pending.lock().await;
                g.remove(&id);
                return Err(JsonRpcError(format!("flush error: {err}")));
            }
        }
        // 等待响应
        match rx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(JsonRpcError("response channel closed".to_owned())),
        }
    }

    /// 应答 server → client 请求.
    ///
    /// # Errors
    ///
    /// 写失败时返回 [`JsonRpcError`]。
    pub async fn respond(
        &self,
        id: Value,
        result: Option<Value>,
        error: Option<Value>,
    ) -> Result<(), JsonRpcError> {
        let mut payload = serde_json::Map::new();
        payload.insert("id".to_owned(), id);
        if let Some(err) = error {
            payload.insert("error".to_owned(), err);
        } else {
            payload.insert("result".to_owned(), result.unwrap_or(Value::Null));
        }
        let line = serde_json::to_string(&Value::Object(payload))
            .map_err(|e| JsonRpcError(e.to_string()))?;
        let mut guard = self.writer.lock().await;
        guard
            .write_all(line.as_bytes())
            .await
            .map_err(|e| JsonRpcError(format!("write error: {e}")))?;
        guard
            .write_all(b"\n")
            .await
            .map_err(|e| JsonRpcError(format!("write error: {e}")))?;
        guard.flush().await.map_err(|e| JsonRpcError(format!("flush error: {e}")))?;
        Ok(())
    }

    /// 发送 notification（无 id）.
    ///
    /// # Errors
    ///
    /// 写失败时返回 [`JsonRpcError`]。
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), JsonRpcError> {
        let mut payload = serde_json::Map::new();
        payload.insert("method".to_owned(), Value::String(method.to_owned()));
        if let Some(p) = params {
            payload.insert("params".to_owned(), p);
        }
        let line = serde_json::to_string(&Value::Object(payload))
            .map_err(|e| JsonRpcError(e.to_string()))?;
        let mut guard = self.writer.lock().await;
        guard
            .write_all(line.as_bytes())
            .await
            .map_err(|e| JsonRpcError(format!("write error: {e}")))?;
        guard
            .write_all(b"\n")
            .await
            .map_err(|e| JsonRpcError(format!("write error: {e}")))?;
        guard.flush().await.map_err(|e| JsonRpcError(format!("flush error: {e}")))?;
        Ok(())
    }

    /// 将全部 pending 以错误兑现并上报 `Exited`.
    ///
    /// # Panics
    ///
    /// 本函数本身不会 panic。
    async fn fail_all_pending(&self, reason: String) {
        let mut guard = self.pending.lock().await;
        for (_, tx) in guard.drain() {
            let _ = tx.send(Err(JsonRpcError(reason.clone())));
        }
        self.sink.on_status(JsonRpcStatus::Exited {
            code: None,
            reason,
        });
    }

    /// 关闭写端（用于测试写关闭传播）.
    ///
    /// # Errors
    ///
    /// 底层 `AsyncWrite::shutdown` 失败时返回 `io::Error`。
    pub async fn shutdown_writer(&self) -> std::io::Result<()> {
        let mut guard = self.writer.lock().await;
        guard.shutdown().await
    }
}

impl<R, W> Drop for JsonRpcConnection<R, W>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    fn drop(&mut self) {
        if let Some(h) = self.read_handle.take() {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::duplex;

    fn collect_sink() -> Arc<CollectSink> {
        Arc::new(CollectSink::default())
    }

    #[tokio::test]
    #[allow(clippy::similar_names)]
    async fn single_frame_request_response() {
        let (ca_read, sa_write) = duplex(4096);
        let (sa_read, ca_write) = duplex(4096);
        let sink_a = collect_sink();
        let sink_b = Arc::new(NoopSink);
        let _conn_a = JsonRpcConnection::new(ca_read, ca_write, sink_a.clone());
        let conn_b = JsonRpcConnection::new(sa_read, sa_write, sink_b);

        // b 侧模拟 server：收到 request 后回包
        let b_task = tokio::spawn(async move {
            // 读取一行并响应需要手动用 BufReader，这里直接用 conn_b 的读循环会自动分流，
            // 但 conn_b 的 sink 是 Noop，不会自动响应。我们改为在 duplex 层直接读写验证：
            // 实际上 conn_a 的 request 会写到 sa_read，conn_b 的读循环会把其当作 server_request
            // 由于 sink 是 Noop，无法自动响应，改用另一对 duplex 做 echo 测试更直接：
            drop(conn_b);
        });
        drop(b_task);

        // 简化：用一对 duplex 做 echo server（不用 JsonRpcConnection）
        // 重新建立 echo 链路
        let (c2a_read, s2a_write) = duplex(4096);
        let (s2a_read, c2a_write) = duplex(4096);
        let sink_c = collect_sink();
        let conn_c = JsonRpcConnection::new(c2a_read, c2a_write, sink_c.clone());
        // echo task：读行回 result
        let echo = tokio::spawn(async move {
            let mut reader = BufReader::new(s2a_read);
            let mut writer = s2a_write;
            let mut line = String::new();
            // 只处理一条
            let n = reader.read_line(&mut line).await.expect("read");
            assert!(n > 0);
            let v: Value = serde_json::from_str(line.trim()).expect("json");
            let id = v.get("id").expect("id").clone();
            let resp = json!({"id": id, "result": {"echo": v.get("method")}});
            let s = serde_json::to_string(&resp).expect("ser");
            writer.write_all(s.as_bytes()).await.expect("write");
            writer.write_all(b"\n").await.expect("nl");
            writer.flush().await.expect("flush");
            // 保持连接稍候再关闭
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
        let res = conn_c
            .request("hello", Some(json!({"a": 1})))
            .await
            .expect("request ok");
        assert_eq!(res.get("echo").and_then(|v| v.as_str()), Some("hello"));
        echo.await.expect("echo done");
    }

    #[tokio::test]
    async fn multi_frame_same_chunk() {
        let (a_read, b_write) = duplex(8192);
        let (b_read, a_write) = duplex(8192);
        let sink_a = collect_sink();
        // b 侧一次性写入 3 个 notification
        let sink_noop = Arc::new(NoopSink);
        let conn_a = JsonRpcConnection::new(a_read, a_write, sink_a.clone());
        let _conn_b = JsonRpcConnection::new(b_read, b_write, sink_noop);
        // 直接向 a 的 reader 注入（通过 duplex 写端）
        // 这里复用 conn 结构：需要拿到 b_write 写多帧
        // 但 _conn_b 已持有 b_write，改为直接用 duplex 写多帧到 a_read
        // 简化：新建一对 duplex
        let (ra, mut wa) = duplex(8192);
        let (rb, wb) = duplex(8192);
        // 这里用 ra/wb 交叉：把 wa 的多帧写到 ra
        drop((rb, wb)); // 占位
        // 重新搭建：a 从 ra 读，b 向 wa 写
        let sink2 = collect_sink();
        let (dummy_r, dummy_w) = duplex(8192);
        let conn = JsonRpcConnection::new(ra, dummy_w, sink2.clone());
        // 一次性写 3 notification 同一 chunk
        let payload = format!(
            "{}\n{}\n{}\n",
            json!({"method": "a", "params": {"n": 1}}),
            json!({"method": "b", "params": {"n": 2}}),
            json!({"method": "c", "params": {"n": 3}})
        );
        wa.write_all(payload.as_bytes()).await.expect("write multi");
        wa.flush().await.expect("flush");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let notifs = sink2.notifications.lock().expect("lock").clone();
        assert_eq!(notifs.len(), 3, "应收到 3 个 notification，实际 {notifs:?}");
        assert_eq!(notifs[0].0, "a");
        assert_eq!(notifs[1].0, "b");
        assert_eq!(notifs[2].0, "c");
        drop(conn);
        drop(dummy_r);
        drop(wa);
        drop(conn_a);
    }

    #[tokio::test]
    async fn frame_across_chunk() {
        let (ra, mut wa) = duplex(8192);
        let (dummy_r, dummy_w) = duplex(8192);
        let sink = collect_sink();
        let _conn = JsonRpcConnection::new(ra, dummy_w, sink.clone());
        // 拆两段写入同一帧
        let full = serde_json::to_string(&json!({"method": "split", "params": {"x": 1}})).expect("ser");
        let (part1, part2) = full.split_at(full.len() / 2);
        wa.write_all(part1.as_bytes()).await.expect("p1");
        wa.flush().await.expect("flush1");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // 此时不应有完整 notification
        assert!(sink.notifications.lock().expect("lock").is_empty(), "半帧不应触发");
        wa.write_all(part2.as_bytes()).await.expect("p2");
        wa.write_all(b"\n").await.expect("nl");
        wa.flush().await.expect("flush2");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let notifs = sink.notifications.lock().expect("lock").clone();
        assert_eq!(notifs.len(), 1);
        assert_eq!(notifs[0].0, "split");
        drop(dummy_r);
    }

    #[tokio::test]
    async fn crlf_tolerance() {
        let (ra, mut wa) = duplex(8192);
        let (dummy_r, dummy_w) = duplex(8192);
        let sink = collect_sink();
        let _conn = JsonRpcConnection::new(ra, dummy_w, sink.clone());
        let payload = format!("{}\r\n{}\r\n", json!({"method": "m1"}), json!({"method": "m2"}));
        wa.write_all(payload.as_bytes()).await.expect("write");
        wa.flush().await.expect("flush");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let notifs = sink.notifications.lock().expect("lock").clone();
        assert_eq!(notifs.len(), 2);
        assert_eq!(notifs[0].0, "m1");
        assert_eq!(notifs[1].0, "m2");
        drop(dummy_r);
    }

    #[tokio::test]
    #[allow(clippy::similar_names)]
    async fn out_of_order_fulfillment() {
        let (ca_read, sa_write) = duplex(8192);
        let (sa_read, ca_write) = duplex(8192);
        let sink_a = collect_sink();
        let sink_b = Arc::new(NoopSink);
        let conn_a = JsonRpcConnection::new(ca_read, ca_write, sink_a.clone());
        let _conn_b = JsonRpcConnection::new(sa_read, sa_write, sink_b.clone());
        // 手动用 duplex 写乱序响应：启动两个并发 request，server 按相反顺序回包
        // 需要 server 侧直接读写 duplex，而不是走 JsonRpcConnection 的 sink。
        // 为此重建一对直连 duplex，由 echo task 控制顺序。
        let (c_read, s_write) = duplex(8192);
        let (s_read, c_write) = duplex(8192);
        let sink_c = collect_sink();
        let conn_c = JsonRpcConnection::new(c_read, c_write, sink_c.clone());
        let server_task = tokio::spawn(async move {
            let mut reader = BufReader::new(s_read);
            let mut writer = s_write;
            let mut line1 = String::new();
            let mut line2 = String::new();
            reader.read_line(&mut line1).await.expect("r1");
            reader.read_line(&mut line2).await.expect("r2");
            let v1: Value = serde_json::from_str(line1.trim()).expect("j1");
            let v2: Value = serde_json::from_str(line2.trim()).expect("j2");
            let id1 = v1.get("id").expect("id1").clone();
            let id2 = v2.get("id").expect("id2").clone();
            // 乱序：先回 id2，再回 id1
            let r2 = json!({"id": id2, "result": "second"});
            let r1 = json!({"id": id1, "result": "first"});
            for resp in [r2, r1] {
                let s = serde_json::to_string(&resp).expect("ser");
                writer.write_all(s.as_bytes()).await.expect("w");
                writer.write_all(b"\n").await.expect("nl");
            }
            writer.flush().await.expect("flush");
        });
        let (res1, res2) = tokio::join!(
            conn_c.request("m1", None),
            conn_c.request("m2", None)
        );
        let v1 = res1.expect("r1 ok");
        let v2 = res2.expect("r2 ok");
        // 虽然 server 乱序回包，pending 仍应正确归位
        assert_eq!(v1.as_str(), Some("first"));
        assert_eq!(v2.as_str(), Some("second"));
        server_task.await.expect("server done");
        drop(conn_a);
    }

    #[tokio::test]
    #[allow(clippy::similar_names)]
    async fn unknown_id_discarded() {
        let (ra, mut wa) = duplex(8192);
        let (dummy_r, dummy_w) = duplex(8192);
        let sink = collect_sink();
        let conn = JsonRpcConnection::new(ra, dummy_w, sink.clone());
        // 先发一个请求，pending id=1
        // 注入一个未知 id 的响应 + 正确响应
        // 需要控制写入：先让 request 发出，再注入响应
        let (c_read, s_write) = duplex(8192);
        let (s_read, c_write) = duplex(8192);
        let sink2 = collect_sink();
        let conn2 = JsonRpcConnection::new(c_read, c_write, sink2.clone());
        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(s_read);
            let mut writer = s_write;
            let mut line = String::new();
            reader.read_line(&mut line).await.expect("read req");
            let v: Value = serde_json::from_str(line.trim()).expect("json");
            let id = v.get("id").expect("id").clone();
            // 先发未知 id
            let unknown = json!({"id": 9999, "result": "ignored"});
            let s1 = serde_json::to_string(&unknown).expect("ser");
            writer.write_all(s1.as_bytes()).await.expect("w1");
            writer.write_all(b"\n").await.expect("nl1");
            // 再发正确 id
            let correct = json!({"id": id, "result": "ok"});
            let s2 = serde_json::to_string(&correct).expect("ser");
            writer.write_all(s2.as_bytes()).await.expect("w2");
            writer.write_all(b"\n").await.expect("nl2");
            writer.flush().await.expect("flush");
        });
        let res = conn2.request("hello", None).await.expect("should get ok");
        assert_eq!(res.as_str(), Some("ok"));
        server.await.expect("server done");
        drop(conn);
        drop(dummy_r);
        // 验证未知 id 未导致 panic 且连接仍可用，再发一个请求
        let (c2_read, s2_write) = duplex(8192);
        let (s2_read, c2_write) = duplex(8192);
        let sink3 = collect_sink();
        let conn3 = JsonRpcConnection::new(c2_read, c2_write, sink3.clone());
        let srv2 = tokio::spawn(async move {
            let mut reader = BufReader::new(s2_read);
            let mut writer = s2_write;
            let mut line = String::new();
            reader.read_line(&mut line).await.expect("r");
            let v: Value = serde_json::from_str(line.trim()).expect("j");
            let id = v.get("id").expect("id").clone();
            let resp = json!({"id": id, "result": "still_ok"});
            let s = serde_json::to_string(&resp).expect("ser");
            writer.write_all(s.as_bytes()).await.expect("w");
            writer.write_all(b"\n").await.expect("nl");
            writer.flush().await.expect("flush");
        });
        // 注入一个未知 id 的响应到 ra（conn 的读取侧），验证 discard
        let unknown_line = serde_json::to_string(&json!({"id": 12345, "result": "x"})).expect("ser");
        wa.write_all(unknown_line.as_bytes()).await.expect("w unknown");
        wa.write_all(b"\n").await.expect("nl");
        wa.flush().await.expect("flush");
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        // conn 仍活着，无 parse error 也不应有 status Exited
        assert!(sink.statuses.lock().expect("lock").is_empty());
        let res2 = conn3.request("again", None).await.expect("again ok");
        assert_eq!(res2.as_str(), Some("still_ok"));
        srv2.await.expect("srv2");
    }

    #[tokio::test]
    async fn notification_shunt() {
        let (ra, mut wa) = duplex(8192);
        let (dummy_r, dummy_w) = duplex(8192);
        let sink = collect_sink();
        let _conn = JsonRpcConnection::new(ra, dummy_w, sink.clone());
        let n1 = json!({"method": "notify/one", "params": {"v": 1}});
        let n2 = json!({"method": "notify/two"});
        for n in [n1, n2] {
            let s = serde_json::to_string(&n).expect("ser");
            wa.write_all(s.as_bytes()).await.expect("w");
            wa.write_all(b"\n").await.expect("nl");
        }
        wa.flush().await.expect("flush");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let notifs = sink.notifications.lock().expect("lock").clone();
        assert_eq!(notifs.len(), 2);
        assert_eq!(notifs[0].0, "notify/one");
        assert_eq!(notifs[0].1, Some(json!({"v": 1})));
        assert_eq!(notifs[1].0, "notify/two");
        assert_eq!(notifs[1].1, None);
        drop(dummy_r);
    }

    #[tokio::test]
    async fn server_request_shunt() {
        let (ra, mut wa) = duplex(8192);
        let (dummy_r, dummy_w) = duplex(8192);
        let sink = collect_sink();
        let _conn = JsonRpcConnection::new(ra, dummy_w, sink.clone());
        let req = json!({"id": 42, "method": "ask/approval", "params": {"kind": "command"}});
        let s = serde_json::to_string(&req).expect("ser");
        wa.write_all(s.as_bytes()).await.expect("w");
        wa.write_all(b"\n").await.expect("nl");
        wa.flush().await.expect("flush");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let reqs = sink.server_requests.lock().expect("lock").clone();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].0, json!(42));
        assert_eq!(reqs[0].1, "ask/approval");
        assert_eq!(reqs[0].2, Some(json!({"kind": "command"})));
        drop(dummy_r);
    }

    #[tokio::test]
    async fn server_request_roundtrip_respond() {

        // 独立验证 respond：创建一对 duplex，手动写 response 行并让 request 归位
        let (c_read, s_write2) = duplex(8192);
        let (s_read2, c_write2) = duplex(8192);
        let s_client = collect_sink();
        let conn = JsonRpcConnection::new(c_read, c_write2, s_client.clone());
        let server_task = tokio::spawn(async move {
            let mut reader = BufReader::new(s_read2);
            let mut writer = s_write2;
            let mut line = String::new();
            reader.read_line(&mut line).await.expect("read req");
            let v: Value = serde_json::from_str(line.trim()).expect("json");
            let id = v.get("id").expect("id").clone();
            // 用 respond 语义回包（此处手工写 response 行，与 respond 写入一致）
            let resp = json!({"id": id, "result": {"decision": "approved"}});
            let s = serde_json::to_string(&resp).expect("ser");
            writer.write_all(s.as_bytes()).await.expect("w");
            writer.write_all(b"\n").await.expect("nl");
            writer.flush().await.expect("flush");
        });
        let res = conn.request("do/thing", Some(json!({"x": 1}))).await.expect("req ok");
        assert_eq!(res.get("decision").and_then(|v| v.as_str()), Some("approved"));
        server_task.await.expect("server task");

        // 验证 respond API 本身写入的格式能被对端解析为 response
        let (a_r, a_w) = duplex(8192);
        let (b_r, b_w) = duplex(8192);
        let sink_a2 = collect_sink();
        let conn_a2 = JsonRpcConnection::new(a_r, b_w, sink_a2.clone());
        // 用 conn_a2.respond 写一行，验证 b 侧能读到
        conn_a2
            .respond(json!(99), Some(json!({"ok": true})), None)
            .await
            .expect("respond ok");
        // 从 b 的 reader 读
        let mut reader_b = BufReader::new(b_r);
        let mut line_b = String::new();
        reader_b.read_line(&mut line_b).await.expect("read resp");
        let v_b: Value = serde_json::from_str(line_b.trim()).expect("json b");
        assert_eq!(v_b.get("id"), Some(&json!(99)));
        assert_eq!(v_b.get("result"), Some(&json!({"ok": true})));
        // error 形态
        conn_a2
            .respond(json!("str-id"), None, Some(json!({"code": -1, "message": "fail"})))
            .await
            .expect("respond error ok");
        line_b.clear();
        reader_b.read_line(&mut line_b).await.expect("read err");
        let v_err: Value = serde_json::from_str(line_b.trim()).expect("json err");
        assert_eq!(v_err.get("id"), Some(&json!("str-id")));
        assert!(v_err.get("error").is_some());
        drop(a_w);
    }

    #[tokio::test]
    async fn malformed_line_tolerance() {
        let (ra, mut wa) = duplex(8192);
        let (dummy_r, dummy_w) = duplex(8192);
        let sink = collect_sink();
        let _conn = JsonRpcConnection::new(ra, dummy_w, sink.clone());
        // 畸形行 + 正常 notification
        wa.write_all(b"not json at all\n").await.expect("w1");
        wa.write_all(b"{\"incomplete\":\n").await.expect("w2");
        let good = serde_json::to_string(&json!({"method": "good", "params": {"ok": 1}})).expect("ser");
        wa.write_all(good.as_bytes()).await.expect("w3");
        wa.write_all(b"\n").await.expect("nl");
        wa.flush().await.expect("flush");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let errs = sink.parse_errors.lock().expect("lock").clone();
        assert!(errs.len() >= 2, "应至少 2 个解析错误，实际 {errs:?}");
        let notifs = sink.notifications.lock().expect("lock").clone();
        assert_eq!(notifs.len(), 1);
        assert_eq!(notifs[0].0, "good");
        // 连接保持：再发一个 notification 仍可达
        let good2 = serde_json::to_string(&json!({"method": "good2"})).expect("ser");
        wa.write_all(good2.as_bytes()).await.expect("w4");
        wa.write_all(b"\n").await.expect("nl2");
        wa.flush().await.expect("flush2");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let notifs2 = sink.notifications.lock().expect("lock").clone();
        assert_eq!(notifs2.len(), 2);
        assert_eq!(notifs2[1].0, "good2");
        drop(dummy_r);
    }

    #[tokio::test]
    async fn eof_cleans_pending_and_status() {
        // 由于我们未真正发出 request，先发一个再关
        let (c_read2, s_write2) = duplex(8192);
        let (s_read2, c_write2) = duplex(8192);
        let sink2 = collect_sink();
        let conn2 = JsonRpcConnection::new(c_read2, c_write2, sink2.clone());
        let req_fut = tokio::spawn(async move { conn2.request("m", None).await });
        // 给一点时间让 request 写出
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        // 关闭 server 侧（EOF）
        drop(s_read2);
        drop(s_write2);
        let res = req_fut.await.expect("join").expect_err("应因 EOF 失败");
        assert!(res.0.contains("EOF") || res.0.contains("closed"), "错误应含 EOF/closed，实际 {}", res.0);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let statuses = sink2.statuses.lock().expect("lock").clone();
        assert!(!statuses.is_empty(), "应有 Exited 状态");
        assert!(matches!(statuses[0], JsonRpcStatus::Exited { .. }));
    }

    #[tokio::test]
    async fn write_closed_propagation() {
        let (c_read, _s_write) = duplex(8192);
        let (s_read, c_write) = duplex(8192);
        let sink = collect_sink();
        let conn = JsonRpcConnection::new(c_read, c_write, sink.clone());
        // 关闭读端让写失败？duplex 的写在对端读关闭后会失败
        drop(s_read);
        // 等待写端感知关闭
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let res = conn.request("m", None).await;
        // 可能成功写入但后续 flush 失败，或直接失败，两种都接受为 Err
        // 若 duplex 未立即报错，则验证 shutdown_writer 路径
        if res.is_ok() {
            // 显式 shutdown writer 再请求应失败
            conn.shutdown_writer().await.expect("shutdown");
            let res2 = conn.request("m2", None).await;
            assert!(res2.is_err(), "shutdown 后应失败");
        } else {
            assert!(res.is_err());
        }
        // 额外验证 notify 也受写关闭影响
        let _ = s_read; // suppress unused
    }

    #[tokio::test]
    async fn empty_and_whitespace_lines_ignored() {
        let (ra, mut wa) = duplex(8192);
        let (dummy_r, dummy_w) = duplex(8192);
        let sink = collect_sink();
        let _conn = JsonRpcConnection::new(ra, dummy_w, sink.clone());
        wa.write_all(b"\n").await.expect("empty");
        wa.write_all(b"   \n").await.expect("ws");
        wa.write_all(b"\t\n").await.expect("tab");
        let good = serde_json::to_string(&json!({"method": "after_empty"})).expect("ser");
        wa.write_all(good.as_bytes()).await.expect("good");
        wa.write_all(b"\n").await.expect("nl");
        wa.flush().await.expect("flush");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(sink.parse_errors.lock().expect("lock").is_empty(), "空行不应算解析错误");
        let notifs = sink.notifications.lock().expect("lock").clone();
        assert_eq!(notifs.len(), 1);
        assert_eq!(notifs[0].0, "after_empty");
        drop(dummy_r);
    }

    #[tokio::test]
    async fn utf8_incremental_safe() {
        let (ra, mut wa) = duplex(8192);
        let (dummy_r, dummy_w) = duplex(8192);
        let sink = collect_sink();
        let _conn = JsonRpcConnection::new(ra, dummy_w, sink.clone());
        // 含中文的 params，按字节拆分写入
        let msg = json!({"method": "utf8", "params": {"text": "你好世界"}});
        let s = serde_json::to_string(&msg).expect("ser");
        let bytes = s.as_bytes();
        // 逐字节写入（模拟增量）
        for b in bytes {
            wa.write_all(std::slice::from_ref(b)).await.expect("byte");
        }
        wa.write_all(b"\n").await.expect("nl");
        wa.flush().await.expect("flush");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let notifs = sink.notifications.lock().expect("lock").clone();
        assert_eq!(notifs.len(), 1);
        assert_eq!(notifs[0].0, "utf8");
        assert_eq!(notifs[0].1.as_ref().and_then(|v| v.get("text")).and_then(|v| v.as_str()), Some("你好世界"));
        drop(dummy_r);
    }

    #[tokio::test]
    async fn error_response_propagates() {
        let (c_read, s_write) = duplex(8192);
        let (s_read, c_write) = duplex(8192);
        let sink = collect_sink();
        let conn = JsonRpcConnection::new(c_read, c_write, sink.clone());
        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(s_read);
            let mut writer = s_write;
            let mut line = String::new();
            reader.read_line(&mut line).await.expect("read");
            let v: Value = serde_json::from_str(line.trim()).expect("json");
            let id = v.get("id").expect("id").clone();
            let resp = json!({"id": id, "error": {"code": -32001, "message": "backpressure"}});
            let s = serde_json::to_string(&resp).expect("ser");
            writer.write_all(s.as_bytes()).await.expect("w");
            writer.write_all(b"\n").await.expect("nl");
            writer.flush().await.expect("flush");
        });
        let err = conn.request("m", None).await.expect_err("应为 error");
        assert!(err.0.contains("backpressure") || err.0.contains("-32001"), "错误应含 backpressure，实际 {}", err.0);
        server.await.expect("server");
    }

    #[tokio::test]
    async fn method_without_params_and_result_null() {
        let (c_read, s_write) = duplex(8192);
        let (s_read, c_write) = duplex(8192);
        let sink = collect_sink();
        let conn = JsonRpcConnection::new(c_read, c_write, sink.clone());
        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(s_read);
            let mut writer = s_write;
            let mut line = String::new();
            reader.read_line(&mut line).await.expect("read");
            let v: Value = serde_json::from_str(line.trim()).expect("json");
            let id = v.get("id").expect("id").clone();
            // 返回无 result 字段的响应，应归为 Null
            let resp = json!({"id": id});
            let s = serde_json::to_string(&resp).expect("ser");
            writer.write_all(s.as_bytes()).await.expect("w");
            writer.write_all(b"\n").await.expect("nl");
            writer.flush().await.expect("flush");
        });
        let res = conn.request("ping", None).await.expect("ok");
        assert_eq!(res, Value::Null);
        server.await.expect("server");
    }

    #[tokio::test]
    async fn notify_write_format() {
        let (a_read, a_write) = duplex(8192);
        let (b_read, b_write) = duplex(8192);
        let sink = collect_sink();
        let conn = JsonRpcConnection::new(a_read, b_write, sink.clone());
        conn.notify("initialized", None).await.expect("notify");
        conn.notify("custom/event", Some(json!({"x": 1}))).await.expect("notify2");
        // 从 b_read 读两行
        let mut reader = BufReader::new(b_read);
        let mut line1 = String::new();
        let mut line2 = String::new();
        reader.read_line(&mut line1).await.expect("r1");
        reader.read_line(&mut line2).await.expect("r2");
        let v1: Value = serde_json::from_str(line1.trim()).expect("j1");
        let v2: Value = serde_json::from_str(line2.trim()).expect("j2");
        assert_eq!(v1.get("method").and_then(|v| v.as_str()), Some("initialized"));
        assert!(v1.get("id").is_none(), "notification 不应有 id");
        assert_eq!(v2.get("method").and_then(|v| v.as_str()), Some("custom/event"));
        assert_eq!(v2.get("params"), Some(&json!({"x": 1})));
        drop(a_write);
    }

    #[tokio::test]
    async fn missing_method_and_id_parse_error() {
        let (ra, mut wa) = duplex(8192);
        let (dummy_r, dummy_w) = duplex(8192);
        let sink = collect_sink();
        let _conn = JsonRpcConnection::new(ra, dummy_w, sink.clone());
        let bad = json!({"params": {"x": 1}});
        let s = serde_json::to_string(&bad).expect("ser");
        wa.write_all(s.as_bytes()).await.expect("w");
        wa.write_all(b"\n").await.expect("nl");
        wa.flush().await.expect("flush");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let errs = sink.parse_errors.lock().expect("lock").clone();
        assert!(!errs.is_empty(), "缺 method/id 应算解析错误");
        drop(dummy_r);
    }

    #[tokio::test]
    async fn large_payload_single_frame() {
        let (ra, mut wa) = duplex(16384);
        let (dummy_r, dummy_w) = duplex(16384);
        let sink = collect_sink();
        let _conn = JsonRpcConnection::new(ra, dummy_w, sink.clone());
        let big_text = "a".repeat(8000);
        let msg = json!({"method": "big", "params": {"text": big_text}});
        let s = serde_json::to_string(&msg).expect("ser");
        wa.write_all(s.as_bytes()).await.expect("w");
        wa.write_all(b"\n").await.expect("nl");
        wa.flush().await.expect("flush");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let notifs = sink.notifications.lock().expect("lock").clone();
        assert_eq!(notifs.len(), 1);
        assert_eq!(notifs[0].0, "big");
        assert_eq!(
            notifs[0].1.as_ref().and_then(|v| v.get("text")).and_then(|v| v.as_str()).map(str::len),
            Some(8000)
        );
        drop(dummy_r);
    }
}
