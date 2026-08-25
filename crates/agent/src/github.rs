//! GitHub REST API 客户端（AI 工作台 GitHub Actions 面板的后端代理层）。
//!
//! 仅封装 HTTP 语义：路径拼接、Bearer 认证头、统一 Accept/X-GitHub-Api-Version、
//! 错误归一化。响应原样透传 `serde_json::Value`，由 API 层（`mgmt/api/agent/
//! github.rs`）决定代理/裁剪给前端。
//!
//! 安全约束：
//! - token 只经 `Authorization: Bearer` 头发送，绝不进 URL / body / 日志；
//! - [`GitHubClient`] 的手写 `Debug` 会把 token 打码为 `<redacted>`；
//! - 所有错误响应与日志消息都不包含 token 明文。

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use thiserror::Error;

/// 生产默认 GitHub REST API 根地址（`base_url` 可注入覆盖，测试指向本地 mock）。
pub const GITHUB_API_BASE: &str = "https://api.github.com";
/// 统一 User-Agent（GitHub 要求必须携带，否则 403）。
const GITHUB_USER_AGENT: &str = "rust-tunnel";
/// GitHub REST API 版本头（2022-11-28 起要求）。
const GITHUB_API_VERSION: &str = "2022-11-28";
/// 作业日志截取上限：只保留尾部 64KB，防超大响应撑爆面板 / 内存。
pub const JOB_LOG_MAX_BYTES: usize = 64 * 1024;

/// GitHub API 调用错误（已按上游状态码归一化；`Display` 不含 token）。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GitHubError {
    /// 上游 4xx/5xx（除已单独分组的 401/403/429）。保留上游 message 供透传。
    #[error("GitHub API error (HTTP {status}): {message}")]
    Api { status: u16, message: String },
    /// 401：token 无效 / 已过期。API 层映射为 400。
    #[error("GitHub token 无效（401），请在 workspace 设置中检查 github_token")]
    Unauthorized,
    /// 403 / 429：限流或权限不足。API 层映射为 429 + 限流提示。
    #[error("GitHub 请求被限流（HTTP {status}）：{message}")]
    RateLimited { status: u16, message: String },
    /// 网络层错误（连接失败 / 超时 / 重定向过多）。API 层映射为 502。
    #[error("GitHub API 网络错误: {0}")]
    Network(String),
}

/// GitHub REST API 客户端。每请求持有一个 token；构造后仅经
/// [`Self::headers`] 注入 Authorization 头，`Debug` 打码 token。
pub struct GitHubClient {
    client: reqwest::Client,
    base_url: String,
    token: String,
}

