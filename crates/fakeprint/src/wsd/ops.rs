//! The WS-Print service operations.
//!
//! [`handle`] takes a parsed [`SoapRequest`] and the kind of endpoint it arrived on and returns
//! the [`SoapReply`] (a fault reply when an operation fails). Dispatch is by `wsa:Action`
//! and endpoint: an action this endpoint does not serve gets `wsa:ActionNotSupported`, a
//! `SOAPAction` that disagrees with the `wsa:Action` gets an invalid-header fault.
//!
//! Operations (v1.0): `GetPrinterElements`, `CreatePrintJob`, `SendDocument`, `AddDocument`
//! (optional), `CancelJob`, `GetJobElements`, `GetActiveJobs`, `GetJobHistory`,
//! `SetEventRate`; (v1.2, when enabled) `SetPrinterElements`. The v2.0 operations have no
//! action URI in the package and cannot be dispatched. There is no `ValidatePrintTicket`,
//! `EndJob` or pause/purge: those actions get `ActionNotSupported`.

use super::ctx::PrintCtx;
use super::fault::{FaultKind, PrintFault};
use super::jobs::{reason_tokens, CancelSource, DocumentIn, MAX_JOB_ID};
use super::soap::SoapReply;
use super::soap::SoapRequest;
use super::ticket::{
    self, child, children, is_print, Capabilities, DocumentProcessing, PrintTicket,
};
use super::types::{EndpointKind, NewJob, PrintJob, PrintOp, PrintVersion};
use super::xml::{ns, Element, QName};

/// Whether `op` is one of the print-service operations handled here (as opposed to eventing,
/// metadata or discovery).
pub fn is_print_operation(op: PrintOp) -> bool {
    !matches!(
        op,
        PrintOp::Subscribe
            | PrintOp::Renew
            | PrintOp::GetStatus
            | PrintOp::Unsubscribe
            | PrintOp::TransferGet
            | PrintOp::GetMetadata
            | PrintOp::Probe
            | PrintOp::Resolve
    )
}

/// Resolve the operation a request asks for on `kind`, or the fault to send.
pub fn resolve(
    ctx: &PrintCtx,
    kind: EndpointKind,
    req: &SoapRequest,
) -> Result<PrintOp, PrintFault> {
    let op = PrintOp::from_action_lenient(&req.action)
        .ok_or_else(|| PrintFault::action_not_supported(&req.action))?;
    if !op.allowed_on(kind) || !is_print_operation(op) {
        return Err(PrintFault::action_not_supported(&req.action));
    }
    if !ctx.config.versions.allows(op.version()) {
        return Err(PrintFault::action_not_supported(&req.action));
    }
    Ok(op)
}

/// Handle one print-service request. Always returns a reply (a fault reply on error).
pub fn handle(ctx: &PrintCtx, kind: EndpointKind, req: &SoapRequest) -> SoapReply {
    let reply = match dispatch(ctx, kind, req) {
        Ok(reply) => reply,
        Err(fault) => req.fault_reply(&fault),
    };
    ctx.validator
        .check_reply(PrintOp::from_action_lenient(&req.action), &reply);
    reply
}

fn dispatch(
    ctx: &PrintCtx,
    kind: EndpointKind,
    req: &SoapRequest,
) -> Result<SoapReply, PrintFault> {
    ctx.tick();
    req.check_must_understand(&[])?;
    let op = resolve(ctx, kind, req)?;
    if let Some(sa) = req.soap_action_mismatch() {
        return Err(PrintFault::header_invalid(
            "Action",
            &format!("the SOAPAction header {sa:?} does not match wsa:Action"),
        ));
    }
    let body = expect_body(req, op)?;
    match op {
        PrintOp::GetPrinterElements => get_printer_elements(ctx, req, body),
        PrintOp::CreatePrintJob => create_print_job(ctx, req, body),
        PrintOp::SendDocument => send_document(ctx, req, body, false),
        PrintOp::AddDocument => send_document(ctx, req, body, true),
        PrintOp::CancelJob => cancel_job(ctx, req, body),
        PrintOp::GetJobElements => get_job_elements(ctx, req, body),
        PrintOp::GetActiveJobs => list_jobs(ctx, req, true),
        PrintOp::GetJobHistory => list_jobs(ctx, req, false),
        PrintOp::SetEventRate => set_event_rate(ctx, req, body),
        PrintOp::SetPrinterElements => set_printer_elements(ctx, req, body),
        _ => Err(PrintFault::action_not_supported(&req.action)),
    }
}

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

fn expect_body(req: &SoapRequest, op: PrintOp) -> Result<&Element, PrintFault> {
    let body = req
        .body()
        .ok_or_else(|| PrintFault::invalid_args("the request has an empty body"))?;
    let want = format!("{}Request", op.name());
    let ok_ns = if op.version() == PrintVersion::V12 {
        body.name.ns == ns::PRINT12
    } else {
        is_print(body, &want)
    };
    if body.name.local != want || !ok_ns {
        return Err(PrintFault::invalid_args(format!(
            "the body element must be {want}, found {}",
            body.name.local
        )));
    }
    Ok(body)
}

fn p(local: &str, text: impl Into<String>) -> Element {
    Element::text_el(ns::PRINT, local, text)
}

fn reply(req: &SoapRequest, op: PrintOp, body: Element) -> Result<SoapReply, PrintFault> {
    let action = op
        .response_action()
        .ok_or_else(|| PrintFault::action_not_supported(&req.action))?;
    Ok(req.reply(action, body))
}

/// `JobId` of a request. Not an integer => `InvalidArgs`; outside `1..=2^31-1` =>
/// `ClientErrorJobIdNotFound`.
fn job_id(body: &Element) -> Result<u32, PrintFault> {
    let el = child(body, "JobId").ok_or_else(|| PrintFault::invalid_args("missing JobId"))?;
    let raw = el.text_trimmed();
    let v: i64 = raw
        .parse()
        .map_err(|_| PrintFault::invalid_args("JobId is not an integer"))?;
    if v < 1 || v > i64::from(MAX_JOB_ID) {
        return Err(PrintFault::job_id_not_found(raw));
    }
    u32::try_from(v).map_err(|_| PrintFault::job_id_not_found(raw))
}

/// Names under `RequestedElements`, normalised to the final print namespace.
fn requested_names(ctx: &PrintCtx, body: &Element) -> Result<Vec<QName>, PrintFault> {
    let list = child(body, "RequestedElements")
        .ok_or_else(|| PrintFault::invalid_args("missing RequestedElements"))?;
    let mut out = Vec::new();
    for n in children(list, "Name") {
        if out.len() >= ctx.config.limits.max_requested_names {
            return Err(PrintFault::invalid_args("too many requested elements"));
        }
        let mut q = n
            .text_qname()
            .ok_or_else(|| PrintFault::invalid_args("RequestedElements/Name is not a QName"))?;
        if q.ns == "http://schemas.microsoft.com/windows/2006/01/wdp/print" {
            q.ns = ns::PRINT.to_string();
        }
        out.push(q);
    }
    if out.is_empty() {
        return Err(PrintFault::invalid_args("RequestedElements has no Name"));
    }
    Ok(out)
}

