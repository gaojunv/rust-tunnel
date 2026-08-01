//! PDF 文本层提取（lopdf）。扫描件（无文本层）返回 NoTextLayer。

use super::error::ExtractError;

/// 提取 PDF 文本层，按页组织，页间空行分隔。
pub fn pdf_to_markdown(bytes: &[u8]) -> Result<String, ExtractError> {
    let doc = lopdf::Document::load_mem(bytes)
        .map_err(|e| ExtractError::ParseFailed(format!("pdf load: {e}")))?;
    let mut pages_out: Vec<String> = Vec::new();
    // get_pages() 返回 BTreeMap<页码, 页对象 id>；lopdf 0.32 的
    // extract_text 吃页码 `&[u32]`（内部再查页对象 id），故传页码。
    for (page_no, _page_id) in doc.get_pages() {
        let text = doc.extract_text(&[page_no]).unwrap_or_default();
        let text = text.trim();
        if !text.is_empty() {
            pages_out.push(text.to_string());
        }
    }
    let joined = pages_out.join("\n\n");
    if joined.trim().is_empty() {
        return Err(ExtractError::NoTextLayer);
    }
    Ok(joined)
}

/// 程序化生成无文本层（空白页）PDF，供本模块与 ingest 任务（Task 6）复用。
#[cfg(test)]
pub(crate) fn make_empty_page_pdf() -> Vec<u8> {
    use lopdf::{dictionary, Document, Object};
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
        }),
    );
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_text_layer() {
        let bytes = make_test_pdf("Hello rust-tunnel");
        let out = pdf_to_markdown(&bytes).unwrap();
        assert!(out.contains("Hello rust-tunnel"), "got: {out}");
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            pdf_to_markdown(b"not a pdf at all"),
            Err(ExtractError::ParseFailed(_)) | Err(ExtractError::InvalidFormat(_))
        ));
    }

    #[test]
    fn no_text_layer_reports_no_text_layer() {
        // 无 content stream 的 PDF（只有空白页）→ 提取结果为空 → NoTextLayer
        assert!(matches!(
            pdf_to_markdown(&make_empty_page_pdf()),
            Err(ExtractError::NoTextLayer)
        ));
    }

    /// 程序化生成最小单页含文本 PDF（避免 repo 提交二进制 fixture）。
    fn make_test_pdf(text: &str) -> Vec<u8> {
        use lopdf::{dictionary, Document, Object, Stream};
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        });
        let content = format!("BT /F1 24 Tf 100 700 Td ({text}) Tj ET");
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => content_id,
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font_id } },
        });
        let pages = dictionary! {
            "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }
}
