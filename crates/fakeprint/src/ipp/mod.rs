//! Minimal IPP/1.1 + IPP/2.0 for Windows Microsoft IPP Class Driver association.
//!
//! Phase 1 still discovers via WSD; Win11 Settings often finishes the spooler
//! queue through the WSD Port **IPP** adapter (Brother lab: Install Protocol=1).
//! This module answers Get-Printer-Attributes (and Validate-Job / Print-Job)
//! on `POST /ipp/print` with `application/ipp`.
//!
//! The handler is always registered when WSD is on (`CID:MS_IPP` in
//! `generic-wsd.json`). WSDMON probes this endpoint before creating
//! `SWD\PRINTENUM\WSD-{uuid}` with `prnms012.inf`.
//!
//! Print-Job stores the document to the inbox directory (same as WSD
//! SendDocument) so `GET /v1/jobs` reflects IPP prints alongside WSD prints.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::state::World;

const IPP_VERSION_MAJOR: u8 = 2;
const IPP_VERSION_MINOR: u8 = 0;

const TAG_OPERATION_ATTRIBUTES: u8 = 0x01;
const TAG_JOB_ATTRIBUTES: u8 = 0x02;
const TAG_END: u8 = 0x03;
const TAG_PRINTER_ATTRIBUTES: u8 = 0x04;

const TAG_CHARSET: u8 = 0x47;
const TAG_NATURAL_LANGUAGE: u8 = 0x48;
const TAG_URI: u8 = 0x45;
const TAG_KEYWORD: u8 = 0x44;
const TAG_TEXT_WITHOUT_LANG: u8 = 0x41;
const TAG_NAME_WITHOUT_LANG: u8 = 0x42;
const TAG_ENUM: u8 = 0x23;
const TAG_BOOLEAN: u8 = 0x22;
const TAG_INTEGER: u8 = 0x21;
const TAG_MIME: u8 = 0x49;

const OP_PRINT_JOB: u16 = 0x0002;
const OP_VALIDATE_JOB: u16 = 0x0004;
const OP_CREATE_JOB: u16 = 0x0005;
const OP_SEND_DOCUMENT: u16 = 0x0006;
const OP_CANCEL_JOB: u16 = 0x0008;
const OP_GET_JOB_ATTRIBUTES: u16 = 0x0009;
const OP_GET_JOBS: u16 = 0x000a;
const OP_GET_PRINTER_ATTRIBUTES: u16 = 0x000b;

const STATUS_OK: u16 = 0x0000;
const STATUS_SERVER_ERROR_OPERATION_NOT_SUPPORTED: u16 = 0x0501;

/// IPP `uri` scheme tag used for printer-uuid (`urn:uuid:…`).
const TAG_URI_SCHEME: u8 = 0x45;

