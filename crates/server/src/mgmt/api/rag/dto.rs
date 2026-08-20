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

/// PUT /api/llm/kb/:id 请求体。emb 配置可编辑：四个 emb 字段均 Optional，
/// 缺省或空字符串 = 沿用 DB 当前值（编辑态 merge，区别于创建态回退全局共享配置）。
/// 仅 `emb_base_url`/`emb_model`/`emb_dimension` 任一变化会触发全量重建；
/// 仅 `emb_api_key` 变化只替换密钥密文、不重建。
#[derive(Debug, serde::Deserialize)]
pub struct UpdateKbRequest {
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
}

/// POST /api/llm/kb/test-embedding 请求体。`kb_id` 可选：编辑 KB 场景提供时，
/// `api_key` 留空则用该 KB 已存密钥探测（后端不回显密钥，前端拿不到旧值）。
#[derive(Debug, serde::Deserialize)]
pub struct TestEmbeddingRequest {
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    pub model: String,
    #[serde(default)]
    pub kb_id: Option<String>,
}

/// POST /api/llm/kb/:id/query 请求体。
#[derive(Debug, serde::Deserialize)]
pub struct QueryKbRequest {
    pub text: String,
}
