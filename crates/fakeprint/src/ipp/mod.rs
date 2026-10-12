//! IPP Everywhere stub (Phase 2 — not required for WSD Add gate).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Reserved path for future IPP Everywhere (`ipp://host:3921/ipp/print`).
pub async fn print_stub() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "ipp_not_implemented",
            "message": "IPP Everywhere is scaffolded for Phase 2; Phase 1 is WSD Print + WSDMON.",
            "see": "docs/roadmap.md",
        })),
    )
        .into_response()
}
