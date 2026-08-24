#[cfg(feature = "embed-frontend")]
use axum::{extract::Path, http::StatusCode, response::IntoResponse};

#[cfg(feature = "embed-frontend")]
use rust_embed::RustEmbed;

/// Embedded frontend assets
#[cfg(feature = "embed-frontend")]
#[derive(RustEmbed)]
#[folder = "../../frontend-dist/"]
pub(crate) struct FrontendAssets;

/// `assets/` 下为 Vite 产物（文件名带内容 hash），可永久缓存；
/// `index.html` 及 SPA fallback 不缓存，保证发新版后立即生效。
#[cfg(feature = "embed-frontend")]
fn cache_control_for(path: &str) -> &'static str {
    if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

/// Serve embedded static files for frontend
#[cfg(feature = "embed-frontend")]
pub async fn serve_static(Path(path): Path<String>) -> impl IntoResponse {
    let path = if path.is_empty() { "index.html" } else { &path };

    match FrontendAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            axum::http::Response::builder()
                .header(axum::http::header::CONTENT_TYPE, mime.as_ref())
                .header(axum::http::header::CACHE_CONTROL, cache_control_for(path))
                .body(axum::body::Body::from(content.data))
                .unwrap()
        }
        None => {
            // Fallback to index.html for SPA routing
            if let Some(index) = FrontendAssets::get("index.html") {
                axum::http::Response::builder()
                    .header(axum::http::header::CONTENT_TYPE, "text/html")
                    .header(axum::http::header::CACHE_CONTROL, "no-cache")
                    .body(axum::body::Body::from(index.data))
                    .unwrap()
            } else {
                axum::http::Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(axum::body::Body::from("Not found"))
                    .unwrap()
            }
        }
    }
}
