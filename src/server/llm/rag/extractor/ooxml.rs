//! OOXML（docx/xlsx/pptx）解包与 XML 解析共享基建。
//! OOXML = zip 容器 + XML 部件；三种格式复用 open_zip/read_part。
//!
//! 安全注意：上传的 docx/xlsx/pptx 是不可信输入，zip 中央目录中声明的
//! 解压大小（`size`）等头部字段可被伪造，绝不能据此分配内存或限制读取。

use std::io::{Cursor, Read, Seek};

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use super::error::ExtractError;

/// 打开 OOXML zip 容器。
pub fn open_zip(bytes: &[u8]) -> Result<zip::ZipArchive<Cursor<&[u8]>>, ExtractError> {
    zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| ExtractError::ParseFailed(format!("zip open: {e}")))
}

/// 读取 zip 内一个部件为 UTF-8 字符串。
///
/// 注意：不根据中央目录声明的 `part.size()` 预分配缓冲——该字段来自不可信
/// 的 zip 头部，可伪造为任意大（见模块注释）。改用 `Vec::new()` 让
/// `read_to_end` 按真实解压字节增长，天然受 inflate 实际输出约束。
pub fn read_part<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<String, ExtractError> {
    let mut part = archive
        .by_name(name)
        .map_err(|_| ExtractError::ParseFailed(format!("missing part: {name}")))?;
    let mut buf = Vec::new();
    part.read_to_end(&mut buf)
        .map_err(|e| ExtractError::ParseFailed(format!("read part {name}: {e}")))?;
    String::from_utf8(buf).map_err(|_| ExtractError::ParseFailed(format!("part {name} not utf-8")))
}

/// docx → Markdown：段落按 w:p 边界；带 HeadingN 样式的段落转 `#`*N 标题。
pub fn docx_to_markdown(bytes: &[u8]) -> Result<String, ExtractError> {
    let mut archive = open_zip(bytes)?;
    let xml = read_part(&mut archive, "word/document.xml")?;
    let mut reader = Reader::from_str(&xml);
    reader.trim_text(true);

    let mut out = String::new();
    let mut para_text = String::new(); // 当前段落累计文本
    let mut para_heading: Option<usize> = None; // 当前段落标题级别
    let mut in_paragraph = false;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"w:p" => {
                    in_paragraph = true;
                    para_text.clear();
                    para_heading = None;
                }
                b"w:pStyle" => apply_pstyle(&mut para_heading, &e),
                _ => {}
            },
            // w:pStyle 是自闭合空元素（真实 Word 文档同样如此）→ Empty 事件。
            Ok(Event::Empty(e)) if e.name().as_ref() == b"w:pStyle" => {
                apply_pstyle(&mut para_heading, &e);
            }
            Ok(Event::Text(e)) if in_paragraph => {
                if let Ok(t) = e.unescape() {
                    para_text.push_str(&t);
                }
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"w:p" => {
                in_paragraph = false;
                let text = para_text.trim();
                if !text.is_empty() {
                    match para_heading {
                        Some(level) => {
                            out.push_str(&"#".repeat(level.min(6)));
                            out.push(' ');
                            out.push_str(text);
                            out.push_str("\n\n");
                        }
                        None => {
                            out.push_str(text);
                            out.push_str("\n\n");
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(ExtractError::ParseFailed(format!("docx xml: {e}"))),
            _ => {}
        }
        buf.clear();
    }

    let result = out.trim().to_string();
    if result.is_empty() {
        return Err(ExtractError::ParseFailed("docx contains no text".into()));
    }
    Ok(result)
}

/// 从 w:pStyle 元素（Start 或 Empty 事件）读取 w:val 设置标题级别。
fn apply_pstyle(para_heading: &mut Option<usize>, elem: &BytesStart<'_>) {
    for attr in elem.attributes().flatten() {
        if attr.key.as_ref() == b"w:val" {
            let val = String::from_utf8_lossy(&attr.value).to_string();
            if let Some(level) = parse_heading_level(&val) {
                *para_heading = Some(level);
            }
        }
    }
}

/// "Heading1" → Some(1)；其余（含纯数字、中文样式名）→ None。
/// w:pStyle 的 w:val 是内部 styleId（内置标题始终为 HeadingN，与 UI 语言无关）。
fn parse_heading_level(style: &str) -> Option<usize> {
    let digits: String = style.chars().filter(|c| c.is_ascii_digit()).collect();
    if style.to_ascii_lowercase().starts_with("heading") {
        digits.parse().ok()
    } else {
        None
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 程序化生成最小 docx（word/document.xml 含两段文本 + 一级标题），
    /// 供本模块与 ingest 任务（Task 6）复用。
    pub(crate) fn make_test_docx() -> Vec<u8> {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file("[Content_Types].xml", opts).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#).unwrap();
            zip.start_file("word/document.xml", opts).unwrap();
            // 含中文，不能用 br# 字节串（字节串字面量仅限 ASCII），转普通字符串写字节。
            let document_xml = r#"<?xml version="1.0"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>安装指南</w:t></w:r></w:p>
<w:p><w:r><w:t>第一步：下载。</w:t></w:r></w:p>
</w:body></w:document>"#;
            zip.write_all(document_xml.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn docx_extracts_headings_and_paragraphs() {
        let out = docx_to_markdown(&make_test_docx()).unwrap();
        assert!(out.contains("# 安装指南"), "got: {out}");
        assert!(out.contains("第一步：下载。"), "got: {out}");
    }

    #[test]
    fn docx_rejects_missing_document_xml() {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::FileOptions::default();
            zip.start_file("[Content_Types].xml", opts).unwrap();
            zip.write_all(b"<Types/>").unwrap();
            zip.finish().unwrap();
        }
        assert!(matches!(
            docx_to_markdown(&buf.into_inner()),
            Err(ExtractError::ParseFailed(_))
        ));
    }
}
