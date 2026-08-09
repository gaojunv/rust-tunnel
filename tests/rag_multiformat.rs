//! 多格式文档摄入的端到端集成测试（RAG 多格式摄入计划 Task 8）。
//!
//! 覆盖 4 个用例：
//!   1. docx 上传 → 201 → 轮询 GET doc → status=ready → chunk_count > 0
//!      （全链路：落盘 → 提取 → 分块 → embedding → 写向量 → 落库）
//!   2. 伪造 pdf（文本内容改 .pdf 名）→ 400（probe 拒绝，非摄入期才失败）
//!   3. 上传 x.doc → 400 + 消息含 "save as .docx"
//!   4. 上传超 20MB pdf → 400 "file too large"
//!
//! 实现取舍：用例 1 的 ready 路径依赖 embedding 服务 —— 这里起一个与
//! `src/server/llm/rag/ingest.rs` / `mgmt/api/rag.rs` 单测同构的 axum mock
//! embedding server（集成测试可直接用库依赖，axum 为常规依赖）。其余三个
//! 400 用例不触发摄入，KB 用不可达的 emb_base_url 即可，无需 mock。
//!
//! 大小上限边界：二进制类（pdf/docx）上限 20MB；multipart 请求体总上限为
//! 20MB + 64KB（DefaultBodyLimit）。因此字面 21MB 的 body 会先被
//! DefaultBodyLimit 以 413 拒绝；用例 4 用 20MB + 1KB 的文件精确命中 handler
//! 的流式超限检查（400 "file too large"，即设计文档「超限 400」的语义）。

// qdrant-edge 门控为可选 `rag` feature（默认关闭），默认 feature 下
// /api/llm/kb* 路由不存在，本测试仅在 `rag` feature 下编译运行。
#![cfg(feature = "rag")]

#[path = "common/mod.rs"]
mod common;

use common::api_client::ApiClient;
use common::{wait_until, HarnessOpts, TestHarness};
use reqwest::StatusCode;
use serde_json::{json, Value};
use std::time::Duration;

