//! 客户端二进制与 wiki 桌面安装包归档的只读列举与下载 API。
//!
//! 数据源是 CI 按 tag 落盘的目录：
//!
//! 客户端归档（`client_dist_dir`）：
//! ```text
//! <client_dist_dir>/
//! ├── v0.8.2/
//! │   ├── rust-tunnel-client-linux-x86_64
//! │   ├── rust-tunnel-client-windows-x86_64.exe
//! │   └── SHA256SUMS
//! └── latest -> v0.8.2
//! ```
//!
//! wiki 桌面归档（`wiki_dist_dir`）：
//! ```text
//! <wiki_dist_dir>/
//! ├── v0.8.2/
//! │   ├── wiki-desktop-macos-aarch64.dmg
//! │   ├── wiki-desktop-macos-x86_64.dmg
//! │   ├── wiki-desktop-windows-x86_64.msi
//! │   ├── wiki-desktop-windows-x86_64-setup.exe
//! │   └── SHA256SUMS
//! └── latest -> v0.8.2
//! ```
//!
//! 服务端只读归档目录，不做写入、不做校验和重算（沿用 CI 生成的 `SHA256SUMS`）。

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::body::Body;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use tower::ServiceExt;
use tower_http::services::ServeFile;

/// CI 生成的校验和清单文件名，本身不作为可下载文件列出。
const CHECKSUM_FILE: &str = "SHA256SUMS";
/// 指向最新版本目录的软链名，不作为独立版本列出。
const LATEST_LINK: &str = "latest";
/// 单个路径段的长度上限，防御异常长的 URL 段。
const MAX_SEGMENT_LEN: usize = 128;

// ── DTO ───────────────────────────────────────────────────────────

/// 归档中的单个平台二进制。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientDownloadFile {
    /// 文件名（同时是下载 URL 的最后一段）。
    pub name: String,
    /// 目标操作系统（`linux` / `macos` / `windows`，无法解析时为 `unknown`）。
    pub os: String,
    /// 目标架构（`x86_64` / `aarch64`，无法解析时为原始后缀）。
    pub arch: String,
    /// 文件扩展名小写（`dmg` / `msi` / `exe` 等），无扩展名时为 `None`。
    pub format: Option<String>,
    /// 文件字节数。
    pub size: u64,
    /// `SHA256SUMS` 中记录的校验和（清单缺失或未收录该文件时为 `None`）。
    pub sha256: Option<String>,
}

/// 一个版本目录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientDownloadVersion {
    /// 版本目录名（即 git tag，如 `v0.8.2`）。
    pub version: String,
    /// 是否为 `latest` 软链的当前指向。
    pub is_latest: bool,
    /// 目录 mtime（Unix 秒，取不到时为 `None`）。
    pub modified_at: Option<u64>,
    /// 该版本下的可下载文件，按文件名升序。
    pub files: Vec<ClientDownloadFile>,
}

/// `GET /api/client-downloads` 响应体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientDownloadsResponse {
    /// 归档目录是否可读（未配置或不存在时为 `false`，前端出空状态而非报错）。
    pub dir_available: bool,
    /// 当前生效的归档目录（便于前端在空状态里提示运维该改哪里）。
    pub configured_dir: Option<String>,
    /// `latest` 软链指向的版本名。
    pub latest: Option<String>,
    /// 全部版本，语义化版本降序（解析不出版本号的排在末尾）。
    pub versions: Vec<ClientDownloadVersion>,
}

/// `GET /api/client-downloads/:version/:file` 的查询参数。
#[derive(Debug, Deserialize)]
pub struct DownloadQuery {
    /// 基于 URL 的认证 token（`<a download>` 无法携带 Header 时使用，与 SSE/WS 同约定）。
    pub token: Option<String>,
}

// ── 纯函数：路径校验与文件名解析 ─────────────────────────────────