impl GitHubClient {
    /// 构造客户端。`base_url` 可注入覆盖（测试指向本地 axum mock）；
    /// `token` 为 GitHub fine-grained / classic PAT，仅进 Authorization 头。
    ///
    /// # Panics
    /// reqwest Client 构建失败属构造期 fatal（TLS 后端不可用），无法恢复。
    #[must_use]
    pub fn new(base_url: &str, token: &str) -> Self {
        Self {
            // 构造期 fatal：TLS 后端初始化失败则整个 GitHub 集成不可用，
            // 返回 Self 的签名无法传播错误，保持 panic 语义。
            #[expect(clippy::panic)]
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|e| panic!("reqwest Client builder failed: {e}")),
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    /// 统一请求头：Bearer 认证 + User-Agent + Accept + API 版本。
    fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", self.token)) {
            headers.insert(AUTHORIZATION, v);
        }
        headers.insert(USER_AGENT, HeaderValue::from_static(GITHUB_USER_AGENT));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            reqwest::header::HeaderName::from_static("x-github-api-version"),
            HeaderValue::from_static(GITHUB_API_VERSION),
        );
        headers
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    /// 发送请求并把非 2xx 归一化为 [`GitHubError`]。上游错误 body 优先取
    /// `message` 字段（GitHub 标准错误 JSON），否则用原文。
    async fn send_json(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, GitHubError> {
        let mut builder = self
            .client
            .request(method, self.url(path))
            .headers(self.headers());
        if let Some(b) = body {
            builder = builder.json(&b);
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| GitHubError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            // 204（dispatch/rerun）与 202（cancel）无 body：先取文本，空则返回 Null，
            // 避免 `.json()` 在空 body 上解析失败。
            let text = resp
                .text()
                .await
                .map_err(|e| GitHubError::Network(format!("读取 GitHub 响应失败: {e}")))?;
            if text.trim().is_empty() {
                return Ok(serde_json::Value::Null);
            }
            return serde_json::from_str(&text)
                .map_err(|e| GitHubError::Network(format!("解析 GitHub 响应失败: {e}")));
        }
        let raw = resp.text().await.unwrap_or_default();
        let message = extract_github_message(&raw);
        match status {
            401 => Err(GitHubError::Unauthorized),
            403 | 429 => Err(GitHubError::RateLimited { status, message }),
            _ => Err(GitHubError::Api { status, message }),
        }
    }

    /// GET /repos/{owner}/{repo} — 仓库元信息。
    pub async fn get_repo(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<serde_json::Value, GitHubError> {
        self.send_json(reqwest::Method::GET, &format!("repos/{owner}/{repo}"), None)
            .await
    }

    /// GET /repos/{owner}/{repo}/actions/workflows — 工作流列表。
    pub async fn list_workflows(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<serde_json::Value, GitHubError> {
        self.send_json(
            reqwest::Method::GET,
            &format!("repos/{owner}/{repo}/actions/workflows"),
            None,
        )
        .await
    }

    /// GET /repos/{owner}/{repo}/actions/runs[?workflow_id=&per_page=] — 工作流运行列表。
    pub async fn list_workflow_runs(
        &self,
        owner: &str,
        repo: &str,
        workflow_id: Option<&str>,
        per_page: usize,
    ) -> Result<serde_json::Value, GitHubError> {
        let mut path = format!("repos/{owner}/{repo}/actions/runs");
        let mut qs: Vec<String> = Vec::new();
        if let Some(id) = workflow_id.filter(|s| !s.is_empty()) {
            qs.push(format!("workflow_id={}", urlencode(id)));
        }
        if per_page > 0 {
            qs.push(format!("per_page={per_page}"));
        }
        if !qs.is_empty() {
            path.push('?');
            path.push_str(&qs.join("&"));
        }
        self.send_json(reqwest::Method::GET, &path, None).await
    }

    /// GET /repos/{owner}/{repo}/actions/runs/{run_id}/jobs — 某次运行的作业列表。
    pub async fn list_run_jobs(
        &self,
        owner: &str,
        repo: &str,
        run_id: &str,
    ) -> Result<serde_json::Value, GitHubError> {
        self.send_json(
            reqwest::Method::GET,
            &format!("repos/{owner}/{repo}/actions/runs/{run_id}/jobs"),
            None,
        )
        .await
    }

    /// GET /repos/{owner}/{repo}/actions/jobs/{job_id}/logs — 作业原始日志。
    ///
    /// GitHub 对 `/logs` 返回 302 跳转到签名 URL（objects.githubusercontent.com）；
    /// reqwest 默认跟随重定向（跨主机自动剥离 Authorization 头，token 不泄露给
    /// 签名 CDN）。响应体流式读取，**只保留尾部 [`JOB_LOG_MAX_BYTES`]**，返回
    /// `(logs, truncated)`。
    pub async fn get_job_logs(
        &self,
        owner: &str,
        repo: &str,
        job_id: &str,
    ) -> Result<(String, bool), GitHubError> {
        let path = format!("repos/{owner}/{repo}/actions/jobs/{job_id}/logs");
        let mut resp = self
            .client
            .get(self.url(&path))
            .headers(self.headers())
            .send()
            .await
            .map_err(|e| GitHubError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let raw = resp.text().await.unwrap_or_default();
            let message = extract_github_message(&raw);
            return Err(match status {
                401 => GitHubError::Unauthorized,
                403 | 429 => GitHubError::RateLimited { status, message },
                _ => GitHubError::Api { status, message },
            });
        }
        // 流式读取，滚动保留尾部窗口（每块超出上限就 memmove 收紧一次，摊还 O(1)）
        let mut tail: Vec<u8> = Vec::with_capacity(JOB_LOG_MAX_BYTES);
        let mut total = 0usize;
        let mut chunk = resp
            .chunk()
            .await
            .map_err(|e| GitHubError::Network(format!("读取日志失败: {e}")))?;
        while let Some(c) = chunk {
            total += c.len();
            tail.extend_from_slice(&c);
            if tail.len() > JOB_LOG_MAX_BYTES {
                let drop = tail.len() - JOB_LOG_MAX_BYTES;
                tail.copy_within(drop.., 0);
                tail.truncate(JOB_LOG_MAX_BYTES);
            }
            chunk = resp
                .chunk()
                .await
                .map_err(|e| GitHubError::Network(format!("读取日志失败: {e}")))?;
        }
        Ok((
            String::from_utf8_lossy(&tail).into_owned(),
            total > JOB_LOG_MAX_BYTES,
        ))
    }

    /// POST /repos/{owner}/{repo}/actions/workflows/{workflow_id}/dispatches
    /// body: {ref, inputs?} — 手动触发工作流。
    pub async fn dispatch_workflow(
        &self,
        owner: &str,
        repo: &str,
        workflow_id: &str,
        ref_: &str,
        inputs: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, GitHubError> {
        let body = serde_json::json!({
            "ref": ref_,
            "inputs": inputs.unwrap_or_else(|| serde_json::Value::Object(Default::default())),
        });
        self.send_json(
            reqwest::Method::POST,
            &format!("repos/{owner}/{repo}/actions/workflows/{workflow_id}/dispatches"),
            Some(body),
        )
        .await
    }

    /// POST /repos/{owner}/{repo}/actions/runs/{run_id}/rerun — 重跑。
    pub async fn rerun_workflow(
        &self,
        owner: &str,
        repo: &str,
        run_id: &str,
    ) -> Result<serde_json::Value, GitHubError> {
        self.send_json(
            reqwest::Method::POST,
            &format!("repos/{owner}/{repo}/actions/runs/{run_id}/rerun"),
            None,
        )
        .await
    }

    /// POST /repos/{owner}/{repo}/actions/runs/{run_id}/cancel — 取消运行。
    pub async fn cancel_run(
        &self,
        owner: &str,
        repo: &str,
        run_id: &str,
    ) -> Result<serde_json::Value, GitHubError> {
        self.send_json(
            reqwest::Method::POST,
            &format!("repos/{owner}/{repo}/actions/runs/{run_id}/cancel"),
            None,
        )
        .await
    }
}

/// `Debug` 手写实现：token 打码，防止 `{client:?}` 形式的日志泄露凭据。
impl std::fmt::Debug for GitHubClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubClient")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// 从 GitHub 标准错误 JSON 提取 `message` 字段；非 JSON 时返回原文（trim 后）。
fn extract_github_message(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => v
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| raw.trim().to_string(), str::to_string),
        Err(_) => raw.trim().to_string(),
    }
}

/// 轻量 URL 编码（owner/repo 之外的 query 参数用；GitHub 名字本身是受限字符集，
/// 这里只做防御性编码）。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};

    /// 在随机端口起一个 axum mock server，返回 `http://127.0.0.1:<port>`。
    async fn spawn_mock(routes: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, routes).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn client(base: &str) -> GitHubClient {
        GitHubClient::new(base, "ghp_test_token")
    }

    /// 检查请求头里带了 Bearer token（模拟上游校验 Authorization）。
    async fn assert_bearer(
        req: axum::http::Request<axum::body::Body>,
        expected: &str,
    ) -> axum::http::Request<axum::body::Body> {
        let got = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert_eq!(got, format!("Bearer {expected}"));
        req
    }

    #[tokio::test]
    async fn test_get_repo_success_and_headers() {
        let base = spawn_mock(Router::new().route(
            "/repos/octo/repo",
            get(|req: axum::http::Request<axum::body::Body>| async move {
                let req = assert_bearer(req, "ghp_test_token").await;
                assert_eq!(
                    req.headers()
                        .get("user-agent")
                        .and_then(|v| v.to_str().ok()),
                    Some("rust-tunnel")
                );
                assert_eq!(
                    req.headers()
                        .get("x-github-api-version")
                        .and_then(|v| v.to_str().ok()),
                    Some("2022-11-28")
                );
                axum::Json(serde_json::json!({"full_name": "octo/repo", "default_branch": "main"}))
            }),
        ))
        .await;

        let value = client(&base)
            .get_repo("octo", "repo")
            .await
            .expect("get_repo should succeed");
        assert_eq!(value["full_name"], "octo/repo");
    }

    #[tokio::test]
    async fn test_error_mapping_404_401_429() {
        // 404 → Api
        let base = spawn_mock(Router::new().route(
            "/repos/ghost/repo",
            get(|| async {
                (
                    axum::http::StatusCode::NOT_FOUND,
                    axum::Json(
                        serde_json::json!({"message": "Not Found", "documentation_url": "..."}),
                    ),
                )
            }),
        ))
        .await;
        let err = client(&base)
            .get_repo("ghost", "repo")
            .await
            .expect_err("404 should map to Api error");
        match err {
            GitHubError::Api { status, message } => {
                assert_eq!(status, 404);
                assert_eq!(message, "Not Found");
            }
            other => panic!("expected Api, got {other:?}"),
        }

        // 401 → Unauthorized（映射 400）
        let base = spawn_mock(Router::new().route(
            "/repos/octo/repo",
            get(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({"message": "Bad credentials"})),
                )
            }),
        ))
        .await;
        let err = client(&base)
            .get_repo("octo", "repo")
            .await
            .expect_err("401 should map to Unauthorized");
        assert!(matches!(err, GitHubError::Unauthorized));

        // 429 → RateLimited
        let base = spawn_mock(Router::new().route(
            "/repos/octo/repo",
            get(|| async {
                (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    axum::Json(serde_json::json!({"message": "API rate limit exceeded"})),
                )
            }),
        ))
        .await;
        let err = client(&base)
            .get_repo("octo", "repo")
            .await
            .expect_err("429 should map to RateLimited");
        match err {
            GitHubError::RateLimited { status, message } => {
                assert_eq!(status, 429);
                assert!(message.contains("rate limit"), "message: {message}");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_error_never_contains_token() {
        // 网络错误（指向不存在的端口）也不得把 token 带进 Display。
        let client = GitHubClient::new("http://127.0.0.1:1", "ghp_super_secret");
        let err = client
            .get_repo("octo", "repo")
            .await
            .expect_err("connection refused should be a network error");
        let rendered = err.to_string();
        assert!(
            !rendered.contains("ghp_super_secret"),
            "err must not leak token"
        );
    }

    /// 日志重定向 + 尾部截取：mock 先 302 到 /logs-body，后者返回超长日志，
    /// 断言只保留尾部窗口且 truncated=true；日志短于上限时原样返回。
    #[tokio::test]
    async fn test_get_job_logs_redirect_and_truncate() {
        // 17B/行（16B 内容 + 换行）× 100_000 行 ≈ 1.7MB，远超 64KB 截取上限
        let big_body = "0123456789abcdef\n".repeat(100_000);
        assert!(big_body.len() > JOB_LOG_MAX_BYTES);

        let base = spawn_mock(
            Router::new()
                .route(
                    "/repos/octo/repo/actions/jobs/42/logs",
                    get(|| async {
                        (
                            axum::http::StatusCode::MOVED_PERMANENTLY,
                            [(axum::http::header::LOCATION, "/logs-body")],
                        )
                    }),
                )
                .route(
                    "/logs-body",
                    get(|| async move {
                        axum::response::Response::new(axum::body::Body::from(big_body))
                    }),
                ),
        )
        .await;

        let (logs, truncated) = client(&base)
            .get_job_logs("octo", "repo", "42")
            .await
            .expect("job logs should be fetched through the redirect");
        assert!(truncated);
        assert!(logs.len() <= JOB_LOG_MAX_BYTES + 1);
        assert!(logs.ends_with("abcdef\n"), "尾部窗口应保留日志末尾");

        // 短日志：不截断、原样返回
        let base = spawn_mock(Router::new().route(
            "/repos/octo/repo/actions/jobs/1/logs",
            get(|| async { "short log body\nline2" }),
        ))
        .await;
        let (logs, truncated) = client(&base)
            .get_job_logs("octo", "repo", "1")
            .await
            .unwrap();
        assert!(!truncated);
        assert_eq!(logs, "short log body\nline2");
    }

    #[tokio::test]
    async fn test_dispatch_and_rerun_and_cancel() {
        // dispatch 收到正确 body（ref + inputs）
        let dispatch_seen = std::sync::Arc::new(std::sync::Mutex::new(serde_json::Value::Null));
        let seen = dispatch_seen.clone();
        let base = spawn_mock(Router::new().route(
            "/repos/octo/repo/actions/workflows/ci.yml/dispatches",
            axum::routing::post(move |body: axum::Json<serde_json::Value>| async move {
                *seen.lock().unwrap() = body.0.clone();
                axum::http::StatusCode::NO_CONTENT
            }),
        ))
        .await;
        client(&base)
            .dispatch_workflow(
                "octo",
                "repo",
                "ci.yml",
                "main",
                Some(serde_json::json!({"env": "prod"})),
            )
            .await
            .expect("dispatch should succeed");
        assert_eq!((*dispatch_seen.lock().unwrap())["ref"], "main");
        assert_eq!((*dispatch_seen.lock().unwrap())["inputs"]["env"], "prod");

        // rerun / cancel 命中正确路径
        let rerun_hit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_hit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rerun = rerun_hit.clone();
        let cancel = cancel_hit.clone();
        let base = spawn_mock(
            Router::new()
                .route(
                    "/repos/octo/repo/actions/runs/123/rerun",
                    axum::routing::post(move || async move {
                        rerun.store(true, std::sync::atomic::Ordering::SeqCst);
                        axum::http::StatusCode::NO_CONTENT
                    }),
                )
                .route(
                    "/repos/octo/repo/actions/runs/123/cancel",
                    axum::routing::post(move || async move {
                        cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                        axum::http::StatusCode::ACCEPTED
                    }),
                ),
        )
        .await;
        client(&base)
            .rerun_workflow("octo", "repo", "123")
            .await
            .unwrap();
        client(&base)
            .cancel_run("octo", "repo", "123")
            .await
            .unwrap();
        assert!(rerun_hit.load(std::sync::atomic::Ordering::SeqCst));
        assert!(cancel_hit.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn test_debug_redacts_token() {
        let c = GitHubClient::new("https://api.github.com", "ghp_secret_999");
        let rendered = format!("{c:?}");
        assert!(!rendered.contains("ghp_secret_999"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn test_urlencode() {
        assert_eq!(urlencode("main"), "main");
        assert_eq!(urlencode("feature/x"), "feature%2Fx");
        assert_eq!(urlencode("a b"), "a%20b");
    }
}