/// GET → short JSON (browsers); POST `application/ipp` → binary IPP.
/// Registered on `/ipp/print` always when WSD is on (needed for CID:MS_IPP
/// WSDMON queue creation; WSDMON probes this before creating PRINTENUM).
pub async fn print_handler(
    State(world): State<Arc<RwLock<World>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if body.is_empty() && !ct.contains("ipp") {
        let ipp_uri = world.read().await.ipp_uri();
        return (
            StatusCode::OK,
            Json(json!({
                "service": "fakeprint-ipp",
                "path": "/ipp/print",
                "printer_uri": ipp_uri,
                "note": "POST application/ipp for Get-Printer-Attributes",
            })),
        )
            .into_response();
    }

    let (ipp_uri, printer_uuid) = {
        let w = world.read().await;
        // Use ipp_uuid (distinct from WSD device_uuid) so that
        // `Add-Printer -IppURL` does not collide with the existing
        // SWD\DAFWSDProvider WSD device and return "printer already exists".
        (w.ipp_uri(), w.ipp_uuid())
    };

    // Parse op code early so document-bearing ops take the async capture path.
    let op = if body.len() >= 4 {
        u16::from_be_bytes([body[2], body[3]])
    } else {
        0
    };

    if op == OP_PRINT_JOB {
        return handle_print_job_async(world, &body, &ipp_uri).await;
    }
    if op == OP_SEND_DOCUMENT {
        return handle_send_document_async(world, &body, &ipp_uri).await;
    }
    if op == OP_CREATE_JOB {
        return handle_create_job_async(world, &body, &ipp_uri).await;
    }

    match handle_ipp(&body, &ipp_uri, printer_uuid) {
        Ok(resp) => {
            let op_name = match op {
                OP_GET_PRINTER_ATTRIBUTES => "GetPrinterAttributes",
                OP_VALIDATE_JOB => "ValidateJob",
                OP_GET_JOBS => "GetJobs",
                OP_GET_JOB_ATTRIBUTES => "GetJobAttributes",
                OP_CANCEL_JOB => "CancelJob",
                _ => "Other",
            };
            world.write().await.note_ipp_action(op_name);
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/ipp")],
                resp,
            )
                .into_response()
        }
        Err(e) => {
            warn!(error = %e, "IPP request failed");
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "ipp_bad_request", "message": e })),
            )
                .into_response()
        }
    }
}

pub async fn print_stub(
    State(world): State<Arc<RwLock<World>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    print_handler(State(world), headers, body).await
}

/// Async Print-Job handler: parses attributes, captures document to inbox,
/// returns a real job-id so `/v1/jobs` shows IPP prints.
async fn handle_print_job_async(world: Arc<RwLock<World>>, body: &[u8], ipp_uri: &str) -> Response {
    let request_id = if body.len() >= 8 {
        u32::from_be_bytes([body[4], body[5], body[6], body[7]])
    } else {
        0
    };

    let (doc_fmt, doc_start) = extract_print_job_document(body);
    let doc_bytes = &body[doc_start..];

    info!(
        request_id,
        doc_fmt = %doc_fmt,
        doc_bytes = doc_bytes.len(),
        "IPP Print-Job"
    );

    // Create job and capture document (mirrors WSD CreatePrintJob + SendDocument).
    let job_id = {
        let mut w = world.write().await;
        w.note_ipp_action("PrintJob");
        w.create_job("ipp-job", &doc_fmt, "ipp")
    };

    if !doc_bytes.is_empty() {
        let mut w = world.write().await;
        match w.capture_document(job_id, doc_bytes, &doc_fmt, "ipp") {
            Ok(cap) => {
                info!(job_id, bytes = cap.bytes, %doc_fmt, "IPP Print-Job captured to inbox");
            }
            Err(e) => {
                warn!(error = %e, job_id, "IPP Print-Job capture failed");
            }
        }
    } else {
        // Print-Job with no trailing document data (e.g., probe or Validate-Job
        // masquerading as Print-Job). Mark completed without capturing.
        info!(job_id, "IPP Print-Job: no document data in request body");
    }

    {
        let mut w = world.write().await;
        w.end_job(job_id);
    }

    let resp = build_print_job_response(request_id, ipp_uri, job_id);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/ipp")],
        resp,
    )
        .into_response()
}

async fn handle_create_job_async(
    world: Arc<RwLock<World>>,
    body: &[u8],
    ipp_uri: &str,
) -> Response {
    let request_id = if body.len() >= 8 {
        u32::from_be_bytes([body[4], body[5], body[6], body[7]])
    } else {
        0
    };
    let job_id = {
        let mut w = world.write().await;
        w.note_ipp_action("CreateJob");
        w.create_job("ipp-create-job", "application/octet-stream", "ipp")
    };
    info!(request_id, job_id, "IPP Create-Job");
    // job-state 3 = pending (document not yet sent).
    let resp = build_job_response(request_id, ipp_uri, job_id, 3, "job-incoming");
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/ipp")],
        resp,
    )
        .into_response()
}

