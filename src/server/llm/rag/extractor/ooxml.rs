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

/// xlsx → Markdown：每 sheet 一个 `## 表名` + Markdown 表格。
///
/// 设计简化：sheet 名按 `xl/worksheets/sheet<N>.xml` 的序号直接对应 workbook
/// `<sheet>` 声明序，不解析 `xl/_rels/workbook.xml.rels` 的 rId→目标文件映射
/// （严格实现应走 rels）。若遇到实际 sheet 顺序错乱的文件再补 rels 解析。
pub fn xlsx_to_markdown(bytes: &[u8]) -> Result<String, ExtractError> {
    let mut archive = open_zip(bytes)?;
    let shared = read_shared_strings(&mut archive);
    let sheet_names = read_sheet_names(&mut archive)?;
    let mut out = String::new();
    for (idx, name) in sheet_names.iter().enumerate() {
        let part = format!("xl/worksheets/sheet{}.xml", idx + 1);
        // 部件缺失容忍：有些生成器从 sheet2 编号——找不到就跳过该 sheet。
        let Ok(xml) = read_part(&mut archive, &part) else {
            continue;
        };
        let rows = parse_sheet_rows(&xml, &shared);
        if rows.is_empty() {
            continue;
        }
        out.push_str("## ");
        out.push_str(name);
        out.push_str("\n\n");
        out.push_str(&rows_to_markdown_table(&rows));
        out.push_str("\n\n");
    }
    let result = out.trim().to_string();
    if result.is_empty() {
        return Err(ExtractError::ParseFailed("xlsx contains no data".into()));
    }
    Ok(result)
}

