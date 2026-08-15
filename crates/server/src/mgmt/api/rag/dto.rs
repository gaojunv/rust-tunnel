//! RAG 知识库管理 API 请求体类型（DTO）。

/// POST /api/llm/kb 请求体。`emb_*` 可选：不提供时回退到全局共享 embedding 配置
/// （`agent_memory_settings`，见 `create_kb` 的解析逻辑）。
#[derive(Debug, serde::Deserialize)]
pub struct CreateKbRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub emb_base_url: Option<String>,
    #[serde(default)]
    pub emb_api_key: Option<String>,
    #[serde(default)]
    pub emb_model: Option<String>,
    #[serde(default)]
    pub emb_dimension: Option<i64>,
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