fn element_data(name: &QName, content: Option<Element>) -> Element {
    let mut e = Element::new(ns::PRINT, "ElementData")
        .with_attr_qname("Name", name.clone())
        .with_attr("Valid", if content.is_some() { "true" } else { "false" });
    e.children.extend(content);
    e
}

// ---------------------------------------------------------------------------------------------
// element builders (also used by tests and by the event layer)
// ---------------------------------------------------------------------------------------------

fn reasons_el(job: &PrintJob) -> Element {
    reason_tokens(job)
        .into_iter()
        .fold(Element::new(ns::PRINT, "JobStateReasons"), |e, r| {
            e.with_child(p("JobStateReason", r))
        })
}

/// `wprt:JobStatus` for a job.
pub fn job_status_element(job: &PrintJob) -> Element {
    Element::new(ns::PRINT, "JobStatus")
        .with_child(p("JobId", job.id.to_string()))
        .with_child(p("JobState", job.state.token()))
        .with_child(reasons_el(job))
        .with_child(p("KOctetsProcessed", job.k_octets_processed().to_string()))
        .with_child(p(
            "MediaSheetsCompleted",
            job.media_sheets_completed.to_string(),
        ))
        .with_child(p(
            "NumberOfDocuments",
            job.number_of_documents().to_string(),
        ))
}

/// `wprt:JobSummary` for a job.
pub fn job_summary_element(job: &PrintJob) -> Element {
    Element::new(ns::PRINT, "JobSummary")
        .with_child(p("JobId", job.id.to_string()))
        .with_child(p("JobState", job.state.token()))
        .with_child(reasons_el(job))
        .with_child(p("JobName", job.name.as_str()))
        .with_child(p("JobOriginatingUserName", job.user.as_str()))
        .with_child(p("KOctetsProcessed", job.k_octets_processed().to_string()))
        .with_child(p(
            "MediaSheetsCompleted",
            job.media_sheets_completed.to_string(),
        ))
        .with_child(p(
            "NumberOfDocuments",
            job.number_of_documents().to_string(),
        ))
}

fn documents_element(job: &PrintJob) -> Option<Element> {
    if job.documents.is_empty() {
        return None;
    }
    let mut docs = Element::new(ns::PRINT, "Documents");
    for d in &job.documents {
        let mut desc = Element::new(ns::PRINT, "DocumentDescription")
            .with_child(p("DocumentId", d.id.to_string()))
            .with_child(p("Compression", d.compression.as_str()))
            .with_child(p("Format", d.format.as_str()));
        if let Some(n) = &d.name {
            desc.children.push(p("DocumentName", n.as_str()));
        }
        if let Some(u) = &d.url {
            desc.children.push(p("DocumentUrl", u.as_str()));
        }
        docs.children
            .push(Element::new(ns::PRINT, "Document").with_child(desc));
    }
    Some(docs)
}

// ---------------------------------------------------------------------------------------------
// operations
// ---------------------------------------------------------------------------------------------

fn get_printer_elements(
    ctx: &PrintCtx,
    req: &SoapRequest,
    body: &Element,
) -> Result<SoapReply, PrintFault> {
    let names = requested_names(ctx, body)?;
    let mut out = Element::new(ns::PRINT, "PrinterElements");
    for n in &names {
        out.children.push(element_data(n, ctx.model.element(n)));
    }
    reply(
        req,
        PrintOp::GetPrinterElements,
        Element::new(ns::PRINT, "GetPrinterElementsResponse").with_child(out),
    )
}

fn create_print_job(
    ctx: &PrintCtx,
    req: &SoapRequest,
    body: &Element,
) -> Result<SoapReply, PrintFault> {
    ctx.engine.check_service_faults()?;
    if !ctx.engine.status_accepting() {
        return Err(PrintFault::new(FaultKind::ServerErrorNotAcceptingJobs));
    }
    let tk = child(body, "PrintTicket")
        .ok_or_else(|| PrintFault::invalid_args("missing PrintTicket"))?;
    let ticket = PrintTicket::parse(tk, &ctx.config.limits, &ctx.config.quirks)?;
    let caps = Capabilities::from_model(ctx.model.as_ref());
    let validated = ticket::validate(&ticket, &caps, &ctx.config.conflict_rules)?;
    let eff = validated.effective;
    let (name, user) = match &eff.description {
        Some(d) => (d.name.clone(), d.user.clone()),
        None => ("job".to_string(), String::new()),
    };
    let id = ctx.engine.create_job(
        NewJob {
            name,
            user,
            ticket: Some(eff.to_element()),
        },
        eff.copies(),
    )?;
    let mut resp =
        Element::new(ns::PRINT, "CreatePrintJobResponse").with_child(p("JobId", id.to_string()));
    if ctx.config.quirks.job_token {
        resp.children.push(p("JobToken", format!("job-{id}")));
    }
    reply(req, PrintOp::CreatePrintJob, resp)
}

fn parse_last(body: &Element) -> Result<bool, PrintFault> {
    let el = child(body, "LastDocument")
        .ok_or_else(|| PrintFault::invalid_args("missing LastDocument"))?;
    match el.text_trimmed() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(PrintFault::invalid_args("LastDocument is not a boolean")),
    }
}

struct Described {
    format: String,
    compression: String,
    name: Option<String>,
    url: Option<String>,
}

fn parse_description(ctx: &PrintCtx, body: &Element) -> Result<Described, PrintFault> {
    let max = ctx.config.limits.max_name;
    let dd = match child(body, "DocumentDescription") {
        Some(d) => d,
        None if ctx.config.quirks.lenient_document_description => {
            return Ok(Described {
                format: "application/octet-stream".into(),
                compression: "None".into(),
                name: None,
                url: child(body, "DocumentUrl").map(|u| u.text_trimmed().to_string()),
            })
        }
        None => return Err(PrintFault::invalid_args("missing DocumentDescription")),
    };
    let id = child(dd, "DocumentId")
        .ok_or_else(|| PrintFault::invalid_args("DocumentDescription without DocumentId"))?;
    match id.text_trimmed().parse::<i64>() {
        Ok(v) if v >= 1 => {}
        _ => {
            return Err(PrintFault::invalid_args(
                "DocumentId must be an integer >= 1",
            ))
        }
    }
    let format = child(dd, "Format")
        .map(|e| e.text_trimmed().to_string())
        .filter(|s| !s.is_empty() && s.len() <= 255)
        .ok_or_else(|| PrintFault::invalid_args("DocumentDescription without a valid Format"))?;
    let compression = child(dd, "Compression")
        .map(|e| e.text_trimmed().to_string())
        .filter(|s| !s.is_empty() && s.len() <= 255 && !s.contains(char::is_whitespace))
        .ok_or_else(|| {
            PrintFault::invalid_args("DocumentDescription without a valid Compression")
        })?;
    let name = child(dd, "DocumentName").map(|e| e.text_trimmed().to_string());
    if name.as_ref().is_some_and(|n| n.len() > max) {
        return Err(PrintFault::invalid_args(
            "DocumentName is longer than 255 characters",
        ));
    }
    let url = child(dd, "DocumentUrl").map(|e| e.text_trimmed().to_string());
    Ok(Described {
        format,
        compression,
        name,
        url,
    })
}