async fn handle_send_document_async(
    world: Arc<RwLock<World>>,
    body: &[u8],
    ipp_uri: &str,
) -> Response {
    let request_id = if body.len() >= 8 {
        u32::from_be_bytes([body[4], body[5], body[6], body[7]])
    } else {
        0
    };
    let (doc_fmt, doc_start) = extract_print_job_document(body);
    let doc_bytes = &body[doc_start..];
    let job_id = {
        let mut w = world.write().await;
        w.note_ipp_action("SendDocument");
        // Prefer attaching to the newest open job; otherwise create one.
        w.create_job("ipp-send-document", &doc_fmt, "ipp")
    };
    if !doc_bytes.is_empty() {
        let mut w = world.write().await;
        match w.capture_document(job_id, doc_bytes, &doc_fmt, "ipp") {
            Ok(cap) => info!(job_id, bytes = cap.bytes, %doc_fmt, "IPP Send-Document captured"),
            Err(e) => warn!(error = %e, job_id, "IPP Send-Document capture failed"),
        }
    }
    {
        let mut w = world.write().await;
        w.end_job(job_id);
    }
    let resp = build_print_job_response(request_id, ipp_uri, job_id);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/ipp")],
        resp,
    )
        .into_response()
}

/// Parse IPP attribute groups to extract `document-format` and find where the
/// document data starts (immediately after the end-of-attributes tag 0x03).
///
/// Returns `(document_format, byte_offset_of_document_start)`.
/// Offset equals `body.len()` when no end-of-attributes tag is found (no doc).
fn extract_print_job_document(body: &[u8]) -> (String, usize) {
    let mut doc_fmt = "application/octet-stream".to_string();
    if body.len() < 8 {
        return (doc_fmt, body.len());
    }
    let mut i = 8; // skip version(2) + op-id(2) + request-id(4)
    while i < body.len() {
        let tag = body[i];
        i += 1;
        match tag {
            0x03 => {
                // end-of-attributes: document data starts here.
                return (doc_fmt, i);
            }
            0x00..=0x0f => {
                // Group delimiter tag (0x01 operation-attrs, 0x02 job-attrs, etc.)
                // No name/value follows; move to next tag.
                continue;
            }
            _ => {
                // Attribute value: name_len(2) + name + value_len(2) + value.
                if i + 2 > body.len() {
                    break;
                }
                let name_len = u16::from_be_bytes([body[i], body[i + 1]]) as usize;
                i += 2;
                let name_end = i + name_len;
                if name_end > body.len() {
                    break;
                }
                let name = std::str::from_utf8(&body[i..name_end])
                    .unwrap_or("")
                    .to_string();
                i = name_end;
                if i + 2 > body.len() {
                    break;
                }
                let val_len = u16::from_be_bytes([body[i], body[i + 1]]) as usize;
                i += 2;
                let val_end = i + val_len;
                if val_end > body.len() {
                    break;
                }
                if name == "document-format" {
                    if let Ok(s) = std::str::from_utf8(&body[i..val_end]) {
                        doc_fmt = s.to_string();
                    }
                }
                i = val_end;
            }
        }
    }
    debug!(
        body_len = body.len(),
        "IPP Print-Job: end-of-attrs not found; no document"
    );
    (doc_fmt, body.len())
}

