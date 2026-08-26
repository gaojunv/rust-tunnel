//! RAG 知识库管理 API 请求体类型（DTO）。

/// POST /api/llm/kb 请求体。`emb_*` 可选：不提供时回退到全局共享 embedding 配置
/// （`agent_memory_settings`，见 `create_kb` 的解析逻辑）。
#[derive(Debug, serde::Deserialize)]
pub struct CreateKbRequest {
    /// 知识库名称，必填，非空字符串。
    pub name: String,
    #[serde(default)]
    /// 知识库描述，可为空字符串。
    pub description: String,
    #[serde(default)]
    /// Embedding 服务 Base URL；`None` 或空字符串表示回退全局共享配置。
    pub emb_base_url: Option<String>,
    #[serde(default)]
    /// Embedding 服务 API Key；`None` 表示不覆盖。
    pub emb_api_key: Option<String>,
    #[serde(default)]
    /// Embedding 模型名；`None` 表示回退全局配置。
    pub emb_model: Option<String>,
    #[serde(default)]
    /// 向量维度，取值 `>= 1`；`None` 表示回退全局配置。
    pub emb_dimension: Option<i64>,
    #[serde(default)]
    /// 检索返回条数，取值 `1..=20`；`None` 时后端默认 5。
    pub top_k: Option<i64>,
    #[serde(default)]
    /// 分块大小，取值 `>= 1`；`None` 时后端默认 512。
    pub chunk_size: Option<i64>,
    #[serde(default)]
    /// 分块重叠大小，必须 `< chunk_size`；`None` 时后端默认 64。
    pub chunk_overlap: Option<i64>,
    #[serde(default)]
    /// 相似度阈值，取值 `[0.0, 1.0]`；`None` 时后端默认 0.3。
    pub score_threshold: Option<f64>,
    #[serde(default)]
    /// 是否启用知识库；`None` 时默认启用。
    pub enabled: Option<bool>,
}

/// PUT /api/llm/kb/:id 请求体。emb 配置可编辑：四个 emb 字段均 Optional，
/// 缺省或空字符串 = 沿用 DB 当前值（编辑态 merge，区别于创建态回退全局共享配置）。
/// 仅 `emb_base_url`/`emb_model`/`emb_dimension` 任一变化会触发全量重建；
/// 仅 `emb_api_key` 变化只替换密钥密文、不重建。
#[derive(Debug, serde::Deserialize)]
pub struct UpdateKbRequest {
    /// 知识库名称，必填，非空字符串。
    pub name: String,
    #[serde(default)]
    /// 知识库描述，可为空字符串。
    pub description: String,
    #[serde(default)]
    /// Embedding 服务 Base URL；`None` 表示沿用既有值，空字符串会返回 400。
    pub emb_base_url: Option<String>,
    #[serde(default)]
    /// Embedding 服务 API Key；`None` 或空字符串表示保留旧密钥。
    pub emb_api_key: Option<String>,
    #[serde(default)]
    /// Embedding 模型名；`None` 表示沿用既有值。
    pub emb_model: Option<String>,
    #[serde(default)]
    /// 向量维度，取值 `>= 1`；`None` 表示沿用既有值。
    pub emb_dimension: Option<i64>,
    #[serde(default)]
    /// 检索返回条数，取值 `1..=20`；`None` 时后端默认 5。
    pub top_k: Option<i64>,
    #[serde(default)]
    /// 分块大小，取值 `>= 1`；`None` 时后端默认 512。
    pub chunk_size: Option<i64>,
    #[serde(default)]
    /// 分块重叠大小，必须 `< chunk_size`；`None` 时后端默认 64。
    pub chunk_overlap: Option<i64>,
    #[serde(default)]
    /// 相似度阈值，取值 `[0.0, 1.0]`；`None` 时后端默认 0.3。
    pub score_threshold: Option<f64>,
}

/// POST /api/llm/kb/test-embedding 请求体。`kb_id` 可选：编辑 KB 场景提供时，
/// `api_key` 留空则用该 KB 已存密钥探测（后端不回显密钥，前端拿不到旧值）。
#[derive(Debug, serde::Deserialize)]
pub struct TestEmbeddingRequest {
    /// Embedding 服务 Base URL，必填。
    pub base_url: String,
    #[serde(default)]
    /// Embedding 服务 API Key；编辑场景留空时若提供 `kb_id` 则用库内已存密钥探测。
    pub api_key: String,
    /// 待探测的 Embedding 模型名。
    pub model: String,
    #[serde(default)]
    /// 关联知识库 ID；提供时可复用库内已存密钥。
    pub kb_id: Option<String>,
}

/// POST /api/llm/kb/:id/query 请求体。
#[derive(Debug, serde::Deserialize)]
pub struct QueryKbRequest {
    /// 检索查询文本，必填。
    pub text: String,
}
