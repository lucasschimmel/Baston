//! `GET /files/{resource}/{*path}` — static resource file download.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use super::AppState;

fn content_type_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("js") => "application/javascript",
        Some("html") => "text/html",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        _ => "application/octet-stream",
    }
}

/// Reject path traversal: only plain relative components are allowed.
fn sanitize(rel: &str) -> Option<PathBuf> {
    let path = PathBuf::from(rel);
    if path.components().all(|c| matches!(c, Component::Normal(_))) {
        Some(path)
    } else {
        None
    }
}

pub async fn serve_resource_file(
    State(state): State<Arc<AppState>>,
    AxumPath((resource, path)): AxumPath<(String, String)>,
) -> Response {
    let (Some(resource), Some(rel)) = (sanitize(&resource), sanitize(&path)) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let file_path = state
        .resource_manager
        .resources_dir()
        .join(resource)
        .join(rel);

    match tokio::fs::read(&file_path).await {
        Ok(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, content_type_for(&file_path))],
            bytes,
        )
            .into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(target: "http", path = %file_path.display(), error = %e, "file read failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