/// `SendDocument` (and, with `by_url`, `AddDocument`).
fn send_document(
    ctx: &PrintCtx,
    req: &SoapRequest,
    body: &Element,
    by_url: bool,
) -> Result<SoapReply, PrintFault> {
    let op = if by_url {
        PrintOp::AddDocument
    } else {
        PrintOp::SendDocument
    };
    if by_url && !ctx.config.add_document {
        return Err(PrintFault::new(
            FaultKind::ServerErrorAddDocumentNotSupported,
        ));
    }
    // schema checks first
    let id = job_id(body)?;
    let desc = parse_description(ctx, body)?;
    let last = parse_last(body)?;
    let doc_proc = match child(body, "DocumentProcessing") {
        Some(dp) => Some(DocumentProcessing::parse(dp)?),
        None => None,
    };
    let data: Option<Vec<u8>> = if by_url {
        None
    } else {
        // a body that cannot be decoded is a schema violation: InvalidArgs
        let bytes = req.document_data().map_err(|f| {
            if f.subcode.is_none() {
                PrintFault::invalid_args(f.reason)
            } else {
                f
            }
        })?;
        if bytes.is_empty() {
            return Err(PrintFault::invalid_args("DocumentData is empty"));
        }
        if bytes.len() > ctx.config.limits.max_document {
            return Err(PrintFault::invalid_args(format!(
                "the document of {} bytes exceeds the limit of {} bytes",
                bytes.len(),
                ctx.config.limits.max_document
            )));
        }
        Some(bytes.into_owned())
    };
    let url = if by_url {
        let u = desc
            .url
            .clone()
            .or_else(|| child(body, "DocumentUrl").map(|e| e.text_trimmed().to_string()))
            .filter(|s| !s.is_empty())
            .ok_or_else(|| PrintFault::invalid_args("AddDocument without DocumentUrl"))?;
        Some(u)
    } else {
        None
    };

    // state checks, in the order of docs/reference/faults.md
    ctx.engine.check_service_faults()?;
    ctx.engine
        .precheck_document(id, last, ctx.config.multiple_document_jobs)?;
    let caps = Capabilities::from_model(ctx.model.as_ref());
    if !caps.format_ok(&desc.format) {
        ctx.engine
            .note_rejected_document(id, super::types::JobStateReason::UnsupportedDocumentFormat);
        return Err(PrintFault::format_not_supported(&caps.format_list()));
    }
    if !caps.compression_ok(&desc.compression) {
        ctx.engine
            .note_rejected_document(id, super::types::JobStateReason::UnsupportedCompression);
        return Err(PrintFault::compression_not_supported(
            &caps.compression_list(),
        ));
    }
    if let Some(u) = &url {
        let scheme = u
            .split_once("://")
            .map(|(s, rest)| (s.to_ascii_lowercase(), rest));
        match scheme {
            Some((s, rest)) if s == "http" || s == "https" => {
                if rest.split('/').next().unwrap_or("").is_empty() {
                    return Err(PrintFault::new(FaultKind::ClientErrorDocumentURLNotFound));
                }
            }
            _ => {
                return Err(PrintFault::new(
                    FaultKind::ClientErrorDocumentURLSchemeNotSupported,
                ))
            }
        }
    }
    if let Some(mut dp) = doc_proc {
        let mut subs = Vec::new();
        ticket::validate_document(&mut dp, &caps, &mut subs)?;
    }

    // capture
    let size = data.as_ref().map_or(0, |d| d.len() as u64);
    let mut capture_path = None;
    if let (Some(bytes), Some(inbox)) = (&data, &ctx.inbox) {
        let job = ctx
            .jobs
            .get(id)
            .ok_or_else(|| PrintFault::job_id_not_found(id))?;
        let doc_no = job.number_of_documents().saturating_add(1);
        let extra = super::jobs::capture_meta_json(&job, doc_no, &desc.format, &desc.compression);
        let cap = inbox
            .capture_with(id, doc_no, bytes, &desc.format, "wsd", &extra)
            .map_err(|e| {
                PrintFault::new(FaultKind::ServerErrorTemporaryError)
                    .with_reason(format!("cannot store the document: {e}"))
            })?;
        capture_path = Some(std::path::PathBuf::from(cap.path));
    }
    ctx.engine.commit_document(
        id,
        DocumentIn {
            name: desc.name,
            format: desc.format,
            compression: desc.compression,
            size,
            url,
            capture_path,
            last,
        },
        ctx.config.multiple_document_jobs,
    )?;
    let name = if by_url {
        "AddDocumentResponse"
    } else {
        "SendDocumentResponse"
    };
    reply(req, op, Element::new(ns::PRINT, name))
}

fn cancel_job(ctx: &PrintCtx, req: &SoapRequest, body: &Element) -> Result<SoapReply, PrintFault> {
    let id = job_id(body)?;
    ctx.engine.cancel(id, CancelSource::User)?;
    // a cancelled job never printed: its documents are not "captured output"
    if let Some(inbox) = &ctx.inbox {
        inbox.discard_job(id);
    }
    reply(
        req,
        PrintOp::CancelJob,
        Element::new(ns::PRINT, "CancelJobResponse"),
    )
}

fn get_job_elements(
    ctx: &PrintCtx,
    req: &SoapRequest,
    body: &Element,
) -> Result<SoapReply, PrintFault> {
    let id = job_id(body)?;
    let names = requested_names(ctx, body)?;
    let job = ctx
        .jobs
        .get(id)
        .ok_or_else(|| PrintFault::job_id_not_found(id))?;
    let mut out = Element::new(ns::PRINT, "JobElements");
    for n in &names {
        let content = if n.ns == ns::PRINT {
            match n.local.as_str() {
                "JobStatus" => Some(job_status_element(&job)),
                "PrintTicket" => job.ticket.clone(),
                "Documents" => documents_element(&job),
                _ => None,
            }
        } else {
            None
        };
        out.children.push(element_data(n, content));
    }
    reply(
        req,
        PrintOp::GetJobElements,
        Element::new(ns::PRINT, "GetJobElementsResponse").with_child(out),
    )
}

fn list_jobs(ctx: &PrintCtx, req: &SoapRequest, active: bool) -> Result<SoapReply, PrintFault> {
    let (jobs, container, resp, op) = if active {
        (
            ctx.jobs.active(),
            "ActiveJobs",
            "GetActiveJobsResponse",
            PrintOp::GetActiveJobs,
        )
    } else {
        (
            ctx.jobs.history(),
            "JobHistory",
            "GetJobHistoryResponse",
            PrintOp::GetJobHistory,
        )
    };
    let list = jobs
        .iter()
        .fold(Element::new(ns::PRINT, container), |e, j| {
            e.with_child(job_summary_element(j))
        });
    reply(req, op, Element::new(ns::PRINT, resp).with_child(list))
}

