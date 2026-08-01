//! 多格式文档文本提取：原始字节 + 文件类型 → Markdown 文本。
//! PDF/Office 解析全部走纯 Rust crate（lopdf/zip/quick-xml），不做 OCR。

pub mod error;

pub use error::ExtractError;

/// 文本类大小上限（2MB，与现状一致）。
const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;
/// 二进制类大小上限（20MB：带图 PPT/PDF 常达数 MB）。
const MAX_BINARY_BYTES: usize = 20 * 1024 * 1024;

/// 支持摄入的文件类型。DB `rag_documents.file_type` 存 `as_str()` 值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Markdown,
    Text,
    Pdf,
    Docx,
    Xlsx,
    Pptx,
}

impl FileType {
    /// 按扩展名判定（调用方传入小写），不在白名单返回 None。
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "md" => Some(Self::Markdown),
            "txt" => Some(Self::Text),
            "pdf" => Some(Self::Pdf),
            "docx" => Some(Self::Docx),
            "xlsx" => Some(Self::Xlsx),
            "pptx" => Some(Self::Pptx),
            _ => None,
        }
    }

    /// 该类型的大小上限（文本 2MB，二进制 20MB）。
    pub fn max_bytes(self) -> usize {
        match self {
            Self::Markdown | Self::Text => MAX_TEXT_BYTES,
            Self::Pdf | Self::Docx | Self::Xlsx | Self::Pptx => MAX_BINARY_BYTES,
        }
    }

    /// DB 存储/落盘扩展名用的规范字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Markdown => "md",
            Self::Text => "txt",
            Self::Pdf => "pdf",
            Self::Docx => "docx",
            Self::Xlsx => "xlsx",
            Self::Pptx => "pptx",
        }
    }

    /// 轻量探测：检查 magic bytes，明显损坏/货不对板返回 Err。
    /// 文本类无探测（UTF-8 校验在 extract 里做）。
    pub fn probe(self, bytes: &[u8]) -> Result<(), ExtractError> {
        match self {
            Self::Markdown | Self::Text => Ok(()),
            Self::Pdf => {
                if bytes.len() >= 5 && &bytes[..5] == b"%PDF-" {
                    Ok(())
                } else {
                    Err(ExtractError::InvalidFormat(
                        "not a PDF file (missing %PDF- header)".to_string(),
                    ))
                }
            }
            Self::Docx | Self::Xlsx | Self::Pptx => {
                if bytes.len() >= 4 && bytes[..4] == [0x50, 0x4B, 0x03, 0x04] {
                    Ok(())
                } else {
                    Err(ExtractError::InvalidFormat(
                        "not an OOXML file (missing zip magic)".to_string(),
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_extension_accepts_whitelist() {
        assert_eq!(FileType::from_extension("md"), Some(FileType::Markdown));
        assert_eq!(FileType::from_extension("txt"), Some(FileType::Text));
        assert_eq!(FileType::from_extension("pdf"), Some(FileType::Pdf));
        assert_eq!(FileType::from_extension("docx"), Some(FileType::Docx));
        assert_eq!(FileType::from_extension("xlsx"), Some(FileType::Xlsx));
        assert_eq!(FileType::from_extension("pptx"), Some(FileType::Pptx));
    }

    #[test]
    fn from_extension_rejects_legacy_office_and_images() {
        for ext in ["doc", "xls", "ppt", "jpg", "png", "gif", "exe", ""] {
            assert_eq!(FileType::from_extension(ext), None, "ext={ext}");
        }
    }

    #[test]
    fn max_bytes_by_category() {
        assert_eq!(FileType::Markdown.max_bytes(), 2 * 1024 * 1024);
        assert_eq!(FileType::Text.max_bytes(), 2 * 1024 * 1024);
        assert_eq!(FileType::Pdf.max_bytes(), 20 * 1024 * 1024);
        assert_eq!(FileType::Docx.max_bytes(), 20 * 1024 * 1024);
        assert_eq!(FileType::Xlsx.max_bytes(), 20 * 1024 * 1024);
        assert_eq!(FileType::Pptx.max_bytes(), 20 * 1024 * 1024);
    }

    #[test]
    fn as_str_roundtrips_from_extension() {
        for ft in [
            FileType::Markdown,
            FileType::Text,
            FileType::Pdf,
            FileType::Docx,
            FileType::Xlsx,
            FileType::Pptx,
        ] {
            assert_eq!(FileType::from_extension(ft.as_str()), Some(ft));
        }
    }

    #[test]
    fn probe_pdf_checks_magic() {
        assert!(FileType::Pdf.probe(b"%PDF-1.7 rest...").is_ok());
        assert!(FileType::Pdf.probe(b"not a pdf").is_err());
        assert!(FileType::Pdf.probe(b"%PD").is_err());
    }

    #[test]
    fn probe_ooxml_checks_zip_magic() {
        let zip_head = [0x50, 0x4B, 0x03, 0x04, 0x14, 0x00];
        assert!(FileType::Docx.probe(&zip_head).is_ok());
        assert!(FileType::Xlsx.probe(&zip_head).is_ok());
        assert!(FileType::Pptx.probe(&zip_head).is_ok());
        assert!(FileType::Docx.probe(b"%PDF-1.7").is_err());
    }
}
