//! The WSD Print Service: module tree and the HTTP entry point.
//!
//! One handler, [`print_service`], serves every WSD endpoint (the device/metadata endpoint, the
//! print service, and the legacy `/PrintService` alias); the request path decides the
//! [`EndpointKind`](types::EndpointKind) and [`service::WsdService`] routes by action.

pub mod ctx;
pub mod elements;
pub mod envelope;
pub mod eventing;
pub mod events;
pub mod fault;
pub mod inject;
pub mod jobs;
pub mod metadata;
pub mod mtom;
pub mod ops;
mod print_schema;
pub mod printer_model;
pub mod service;
pub mod soap;
pub mod ticket;
pub mod types;
pub mod validate;
pub mod xml;

use std::sync::Arc;

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::sync::RwLock;
use tracing::info;

use crate::state::World;
use crate::wsd::ctx::PrintCtx;
use crate::wsd::fault::PrintFault;
use crate::wsd::inject::InjectQuery;
use crate::wsd::soap::{HttpReply, RequestLimits, SoapRequest};

pub type SharedWorld = Arc<RwLock<World>>;

/// URL paths the WSD handler is mounted on: the device (metadata) endpoint, the hosted print
/// service Windows is told about, and the older `/PrintService` alias the docs and the Python
/// client use. A path ending in `/device` is the device endpoint; the others are the print
/// service.
pub const WSD_PATHS: [&str; 3] = [
    "/WebServices/Device",
    "/WebServices/PrinterService",
    "/PrintService",
];

pub fn http_response(h: HttpReply) -> Response {
    let status = StatusCode::from_u16(h.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, [(header::CONTENT_TYPE, h.content_type)], h.body).into_response()
}

/// HTTP entry point of the WSD endpoints.
pub async fn print_service(
    axum::extract::State(world): axum::extract::State<SharedWorld>,
    req: axum::extract::Request,
) -> Response {
    let (parts, body) = req.into_parts();
    let svc = world.read().await.wsd_service();
    let max_body = svc.ctx.config.limits.max_body;
    let kind = PrintCtx::endpoint_kind_for_path(parts.uri.path());
    let raw = match axum::body::to_bytes(body, max_body).await {
        Ok(b) => b,
        Err(_) => {
            let fault = PrintFault::malformed("the request body is too large or could not be read");
            let mut h = fault.to_reply(None).into_http();
            h.status = 413;
            return http_response(h);
        }
    };
    let header_str = |name: header::HeaderName| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let content_type = header_str(header::CONTENT_TYPE);
    let soap_action = parts
        .headers
        .get("soapaction")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mut limits = RequestLimits {
        max_document: svc.ctx.config.limits.max_document,
        ..RequestLimits::default()
    };
    limits.mtom.max_body = max_body;
    limits.mtom.max_part = max_body;
    let sreq = match SoapRequest::parse_with(
        content_type.as_deref(),
        soap_action.as_deref(),
        &raw,
        &limits,
    ) {
        Ok(r) => r,
        Err(fault) => {
            let reply = fault.to_reply(None);
            svc.ctx.validator.check_reply(None, &reply);
            return http_response(reply.into_http());
        }
    };
    let short = soap::short_action(&sreq.action).to_string();
    world.write().await.note_wsd_action(&short);
    info!(short = %short, action = %sreq.action, bytes = raw.len(), "WSD action");
    crate::reqlog::annotate(|n| {
        n.action = Some(short.clone());
        n.action_uri = Some(sreq.action.clone());
        n.message_id = sreq.envelope.addressing.message_id.clone();
        n.document = matches!(short.as_str(), "SendDocument" | "AddDocument");
        n.job_id = sreq
            .envelope
            .body
            .first()
            .and_then(|b| b.children.iter().find(|c| c.name.local == "JobId"))
            .and_then(|e| e.text_trimmed().parse().ok());
        if matches!(short.as_str(), "CreatePrintJob" | "ValidatePrintTicket") {
            n.ticket_raw = crate::reqlog::extract_raw_ticket(&raw);
        }
    });

    // Test-time injection: decided before dispatch, so a faulted request never reaches the job
    // engine.
    let injection = svc.ctx.inject.take(&InjectQuery {
        endpoint: kind.into(),
        action: Some(&sreq.action),
    });
    if let Some(inj) = &injection {
        crate::reqlog::annotate(|n| n.fault_rule = Some(inj.rule.clone()));
        inj.sleep(false).await;
        if inj.closes() {
            crate::reqlog::annotate(|n| n.dropped = true);
            return aborted_response();
        }
        if let Some(h) = inj.wsd_reply(&sreq, &svc.ctx.validator) {
            inj.sleep(true).await;
            return http_response(h);
        }
    }

    let note = crate::reqlog::current();
    let handled = tokio::task::spawn_blocking(move || {
        crate::reqlog::with_note(note, || svc.handle(kind, &sreq).into_http())
    })
    .await;
    if let Some(inj) = &injection {
        inj.sleep(true).await;
    }
    match handled {
        Ok(h) => http_response(h),
        Err(_) => http_response(
            PrintFault::internal("the handler failed")
                .to_reply(None)
                .into_http(),
        ),
    }
}

/// A response whose body fails before the first byte: hyper then drops the connection
/// without writing a status line (what a printer that resets the socket looks like).
pub fn aborted_response() -> Response {
    let body = axum::body::Body::from_stream(futures_util::stream::once(async {
        Err::<axum::body::Bytes, std::io::Error>(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "fakeprint: injected connection drop",
        ))
    }));
    Response::new(body)
}