/// 解析 `xl/sharedStrings.xml`（可选部件，缺失则空表）：每个 `<si>` 内所有 `<t>`
/// 文本拼接为一条共享字符串。
fn read_shared_strings<R: Read + Seek>(archive: &mut zip::ZipArchive<R>) -> Vec<String> {
    let Ok(xml) = read_part(archive, "xl/sharedStrings.xml") else {
        return Vec::new();
    };
    let mut reader = Reader::from_str(&xml);
    reader.trim_text(true);
    let mut strings = Vec::new();
    let mut in_si = false;
    let mut si_text = String::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == b"si" => {
                in_si = true;
                si_text.clear();
            }
            Ok(Event::Text(e)) if in_si => {
                if let Ok(t) = e.unescape() {
                    si_text.push_str(&t);
                }
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"si" => {
                in_si = false;
                strings.push(std::mem::take(&mut si_text));
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    strings
}

/// 解析 `xl/workbook.xml` 的 `<sheet name="...">` 属性得 sheet 名列表。
/// `<sheet>` 是自闭合元素（Empty 事件），显式闭合（Start）也兼容。
fn read_sheet_names<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
) -> Result<Vec<String>, ExtractError> {
    let xml = read_part(archive, "xl/workbook.xml")?;
    let mut reader = Reader::from_str(&xml);
    reader.trim_text(true);
    let mut names = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if e.name().as_ref() == b"sheet" => {
                if let Some(name) = elem_attr(&e, b"name") {
                    names.push(name);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(ExtractError::ParseFailed(format!("workbook xml: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(names)
}

/// 解析 sheet 工作表的 `<row>`/`<c>`：返回每行按列号定位的文本（缺失列补空）。
fn parse_sheet_rows(xml: &str, shared: &[String]) -> Vec<Vec<String>> {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell: Option<Cell> = None;
    let mut in_value = false;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"row" => row.clear(),
                b"c" => cell = Some(Cell::from_elem(&e)),
                // 只收集 `<v>` 内的文本，避免 `<f>`（公式）污染值。
                b"v" => {
                    in_value = true;
                    if let Some(c) = &mut cell {
                        c.value.clear();
                    }
                }
                _ => {}
            },
            // 自闭合空单元格（`<c r="A1" t="s"/>`，无 `<v>`）：有位置信息，值为空。
            Ok(Event::Empty(e)) if e.name().as_ref() == b"c" => {
                cell = Some(Cell::from_elem(&e));
            }
            Ok(Event::Text(e)) if in_value => {
                if let Some(c) = &mut cell {
                    if let Ok(t) = e.unescape() {
                        c.value.push_str(&t);
                    }
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"v" => in_value = false,
                b"c" => {
                    in_value = false;
                    if let Some(c) = cell.take() {
                        push_cell(&mut row, c, shared);
                    }
                }
                b"row" => {
                    in_value = false;
                    if !row.is_empty() {
                        rows.push(std::mem::take(&mut row));
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    rows
}

/// 解析中的单元格：列位置（无 r 属性则按序追加）+ 是否共享字符串 + `<v>` 原始值。
struct Cell {
    col: Option<usize>,
    shared: bool,
    value: String,
}

impl Cell {
    /// 从 `<c>` 的 r/t 属性初始化（值在 `<v>` 文本中累计）。
    fn from_elem(elem: &BytesStart<'_>) -> Self {
        let mut col = None;
        let mut shared = false;
        for attr in elem.attributes().flatten() {
            match attr.key.as_ref() {
                b"r" => col = Some(col_index(&String::from_utf8_lossy(&attr.value))),
                b"t" => shared = attr.value.as_ref() == b"s",
                _ => {}
            }
        }
        Self {
            col,
            shared,
            value: String::new(),
        }
    }
}

/// 把一个单元格落进行：共享字符串按下标查表；`t="str"`/无 `t` 取字面值；
/// 有列号则补空单元格到对应列，无列号则按序追加。
fn push_cell(row: &mut Vec<String>, cell: Cell, shared: &[String]) {
    let text = if cell.shared {
        cell.value
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|idx| shared.get(idx))
            .cloned()
            .unwrap_or_default()
    } else {
        cell.value
    };
    let text = text.trim().to_string();
    match cell.col {
        Some(col) => {
            if row.len() <= col {
                row.resize(col + 1, String::new());
            }
            row[col] = text;
        }
        None => row.push(text),
    }
}

/// 行集合 → Markdown 表格：首行表头，`| --- |` 分隔行，之后数据行。
fn rows_to_markdown_table(rows: &[Vec<String>]) -> String {
    let Some((header, body)) = rows.split_first() else {
        return String::new();
    };
    let mut out = String::new();
    out.push_str(&markdown_row(header));
    out.push('\n');
    out.push_str(&separator_row(header.len()));
    for row in body {
        out.push('\n');
        out.push_str(&markdown_row(row));
    }
    out.push('\n');
    out
}

fn markdown_row(cells: &[String]) -> String {
    let parts: Vec<String> = cells.iter().map(|c| escape_cell(c)).collect();
    let mut row = String::from("| ");
    row.push_str(&parts.join(" | "));
    row.push_str(" |");
    row
}

fn separator_row(cols: usize) -> String {
    let mut sep = String::from("|");
    for _ in 0..cols {
        sep.push_str(" --- |");
    }
    sep
}

/// 单元格转义：`|` → `\|`，换行 → 空格。
fn escape_cell(cell: &str) -> String {
    cell.replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// Excel 列号引用 → 0 基列号："A"→0, "B"→1, "Z"→25, "AA"→26；无字母（纯行号）→ 0。
/// 用饱和运算防御超长列名（不可信输入）的溢出。
fn col_index(cell_ref: &str) -> usize {
    let mut idx = 0usize;
    for c in cell_ref.bytes() {
        let c = c.to_ascii_uppercase();
        if c.is_ascii_uppercase() {
            idx = idx
                .saturating_mul(26)
                .saturating_add((c - b'A') as usize + 1);
        } else {
            break;
        }
    }
    idx.saturating_sub(1)
}

/// 读取元素指定属性的文本值。
fn elem_attr(elem: &BytesStart<'_>, key: &[u8]) -> Option<String> {
    elem.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
}

/// pptx → Markdown：每页一个 `## 标题` section，页面备注作 blockquote。
pub fn pptx_to_markdown(bytes: &[u8]) -> Result<String, ExtractError> {
    let mut archive = open_zip(bytes)?;
    let mut out = String::new();
    for (num, part) in slide_parts(&archive) {
        // 部件缺失/损坏：跳过该页，不整体失败。
        let Ok(xml) = read_part(&mut archive, &part) else {
            continue;
        };
        let blocks = parse_slide_blocks(&xml);
        let Some((title, rest)) = blocks.split_first() else {
            continue;
        };
        out.push_str("## ");
        out.push_str(title);
        out.push_str("\n\n");
        for block in rest {
            out.push_str(block);
            out.push_str("\n\n");
        }
        if let Some(notes) = read_notes(&mut archive, num) {
            out.push_str("> 备注：");
            out.push_str(&notes);
            out.push_str("\n\n");
        }
    }
    let result = out.trim().to_string();
    if result.is_empty() {
        return Err(ExtractError::ParseFailed("pptx contains no text".into()));
    }
    Ok(result)
}

/// 枚举 zip 内 `ppt/slides/slideN.xml`（按 N 升序）为 (N, 部件名) 列表。
fn slide_parts<R: Read + Seek>(archive: &zip::ZipArchive<R>) -> Vec<(usize, String)> {
    const PREFIX: &str = "ppt/slides/slide";
    const SUFFIX: &str = ".xml";
    let mut parts: Vec<(usize, String)> = archive
        .file_names()
        .filter(|n| n.starts_with(PREFIX) && n.ends_with(SUFFIX))
        .filter_map(|n| {
            let digits = n.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
            digits.parse::<usize>().ok().map(|num| (num, n.to_string()))
        })
        .collect();
    parts.sort_unstable_by_key(|(num, _)| *num);
    parts
}

/// 解析单页 slide XML：按 `<p:sp>`（shape）分组 `<a:t>` 文本，shape 内段落以
/// 换行分隔；返回非空文本块（首个即标题）。
fn parse_slide_blocks(xml: &str) -> Vec<String> {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);
    let mut blocks = Vec::new();
    let mut in_shape = false;
    let mut in_para = false;
    let mut in_text = false;
    let mut shape_text = String::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"p:sp" => {
                    in_shape = true;
                    in_para = false;
                    in_text = false;
                    shape_text.clear();
                }
                b"a:p" if in_shape => {
                    if !shape_text.is_empty() {
                        shape_text.push('\n');
                    }
                    in_para = true;
                }
                b"a:t" if in_para => in_text = true,
                _ => {}
            },
            Ok(Event::Text(e)) if in_text => {
                if let Ok(t) = e.unescape() {
                    shape_text.push_str(&t);
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"a:t" => in_text = false,
                b"a:p" => in_para = false,
                b"p:sp" => {
                    in_shape = false;
                    in_para = false;
                    in_text = false;
                    let text = shape_text.trim().to_string();
                    if !text.is_empty() {
                        blocks.push(text);
                    }
                    shape_text.clear();
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    blocks
}

/// 对应页码的备注页（`ppt/notesSlides/notesSlideN.xml`，可选）：收集所有 `<a:t>`
/// 文本，无文本返回 None。
fn read_notes<R: Read + Seek>(archive: &mut zip::ZipArchive<R>, num: usize) -> Option<String> {
    let part = format!("ppt/notesSlides/notesSlide{num}.xml");
    let Ok(xml) = read_part(archive, &part) else {
        return None;
    };
    let texts = collect_a_t_texts(&xml);
    if texts.is_empty() {
        return None;
    }
    Some(texts.join("\n"))
}

/// 收集 XML 中所有 `<a:t>` 文本（按文档序，单条去首尾空白、空条丢弃）。
fn collect_a_t_texts(xml: &str) -> Vec<String> {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);
    let mut texts = Vec::new();
    let mut in_text = false;
    let mut cur = String::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == b"a:t" => {
                in_text = true;
                cur.clear();
            }
            Ok(Event::Text(e)) if in_text => {
                if let Ok(t) = e.unescape() {
                    cur.push_str(&t);
                }
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"a:t" => {
                in_text = false;
                let text = cur.trim().to_string();
                if !text.is_empty() {
                    texts.push(text);
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    texts
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

    /// 最小 xlsx：sharedStrings + 单 sheet 两行两列。
    /// 供本模块与 ingest 任务（Task 6）复用。
    pub(crate) fn make_test_xlsx() -> Vec<u8> {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file("[Content_Types].xml", opts).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#).unwrap();
            zip.start_file("xl/sharedStrings.xml", opts).unwrap();
            // 文本含中文，不能写进 br# 字节串（字节串字面量仅限 ASCII），转普通字符串写字节。
            let shared = r#"<?xml version="1.0"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="4" uniqueCount="4">
<si><t>姓名</t></si><si><t>年龄</t></si><si><t>张三</t></si><si><t>30</t></si>
</sst>"#;
            zip.write_all(shared.as_bytes()).unwrap();
            zip.start_file("xl/workbook.xml", opts).unwrap();
            let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="员工表" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
            zip.write_all(workbook.as_bytes()).unwrap();
            zip.start_file("xl/worksheets/sheet1.xml", opts).unwrap();
            let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData>
<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row>
<row r="2"><c r="A2" t="s"><v>2</v></c><c r="B2" t="s"><v>3</v></c></row>
</sheetData></worksheet>"#;
            zip.write_all(sheet.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn xlsx_extracts_markdown_table() {
        let out = xlsx_to_markdown(&make_test_xlsx()).unwrap();
        assert!(out.contains("## 员工表"), "got: {out}");
        assert!(out.contains("| 姓名 | 年龄 |"), "got: {out}");
        assert!(out.contains("| 张三 | 30 |"), "got: {out}");
    }

    /// 无 sharedStrings 的 xlsx：`t="str"`/无 `t` 单元格取字面值，`|` 需转义。
    #[test]
    fn xlsx_literal_values_without_shared_strings() {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::FileOptions::default();
            zip.start_file("[Content_Types].xml", opts).unwrap();
            zip.write_all(b"<Types/>").unwrap();
            zip.start_file("xl/workbook.xml", opts).unwrap();
            zip.write_all(br#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheets><sheet name="literal" sheetId="1"/></sheets></workbook>"#).unwrap();
            zip.start_file("xl/worksheets/sheet1.xml", opts).unwrap();
            // 单元格文本含中文/竖线，不能写进 br# 字节串，转普通字符串写字节。
            let sheet = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData>
<row r="1"><c r="A1" t="str"><v>名称|数量</v></c><c r="B1"><v>42</v></c></row>
</sheetData></worksheet>"#;
            zip.write_all(sheet.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        let out = xlsx_to_markdown(&buf.into_inner()).unwrap();
        assert!(out.contains("| 名称\\|数量 | 42 |"), "got: {out}");
    }

    /// 最小 pptx：两页，页 1 有标题+要点，页 2 有标题+备注。
    /// 供本模块与 ingest 任务（Task 6）复用。
    pub(crate) fn make_test_pptx() -> Vec<u8> {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file("[Content_Types].xml", opts).unwrap();
            zip.write_all(br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#).unwrap();
            let slide1 = r#"<?xml version="1.0"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree>
<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title 1"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr/>
<p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>产品方案</a:t></a:r></a:p></p:txBody>
</p:sp>
<p:sp><p:nvSpPr><p:cNvPr id="3" name="Content Placeholder 2"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr/>
<p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>要点一</a:t></a:r></a:p></p:txBody>
</p:sp>
</p:spTree></p:cSld></p:sld>"#;
            zip.start_file("ppt/slides/slide1.xml", opts).unwrap();
            zip.write_all(slide1.as_bytes()).unwrap();
            let slide2 = r#"<?xml version="1.0"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree>
<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title 1"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr/>
<p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>总结</a:t></a:r></a:p></p:txBody>
</p:sp>
</p:spTree></p:cSld></p:sld>"#;
            zip.start_file("ppt/slides/slide2.xml", opts).unwrap();
            zip.write_all(slide2.as_bytes()).unwrap();
            let notes = r#"<?xml version="1.0"?>
<p:notes xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree>
<p:sp><p:nvSpPr><p:cNvPr id="2" name="Notes Text"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr/>
<p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>强调三点</a:t></a:r></a:p></p:txBody>
</p:sp>
</p:spTree></p:cSld></p:notes>"#;
            zip.start_file("ppt/notesSlides/notesSlide2.xml", opts)
                .unwrap();
            zip.write_all(notes.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn pptx_extracts_sections_with_notes() {
        let out = pptx_to_markdown(&make_test_pptx()).unwrap();
        assert!(out.contains("## 产品方案"), "got: {out}");
        assert!(out.contains("要点一"), "got: {out}");
        assert!(out.contains("## 总结"), "got: {out}");
        assert!(out.contains("备注：强调三点"), "got: {out}");
    }

    #[test]
    fn pptx_empty_slides_parse_failed() {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::FileOptions::default();
            zip.start_file("[Content_Types].xml", opts).unwrap();
            zip.write_all(b"<Types/>").unwrap();
            // 只有一张空幻灯片（无任何 p:sp 文本）。
            let slide = r#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
<p:cSld><p:spTree></p:spTree></p:cSld></p:sld>"#;
            zip.start_file("ppt/slides/slide1.xml", opts).unwrap();
            zip.write_all(slide.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        assert!(matches!(
            pptx_to_markdown(&buf.into_inner()),
            Err(ExtractError::ParseFailed(_))
        ));
    }
}
