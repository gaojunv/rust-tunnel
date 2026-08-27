//! OpenAI 兼容 embedding API 客户端。
//!
//! 调用 `POST {base_url}/embeddings`，body `{model, input: [texts]}`。
//! 用 KB 固化的 emb 配置；摄入批量、检索单条。embedding 应秒级返回，
//! 因此用独立的短超时 client（区别于上游 LLM 的长读超时）。

use reqwest::Client;
use rust_tunnel_common::http_client;

/// 单次 embed 请求的批大小上限。上游 embedding API 通常限制单请求文本数，
/// 超长请求会被拒（400/413）或超时；超出时内部切多批逐个请求再按序拼接。
const EMBED_BATCH_SIZE: usize = 64;
/// 单批最大尝试次数（首次 + 2 次重试），仍失败才向调用方返回 Err。
const EMBED_MAX_ATTEMPTS: usize = 3;
/// 重试基础退避（毫秒）：第 1 次重试 500ms、第 2 次 1s，避免瞬时故障集中重试。
const EMBED_RETRY_BASE_MS: u64 = 500;
/// embedding 整体请求超时：embedding 应秒级返回，1 分钟足够覆盖慢端点。
const EMBED_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(1);

/// 向量化错误类型。
#[derive(Debug)]
pub enum EmbedError {
    /// HTTP 传输失败。
    Http(reqwest::Error),
    /// 上游 API 返回非成功状态或响应解析失败。
    Api(String),
    /// 响应中未包含任何向量数据。
    EmptyResponse,
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(e) => write!(f, "embedding http error: {e}"),
            Self::Api(m) => write!(f, "embedding api error: {m}"),
            Self::EmptyResponse => write!(f, "embedding returned no data"),
        }
    }
}
impl std::error::Error for EmbedError {}
impl From<reqwest::Error> for EmbedError {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e)
    }
}

/// OpenAI 兼容 embedding 客户端，负责批量与单条向量化请求。
pub struct Embedder {
    base_url: String,
    api_key: String,
    model: String,
    client: Client,
}

