//! 提取错误类型。

/// 文档提取失败原因。Display 输出写入 `rag_documents.error` 并经 SSE 推给前端。
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    /// 文件损坏或货不对板（magic bytes 不符）。
    #[error("invalid file format: {0}")]
    InvalidFormat(String),
    /// PDF 无文本层（扫描件）。
    #[error("PDF has no text layer (likely a scanned document); OCR is not supported")]
    NoTextLayer,
    /// 解析过程失败（结构损坏、缺部件等）。
    #[error("failed to parse document: {0}")]
    ParseFailed(String),
    /// 文本类文件不是合法 UTF-8。
    #[error("file must be UTF-8 text")]
    NotUtf8,
}
