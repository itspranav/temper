//! Internal blob storage endpoint for TemperFS.
//!
//! Provides `PUT/GET /_internal/blobs/{*path}` backed by Turso.
//! The blob_adapter WASM module uploads/downloads through these endpoints
//! when no external blob storage (R2/S3) is configured.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use std::sync::OnceLock;
use std::time::Instant;
use tokio::sync::Semaphore;

use crate::state::ServerState;

const DEFAULT_BLOB_IO_MAX_CONCURRENCY: usize = 32;

fn blob_io_semaphore() -> &'static Semaphore {
    static SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();
    SEMAPHORE.get_or_init(|| {
        let limit = std::env::var("TEMPER_BLOB_IO_MAX_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_BLOB_IO_MAX_CONCURRENCY);
        Semaphore::new(limit)
    })
}

pub(crate) async fn put_blob_bytes(
    store: &temper_store_turso::TursoEventStore,
    key: &str,
    body: &[u8],
) -> Result<(), String> {
    let queued_at = Instant::now();
    let _permit = blob_io_semaphore()
        .acquire()
        .await
        .expect("blob semaphore closed"); // ci-ok: semaphore is process-global and never closed
    let wait_duration = queued_at.elapsed();
    let wait_ms = wait_duration.as_millis() as u64;
    crate::runtime_metrics::record_blob_io_wait_duration(wait_duration, "put");
    if wait_ms > 0 {
        tracing::info!(path = %key, wait_ms, "blob put queued");
    }

    store.put_blob(key, body).await
}

pub(crate) async fn get_blob_bytes(
    store: &temper_store_turso::TursoEventStore,
    key: &str,
) -> Result<Option<Vec<u8>>, String> {
    let queued_at = Instant::now();
    let _permit = blob_io_semaphore()
        .acquire()
        .await
        .expect("blob semaphore closed"); // ci-ok: semaphore is process-global and never closed
    let wait_duration = queued_at.elapsed();
    let wait_ms = wait_duration.as_millis() as u64;
    crate::runtime_metrics::record_blob_io_wait_duration(wait_duration, "get");
    if wait_ms > 0 {
        tracing::info!(path = %key, wait_ms, "blob get queued");
    }

    store.get_blob(key).await
}

/// `PUT /_internal/blobs/{*path}` — store a blob.
pub async fn put_blob(
    State(state): State<ServerState>,
    Path(path): Path<String>,
    body: Bytes,
) -> impl IntoResponse {
    let Some(store) = state.platform_persistent_store().cloned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Blob storage requires Turso".to_string(),
        )
            .into_response();
    };

    match put_blob_bytes(&store, &path, &body).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!(error = %e, path = %path, "blob put failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}

/// `GET /_internal/blobs/{*path}` — retrieve a blob.
pub async fn get_blob(
    State(state): State<ServerState>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    let Some(store) = state.platform_persistent_store().cloned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Blob storage requires Turso".to_string(),
        )
            .into_response();
    };

    match get_blob_bytes(&store, &path).await {
        Ok(Some(data)) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            data,
        )
            .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, path = %path, "blob get failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}
