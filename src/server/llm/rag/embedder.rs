//! OpenAI 兼容 embedding API 客户端。
//!
//! 调用 `POST {base_url}/embeddings`，body `{model, input: [texts]}`。
//! 用 KB 固化的 emb 配置；摄入批量、检索单条。embedding 应秒级返回，
//! 因此用独立的短超时 client（区别于上游 LLM 的长读超时）。

use reqwest::Client;

#[derive(Debug)]
pub enum EmbedError {
    Http(reqwest::Error),
    Api(String),
    EmptyResponse,
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(e) => write!(f, "embedding http error: {}", e),
            Self::Api(m) => write!(f, "embedding api error: {}", m),
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

pub struct Embedder {
    base_url: String,
    api_key: String,
    model: String,
    client: Client,
}

impl Embedder {
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Self {
        let client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("failed to build embedding client");
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            client,
        }
    }

    /// 批量向量化，返回向量顺序与输入一致。
    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!("{}/embeddings", self.base_url);
        let body = serde_json::json!({ "model": self.model, "input": texts });
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
            return Err(EmbedError::Api(format!("status {}: {}", status, snippet)));
        }
        let v: serde_json::Value = resp.json().await?;
        let mut data = v["data"]
            .as_array()
            .cloned()
            .ok_or(EmbedError::EmptyResponse)?;
        // 按 index 排序保证与输入顺序一致
        data.sort_by_key(|d| d["index"].as_u64().unwrap_or(0));
        let mut out = Vec::with_capacity(data.len());
        for d in data {
            let vec: Vec<f32> = serde_json::from_value(d["embedding"].clone())
                .map_err(|e| EmbedError::Api(format!("bad embedding vector: {}", e)))?;
            out.push(vec);
        }
        if out.is_empty() {
            return Err(EmbedError::EmptyResponse);
        }
        Ok(out)
    }

    /// 单条向量化（检索查询用）。
    pub async fn embed_one(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut v = self.embed(&[text.to_string()]).await?;
        v.pop().ok_or(EmbedError::EmptyResponse)
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
                let n = body["input"].as_array().map(|a| a.len()).unwrap_or(1);
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{}", addr)
    }

    #[tokio::test]
    async fn embed_returns_vectors_in_order() {
        let base = mock_embedding_server(8).await;
        let e = Embedder::new(&base, "sk-test", "test-model");
        let out = e.embed(&["a".into(), "b".into(), "c".into()]).await.unwrap();
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
}