fn handle_ipp(body: &[u8], printer_uri: &str, printer_uuid: uuid::Uuid) -> Result<Vec<u8>, String> {
    if body.len() < 8 {
        return Err(format!("IPP body too short ({})", body.len()));
    }
    let version_major = body[0];
    let version_minor = body[1];
    let op = u16::from_be_bytes([body[2], body[3]]);
    let request_id = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
    debug!(
        version = %format!("{version_major}.{version_minor}"),
        op,
        request_id,
        len = body.len(),
        "IPP request"
    );

    match op {
        OP_GET_PRINTER_ATTRIBUTES => {
            info!(request_id, %printer_uri, "IPP Get-Printer-Attributes");
            Ok(build_printer_attributes_response(
                request_id,
                printer_uri,
                printer_uuid,
            ))
        }
        OP_VALIDATE_JOB => {
            info!(request_id, "IPP Validate-Job → successful-ok");
            Ok(build_simple_ok(request_id))
        }
        OP_PRINT_JOB => {
            // Sync fallback (e.g. unit tests); real path goes through handle_print_job_async.
            info!(request_id, "IPP Print-Job (sync stub — no inbox capture)");
            Ok(build_print_job_response(request_id, printer_uri, 1))
        }
        OP_GET_JOBS | OP_GET_JOB_ATTRIBUTES | OP_CANCEL_JOB => {
            info!(request_id, op, "IPP job query/cancel → successful-ok");
            Ok(build_simple_ok(request_id))
        }
        other => {
            warn!(op = other, "IPP operation not supported");
            Ok(build_status(
                request_id,
                STATUS_SERVER_ERROR_OPERATION_NOT_SUPPORTED,
            ))
        }
    }
}

fn build_simple_ok(request_id: u32) -> Vec<u8> {
    let mut w = IppWriter::new(STATUS_OK, request_id);
    w.group(TAG_OPERATION_ATTRIBUTES);
    w.attr_charset("attributes-charset", "utf-8");
    w.attr_natural_language("attributes-natural-language", "en-us");
    w.end();
    w.into_bytes()
}

/// Build a Print-Job successful response with the real (or stub) job id.
fn build_print_job_response(request_id: u32, printer_uri: &str, job_id: u32) -> Vec<u8> {
    build_job_response(
        request_id,
        printer_uri,
        job_id,
        9, // completed
        "job-completed-successfully",
    )
}

fn build_job_response(
    request_id: u32,
    printer_uri: &str,
    job_id: u32,
    job_state: i32,
    reason: &str,
) -> Vec<u8> {
    let job_uri = format!("{printer_uri}/{job_id}");
    let mut w = IppWriter::new(STATUS_OK, request_id);
    w.group(TAG_OPERATION_ATTRIBUTES);
    w.attr_charset("attributes-charset", "utf-8");
    w.attr_natural_language("attributes-natural-language", "en-us");
    w.group(TAG_JOB_ATTRIBUTES);
    w.attr_integer("job-id", job_id as i32);
    w.attr_uri("job-uri", &job_uri);
    w.attr_enum("job-state", job_state);
    w.attr_keyword("job-state-reasons", reason);
    w.end();
    w.into_bytes()
}

fn build_status(request_id: u32, status: u16) -> Vec<u8> {
    let mut w = IppWriter::new(status, request_id);
    w.group(TAG_OPERATION_ATTRIBUTES);
    w.attr_charset("attributes-charset", "utf-8");
    w.attr_natural_language("attributes-natural-language", "en-us");
    w.end();
    w.into_bytes()
}

