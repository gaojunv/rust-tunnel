//! 多格式文档文本提取：原始字节 + 文件类型 → Markdown 文本。
//! PDF/Office 解析全部走纯 Rust crate（lopdf/zip/quick-xml），不做 OCR。

pub mod error;
pub mod ooxml;
pub mod pdf;

pub use error::ExtractError;

/// 文本类大小上限（2MB，与现状一致）。
const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;
/// 二进制类大小上限（20MB：带图 PPT/PDF 常达数 MB）。
const MAX_BINARY_BYTES: usize = 20 * 1024 * 1024;
/// 提取文本累计字节上限（20MB，与二进制上传上限一致）。PDF 逐页累计文本、
/// OOXML 单部件解压共享同一数量级：上传的二进制是不可信输入，解压（deflate /
/// FlateDecode）膨胀比可达 ~1000:1，必须在提取侧硬性封顶，防止膨胀到 GB 级。
pub(crate) const MAX_EXTRACT_TEXT_BYTES: usize = 20 * 1024 * 1024;

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

/// 统一提取入口：原始字节 → Markdown 文本。
/// Markdown/Text 直通（UTF-8 校验）；二进制格式走各自解析器。
pub fn extract(bytes: &[u8], file_type: FileType) -> Result<String, ExtractError> {
    match file_type {
        FileType::Markdown | FileType::Text => {
            String::from_utf8(bytes.to_vec()).map_err(|_| ExtractError::NotUtf8)
        }
        FileType::Pdf => pdf::pdf_to_markdown(bytes),
        FileType::Docx => ooxml::docx_to_markdown(bytes),
        FileType::Xlsx => ooxml::xlsx_to_markdown(bytes),
        FileType::Pptx => ooxml::pptx_to_markdown(bytes),
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

    #[test]
    fn extract_text_passthrough() {
        let md = "# 标题\n\n内容。\n";
        assert_eq!(extract(md.as_bytes(), FileType::Markdown).unwrap(), md);
        assert_eq!(
            extract(b"plain text", FileType::Text).unwrap(),
            "plain text"
        );
    }

    #[test]
    fn extract_text_rejects_non_utf8() {
        let bad = [0xFF, 0xFE, 0x41];
        assert!(matches!(
            extract(&bad, FileType::Markdown),
            Err(ExtractError::NotUtf8)
        ));
    }

    #[test]
    fn extract_docx_dispatches_to_ooxml() {
        let bytes = ooxml::tests::make_test_docx();
        let out = extract(&bytes, FileType::Docx).unwrap();
        assert!(out.contains("# 安装指南"), "got: {out}");
        assert!(out.contains("第一步：下载。"), "got: {out}");
    }

    #[test]
    fn extract_xlsx_and_pptx_dispatch_to_ooxml() {
        let xlsx_out = extract(&ooxml::tests::make_test_xlsx(), FileType::Xlsx).unwrap();
        assert!(xlsx_out.contains("| 姓名 | 年龄 |"), "got: {xlsx_out}");
        let pptx_out = extract(&ooxml::tests::make_test_pptx(), FileType::Pptx).unwrap();
        assert!(pptx_out.contains("## 产品方案"), "got: {pptx_out}");
    }
}
