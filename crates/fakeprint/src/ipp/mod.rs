//! A strict IPP/2.0 (IPP Everywhere style) test device on `POST /ipp/print`.
//!
//! The device speaks RFC 8010 (encoding, [`codec`]) and RFC 8011 (model, [`service`]) and
//! shares its job engine with the WS-Print side: a job created over IPP is an ordinary engine
//! job, listed by `/v1/jobs`, subject to the same timing presets, fault plan, inbox and
//! history. `docs/ipp.md` is the reference for what is and is not implemented.
//!
//! Layout: [`codec`] (wire format, strict parse), [`caps`] (profile -> IPP values), [`attrs`]
//! (printer and job attributes, `requested-attributes`, job-template validation), [`service`]
//! (the operations), [`validate`] (reply validation switch), [`job`] (what an IPP job keeps).
//! This file is the HTTP face: content-type handling, request injection (including the
//! `ipp_status` effect), counters, validation and the browser-friendly `GET`.
//!
//! The route is mounted on `/ipp/print` (and `/ipp/print/` and `/ipp/print/<job-id>`, which a
//! client may use as the HTTP target of a job operation) for the default device and on
//! `/d/{id}/ipp/print` for the others; the WSD port-monitor probe of the Windows IPP class
//! driver association keeps working unchanged.

pub mod attrs;
pub mod caps;
pub mod codec;
pub mod job;
pub mod service;
pub mod validate;

use std::sync::Arc;
use std::time::SystemTime;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use tokio::sync::RwLock;
use tracing::{info, warn};

pub use service::IppService;

use crate::state::World;
use crate::wsd::inject::{InjectEffect, InjectQuery, RuleEndpoint};
use crate::wsd::validate::ReplyValidator;

/// Per-printer IPP runtime state that outlives a profile swap: the printer's start time (for
/// `printer-up-time` and the job time stamps) and the reply validator.
pub struct IppRuntime {
    /// When the printer "booted" (the context was created), on the engine's clock.
    pub started: SystemTime,
    pub validator: validate::IppValidator,
}

impl IppRuntime {
    pub fn new(started: SystemTime, switch: Arc<ReplyValidator>) -> Self {
        Self {
            started,
            validator: validate::IppValidator::new(switch),
        }
    }
}

/// `GET` -> short JSON (browsers); `POST application/ipp` -> binary IPP.
///
/// Registered on `/ipp/print` always when IPP is on (also needed for CID:MS_IPP WSDMON queue
/// creation, which probes this endpoint before creating `SWD\PRINTENUM`).
pub async fn print_handler(
    State(world): State<Arc<RwLock<World>>>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let svc = world.read().await.ipp_service();

    // A browser (or a probe) with no IPP body gets the JSON summary.
    let is_ipp = method == Method::POST && ct == "application/ipp";
    let op_name = is_ipp.then(|| {
        if body.len() >= 4 {
            codec::op_inject_name(u16::from_be_bytes([body[2], body[3]]))
        } else {
            "malformed".to_string()
        }
    });
    let injection = svc.ctx.inject.take(&InjectQuery {
        endpoint: RuleEndpoint::Ipp,
        action: op_name.as_deref(),
    });
    if let Some(inj) = &injection {
        inj.sleep(false).await;
        if inj.closes() {
            return crate::wsd::aborted_response();
        }
        if let InjectEffect::IppStatus { status, message } = &inj.effect {
            let (version, request_id) = if body.len() >= 8 {
                (
                    (body[0], body[1]),
                    u32::from_be_bytes([body[4], body[5], body[6], body[7]]),
                )
            } else {
                ((2, 0), 1)
            };
            let bytes = svc.status_reply(
                version,
                request_id,
                status
                    .code()
                    .unwrap_or(codec::status::SERVER_INTERNAL_ERROR),
                message.clone(),
            );
            svc.ctx
                .ipp
                .validator
                .check(codec::Message::parse(&body).ok().as_ref(), &bytes);
            inj.sleep(true).await;
            return ipp_response(bytes);
        }
        if let Some(h) = inj.http_reply() {
            inj.sleep(true).await;
            return crate::wsd::http_response(h);
        }
    }

    let response = if body.is_empty() && !ct.contains("ipp") {
        json_summary(&svc)
    } else if !is_ipp {
        (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "IPP needs POST with Content-Type: application/ipp",
        )
            .into_response()
    } else {
        let max = svc.ctx.config.limits.max_body;
        if body.len() > max {
            warn!(bytes = body.len(), max, "IPP request over the body limit");
        }
        let request = codec::Message::parse(&body).ok();
        let svc2 = svc.clone();
        let b = body.clone();
        let outcome = tokio::task::spawn_blocking(move || svc2.handle(&b)).await;
        match outcome {
            Ok(o) => {
                info!(
                    op = %codec::op_name(o.op).unwrap_or("?"),
                    status = %codec::status_name(o.status),
                    bytes = body.len(),
                    "IPP"
                );
                world
                    .write()
                    .await
                    .note_ipp_action(codec::op_counter_name(o.op));
                svc.ctx.ipp.validator.check(request.as_ref(), &o.bytes);
                ipp_response(o.bytes)
            }
            Err(_) => ipp_response(svc.status_reply(
                (2, 0),
                1,
                codec::status::SERVER_INTERNAL_ERROR,
                Some("the handler failed".into()),
            )),
        }
    };
    if let Some(inj) = &injection {
        inj.sleep(true).await;
    }
    response
}

pub async fn print_stub(
    State(world): State<Arc<RwLock<World>>>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    print_handler(State(world), method, headers, body).await
}

fn ipp_response(bytes: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/ipp")],
        bytes,
    )
        .into_response()
}

fn json_summary(svc: &IppService) -> Response {
    (
        StatusCode::OK,
        Json(json!({
            "service": "fakeprint-ipp",
            "path": "/ipp/print",
            "printer_uri": svc.printer_uri,
            "note": "POST application/ipp (see docs/ipp.md)",
        })),
    )
        .into_response()
}