fn build_printer_attributes_response(
    request_id: u32,
    printer_uri: &str,
    printer_uuid: uuid::Uuid,
) -> Vec<u8> {
    let uuid_urn = format!("urn:uuid:{printer_uuid}");
    // Also advertise the http:// form Windows directed discovery often probes.
    let http_uri = printer_uri.replacen("ipp://", "http://", 1);

    let mut w = IppWriter::new(STATUS_OK, request_id);
    w.group(TAG_OPERATION_ATTRIBUTES);
    w.attr_charset("attributes-charset", "utf-8");
    w.attr_natural_language("attributes-natural-language", "en-us");

    w.group(TAG_PRINTER_ATTRIBUTES);
    w.attr_uri("printer-uri-supported", printer_uri);
    w.attr_uri_more(&http_uri);
    w.attr(TAG_URI_SCHEME, "printer-uuid", uuid_urn.as_bytes());
    w.attr_keyword("uri-security-supported", "none");
    w.attr_keyword("uri-authentication-supported", "none");
    w.attr_name("printer-name", "Fake WSD printer");
    w.attr_text("printer-info", "fakeprint IPP stub");
    w.attr_text("printer-location", "lab");
    w.attr_text("printer-make-and-model", "fakeprint Fake WSD Print");
    w.attr_enum("printer-state", 3); // idle
    w.attr_keyword("printer-state-reasons", "none");
    w.attr_keyword("ipp-versions-supported", "1.1");
    w.attr_keyword_more("2.0");
    w.attr_enum("operations-supported", i32::from(OP_PRINT_JOB));
    w.attr_enum_more(i32::from(OP_VALIDATE_JOB));
    w.attr_enum_more(i32::from(OP_CREATE_JOB));
    w.attr_enum_more(i32::from(OP_SEND_DOCUMENT));
    w.attr_enum_more(i32::from(OP_CANCEL_JOB));
    w.attr_enum_more(i32::from(OP_GET_JOB_ATTRIBUTES));
    w.attr_enum_more(i32::from(OP_GET_PRINTER_ATTRIBUTES));
    w.attr_enum_more(i32::from(OP_GET_JOBS));
    w.attr_charset("charset-configured", "utf-8");
    w.attr_charset("charset-supported", "utf-8");
    w.attr_natural_language("natural-language-configured", "en-us");
    w.attr_natural_language("generated-natural-language-supported", "en-us");
    w.attr_mime("document-format-default", "application/pdf");
    w.attr_mime("document-format-supported", "application/pdf");
    w.attr_mime_more("application/octet-stream");
    w.attr_mime_more("image/pwg-raster");
    w.attr_mime_more("application/postscript");
    w.attr_boolean("color-supported", true);
    w.attr_integer("pages-per-minute", 20);
    w.attr_integer("pages-per-minute-color", 12);
    w.attr_keyword("pdl-override-supported", "attempted");
    w.attr_boolean("printer-is-accepting-jobs", true);
    w.attr_integer("queued-job-count", 0);
    w.attr_keyword("compression-supported", "none");
    w.attr_keyword("media-default", "na_letter_8.5x11in");
    w.attr_keyword("media-supported", "na_letter_8.5x11in");
    w.attr_keyword_more("na_legal_8.5x14in");
    w.attr_keyword_more("iso_a4_210x297mm");
    w.attr_keyword("print-color-mode-default", "color");
    w.attr_keyword("print-color-mode-supported", "color");
    w.attr_keyword_more("monochrome");
    w.attr_keyword("sides-default", "one-sided");
    w.attr_keyword("sides-supported", "one-sided");
    w.attr_keyword("mopria-certified", "1.3");
    w.end();
    w.into_bytes()
}

struct IppWriter {
    buf: Vec<u8>,
}

impl IppWriter {
    fn new(status: u16, request_id: u32) -> Self {
        let mut buf = Vec::with_capacity(512);
        buf.push(IPP_VERSION_MAJOR);
        buf.push(IPP_VERSION_MINOR);
        buf.extend_from_slice(&status.to_be_bytes());
        buf.extend_from_slice(&request_id.to_be_bytes());
        Self { buf }
    }

    fn group(&mut self, tag: u8) {
        self.buf.push(tag);
    }

    fn end(&mut self) {
        self.buf.push(TAG_END);
    }

    fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    fn attr(&mut self, value_tag: u8, name: &str, value: &[u8]) {
        self.buf.push(value_tag);
        let nb = name.as_bytes();
        self.buf.extend_from_slice(&(nb.len() as u16).to_be_bytes());
        self.buf.extend_from_slice(nb);
        self.buf
            .extend_from_slice(&(value.len() as u16).to_be_bytes());
        self.buf.extend_from_slice(value);
    }