fn set_event_rate(
    ctx: &PrintCtx,
    req: &SoapRequest,
    body: &Element,
) -> Result<SoapReply, PrintFault> {
    let el =
        child(body, "EventRate").ok_or_else(|| PrintFault::invalid_args("missing EventRate"))?;
    let v: i64 = el
        .text_trimmed()
        .parse()
        .map_err(|_| PrintFault::invalid_args("EventRate is not an integer"))?;
    if !(1..=600).contains(&v) {
        return Err(PrintFault::invalid_args(
            "EventRate must be between 1 and 600 seconds",
        ));
    }
    ctx.engine.set_event_rate(u32::try_from(v).unwrap_or(30));
    reply(
        req,
        PrintOp::SetEventRate,
        Element::new(ns::PRINT, "SetEventRateResponse"),
    )
}

/// `SetPrinterElements` (v1.2). Nothing in this printer is settable: a path that names a
/// section the model knows is `ElementReadOnly`, anything else `UnknownElementPath`.
fn set_printer_elements(
    ctx: &PrintCtx,
    req: &SoapRequest,
    body: &Element,
) -> Result<SoapReply, PrintFault> {
    let find12 = |el: &Element, local: &str| -> Option<Element> {
        el.children
            .iter()
            .find(|c| c.name.ns == ns::PRINT12 && c.name.local == local)
            .cloned()
    };
    let to_set = find12(body, "ElementsToSet")
        .ok_or_else(|| PrintFault::invalid_args("missing ElementsToSet"))?;
    let mut results = Element::new(ns::PRINT12, "ElementsToSetResults");
    let mut n = 0usize;
    for e in to_set
        .children
        .iter()
        .filter(|c| c.name.ns == ns::PRINT12 && c.name.local == "ElementToSet")
    {
        n += 1;
        if n > ctx.config.limits.max_requested_names {
            return Err(PrintFault::invalid_args("too many elements to set"));
        }
        let path = find12(e, "ElementPath")
            .map(|p| p.text_trimmed().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| PrintFault::invalid_args("ElementToSet without ElementPath"))?;
        let first = path.trim_start_matches('/').split('/').next().unwrap_or("");
        let local = first.rsplit(':').next().unwrap_or("");
        let known = ctx.model.element(&QName::new(ns::PRINT, local)).is_some();
        let result = if known {
            "ElementReadOnly"
        } else {
            "UnknownElementPath"
        };
        results.children.push(
            Element::new(ns::PRINT12, "ElementToSetResult")
                .with_child(Element::text_el(ns::PRINT12, "ElementPath", path))
                .with_child(Element::text_el(ns::PRINT12, "Result", result)),
        );
    }
    if n == 0 {
        return Err(PrintFault::invalid_args(
            "ElementsToSet has no ElementToSet",
        ));
    }
    reply(
        req,
        PrintOp::SetPrinterElements,
        Element::new(ns::PRINT12, "SetPrinterElementsResponse").with_child(results),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::profile::PrinterProfile;
    use crate::state::Inbox;
    use crate::wsd::ctx::{FaultBody, ManualClock, PrintConfig};
    use crate::wsd::printer_model::ProfilePrinterModel;
    use crate::wsd::types::{PrintEvent, RecordingEventBus};

    const NS: &str = "http://schemas.microsoft.com/windows/2006/08/wdp/print";

    struct Rig {
        ctx: PrintCtx,
        clock: Arc<ManualClock>,
        events: Arc<RecordingEventBus>,
    }

    fn profile() -> PrinterProfile {
        let mut p = PrinterProfile::new("t", "Test printer");
        p.documents.formats = vec!["application/pdf".into(), "application/octet-stream".into()];
        p
    }

    fn rig_with(cfg: PrintConfig) -> Rig {
        let clock = Arc::new(ManualClock::new());
        let ctx = PrintCtx::build(cfg, clock.clone(), |st| {
            Arc::new(ProfilePrinterModel::with_status(profile(), st))
        });
        let events = Arc::new(RecordingEventBus::new());
        ctx.set_event_sink(events.clone());
        Rig { ctx, clock, events }
    }

    fn rig() -> Rig {
        rig_with(PrintConfig::default())
    }

    fn envelope(action: &str, body: &str) -> Vec<u8> {
        format!(
            r#"<?xml version="1.0"?><soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope" xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:wprt="{NS}" xmlns:wprt12="http://schemas.microsoft.com/windows/2012/10/wdp/printV12"><soap:Header><wsa:Action>{action}</wsa:Action><wsa:MessageID>urn:uuid:11111111-1111-4111-8111-111111111111</wsa:MessageID></soap:Header><soap:Body>{body}</soap:Body></soap:Envelope>"#
        )
        .into_bytes()
    }

    fn call_on(r: &Rig, kind: EndpointKind, op: &str, body: &str) -> SoapReply {
        let action = format!("{NS}/{op}");
        let req = SoapRequest::parse(Some("application/soap+xml"), None, &envelope(&action, body))
            .unwrap();
        let reply = handle(&r.ctx, kind, &req);
        assert_eq!(r.ctx.validation_problems(), Vec::<String>::new(), "{op}");
        reply
    }

    fn call(r: &Rig, op: &str, body: &str) -> SoapReply {
        call_on(r, EndpointKind::PrintService, op, body)
    }

    /// Subcode local name of a fault reply, `None` for a non-fault.
    fn fault(r: &SoapReply) -> Option<String> {
        let f = r.body.as_ref().filter(|b| b.is(ns::SOAP12, "Fault"))?;
        let sub = f
            .child(ns::SOAP12, "Code")?
            .child(ns::SOAP12, "Subcode")?
            .child(ns::SOAP12, "Value")?;
        Some(sub.text_qname()?.local)
    }

    fn ok(r: &SoapReply) -> &Element {
        assert_eq!(r.status, 200, "{:?}", r.body);
        r.body.as_ref().unwrap()
    }

    const TICKET: &str = "<wprt:CreatePrintJobRequest><wprt:PrintTicket><wprt:JobDescription><wprt:JobName>A&amp;B</wprt:JobName><wprt:JobOriginatingUserName>alice</wprt:JobOriginatingUserName></wprt:JobDescription></wprt:PrintTicket></wprt:CreatePrintJobRequest>";

    fn create(r: &Rig) -> u32 {
        let rep = call(r, "CreatePrintJob", TICKET);
        ok(&rep)
            .child(ns::PRINT, "JobId")
            .unwrap()
            .text_trimmed()
            .parse()
            .unwrap()
    }

    fn send_xml(job: u32, fmt: &str, comp: &str, last: bool, data_b64: &str) -> String {
        format!("<wprt:SendDocumentRequest><wprt:JobId>{job}</wprt:JobId><wprt:DocumentDescription><wprt:DocumentId>1</wprt:DocumentId><wprt:Compression>{comp}</wprt:Compression><wprt:Format>{fmt}</wprt:Format></wprt:DocumentDescription><wprt:LastDocument>{last}</wprt:LastDocument><wprt:DocumentData>{data_b64}</wprt:DocumentData></wprt:SendDocumentRequest>")
    }

    fn send(r: &Rig, job: u32, last: bool) -> SoapReply {
        call(
            r,
            "SendDocument",
            &send_xml(job, "application/pdf", "None", last, "JVBERi0xLjQ="),
        )
    }

    fn job_status_req(id: u32) -> String {
        format!("<wprt:GetJobElementsRequest><wprt:JobId>{id}</wprt:JobId><wprt:RequestedElements><wprt:Name>wprt:JobStatus</wprt:Name></wprt:RequestedElements></wprt:GetJobElementsRequest>")
    }

    fn job_state(r: &Rig, id: u32) -> String {
        let rep = call(r, "GetJobElements", &job_status_req(id));
        let b = ok(&rep);
        b.descendant(&QName::new(ns::PRINT, "JobState"))
            .unwrap()
            .text_trimmed()
            .to_string()
    }

    fn cancel_req(id: &str) -> String {
        format!("<wprt:CancelJobRequest><wprt:JobId>{id}</wprt:JobId></wprt:CancelJobRequest>")
    }

    #[test]
    fn get_printer_elements_filters_and_marks_unknown_names() {
        let r = rig();
        let rep = call(&r, "GetPrinterElements", "<wprt:GetPrinterElementsRequest xmlns:v=\"urn:vendor\"><wprt:RequestedElements><wprt:Name>wprt:PrinterStatus</wprt:Name><wprt:Name>v:Secret</wprt:Name><wprt:Name>wprt:PrinterCapabilities</wprt:Name><wprt:Name>wprt:Bogus</wprt:Name></wprt:RequestedElements></wprt:GetPrinterElementsRequest>");
        assert_eq!(rep.action, format!("{NS}/GetPrinterElementsResponse"));
        let data: Vec<(String, String)> = ok(&rep)
            .child(ns::PRINT, "PrinterElements")
            .unwrap()
            .children
            .iter()
            .map(|e| {
                (
                    e.attr_qname(&QName::unqualified("Name")).unwrap().local,
                    e.attr_local("Valid").unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            data,
            vec![
                ("PrinterStatus".to_string(), "true".to_string()),
                ("Secret".to_string(), "false".to_string()),
                ("PrinterCapabilities".to_string(), "true".to_string()),
                ("Bogus".to_string(), "false".to_string()),
            ]
        );
        let status = ok(&rep)
            .descendant(&QName::new(ns::PRINT, "PrinterState"))
            .unwrap();
        assert_eq!(status.text_trimmed(), "Idle");
        let xml = rep.to_xml();
        assert!(
            xml.contains("Valid=\"false\"") && xml.contains("PrinterCurrentTime"),
            "{xml}"
        );
    }

    #[test]
    fn get_printer_elements_argument_errors() {
        let r = rig();
        for body in [
            "<wprt:GetPrinterElementsRequest/>",
            "<wprt:GetPrinterElementsRequest><wprt:RequestedElements/></wprt:GetPrinterElementsRequest>",
            "<wprt:GetPrinterElementsRequest><wprt:RequestedElements><wprt:Name>nope:X</wprt:Name></wprt:RequestedElements></wprt:GetPrinterElementsRequest>",
        ] {
            let rep = call(&r, "GetPrinterElements", body);
            assert_eq!(fault(&rep).as_deref(), Some("InvalidArgs"), "{body}");
            assert_eq!(rep.status, 400);
        }
    }

    #[test]
    fn printer_status_reflects_live_state() {
        let r = rig();
        r.ctx.apply_faults(&FaultBody {
            paper_out: Some(true),
            ..FaultBody::default()
        });
        let rep = call(&r, "GetPrinterElements", "<wprt:GetPrinterElementsRequest><wprt:RequestedElements><wprt:Name>wprt:PrinterStatus</wprt:Name></wprt:RequestedElements></wprt:GetPrinterElementsRequest>");
        let xml = rep.to_xml();
        assert!(
            xml.contains("<wprt:PrinterState>Stopped</wprt:PrinterState>"),
            "{xml}"
        );
        assert!(xml.contains("MediaEmpty"), "{xml}");
    }

    #[test]
    fn create_returns_a_job_id_and_stores_the_effective_ticket() {
        let r = rig();
        let rep = call(&r, "CreatePrintJob", TICKET);
        assert_eq!(rep.action, format!("{NS}/CreatePrintJobResponse"));
        let body = ok(&rep);
        assert_eq!(body.children.len(), 1, "a bare JobId and nothing else");
        assert_eq!(body.child(ns::PRINT, "JobId").unwrap().text_trimmed(), "1");
        let q = call(&r, "GetJobElements", "<wprt:GetJobElementsRequest><wprt:JobId>1</wprt:JobId><wprt:RequestedElements><wprt:Name>wprt:PrintTicket</wprt:Name><wprt:Name>wprt:Documents</wprt:Name></wprt:RequestedElements></wprt:GetJobElementsRequest>");
        let xml = q.to_xml();
        // the job name is escaped exactly once
        assert!(
            xml.contains("<wprt:JobName>A&amp;B</wprt:JobName>"),
            "{xml}"
        );
        assert!(!xml.contains("&amp;amp;"), "{xml}");
        // defaults were filled into the effective ticket; no documents yet => Valid=false
        assert!(xml.contains("<wprt:Sides>OneSided</wprt:Sides>"), "{xml}");
        let datas = &ok(&q).child(ns::PRINT, "JobElements").unwrap().children;
        assert_eq!(datas[0].attr_local("Valid"), Some("true"));
        assert_eq!(datas[1].attr_local("Valid"), Some("false"));
    }

    #[test]
    fn job_token_is_an_opt_in_quirk() {
        let mut cfg = PrintConfig::default();
        cfg.quirks.job_token = true;
        let r = rig_with(cfg);
        let rep = call(&r, "CreatePrintJob", TICKET);
        assert!(ok(&rep).child(ns::PRINT, "JobToken").is_some());
    }

    #[test]
    fn create_error_matrix() {
        let r = rig();
        let cases = [
            ("<wprt:CreatePrintJobRequest/>", "InvalidArgs"),
            ("<wprt:CreatePrintJobRequest><wprt:PrintTicket/></wprt:CreatePrintJobRequest>", "InvalidArgs"),
            ("<wprt:CreatePrintJobRequest><wprt:PrintTicket><wprt:JobDescription><wprt:JobName>x</wprt:JobName><wprt:JobOriginatingUserName>u</wprt:JobOriginatingUserName></wprt:JobDescription><wprt:DocumentProcessing><wprt:Sides wprt:MustHonor=\"true\">TwoSidedLongEdge</wprt:Sides></wprt:DocumentProcessing></wprt:PrintTicket></wprt:CreatePrintJobRequest>", "InvalidArgs"),
            ("<wprt:SendDocumentRequest/>", "InvalidArgs"),
        ];
        for (body, want) in cases {
            let rep = call(&r, "CreatePrintJob", body);
            assert_eq!(fault(&rep).as_deref(), Some(want), "{body}");
        }
        assert!(r.ctx.jobs.active().is_empty());
        let soft = "<wprt:CreatePrintJobRequest><wprt:PrintTicket><wprt:JobDescription><wprt:JobName>x</wprt:JobName><wprt:JobOriginatingUserName>u</wprt:JobOriginatingUserName></wprt:JobDescription><wprt:DocumentProcessing><wprt:Sides>TwoSidedLongEdge</wprt:Sides></wprt:DocumentProcessing></wprt:PrintTicket></wprt:CreatePrintJobRequest>";
        assert!(fault(&call(&r, "CreatePrintJob", soft)).is_none());
        r.ctx.apply_faults(&FaultBody {
            not_accepting_jobs: Some(true),
            ..FaultBody::default()
        });
        assert_eq!(
            fault(&call(&r, "CreatePrintJob", TICKET)).as_deref(),
            Some("ServerErrorNotAcceptingJobs")
        );
        r.ctx.apply_faults(&FaultBody {
            reset: true,
            internal_error: Some(true),
            ..FaultBody::default()
        });
        let rep = call(&r, "CreatePrintJob", TICKET);
        assert_eq!(fault(&rep).as_deref(), Some("ServerErrorInternalError"));
        assert_eq!(rep.status, 500);
    }

    #[test]
    fn send_document_error_matrix() {
        let r = rig();
        let id = create(&r);
        let f = |rep: SoapReply| fault(&rep);
        assert_eq!(
            f(call(
                &r,
                "SendDocument",
                &send_xml(99, "application/pdf", "None", true, "JVBERg==")
            ))
            .as_deref(),
            Some("ClientErrorJobIdNotFound")
        );
        assert_eq!(
            f(call(
                &r,
                "SendDocument",
                &send_xml(0, "application/pdf", "None", true, "JVBERg==")
            ))
            .as_deref(),
            Some("ClientErrorJobIdNotFound")
        );
        assert_eq!(
            f(call(
                &r,
                "SendDocument",
                &send_xml(id, "image/x-bogus", "None", true, "JVBERg==")
            ))
            .as_deref(),
            Some("ClientErrorFormatNotSupported")
        );
        assert_eq!(
            f(call(
                &r,
                "SendDocument",
                &send_xml(id, "application/pdf", "Gzip", true, "JVBERg==")
            ))
            .as_deref(),
            Some("ClientErrorCompressionNotSupported")
        );
        assert_eq!(
            f(call(
                &r,
                "SendDocument",
                &send_xml(id, "image/x-bogus", "Gzip", true, "JVBERg==")
            ))
            .as_deref(),
            Some("ClientErrorFormatNotSupported")
        );
        assert_eq!(
            f(call(
                &r,
                "SendDocument",
                &send_xml(id, "application/pdf", "None", true, "")
            ))
            .as_deref(),
            Some("InvalidArgs")
        );
        assert_eq!(
            f(call(
                &r,
                "SendDocument",
                &send_xml(id, "application/pdf", "None", true, "not base64!!")
            ))
            .as_deref(),
            Some("InvalidArgs")
        );
        assert_eq!(f(call(&r, "SendDocument", "<wprt:SendDocumentRequest><wprt:JobId>1</wprt:JobId><wprt:LastDocument>true</wprt:LastDocument><wprt:DocumentData>JVBERg==</wprt:DocumentData></wprt:SendDocumentRequest>")).as_deref(), Some("InvalidArgs"));
        assert_eq!(
            f(call(
                &r,
                "SendDocument",
                &send_xml(id, "application/pdf", "None", true, "JVBERg==")
                    .replace("<wprt:LastDocument>true", "<wprt:LastDocument>maybe")
            ))
            .as_deref(),
            Some("InvalidArgs")
        );
        let j = r.ctx.jobs.get(id).unwrap();
        assert_eq!(j.state, crate::wsd::types::JobState::Pending);
        let rep = call(
            &r,
            "SendDocument",
            &send_xml(id, "image/x-bogus", "None", true, "JVBERg=="),
        );
        assert!(rep.to_xml().contains("application/pdf"));
        assert!(fault(&send(&r, id, true)).is_none());
        assert_eq!(
            fault(&send(&r, id, true)).as_deref(),
            Some("ClientErrorLastDocumentAlreadySent")
        );
    }

    #[test]
    fn single_document_printers_refuse_open_ended_jobs() {
        let cfg = PrintConfig {
            multiple_document_jobs: false,
            ..PrintConfig::default()
        };
        let r = rig_with(cfg);
        let id = create(&r);
        assert_eq!(
            fault(&send(&r, id, false)).as_deref(),
            Some("ClientErrorMultipleDocumentsNotSupported")
        );
        assert!(fault(&send(&r, id, true)).is_none());
    }

    #[test]
    fn document_active_and_cancelled_job() {
        let r = rig();
        let id = create(&r);
        r.ctx.apply_faults(&FaultBody {
            document_active: Some(true),
            ..FaultBody::default()
        });
        let rep = send(&r, id, true);
        assert_eq!(fault(&rep).as_deref(), Some("ServerErrorDocumentActive"));
        assert_eq!(rep.status, 500);
        r.ctx.apply_faults(&FaultBody {
            reset: true,
            ..FaultBody::default()
        });
        let c = create(&r);
        assert!(fault(&call(&r, "CancelJob", &cancel_req(&c.to_string()))).is_none());
        assert_eq!(
            fault(&send(&r, c, true)).as_deref(),
            Some("ServerErrorJobCancelled")
        );
    }

    #[test]
    fn full_job_runs_to_completion_with_events_and_history() {
        let r = rig();
        let id = create(&r);
        assert_eq!(job_state(&r, id), "Pending");
        let rep = send(&r, id, true);
        assert_eq!(rep.action, format!("{NS}/SendDocumentResponse"));
        assert_eq!(job_state(&r, id), "Processing");
        let active = call(&r, "GetActiveJobs", "<wprt:GetActiveJobsRequest/>");
        assert_eq!(
            ok(&active)
                .child(ns::PRINT, "ActiveJobs")
                .unwrap()
                .children
                .len(),
            1
        );
        r.clock.advance(Duration::from_secs(3));
        assert_eq!(job_state(&r, id), "Completed");
        let active = call(&r, "GetActiveJobs", "<wprt:GetActiveJobsRequest/>");
        assert!(ok(&active)
            .child(ns::PRINT, "ActiveJobs")
            .unwrap()
            .children
            .is_empty());
        let hist = call(&r, "GetJobHistory", "<wprt:GetJobHistoryRequest/>");
        let xml = hist.to_xml();
        let sums = &ok(&hist).child(ns::PRINT, "JobHistory").unwrap().children;
        assert_eq!(sums.len(), 1);
        assert!(
            xml.contains("JobCompletedSuccessfully")
                && xml.contains("<wprt:NumberOfDocuments>1</wprt:NumberOfDocuments>"),
            "{xml}"
        );
        let kinds: Vec<&str> = r.events.events().iter().map(PrintEvent::name).collect();
        assert!(
            kinds.contains(&"JobEndStateEvent") && kinds.contains(&"JobStatusEvent"),
            "{kinds:?}"
        );
    }

    #[test]
    fn missing_document_aborts_after_the_timeout() {
        let r = rig();
        let id = create(&r);
        r.clock.advance(Duration::from_secs(61));
        assert_eq!(job_state(&r, id), "Aborted");
        let xml = call(&r, "GetJobElements", &job_status_req(id)).to_xml();
        assert!(xml.contains("DocumentTimeoutError"), "{xml}");
        assert_eq!(
            fault(&send(&r, id, true)).as_deref(),
            Some("OperationFailed")
        );
    }

    #[test]
    fn cancel_and_job_lookup_matrix() {
        let r = rig();
        let cancel = |id: &str| call(&r, "CancelJob", &cancel_req(id));
        assert_eq!(
            fault(&cancel("5")).as_deref(),
            Some("ClientErrorJobIdNotFound")
        );
        assert_eq!(
            fault(&cancel("-1")).as_deref(),
            Some("ClientErrorJobIdNotFound")
        );
        assert_eq!(
            fault(&cancel("2147483648")).as_deref(),
            Some("ClientErrorJobIdNotFound")
        );
        assert_eq!(fault(&cancel("abc")).as_deref(), Some("InvalidArgs"));
        let id = create(&r);
        assert_eq!(
            cancel(&id.to_string()).action,
            format!("{NS}/CancelJobResponse")
        );
        assert_eq!(job_state(&r, id), "Canceled");
        assert_eq!(
            fault(&cancel(&id.to_string())).as_deref(),
            Some("OperationFailed")
        );
        let hist = call(&r, "GetJobHistory", "<wprt:GetJobHistoryRequest/>").to_xml();
        assert!(hist.contains("JobCanceledByUser"), "{hist}");
    }

    #[test]
    fn get_job_elements_unknown_names_and_jobs() {
        let r = rig();
        let id = create(&r);
        let rep = call(&r, "GetJobElements", &format!("<wprt:GetJobElementsRequest xmlns:v=\"urn:v\"><wprt:JobId>{id}</wprt:JobId><wprt:RequestedElements><wprt:Name>wprt:JobStatus</wprt:Name><wprt:Name>wprt:Nope</wprt:Name><wprt:Name>v:JobStatus</wprt:Name></wprt:RequestedElements></wprt:GetJobElementsRequest>"));
        let valid: Vec<&str> = ok(&rep)
            .child(ns::PRINT, "JobElements")
            .unwrap()
            .children
            .iter()
            .map(|e| e.attr_local("Valid").unwrap())
            .collect();
        assert_eq!(valid, vec!["true", "false", "false"]);
        let rep = call(&r, "GetJobElements", &job_status_req(77));
        assert_eq!(fault(&rep).as_deref(), Some("ClientErrorJobIdNotFound"));
    }

    #[test]
    fn documents_section_describes_what_was_sent() {
        let r = rig();
        let id = create(&r);
        send(&r, id, true);
        let rep = call(&r, "GetJobElements", &format!("<wprt:GetJobElementsRequest><wprt:JobId>{id}</wprt:JobId><wprt:RequestedElements><wprt:Name>wprt:Documents</wprt:Name></wprt:RequestedElements></wprt:GetJobElementsRequest>"));
        let xml = rep.to_xml();
        assert!(
            xml.contains("<wprt:Format>application/pdf</wprt:Format>")
                && xml.contains("<wprt:DocumentId>1</wprt:DocumentId>"),
            "{xml}"
        );
    }

    #[test]
    fn dispatch_by_action_and_endpoint() {
        let r = rig();
        for op in [
            "ValidatePrintTicket",
            "EndJob",
            "PausePrinter",
            "PurgeJobs",
            "Frobnicate",
        ] {
            let rep = call(&r, op, "<x/>");
            assert_eq!(fault(&rep).as_deref(), Some("ActionNotSupported"), "{op}");
            assert_eq!(rep.status, 400);
        }
        let rep = call_on(
            &r,
            EndpointKind::Device,
            "GetActiveJobs",
            "<wprt:GetActiveJobsRequest/>",
        );
        assert_eq!(fault(&rep).as_deref(), Some("ActionNotSupported"));
        let action = "http://schemas.xmlsoap.org/ws/2004/08/eventing/Subscribe";
        let req = SoapRequest::parse(
            Some("application/soap+xml"),
            None,
            &envelope(action, "<x/>"),
        )
        .unwrap();
        assert_eq!(
            fault(&handle(&r.ctx, EndpointKind::PrintService, &req)).as_deref(),
            Some("ActionNotSupported")
        );
        assert_eq!(
            fault(&call(&r, "GetActiveJobs", "<wprt:GetJobHistoryRequest/>")).as_deref(),
            Some("InvalidArgs")
        );
        let action = format!("{NS}/GetActiveJobs");
        let req = SoapRequest::parse(
            Some("application/soap+xml"),
            Some("\"urn:other\""),
            &envelope(&action, "<wprt:GetActiveJobsRequest/>"),
        )
        .unwrap();
        let rep = handle(&r.ctx, EndpointKind::PrintService, &req);
        assert_eq!(
            fault(&rep).as_deref(),
            Some("InvalidMessageInformationHeader")
        );
        let req = SoapRequest::parse(
            Some("application/soap+xml"),
            Some(&format!("\"{action}\"")),
            &envelope(&action, "<wprt:GetActiveJobsRequest/>"),
        )
        .unwrap();
        assert!(fault(&handle(&r.ctx, EndpointKind::PrintService, &req)).is_none());
        let legacy = "http://schemas.microsoft.com/windows/2006/01/wdp/print/GetActiveJobs";
        let req = SoapRequest::parse(
            Some("application/soap+xml"),
            None,
            &envelope(legacy, "<wprt:GetActiveJobsRequest/>"),
        )
        .unwrap();
        assert!(fault(&handle(&r.ctx, EndpointKind::PrintService, &req)).is_none());
    }

    #[test]
    fn fault_replies_correlate_with_the_request() {
        let r = rig();
        let rep = call(&r, "CancelJob", &cancel_req("9"));
        assert_eq!(
            rep.relates_to.as_deref(),
            Some("urn:uuid:11111111-1111-4111-8111-111111111111")
        );
        assert_eq!(
            rep.action,
            "http://schemas.xmlsoap.org/ws/2004/08/addressing/fault"
        );
    }

    #[test]
    fn version_gated_and_optional_operations() {
        let r = rig();
        let set = "<wprt12:SetPrinterElementsRequest><wprt12:ElementsToSet><wprt12:ElementToSet><wprt12:ElementPath>PrinterDescription/PrinterName</wprt12:ElementPath><wprt12:String>x</wprt12:String></wprt12:ElementToSet><wprt12:ElementToSet><wprt12:ElementPath>Nothing/Here</wprt12:ElementPath><wprt12:Boolean>true</wprt12:Boolean></wprt12:ElementToSet></wprt12:ElementsToSet></wprt12:SetPrinterElementsRequest>";
        let action = "http://schemas.microsoft.com/windows/2012/10/wdp/printV12/SetPrinterElements";
        let mk = || {
            SoapRequest::parse(Some("application/soap+xml"), None, &envelope(action, set)).unwrap()
        };
        assert_eq!(
            fault(&handle(&r.ctx, EndpointKind::PrintService, &mk())).as_deref(),
            Some("ActionNotSupported")
        );
        let mut cfg = PrintConfig::default();
        cfg.versions.v12 = true;
        let r12 = rig_with(cfg);
        let rep = handle(&r12.ctx, EndpointKind::PrintService, &mk());
        let results: Vec<String> = ok(&rep)
            .child(ns::PRINT12, "ElementsToSetResults")
            .unwrap()
            .children
            .iter()
            .map(|e| {
                e.child(ns::PRINT12, "Result")
                    .unwrap()
                    .text_trimmed()
                    .to_string()
            })
            .collect();
        assert_eq!(results, vec!["ElementReadOnly", "UnknownElementPath"]);
        let add = "<wprt:AddDocumentRequest><wprt:JobId>1</wprt:JobId><wprt:DocumentDescription><wprt:DocumentId>1</wprt:DocumentId><wprt:Compression>None</wprt:Compression><wprt:Format>application/pdf</wprt:Format><wprt:DocumentUrl>http://127.0.0.1/a.pdf</wprt:DocumentUrl></wprt:DocumentDescription><wprt:LastDocument>true</wprt:LastDocument></wprt:AddDocumentRequest>";
        assert_eq!(
            fault(&call(&r, "AddDocument", add)).as_deref(),
            Some("ServerErrorAddDocumentNotSupported")
        );
    }

    #[test]
    fn add_document_when_enabled_checks_the_url() {
        let cfg = PrintConfig {
            add_document: true,
            ..PrintConfig::default()
        };
        let r = rig_with(cfg);
        let id = create(&r);
        let add = |url: &str| {
            format!("<wprt:AddDocumentRequest><wprt:JobId>{id}</wprt:JobId><wprt:DocumentDescription><wprt:DocumentId>1</wprt:DocumentId><wprt:Compression>None</wprt:Compression><wprt:Format>application/pdf</wprt:Format><wprt:DocumentUrl>{url}</wprt:DocumentUrl></wprt:DocumentDescription><wprt:LastDocument>true</wprt:LastDocument></wprt:AddDocumentRequest>")
        };
        assert_eq!(
            fault(&call(&r, "AddDocument", &add("gopher://x/y"))).as_deref(),
            Some("ClientErrorDocumentURLSchemeNotSupported")
        );
        assert_eq!(
            fault(&call(&r, "AddDocument", &add("http:///nohost"))).as_deref(),
            Some("ClientErrorDocumentURLNotFound")
        );
        let rep = call(&r, "AddDocument", &add("http://127.0.0.1/a.pdf"));
        assert_eq!(rep.action, format!("{NS}/AddDocumentResponse"));
        assert_eq!(
            r.ctx.jobs.get(id).unwrap().documents[0].url.as_deref(),
            Some("http://127.0.0.1/a.pdf")
        );
    }

    #[test]
    fn set_event_rate_range() {
        let r = rig();
        let set = |v: &str| {
            call(&r, "SetEventRate", &format!("<wprt:SetEventRateRequest><wprt:EventRate>{v}</wprt:EventRate></wprt:SetEventRateRequest>"))
        };
        assert_eq!(r.ctx.event_rate_secs(), 30);
        assert!(fault(&set("5")).is_none());
        assert_eq!(r.ctx.event_rate_secs(), 5);
        for bad in ["0", "601", "x"] {
            assert_eq!(fault(&set(bad)).as_deref(), Some("InvalidArgs"), "{bad}");
        }
        assert_eq!(r.ctx.event_rate_secs(), 5);
    }

    #[test]
    fn documents_are_captured_with_one_file_each_and_their_own_format() {
        let dir = std::env::temp_dir().join(format!("fp-ops-{}", uuid::Uuid::new_v4()));
        let inbox = Arc::new(Inbox::new(dir.clone()).unwrap());
        let mut r = rig();
        r.ctx = r.ctx.with_inbox(inbox.clone());
        let id = create(&r);
        assert!(fault(&call(
            &r,
            "SendDocument",
            &send_xml(id, "application/pdf", "None", false, "JVBERi0xLjQ=")
        ))
        .is_none());
        r.clock.advance(Duration::from_secs(2));
        assert!(fault(&call(
            &r,
            "SendDocument",
            &send_xml(id, "application/octet-stream", "None", true, "AAEC")
        ))
        .is_none());
        let caps = inbox.captured();
        assert_eq!(caps.len(), 2);
        assert_ne!(caps[0].path, caps[1].path);
        assert_eq!(std::fs::read(&caps[0].path).unwrap(), b"%PDF-1.4");
        assert_eq!(std::fs::read(&caps[1].path).unwrap(), [0u8, 1, 2]);
        assert_eq!(caps[0].document_format, "application/pdf");
        assert_eq!(caps[1].document_format, "application/octet-stream");
        assert_eq!((caps[0].document_id, caps[1].document_id), (1, 2));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn oversized_documents_are_refused() {
        let mut cfg = PrintConfig::default();
        cfg.limits.max_document = 4;
        let r = rig_with(cfg);
        let id = create(&r);
        assert_eq!(fault(&send(&r, id, true)).as_deref(), Some("InvalidArgs"));
        assert_eq!(r.ctx.jobs.get(id).unwrap().documents.len(), 0);
    }

    #[test]
    fn xop_documents_are_read_from_their_part() {
        let r = rig();
        let id = create(&r);
        let body = format!("<wprt:SendDocumentRequest xmlns:xop=\"http://www.w3.org/2004/08/xop/include\"><wprt:JobId>{id}</wprt:JobId><wprt:DocumentDescription><wprt:DocumentId>1</wprt:DocumentId><wprt:Compression>None</wprt:Compression><wprt:Format>application/pdf</wprt:Format></wprt:DocumentDescription><wprt:LastDocument>true</wprt:LastDocument><wprt:DocumentData><xop:Include href=\"cid:doc@x\"/></wprt:DocumentData></wprt:SendDocumentRequest>");
        let action = format!("{NS}/SendDocument");
        let mut mp = Vec::new();
        mp.extend_from_slice(b"--B\r\nContent-Type: application/xop+xml; charset=utf-8; type=\"application/soap+xml\"\r\n\r\n");
        mp.extend_from_slice(&envelope(&action, &body));
        mp.extend_from_slice(b"\r\n--B\r\nContent-Type: application/pdf\r\nContent-ID: <doc@x>\r\nContent-Transfer-Encoding: binary\r\n\r\n%PDF-bin\r\n--B--\r\n");
        let ct = "multipart/related; boundary=\"B\"; type=\"application/xop+xml\"; start-info=\"application/soap+xml\"";
        let req = SoapRequest::parse(Some(ct), None, &mp).unwrap();
        let rep = handle(&r.ctx, EndpointKind::PrintService, &req);
        assert!(fault(&rep).is_none(), "{:?}", rep.body);
        assert_eq!(r.ctx.jobs.get(id).unwrap().documents[0].size, 8);
    }
}