/// 起一个返回固定维度向量的本地 embedding server，返回 `base_url`。
/// 与 src 单测的 `mock_embedding_server` 同构（`POST {base}/embeddings`）。
async fn start_mock_embedding(dim: usize) -> String {
    use axum::extract::Json as J;
    use axum::routing::post;
    use axum::Router;
    let app = Router::new().route(
        "/embeddings",
        post(move |body: J<Value>| async move {
            let n = body["input"].as_array().map_or(1, Vec::len);
            let data: Vec<_> = (0..n)
                .map(|i| {
                    json!({
                        "index": i,
                        "embedding": vec![0.1f32; dim],
                        "object": "embedding"
                    })
                })
                .collect();
            J(json!({ "object": "list", "data": data }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock embedding server");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock embedding serve");
    });
    format!("http://{addr}")
}

/// 程序化生成最小 docx（一级标题 + 一段文本）。复制自
/// `src/server/llm/rag/extractor/ooxml.rs::tests::make_test_docx`
/// （`pub(crate)` 测试 helper，tests/ 集成测试不可达）。
fn make_test_docx() -> Vec<u8> {
    use std::io::Write;
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let opts =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
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

/// 字节版 multipart 请求体（二进制 fixture 用）。调用方需同时设置
/// `Content-Type: multipart/form-data; boundary={boundary}`。
fn multipart_body_bytes(boundary: &str, filename: &str, content: &[u8]) -> Vec<u8> {
    let mut v = format!(
        "--{boundary}\r\n\
         Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    v.extend_from_slice(content);
    v.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    v
}

/// 走真实 HTTP 上传单文件字段，返回 (status, 响应原文)。
/// 不使用 `ApiClient`（其方法按 JSON 解析响应，错误体为纯文本需要原文）。
async fn upload_file(
    api_base: &str,
    kb_id: &str,
    filename: &str,
    content: &[u8],
) -> (StatusCode, String) {
    let boundary = format!("rt-boundary-{}", uuid::Uuid::new_v4());
    let body = multipart_body_bytes(&boundary, filename, content);
    let resp = reqwest::Client::new()
        .post(format!("{api_base}/api/llm/kb/{kb_id}/docs"))
        .header(
            reqwest::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .expect("upload request");
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    (status, text)
}

/// 创建指向 `emb_base` 的知识库，返回 `kb_id`。
async fn create_kb(api: &ApiClient, emb_base: &str) -> String {
    let (status, body) = api
        .post_json(
            "/api/llm/kb",
            json!({
                "name": "多格式摄入测试库",
                "description": "integration test",
                "emb_base_url": emb_base,
                "emb_api_key": "sk-test",
                "emb_model": "test-model",
                "emb_dimension": 8,
                "top_k": 5,
                "chunk_size": 512,
                "chunk_overlap": 64,
                "score_threshold": 0.3,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "create kb: {body}");
    body["id"].as_str().expect("kb id").to_string()
}

/// 用例 1：docx 上传 → ready，chunk_count > 0（全链路含 mock embedding）。
#[tokio::test(flavor = "multi_thread")]
async fn docx_upload_reaches_ready() {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let emb = start_mock_embedding(8).await;
        let harness = TestHarness::spawn(HarnessOpts::default()).await;
        let api = harness.api_client();
        let kb_id = create_kb(&api, &emb).await;

        let (status, body) =
            upload_file(&harness.api_base, &kb_id, "guide.docx", &make_test_docx()).await;
        assert_eq!(status, StatusCode::CREATED, "docx upload: {body}");
        let parsed: Value = serde_json::from_str(&body)
            .unwrap_or_else(|_| panic!("upload must return doc JSON, got: {body}"));
        let doc_id = parsed["id"].as_str().expect("doc id").to_string();
        let st = parsed["status"].as_str().unwrap_or("");
        assert!(
            st == "pending" || st == "processing",
            "initial doc status should be pending/processing, got {st}"
        );

        // 轮询 GET doc 直到终态 ready/failed（wait_until：指数退避，非忙等）。
        let base = harness.api_base.clone();
        let kb_id_cl = kb_id.clone();
        let (final_status, chunk_count) = wait_until("docx doc final status", move || {
            let api = ApiClient::new(base.clone());
            let kb_id = kb_id_cl.clone();
            let doc_id = doc_id.clone();
            async move {
                let (code, body) = api
                    .get_json(&format!("/api/llm/kb/{kb_id}/docs/{doc_id}"))
                    .await;
                if !code.is_success() {
                    return None;
                }
                let status = body["status"].as_str().unwrap_or("");
                if status == "ready" || status == "failed" {
                    Some((
                        status.to_string(),
                        body["chunk_count"].as_i64().unwrap_or(0),
                    ))
                } else {
                    None
                }
            }
        })
        .await
        .expect("docx doc reached ready or failed");

        assert_eq!(final_status, "ready", "docx 摄入应成功");
        assert!(chunk_count > 0, "docx 摄入后 chunk_count 应 > 0");

        // 清理：删 KB 以释放向量 shard（EdgeShard Drop 会同步 flush；先删可避免
        // harness 析构时 tempdir 已被移除而 flush 失败触发任务级 panic）。
        let del_status = api.delete_status(&format!("/api/llm/kb/{kb_id}")).await;
        assert_eq!(del_status, StatusCode::OK, "delete kb cleanup");
    })
    .await;
    result.expect("test timed out");
}

/// 用例 2：伪造 pdf（文本内容改 .pdf 名）→ 400，probe 在上传阶段拒绝。
#[tokio::test(flavor = "multi_thread")]
async fn fake_pdf_rejected_by_probe() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let harness = TestHarness::spawn(HarnessOpts::default()).await;
        let api = harness.api_client();
        // 400 用例不触发摄入，emb_base_url 用不可达地址即可。
        let kb_id = create_kb(&api, "http://127.0.0.1:1").await;

        let (status, body) = upload_file(
            &harness.api_base,
            &kb_id,
            "fake.pdf",
            b"hello, this is not a pdf",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "伪造 pdf 应 400: {body}");
        assert!(body.contains("not a PDF file"), "probe 拒绝消息: {body}");
    })
    .await;
    result.expect("test timed out");
}

/// 用例 3：上传 x.doc → 400 + 消息含 "save as .docx"。
#[tokio::test(flavor = "multi_thread")]
async fn legacy_doc_rejected_with_save_as_docx() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let harness = TestHarness::spawn(HarnessOpts::default()).await;
        let api = harness.api_client();
        let kb_id = create_kb(&api, "http://127.0.0.1:1").await;

        let (status, body) =
            upload_file(&harness.api_base, &kb_id, "old.doc", b"whatever bytes").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "x.doc 应 400: {body}");
        assert!(
            body.contains("save as .docx"),
            "消息应提示另存为 .docx: {body}"
        );
    })
    .await;
    result.expect("test timed out");
}

/// 用例 4：上传超 20MB pdf → 400 "file too large"。
///
/// 用 20MB + 1KB（带 %PDF- 头）：超过二进制类上限（20MB）但低于 multipart
/// 请求体总上限（20MB + 64KB），精确命中 handler 流式超限检查。字面 21MB
/// 的 body 会先被 DefaultBodyLimit 以 413 拒绝（外圈护栏，见文件头注释）。
#[tokio::test(flavor = "multi_thread")]
async fn oversized_pdf_rejected() {
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let harness = TestHarness::spawn(HarnessOpts::default()).await;
        let api = harness.api_client();
        let kb_id = create_kb(&api, "http://127.0.0.1:1").await;

        let mut pdf = Vec::with_capacity(20 * 1024 * 1024 + 1024);
        pdf.extend_from_slice(b"%PDF-1.7\n");
        pdf.resize(20 * 1024 * 1024 + 1024, b'x');
        let (status, body) = upload_file(&harness.api_base, &kb_id, "big.pdf", &pdf).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "超 20MB pdf 应 400: {body}"
        );
        assert!(body.contains("file too large"), "超限消息: {body}");
    })
    .await;
    result.expect("test timed out");
}
