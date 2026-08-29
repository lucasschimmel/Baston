//! `GET|HEAD /files/{resource}/{*path}` — bounded resource delivery.
//!
//! Only the generated packfile, manifest-declared client artifacts, and
//! scanned `stream/` assets are reachable. Disk files are streamed in bounded
//! chunks, so a large asset never becomes one gateway allocation.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::stream;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::OwnedSemaphorePermit;

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

/// Reject traversal and platform prefixes before touching the filesystem.
fn sanitize(rel: &str) -> Option<PathBuf> {
    let path = PathBuf::from(rel);
    (!path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_))))
    .then_some(path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ByteRange {
    start: u64,
    end_inclusive: u64,
}

impl ByteRange {
    fn len(self) -> u64 {
        self.end_inclusive - self.start + 1
    }
}

fn parse_range(value: &str, size: u64) -> Result<ByteRange, ()> {
    let spec = value.strip_prefix("bytes=").ok_or(())?;
    if spec.contains(',') {
        return Err(());
    }
    let (start, end) = spec.split_once('-').ok_or(())?;
    if size == 0 {
        return Err(());
    }

    match (start.is_empty(), end.is_empty()) {
        (false, false) => {
            let start = start.parse::<u64>().map_err(|_| ())?;
            let end = end.parse::<u64>().map_err(|_| ())?;
            if start > end || start >= size {
                return Err(());
            }
            Ok(ByteRange {
                start,
                end_inclusive: end.min(size - 1),
            })
        }
        (false, true) => {
            let start = start.parse::<u64>().map_err(|_| ())?;
            if start >= size {
                return Err(());
            }
            Ok(ByteRange {
                start,
                end_inclusive: size - 1,
            })
        }
        (true, false) => {
            let suffix = end.parse::<u64>().map_err(|_| ())?;
            if suffix == 0 {
                return Err(());
            }
            let len = suffix.min(size);
            Ok(ByteRange {
                start: size - len,
                end_inclusive: size - 1,
            })
        }
        (true, true) => Err(()),
    }
}

fn requested_range(headers: &HeaderMap, size: u64) -> Result<Option<ByteRange>, ()> {
    headers
        .get(header::RANGE)
        .map(|value| {
            value
                .to_str()
                .map_err(|_| ())
                .and_then(|v| parse_range(v, size))
        })
        .transpose()
}

fn range_not_satisfiable(size: u64) -> Response {
    metrics::counter!("resource_download_rejections_total", "reason" => "range").increment(1);
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_RANGE, format!("bytes */{size}"))
        .body(Body::empty())
        .expect("static range response is valid")
}

fn response_headers(
    status: StatusCode,
    content_type: &'static str,
    size: u64,
    range: Option<ByteRange>,
    body: Body,
) -> Response {
    let length = range.map_or(size, ByteRange::len);
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, length.to_string());
    if let Some(range) = range {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{size}", range.start, range.end_inclusive),
        );
    }
    builder
        .body(body)
        .expect("static download headers are valid")
}

struct DownloadPermit {
    _permit: OwnedSemaphorePermit,
}

impl DownloadPermit {
    fn new(permit: OwnedSemaphorePermit) -> Self {
        metrics::gauge!("resource_download_active").increment(1.0);
        Self { _permit: permit }
    }
}

impl Drop for DownloadPermit {
    fn drop(&mut self) {
        metrics::gauge!("resource_download_active").decrement(1.0);
    }
}

struct DiskStream {
    file: tokio::fs::File,
    remaining: u64,
    chunk_size: usize,
    timeout: std::time::Duration,
    _permit: DownloadPermit,
}

fn disk_body(state: DiskStream) -> Body {
    let chunks = stream::try_unfold(state, |mut state| async move {
        if state.remaining == 0 {
            return Ok(None);
        }
        let capacity = state
            .remaining
            .min(state.chunk_size as u64)
            .try_into()
            .unwrap_or(state.chunk_size);
        let mut bytes = vec![0; capacity];
        let read = match tokio::time::timeout(state.timeout, state.file.read(&mut bytes)).await {
            Ok(result) => result?,
            Err(_) => {
                metrics::counter!("resource_download_timeouts_total").increment(1);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "resource download read timed out",
                ));
            }
        };
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "resource changed during download",
            ));
        }
        bytes.truncate(read);
        state.remaining -= read as u64;
        metrics::counter!("resource_download_bytes_total").increment(read as u64);
        Ok(Some((Bytes::from(bytes), state)))
    });
    Body::from_stream(chunks)
}

