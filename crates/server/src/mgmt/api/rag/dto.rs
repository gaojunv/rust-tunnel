//! RAG 知识库管理 API 请求体类型（DTO）。

/// POST /api/llm/kb 请求体。`emb_dimension` 必填（前端经 `test-embedding` 探测）。
#[derive(Debug, serde::Deserialize)]
pub struct CreateKbRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub emb_base_url: String,
    #[serde(default)]
    pub emb_api_key: String,
    pub emb_model: String,
    pub emb_dimension: i64,
    #[serde(default)]
    pub top_k: Option<i64>,
    #[serde(default)]
    pub chunk_size: Option<i64>,
    #[serde(default)]
    pub chunk_overlap: Option<i64>,
    #[serde(default)]
    pub score_threshold: Option<f64>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// PUT /api/llm/kb/:id 请求体。emb 配置建库后锁定不可改，此处只更新名称/描述/检索参数。
#[derive(Debug, serde::Deserialize)]
pub struct UpdateKbRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub top_k: Option<i64>,
    #[serde(default)]
    pub chunk_size: Option<i64>,
    #[serde(default)]
    pub chunk_overlap: Option<i64>,
    #[serde(default)]
    pub score_threshold: Option<f64>,
}

/// POST /api/llm/kb/test-embedding 请求体。
#[derive(Debug, serde::Deserialize)]
pub struct TestEmbeddingRequest {
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    pub model: String,
}

/// POST /api/llm/kb/:id/query 请求体。
#[derive(Debug, serde::Deserialize)]
pub struct QueryKbRequest {
    pub text: String,
}