/// 判断单个 URL 路径段是否可安全拼进归档目录。
///
/// 只放行 ASCII 字母数字与 `.` `_` `-` `+`，并显式拒绝 `.` / `..`。
/// 由于字符集不含任何路径分隔符，通过此校验的段无法跨出单层目录。
fn is_safe_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= MAX_SEGMENT_LEN
        && segment != "."
        && segment != ".."
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

/// 从 CI 产物名解析 `(os, arch, format)`。
///
/// 形如 `rust-tunnel-client-<os>-<arch>[.exe]` 或 `wiki-desktop-<os>-<arch>[-setup].<ext>`，
/// 按已知扩展名与前缀剥离后，首段为 os、次段为 arch，其余丢弃。
/// 不符合该形态时回退 `("unknown", <剩余部分>, format)`，保证列表不因命名变化而漏文件。
fn platform_from_filename(name: &str) -> (String, String, Option<String>) {
    const KNOWN_EXTS: &[&str] = &["dmg", "msi", "exe", "deb", "AppImage", "rpm", "zip"];
    const KNOWN_PREFIXES: &[&str] = &["rust-tunnel-client-", "wiki-desktop-"];

    let (stem, format) = match name.rsplit_once('.') {
        Some((prefix, suffix))
            if KNOWN_EXTS
                .iter()
                .any(|ext| ext.eq_ignore_ascii_case(suffix)) =>
        {
            (prefix, Some(suffix.to_ascii_lowercase()))
        }
        _ => (name, None),
    };

    let mut rest = stem;
    for prefix in KNOWN_PREFIXES {
        if let Some(stripped) = stem.strip_prefix(prefix) {
            rest = stripped;
            break;
        }
    }

    let mut parts = rest.split('-');
    match (parts.next(), parts.next()) {
        (Some(os), Some(arch)) if !os.is_empty() && !arch.is_empty() => {
            (os.to_string(), arch.to_string(), format)
        }
        _ => ("unknown".to_string(), rest.to_string(), format),
    }
}

/// 解析 `sha256sum` 输出为 `文件名 → 小写十六进制摘要`。
///
/// 兼容 binary 模式输出的 `*filename` 前缀；非 64 位十六进制的行直接丢弃。
fn parse_checksums(content: &str) -> HashMap<String, String> {
    content
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let digest = fields.next()?;
            let name = fields.next()?;
            if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            let name = name.strip_prefix('*').unwrap_or(name);
            Some((name.to_string(), digest.to_ascii_lowercase()))
        })
        .collect()
}