impl Embedder {
    /// # Panics
    /// reqwest Client 构建失败属构造期 fatal（TLS 后端不可用），无法恢复。
    #[must_use]
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Self {
        // embedding 属慢端点：整体超时放宽到 1 分钟，其余默认走统一工厂
        let client = http_client::build(http_client::builder().timeout(EMBED_TIMEOUT));
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            client,
        }
    }

    /// 批量向量化，返回向量顺序与输入一致。超过 `EMBED_BATCH_SIZE` 时内部
    /// 切多批逐个请求再拼接，对调用方透明；每批失败重试 `EMBED_MAX_ATTEMPTS`
    /// 次（500ms/1s 退避），仍失败才返回 `Err`。
    ///
    /// # Errors
    /// 当上游 embedding API 返回非成功状态、网络请求失败或响应体解析失败时返回 `Err`。
    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(EMBED_BATCH_SIZE) {
            out.extend(self.embed_batch(batch).await?);
        }
        Ok(out)
    }

    /// 单条向量化（检索查询用）。
    ///
    /// # Errors
    /// 当底层 [`Self::embed`] 调用失败或响应为空时返回 `Err`。
    pub async fn embed_one(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut v = self.embed(&[text.to_string()]).await?;
        v.pop().ok_or(EmbedError::EmptyResponse)
    }

    /// 单批 embed（≤ `EMBED_BATCH_SIZE` 条）：失败重试，仍失败返回最后一次错误。
    async fn embed_batch(&self, batch: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut attempt = 0;
        loop {
            match self.embed_batch_once(batch).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    attempt += 1;
                    if attempt >= EMBED_MAX_ATTEMPTS {
                        return Err(e);
                    }
                    // 退避 500ms → 1s（第 1 次重试睡 500ms，第 2 次睡 1s）
                    let delay_ms = EMBED_RETRY_BASE_MS << (attempt - 1);
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
            }
        }
    }

    /// 单次 embed 请求（无重试）。
    async fn embed_batch_once(&self, batch: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let url = format!("{}/embeddings", self.base_url);
        let body = serde_json::json!({ "model": self.model, "input": batch });
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            // char 安全截断：错误体可能含多字节 UTF-8，按字节切片会 panic
            let snippet = text.chars().take(300).collect::<String>();
            return Err(EmbedError::Api(format!("status {status}: {snippet}")));
        }
        let v: serde_json::Value = resp.json().await?;
        let mut data = v["data"]
            .as_array()
            .cloned()
            .ok_or(EmbedError::EmptyResponse)?;
        // 按 index 排序保证与输入顺序一致（每批内的 index 从 0 开始）
        data.sort_by_key(|d| d["index"].as_u64().unwrap_or(0));
        let mut out = Vec::with_capacity(data.len());
        for d in data {
            let vec: Vec<f32> = serde_json::from_value(d["embedding"].clone())
                .map_err(|e| EmbedError::Api(format!("bad embedding vector: {e}")))?;
            out.push(vec);
        }
        if out.is_empty() {
            return Err(EmbedError::EmptyResponse);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 起一个返回固定 embedding 的本地 HTTP server，返回 base_url。
    async fn mock_embedding_server(dim: usize) -> String {
        use axum::extract::Json;
        use axum::routing::post;
        use axum::Router;
        use serde_json::{json, Value};
        let _ = dim; // 固定维度 8，参数保留接口形状
        let app = Router::new().route(
            "/embeddings",
            post(|body: Json<Value>| async move {
                let n = body["input"].as_array().map_or(1, std::vec::Vec::len);
                let data: Vec<_> = (0..n)
                    .map(|i| {
                        json!({
                            "index": i,
                            "embedding": vec![0.1f32; 8], // 测试固定维度 8
                            "object": "embedding"
                        })
                    })
                    .collect();
                Json(json!({"object": "list", "data": data}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn embed_returns_vectors_in_order() {
        let base = mock_embedding_server(8).await;
        let e = Embedder::new(&base, "sk-test", "test-model");
        let out = e
            .embed(&["a".into(), "b".into(), "c".into()])
            .await
            .unwrap();
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|v| v.len() == 8));
    }

    #[tokio::test]
    async fn embed_one_returns_single_vector() {
        let base = mock_embedding_server(8).await;
        let e = Embedder::new(&base, "sk-test", "test-model");
        let v = e.embed_one("hello").await.unwrap();
        assert_eq!(v.len(), 8);
    }

    #[tokio::test]
    async fn embed_propagates_http_error() {
        // 连一个几乎不可能开放的端口
        let e = Embedder::new("http://127.0.0.1:1", "sk", "m");
        assert!(e.embed_one("x").await.is_err());
    }

    /// mock 记录每批 input 长度，且向量首位编码文本自身序号（"item-<i>"），
    /// 用于断言跨批拼接后顺序与输入一致。
    #[tokio::test]
    async fn embed_splits_large_input_into_batches() {
        use axum::extract::Json;
        use axum::routing::post;
        use axum::Router;
        use serde_json::{json, Value};
        use std::sync::{Arc, Mutex};

        let batch_sizes = Arc::new(Mutex::new(Vec::<usize>::new()));
        let sizes = batch_sizes.clone();
        let app = Router::new().route(
            "/embeddings",
            post(move |body: Json<Value>| async move {
                let arr = body["input"].as_array().cloned().unwrap_or_default();
                sizes.lock().unwrap().push(arr.len());
                let data: Vec<_> = arr
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let idx: f32 = t
                            .as_str()
                            .unwrap_or("")
                            .trim_start_matches("item-")
                            .parse()
                            .unwrap();
                        json!({
                            "index": i,
                            "embedding": vec![idx, 0.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                            "object": "embedding"
                        })
                    })
                    .collect();
                Json(json!({"object": "list", "data": data}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let e = Embedder::new(&format!("http://{addr}"), "sk-test", "test-model");
        let texts: Vec<String> = (0..150).map(|i| format!("item-{i}")).collect();
        let out = e.embed(&texts).await.unwrap();

        assert_eq!(out.len(), 150);
        // 超过批大小（64）被拆成 64/64/22 三批
        let sizes = batch_sizes.lock().unwrap();
        assert_eq!(sizes.as_slice(), &[64, 64, 22]);
        // 跨批拼接顺序与输入一致
        #[allow(
            clippy::cast_precision_loss,
            reason = "测试断言：索引 i 最大 150，远小于 f32 精确整数范围"
        )]
        for (i, v) in out.iter().enumerate() {
            #[allow(
                clippy::float_cmp,
                reason = "测试断言：上限位编码 0.0 exact 值，精确比较为所需语义"
            )]
            {
                assert_eq!(v[0], i as f32, "text {i} out of order");
            }
        }
    }

    #[tokio::test]
    async fn embed_retries_then_succeeds() {
        use axum::extract::Json;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::Router;
        use serde_json::{json, Value};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        // 首次返回 500 触发重试，随后成功；断言总请求数 = 2 且结果正常。
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let app = Router::new().route(
            "/embeddings",
            post(move |body: Json<Value>| async move {
                if calls_clone.fetch_add(1, Ordering::SeqCst) == 0 {
                    return (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
                }
                let arr = body["input"].as_array().cloned().unwrap_or_default();
                let data: Vec<_> = (0..arr.len())
                    .map(|i| {
                        json!({
                            "index": i,
                            "embedding": vec![0.1f32; 8],
                            "object": "embedding"
                        })
                    })
                    .collect();
                Json(json!({"object": "list", "data": data})).into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let e = Embedder::new(&format!("http://{addr}"), "sk-test", "test-model");
        let out = e.embed(&["a".to_string(), "b".to_string()]).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