async fn acquire_download(state: &AppState) -> Result<DownloadPermit, Response> {
    match tokio::time::timeout(
        state.downloads.timeout,
        Arc::clone(&state.downloads.semaphore).acquire_owned(),
    )
    .await
    {
        Ok(Ok(permit)) => Ok(DownloadPermit::new(permit)),
        Ok(Err(_)) | Err(_) => {
            metrics::counter!(
                "resource_download_rejections_total",
                "reason" => "concurrency"
            )
            .increment(1);
            Err(StatusCode::SERVICE_UNAVAILABLE.into_response())
        }
    }
}

async fn serve_memory(
    state: &AppState,
    method: &Method,
    headers: &HeaderMap,
    bytes: Arc<Vec<u8>>,
) -> Response {
    let size = bytes.len() as u64;
    let range = match requested_range(headers, size) {
        Ok(range) => range,
        Err(()) => return range_not_satisfiable(size),
    };
    let status = if range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    if method == Method::HEAD {
        return response_headers(
            status,
            "application/octet-stream",
            size,
            range,
            Body::empty(),
        );
    }

    let permit = match acquire_download(state).await {
        Ok(permit) => permit,
        Err(response) => return response,
    };
    struct SharedBytes(Arc<Vec<u8>>);
    impl AsRef<[u8]> for SharedBytes {
        fn as_ref(&self) -> &[u8] {
            self.0.as_slice()
        }
    }
    // `Bytes::from_owner` keeps the cached Arc alive and slices it without a
    // deep clone, including for range responses.
    let all = Bytes::from_owner(SharedBytes(bytes));
    let selected = match range {
        Some(range) => all.slice(range.start as usize..=range.end_inclusive as usize),
        None => all,
    };
    metrics::counter!("resource_download_bytes_total").increment(selected.len() as u64);
    enum MemoryStream {
        Pending {
            bytes: Bytes,
            permit: DownloadPermit,
        },
        Finished {
            _permit: DownloadPermit,
        },
    }
    let body = Body::from_stream(stream::unfold(
        MemoryStream::Pending {
            bytes: selected,
            permit,
        },
        |state| async move {
            match state {
                MemoryStream::Pending { bytes, permit } => Some((
                    Ok::<_, std::convert::Infallible>(bytes),
                    MemoryStream::Finished { _permit: permit },
                )),
                MemoryStream::Finished { .. } => None,
            }
        },
    ));
    response_headers(status, "application/octet-stream", size, range, body)
}