/// 解析 `v1.2.3` / `1.2.3` / `1.2.3-rc1` 为可比较三元组；无法解析返回 `None`。
fn semver_key(version: &str) -> Option<(u64, u64, u64)> {
    let trimmed = version.strip_prefix('v').unwrap_or(version);
    // 预发布 / 构建元数据后缀不参与数值比较
    let core = trimmed.split(['-', '+']).next().unwrap_or(trimmed);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// 版本列表排序：语义化版本降序，解析不出版本号的按名称降序排在末尾。
fn compare_versions(a: &ClientDownloadVersion, b: &ClientDownloadVersion) -> Ordering {
    match (semver_key(&a.version), semver_key(&b.version)) {
        (Some(left), Some(right)) => right.cmp(&left),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => b.version.cmp(&a.version),
    }
}

/// 把 `<base>/<version>/<file>` 解析为规范化的真实路径。
///
/// 两段都过 [`is_safe_segment`]，随后规范化并断言仍落在 `base` 之内 ——
/// 后一步兜住软链指向目录外的情况（正常 CI 布局不会出现）。
fn resolve_download_path(
    base: &Path,
    version: &str,
    file: &str,
    unavailable_msg: &'static str,
) -> Result<PathBuf, (StatusCode, &'static str)> {
    if !is_safe_segment(version) || !is_safe_segment(file) {
        return Err((StatusCode::BAD_REQUEST, "invalid path segment"));
    }
    let base = base
        .canonicalize()
        .map_err(|_| (StatusCode::SERVICE_UNAVAILABLE, unavailable_msg))?;
    let target = base
        .join(version)
        .join(file)
        .canonicalize()
        .map_err(|_| (StatusCode::NOT_FOUND, "not found"))?;
    if !target.starts_with(&base) {
        return Err((
            StatusCode::FORBIDDEN,
            "resolved path escapes the archive directory",
        ));
    }
    if !target.is_file() {
        return Err((StatusCode::NOT_FOUND, "not found"));
    }
    Ok(target)
}

// ── 目录扫描 ──────────────────────────────────────────────────────

/// 取目录/文件 mtime 的 Unix 秒。
fn mtime_secs(meta: &std::fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// 读取单个版本目录；目录不可读或不含任何可下载文件时返回 `None`。
async fn read_version(
    dir: &Path,
    version: &str,
    latest: Option<&str>,
) -> Option<ClientDownloadVersion> {
    let checksums = tokio::fs::read_to_string(dir.join(CHECKSUM_FILE))
        .await
        .map(|content| parse_checksums(&content))
        .unwrap_or_default();

    let mut entries = tokio::fs::read_dir(dir).await.ok()?;
    let mut files = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == CHECKSUM_FILE || !is_safe_segment(&name) {
            continue;
        }
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let (os, arch, format) = platform_from_filename(&name);
        files.push(ClientDownloadFile {
            sha256: checksums.get(&name).cloned(),
            name,
            os,
            arch,
            format,
            size: meta.len(),
        });
    }
    if files.is_empty() {
        return None;
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));

    let modified_at = tokio::fs::metadata(dir)
        .await
        .ok()
        .and_then(|meta| mtime_secs(&meta));

    Some(ClientDownloadVersion {
        version: version.to_string(),
        is_latest: latest == Some(version),
        modified_at,
        files,
    })
}

/// 扫描归档目录，产出 `GET /api/client-downloads` 的响应体。
///
/// 目录不存在或不可读时返回 `dir_available: false` 而非错误 —— 未部署过客户端
/// 是正常状态，前端据此渲染空状态。
pub async fn scan_impl(base: &Path) -> ClientDownloadsResponse {
    let configured_dir = Some(base.display().to_string());
    let unavailable = || ClientDownloadsResponse {
        dir_available: false,
        configured_dir: configured_dir.clone(),
        latest: None,
        versions: Vec::new(),
    };

    // 归档目录本身也可能是软链，规范化后统一用真实路径做后续判定
    let Ok(root) = base.canonicalize() else {
        return unavailable();
    };
    if !root.is_dir() {
        return unavailable();
    }

    // `latest` 是软链，读它的目标名而不是把它当成一个版本
    let latest = tokio::fs::read_link(root.join(LATEST_LINK))
        .await
        .ok()
        .and_then(|target| {
            target
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });

    let Ok(mut entries) = tokio::fs::read_dir(&root).await else {
        return unavailable();
    };
    let mut versions = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == LATEST_LINK || !is_safe_segment(&name) {
            continue;
        }
        // 与下载端点同一把尺子：规范化后必须仍在归档目录内，避免"列得出却下不了"
        let Ok(dir) = root.join(&name).canonicalize() else {
            continue;
        };
        if !dir.is_dir() || !dir.starts_with(&root) {
            continue;
        }
        if let Some(version) = read_version(&dir, &name, latest.as_deref()).await {
            versions.push(version);
        }
    }
    versions.sort_by(compare_versions);

    ClientDownloadsResponse {
        dir_available: true,
        configured_dir,
        latest,
        versions,
    }
}

// ── 内部实现：供 client / wiki 复用 ────────────────────────────────

/// 列举归档的内部实现，`base` 为 `None` 时返回空状态而非报错。
async fn list_impl(base: Option<PathBuf>) -> Response {
    let Some(base) = base else {
        return Json(ClientDownloadsResponse {
            dir_available: false,
            configured_dir: None,
            latest: None,
            versions: Vec::new(),
        })
        .into_response();
    };
    Json(scan_impl(&base).await).into_response()
}