    fn attr_more(&mut self, value_tag: u8, value: &[u8]) {
        self.buf.push(value_tag);
        self.buf.extend_from_slice(&0u16.to_be_bytes()); // name length 0
        self.buf
            .extend_from_slice(&(value.len() as u16).to_be_bytes());
        self.buf.extend_from_slice(value);
    }

    fn attr_charset(&mut self, name: &str, v: &str) {
        self.attr(TAG_CHARSET, name, v.as_bytes());
    }
    fn attr_natural_language(&mut self, name: &str, v: &str) {
        self.attr(TAG_NATURAL_LANGUAGE, name, v.as_bytes());
    }
    fn attr_uri(&mut self, name: &str, v: &str) {
        self.attr(TAG_URI, name, v.as_bytes());
    }
    fn attr_uri_more(&mut self, v: &str) {
        self.attr_more(TAG_URI, v.as_bytes());
    }
    fn attr_keyword(&mut self, name: &str, v: &str) {
        self.attr(TAG_KEYWORD, name, v.as_bytes());
    }
    fn attr_keyword_more(&mut self, v: &str) {
        self.attr_more(TAG_KEYWORD, v.as_bytes());
    }
    fn attr_text(&mut self, name: &str, v: &str) {
        self.attr(TAG_TEXT_WITHOUT_LANG, name, v.as_bytes());
    }
    fn attr_name(&mut self, name: &str, v: &str) {
        self.attr(TAG_NAME_WITHOUT_LANG, name, v.as_bytes());
    }
    fn attr_mime(&mut self, name: &str, v: &str) {
        self.attr(TAG_MIME, name, v.as_bytes());
    }
    fn attr_mime_more(&mut self, v: &str) {
        self.attr_more(TAG_MIME, v.as_bytes());
    }
    fn attr_boolean(&mut self, name: &str, v: bool) {
        self.attr(TAG_BOOLEAN, name, &[u8::from(v)]);
    }
    fn attr_integer(&mut self, name: &str, v: i32) {
        self.attr(TAG_INTEGER, name, &v.to_be_bytes());
    }
    fn attr_enum(&mut self, name: &str, v: i32) {
        self.attr(TAG_ENUM, name, &v.to_be_bytes());
    }
    fn attr_enum_more(&mut self, v: i32) {
        self.attr_more(TAG_ENUM, &v.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_printer_attributes_roundtrip_header() {
        // version 2.0, op Get-Printer-Attributes, request-id 7, empty groups + end
        let req = [
            2u8,
            0,
            0x00,
            0x0b,
            0,
            0,
            0,
            7,
            TAG_OPERATION_ATTRIBUTES,
            TAG_END,
        ];
        let uuid = uuid::Uuid::parse_str("00000000-0000-4000-8000-0000000000f3").unwrap();
        let resp = handle_ipp(&req, "ipp://127.0.0.1:3921/ipp/print", uuid).expect("ok");
        assert!(resp.len() > 16);
        assert_eq!(resp[0], 2);
        assert_eq!(resp[1], 0);
        assert_eq!(u16::from_be_bytes([resp[2], resp[3]]), STATUS_OK);
        assert_eq!(u32::from_be_bytes([resp[4], resp[5], resp[6], resp[7]]), 7);
        assert!(resp.contains(&TAG_END));
        let s = String::from_utf8_lossy(&resp);
        assert!(s.contains("printer-name") || resp.windows(12).any(|w| w == b"printer-name"));
        assert!(s.contains("printer-uuid") || resp.windows(12).any(|w| w == b"printer-uuid"));
        assert!(s.contains("urn:uuid:") || resp.windows(9).any(|w| w == b"urn:uuid:"));
    }

    #[test]
    fn unsupported_op_status() {
        let req = [2u8, 0, 0x00, 0xff, 0, 0, 0, 1, TAG_END];
        let uuid = uuid::Uuid::nil();
        let resp = handle_ipp(&req, "ipp://127.0.0.1:3921/ipp/print", uuid).unwrap();
        assert_eq!(
            u16::from_be_bytes([resp[2], resp[3]]),
            STATUS_SERVER_ERROR_OPERATION_NOT_SUPPORTED
        );
    }

    #[test]
    fn short_body_errors() {
        assert!(handle_ipp(
            &[1, 2, 3],
            "ipp://127.0.0.1:3921/ipp/print",
            uuid::Uuid::nil()
        )
        .is_err());
    }

    /// Build a minimal IPP Print-Job request with operation attributes and
    /// a synthetic PDF payload following the end-of-attributes tag.
    fn make_print_job_req(doc_fmt: &str, document: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        // version 2.0, op Print-Job (0x0002), request-id 42
        buf.extend_from_slice(&[2u8, 0, 0x00, 0x02, 0, 0, 0, 42]);
        // operation-attributes group
        buf.push(TAG_OPERATION_ATTRIBUTES);
        // attributes-charset
        let cs = b"utf-8";
        buf.push(TAG_CHARSET);
        buf.extend_from_slice(&(b"attributes-charset".len() as u16).to_be_bytes());
        buf.extend_from_slice(b"attributes-charset");
        buf.extend_from_slice(&(cs.len() as u16).to_be_bytes());
        buf.extend_from_slice(cs);
        // attributes-natural-language
        let nl = b"en-us";
        buf.push(TAG_NATURAL_LANGUAGE);
        buf.extend_from_slice(&(b"attributes-natural-language".len() as u16).to_be_bytes());
        buf.extend_from_slice(b"attributes-natural-language");
        buf.extend_from_slice(&(nl.len() as u16).to_be_bytes());
        buf.extend_from_slice(nl);
        // document-format
        let fmtb = doc_fmt.as_bytes();
        buf.push(TAG_MIME);
        buf.extend_from_slice(&(b"document-format".len() as u16).to_be_bytes());
        buf.extend_from_slice(b"document-format");
        buf.extend_from_slice(&(fmtb.len() as u16).to_be_bytes());
        buf.extend_from_slice(fmtb);
        // end-of-attributes
        buf.push(TAG_END);
        // document data
        buf.extend_from_slice(document);
        buf
    }

    #[test]
    fn extract_print_job_document_with_payload() {
        let doc = b"%PDF-1.4 fake document";
        let req = make_print_job_req("application/pdf", doc);
        let (fmt, offset) = extract_print_job_document(&req);
        assert_eq!(fmt, "application/pdf");
        assert_eq!(&req[offset..], doc);
    }

    #[test]
    fn extract_print_job_document_no_payload() {
        // Print-Job with no trailing document (probe / empty body after end-of-attrs)
        let req = make_print_job_req("application/pdf", &[]);
        let (fmt, offset) = extract_print_job_document(&req);
        assert_eq!(fmt, "application/pdf");
        assert_eq!(offset, req.len()); // no document bytes
    }

    #[test]
    fn extract_print_job_document_short_body() {
        // Too short to contain IPP header; should not panic.
        let (fmt, offset) = extract_print_job_document(&[1, 2, 3]);
        assert_eq!(fmt, "application/octet-stream");
        assert_eq!(offset, 3);
    }

    #[test]
    fn build_print_job_response_roundtrip() {
        let resp = build_print_job_response(42, "ipp://127.0.0.1:3921/ipp/print", 7);
        assert!(resp.len() > 8);
        assert_eq!(u16::from_be_bytes([resp[2], resp[3]]), STATUS_OK);
        assert_eq!(u32::from_be_bytes([resp[4], resp[5], resp[6], resp[7]]), 42);
        // job-id value should appear (big-endian int 7)
        let payload = &resp[8..];
        assert!(payload.windows(4).any(|w| w == 7u32.to_be_bytes()));
    }
}
