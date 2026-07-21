#[cfg(feature = "embed-frontend")]
use axum::{extract::Path, http::StatusCode, response::IntoResponse};

#[cfg(feature = "embed-frontend")]
use rust_embed::RustEmbed;

/// Embedded frontend assets
#[cfg(feature = "embed-frontend")]
#[derive(RustEmbed)]
#[folder = "frontend-dist/"]
pub(crate) struct FrontendAssets;

/// Serve embedded static files for frontend
#[cfg(feature = "embed-frontend")]
pub async fn serve_static(Path(path): Path<String>) -> impl IntoResponse {
    let path = if path.is_empty() { "index.html" } else { &path };

    match FrontendAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            axum::http::Response::builder()
                .header(axum::http::header::CONTENT_TYPE, mime.as_ref())
                .body(axum::body::Body::from(content.data))
                .unwrap()
        }
        None => {
            // Fallback to index.html for SPA routing
            if let Some(index) = FrontendAssets::get("index.html") {
                axum::http::Response::builder()
                    .header(axum::http::header::CONTENT_TYPE, "text/html")
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