/// 校验下载请求的凭据：优先 `Authorization: Bearer`，回退 `?token=`。
fn download_authorized(
    state: &super::ApiState,
    request: &Request<Body>,
    query_token: Option<&str>,
) -> bool {
    if !state.auth_config.is_enabled() {
        return true;
    }
    let header_token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let token = header_token.or(query_token).unwrap_or_default();
    !token.is_empty() && crate::auth::validate_token(token, &state.auth_config.jwt_secret).is_ok()
}

/// 下载端点的内部实现，`not_configured_msg` / `unavailable_msg` 按产品区分文案。
#[allow(
    clippy::too_many_arguments,
    reason = "下载端点复用：base/version/file/token/request + 两条按产品区分的文案，打包成结构体反而增加装配成本"
)]
async fn download_impl(
    state: super::ApiState,
    base: Option<PathBuf>,
    version: String,
    file: String,
    query_token: Option<String>,
    request: Request<Body>,
    not_configured_msg: &'static str,
    unavailable_msg: &'static str,
) -> Response {
    if !download_authorized(&state, &request, query_token.as_deref()) {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let Some(base) = base else {
        return (StatusCode::SERVICE_UNAVAILABLE, not_configured_msg).into_response();
    };
    let path = match resolve_download_path(&base, &version, &file, unavailable_msg) {
        Ok(path) => path,
        Err(rejection) => return rejection.into_response(),
    };

    // ServeFile 负责 Content-Length / Range / If-Modified-Since，无需自行分片。
    // 其 Error = Infallible：读盘失败体现为 404/500 响应而非 Err，故此处不可能取到 Err。
    let Ok(mut response) = ServeFile::new(&path).oneshot(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    // `file` 已过 is_safe_segment，不含引号与控制字符，可直接内插
    if let Ok(disposition) = HeaderValue::from_str(&format!("attachment; filename=\"{file}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, disposition);
    }
    response.into_response()
}

// ── Axum handlers ─────────────────────────────────────────────────

/// `GET /api/client-downloads`：列出归档中的客户端版本（JWT Header 保护）。
pub async fn list_client_downloads(State(state): State<super::ApiState>) -> Response {
    list_impl(state.server_state.client_dist_dir.clone()).await
}

/// `GET /api/wiki-downloads`：列出归档中的 wiki 桌面版本（JWT Header 保护）。
pub async fn list_wiki_downloads(State(state): State<super::ApiState>) -> Response {
    list_impl(state.server_state.wiki_dist_dir.clone()).await
}

/// `GET /api/client-downloads/:version/:file`：下载指定版本的平台二进制。
///
/// 在公开路由上自带鉴权（`?token=` 或 `Authorization` Header），因为浏览器
/// 原生下载（`<a download>`）无法携带 Header。
pub async fn download_client_binary(
    State(state): State<super::ApiState>,
    UrlPath((version, file)): UrlPath<(String, String)>,
    Query(query): Query<DownloadQuery>,
    request: Request<Body>,
) -> Response {
    let base = state.server_state.client_dist_dir.clone();
    download_impl(
        state,
        base,
        version,
        file,
        query.token,
        request,
        "client_dist_dir is not configured",
        "client archive directory unavailable",
    )
    .await
}

/// `GET /api/wiki-downloads/:version/:file`：下载指定版本的 wiki 桌面安装包。
///
/// 在公开路由上自带鉴权（`?token=` 或 `Authorization` Header），因为浏览器
/// 原生下载（`<a download>`）无法携带 Header。
pub async fn download_wiki_binary(
    State(state): State<super::ApiState>,
    UrlPath((version, file)): UrlPath<(String, String)>,
    Query(query): Query<DownloadQuery>,
    request: Request<Body>,
) -> Response {
    let base = state.server_state.wiki_dist_dir.clone();
    download_impl(
        state,
        base,
        version,
        file,
        query.token,
        request,
        "wiki_dist_dir is not configured",
        "wiki archive directory unavailable",
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个和 CI 落盘同构的归档目录。
    fn make_archive(root: &Path, version: &str, with_checksums: bool) {
        let dir = root.join(version);
        std::fs::create_dir_all(&dir).unwrap();
        let files = [
            "rust-tunnel-client-linux-x86_64",
            "rust-tunnel-client-macos-aarch64",
            "rust-tunnel-client-windows-x86_64.exe",
        ];
        for name in files {
            std::fs::write(dir.join(name), b"binary-payload").unwrap();
        }
        if with_checksums {
            let digest = "a".repeat(64);
            let mut manifest = String::new();
            for name in files {
                manifest.push_str(&digest);
                manifest.push_str("  ");
                manifest.push_str(name);
                manifest.push('\n');
            }
            std::fs::write(dir.join(CHECKSUM_FILE), manifest).unwrap();
        }
    }

    /// 造一个 wiki 归档目录。
    fn make_wiki_archive(root: &Path, version: &str, with_checksums: bool) {
        let dir = root.join(version);
        std::fs::create_dir_all(&dir).unwrap();
        let files = [
            "wiki-desktop-macos-aarch64.dmg",
            "wiki-desktop-macos-x86_64.dmg",
            "wiki-desktop-windows-x86_64.msi",
            "wiki-desktop-windows-x86_64-setup.exe",
        ];
        for name in files {
            std::fs::write(dir.join(name), b"wiki-payload").unwrap();
        }
        if with_checksums {
            let digest = "c".repeat(64);
            let mut manifest = String::new();
            for name in files {
                manifest.push_str(&digest);
                manifest.push_str("  ");
                manifest.push_str(name);
                manifest.push('\n');
            }
            std::fs::write(dir.join(CHECKSUM_FILE), manifest).unwrap();
        }
    }

    #[test]
    fn safe_segment_rejects_traversal() {
        assert!(is_safe_segment("v0.8.2"));
        assert!(is_safe_segment("rust-tunnel-client-windows-x86_64.exe"));
        assert!(!is_safe_segment(".."));
        assert!(!is_safe_segment("."));
        assert!(!is_safe_segment("../etc/passwd"));
        assert!(!is_safe_segment("a/b"));
        assert!(!is_safe_segment("/etc/passwd"));
        assert!(!is_safe_segment("a\\b"));
        assert!(!is_safe_segment(""));
        assert!(!is_safe_segment(&"v".repeat(MAX_SEGMENT_LEN + 1)));
    }

    #[test]
    fn platform_parsed_for_all_ci_targets() {
        assert_eq!(
            platform_from_filename("rust-tunnel-client-linux-x86_64"),
            ("linux".into(), "x86_64".into(), None)
        );
        assert_eq!(
            platform_from_filename("rust-tunnel-client-macos-x86_64"),
            ("macos".into(), "x86_64".into(), None)
        );
        assert_eq!(
            platform_from_filename("rust-tunnel-client-macos-aarch64"),
            ("macos".into(), "aarch64".into(), None)
        );
        assert_eq!(
            platform_from_filename("rust-tunnel-client-windows-x86_64.exe"),
            ("windows".into(), "x86_64".into(), Some("exe".into()))
        );
        // 未知命名不丢文件，退化为 unknown
        assert_eq!(
            platform_from_filename("something-else"),
            ("something".into(), "else".into(), None)
        );
        assert_eq!(
            platform_from_filename("blob"),
            ("unknown".into(), "blob".into(), None)
        );
    }

    #[test]
    fn wiki_platform_parsed() {
        assert_eq!(
            platform_from_filename("wiki-desktop-macos-aarch64.dmg"),
            ("macos".into(), "aarch64".into(), Some("dmg".into()))
        );
        assert_eq!(
            platform_from_filename("wiki-desktop-macos-x86_64.dmg"),
            ("macos".into(), "x86_64".into(), Some("dmg".into()))
        );
        assert_eq!(
            platform_from_filename("wiki-desktop-windows-x86_64.msi"),
            ("windows".into(), "x86_64".into(), Some("msi".into()))
        );
        assert_eq!(
            platform_from_filename("wiki-desktop-windows-x86_64-setup.exe"),
            ("windows".into(), "x86_64".into(), Some("exe".into()))
        );
    }

    #[test]
    fn checksums_parsed_and_bad_lines_dropped() {
        let digest = "b".repeat(64);
        let content = format!(
            "{digest}  rust-tunnel-client-linux-x86_64\n\
             {digest} *rust-tunnel-client-windows-x86_64.exe\n\
             deadbeef  too-short\n\
             \n\
             garbage line without digest\n"
        );
        let map = parse_checksums(&content);
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.get("rust-tunnel-client-linux-x86_64"),
            Some(&digest.clone())
        );
        // binary 模式的 `*` 前缀被剥掉
        assert_eq!(
            map.get("rust-tunnel-client-windows-x86_64.exe"),
            Some(&digest)
        );
        assert!(!map.contains_key("too-short"));
    }

    #[test]
    fn semver_key_parses_and_rejects() {
        assert_eq!(semver_key("v0.8.2"), Some((0, 8, 2)));
        assert_eq!(semver_key("1.2.3"), Some((1, 2, 3)));
        assert_eq!(semver_key("v2.0"), Some((2, 0, 0)));
        assert_eq!(semver_key("v1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(semver_key("nightly"), None);
        assert_eq!(semver_key("1.2.3.4"), None);
    }

    #[test]
    fn versions_sorted_semver_desc_unparsable_last() {
        let mut versions: Vec<ClientDownloadVersion> = ["v0.8.1", "nightly", "v0.10.0", "v0.8.2"]
            .into_iter()
            .map(|version| ClientDownloadVersion {
                version: version.to_string(),
                is_latest: false,
                modified_at: None,
                files: Vec::new(),
            })
            .collect();
        versions.sort_by(compare_versions);
        let names: Vec<&str> = versions.iter().map(|v| v.version.as_str()).collect();
        // 0.10.0 > 0.8.2 —— 字符串序会排错，这里验证走的是数值比较
        assert_eq!(names, ["v0.10.0", "v0.8.2", "v0.8.1", "nightly"]);
    }

    #[tokio::test]
    async fn scan_missing_dir_reports_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let response = scan_impl(&tmp.path().join("no-such-dir")).await;
        assert!(!response.dir_available);
        assert!(response.versions.is_empty());
        assert!(response.latest.is_none());
    }

    #[tokio::test]
    async fn scan_lists_versions_with_latest_and_checksums() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        make_archive(root, "v0.8.1", false);
        make_archive(root, "v0.8.2", true);
        std::os::unix::fs::symlink("v0.8.2", root.join(LATEST_LINK)).unwrap();

        let response = scan_impl(root).await;
        assert!(response.dir_available);
        assert_eq!(response.latest.as_deref(), Some("v0.8.2"));
        // latest 软链不算一个版本
        assert_eq!(response.versions.len(), 2);
        assert_eq!(response.versions[0].version, "v0.8.2");
        assert!(response.versions[0].is_latest);
        assert!(!response.versions[1].is_latest);

        let newest = &response.versions[0];
        // SHA256SUMS 自身不列出
        assert_eq!(newest.files.len(), 3);
        assert!(newest.files.iter().all(|f| f.name != CHECKSUM_FILE));
        assert!(newest.files.iter().all(|f| f.sha256.is_some()));
        assert!(newest.files.iter().all(|f| f.size == 14));
        assert!(newest.modified_at.is_some());

        // 无 SHA256SUMS 的版本仍可列出，只是校验和为空
        assert!(response.versions[1]
            .files
            .iter()
            .all(|f| f.sha256.is_none()));
    }

    #[tokio::test]
    async fn scan_wiki_archive_with_format_and_checksums() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        make_wiki_archive(root, "v0.9.0", true);
        std::os::unix::fs::symlink("v0.9.0", root.join(LATEST_LINK)).unwrap();

        let response = scan_impl(root).await;
        assert!(response.dir_available);
        assert_eq!(response.latest.as_deref(), Some("v0.9.0"));
        assert_eq!(response.versions.len(), 1);
        let version = &response.versions[0];
        assert_eq!(version.files.len(), 4);
        // 按文件名升序，校验 format
        let by_name: std::collections::HashMap<_, _> =
            version.files.iter().map(|f| (f.name.as_str(), f)).collect();
        assert_eq!(
            by_name["wiki-desktop-macos-aarch64.dmg"].format.as_deref(),
            Some("dmg")
        );
        assert_eq!(
            by_name["wiki-desktop-macos-x86_64.dmg"].format.as_deref(),
            Some("dmg")
        );
        assert_eq!(
            by_name["wiki-desktop-windows-x86_64.msi"].format.as_deref(),
            Some("msi")
        );
        assert_eq!(
            by_name["wiki-desktop-windows-x86_64-setup.exe"]
                .format
                .as_deref(),
            Some("exe")
        );
        // 同 os/arch 的 msi 与 exe 能通过 format 区分
        assert_ne!(
            by_name["wiki-desktop-windows-x86_64.msi"].format,
            by_name["wiki-desktop-windows-x86_64-setup.exe"].format
        );
        assert!(version.files.iter().all(|f| f.sha256.is_some()));
        assert!(version.files.iter().all(|f| f.size == 12));
    }

    #[tokio::test]
    async fn scan_skips_stray_files_and_empty_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        make_archive(root, "v1.0.0", true);
        // 顶层散落文件不是版本
        std::fs::write(root.join("README.txt"), b"note").unwrap();
        // 空目录不产生条目
        std::fs::create_dir_all(root.join("v0.0.1")).unwrap();

        let response = scan_impl(root).await;
        let names: Vec<&str> = response
            .versions
            .iter()
            .map(|v| v.version.as_str())
            .collect();
        assert_eq!(names, ["v1.0.0"]);
    }

    #[test]
    fn resolve_rejects_traversal_and_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        make_archive(root, "v1.0.0", true);
        std::fs::write(root.join("outside-secret"), b"nope").unwrap();

        // 正常路径
        let ok = resolve_download_path(
            root,
            "v1.0.0",
            "rust-tunnel-client-linux-x86_64",
            "client archive directory unavailable",
        )
        .unwrap();
        assert!(ok.is_file());

        // 穿越尝试在段校验阶段就被挡住
        for (version, file) in [
            ("..", "outside-secret"),
            ("v1.0.0", ".."),
            ("v1.0.0", "../outside-secret"),
            ("v1.0.0/..", "outside-secret"),
        ] {
            let err =
                resolve_download_path(root, version, file, "client archive directory unavailable")
                    .unwrap_err();
            assert_eq!(err.0, StatusCode::BAD_REQUEST, "{version}/{file}");
        }

        // 合法段但文件不存在
        let err = resolve_download_path(
            root,
            "v1.0.0",
            "nope",
            "client archive directory unavailable",
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);

        // 目录不是文件
        let err = resolve_download_path(
            root,
            "v1.0.0",
            "v1.0.0",
            "client archive directory unavailable",
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn resolve_rejects_symlink_escaping_base() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("archive");
        std::fs::create_dir_all(root.join("v1.0.0")).unwrap();
        let secret = tmp.path().join("secret.txt");
        std::fs::write(&secret, b"top secret").unwrap();
        // 归档目录内的软链指向目录外 —— 段校验过得去，规范化后必须被拦
        std::os::unix::fs::symlink(&secret, root.join("v1.0.0").join("leak")).unwrap();

        let err = resolve_download_path(
            &root,
            "v1.0.0",
            "leak",
            "client archive directory unavailable",
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }
}