async fn serve_disk(
    state: &AppState,
    method: &Method,
    headers: &HeaderMap,
    path: PathBuf,
    allowed_root: PathBuf,
) -> Response {
    let canonical = match tokio::fs::canonicalize(&path).await {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return StatusCode::NOT_FOUND.into_response()
        }
        Err(error) => {
            tracing::warn!(target: "http", path = %path.display(), %error, "download path rejected");
            return StatusCode::NOT_FOUND.into_response();
        }
    };
    let root = match tokio::fs::canonicalize(&allowed_root).await {
        Ok(root) => root,
        Err(error) => {
            tracing::error!(target: "http", root = %allowed_root.display(), %error, "resource root canonicalization failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if !canonical.starts_with(&root) {
        metrics::counter!(
            "resource_download_rejections_total",
            "reason" => "canonical_escape"
        )
        .increment(1);
        return StatusCode::NOT_FOUND.into_response();
    }

    let metadata = match tokio::fs::metadata(&canonical).await {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return StatusCode::NOT_FOUND.into_response()
        }
        Err(error) => {
            tracing::error!(target: "http", path = %canonical.display(), %error, "file metadata failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let size = metadata.len();
    let range = match requested_range(headers, size) {
        Ok(range) => range,
        Err(()) => return range_not_satisfiable(size),
    };
    let status = if range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let content_type = content_type_for(&canonical);
    if method == Method::HEAD {
        return response_headers(status, content_type, size, range, Body::empty());
    }

    let permit = match acquire_download(state).await {
        Ok(permit) => permit,
        Err(response) => return response,
    };
    let mut file = match tokio::time::timeout(
        state.downloads.timeout,
        tokio::fs::File::open(&canonical),
    )
    .await
    {
        Ok(Ok(file)) => file,
        Ok(Err(error)) => {
            tracing::error!(target: "http", path = %canonical.display(), %error, "file open failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(_) => {
            metrics::counter!("resource_download_timeouts_total").increment(1);
            return StatusCode::REQUEST_TIMEOUT.into_response();
        }
    };
    let start = range.map_or(0, |range| range.start);
    if start != 0 {
        match tokio::time::timeout(
            state.downloads.timeout,
            file.seek(std::io::SeekFrom::Start(start)),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::error!(target: "http", path = %canonical.display(), %error, "file seek failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            Err(_) => {
                metrics::counter!("resource_download_timeouts_total").increment(1);
                return StatusCode::REQUEST_TIMEOUT.into_response();
            }
        }
    }
    let remaining = range.map_or(size, ByteRange::len);
    let body = disk_body(DiskStream {
        file,
        remaining,
        chunk_size: state.downloads.chunk_size,
        timeout: state.downloads.timeout,
        _permit: permit,
    });
    response_headers(status, content_type, size, range, body)
}

pub async fn serve_resource_file(
    State(state): State<Arc<AppState>>,
    method: Method,
    headers: HeaderMap,
    AxumPath((resource, path)): AxumPath<(String, String)>,
) -> Response {
    metrics::counter!(
        "resource_download_requests_total",
        "method" => method.as_str().to_owned()
    )
    .increment(1);

    let (Some(resource_component), Some(relative_path)) = (sanitize(&resource), sanitize(&path))
    else {
        metrics::counter!(
            "resource_download_rejections_total",
            "reason" => "invalid_path"
        )
        .increment(1);
        return StatusCode::BAD_REQUEST.into_response();
    };
    // Resource names are exactly one path component.
    if resource_component.components().count() != 1 {
        return StatusCode::BAD_REQUEST.into_response();
    }

    if path == baston_protocol::connection::DEFAULT_RESOURCE_SET {
        return match state
            .packfiles
            .get(&state.resource_manager, &resource, true)
            .await
        {
            Some(pack) => serve_memory(&state, &method, &headers, Arc::clone(&pack.bytes)).await,
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }

    let Some((resource_root, manifest)) = state.resource_manager.started_resource(&resource).await
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let normalized = relative_path.to_string_lossy().replace('\\', "/");
    let is_client_file = manifest
        .client_scripts
        .iter()
        .chain(manifest.files.iter())
        .any(|allowed| allowed == &normalized);

    let on_disk = if is_client_file {
        Some(resource_root.join(&relative_path))
    } else if relative_path.components().count() == 1 {
        state
            .streams
            .resolve(&state.resource_manager, &resource, &path)
            .await
    } else {
        None
    };
    let Some(on_disk) = on_disk else {
        metrics::counter!(
            "resource_download_rejections_total",
            "reason" => "not_allowlisted"
        )
        .increment(1);
        return StatusCode::NOT_FOUND.into_response();
    };
    serve_disk(&state, &method, &headers, on_disk, resource_root).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_ranges_cover_supported_forms() {
        assert_eq!(
            parse_range("bytes=2-5", 10),
            Ok(ByteRange {
                start: 2,
                end_inclusive: 5
            })
        );
        assert_eq!(
            parse_range("bytes=8-", 10),
            Ok(ByteRange {
                start: 8,
                end_inclusive: 9
            })
        );
        assert_eq!(
            parse_range("bytes=-3", 10),
            Ok(ByteRange {
                start: 7,
                end_inclusive: 9
            })
        );
        assert!(parse_range("bytes=10-", 10).is_err());
        assert!(parse_range("bytes=1-2,4-5", 10).is_err());
        assert!(parse_range("items=1-2", 10).is_err());
    }
}
