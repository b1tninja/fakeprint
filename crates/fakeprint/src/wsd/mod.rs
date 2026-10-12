//! WSD Print Service + WS-Transfer Get handler.

pub mod elements;
mod envelope;
mod print_schema;
pub mod soap;

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::state::{PrintEventSink, World};
use crate::wsd::envelope::Envelope;
use crate::wsd::soap::PRINT_NS;

pub type SharedWorld = Arc<RwLock<World>>;

enum SoapOut {
    Xml(String),
}

pub struct FaultOut {
    reason: String,
    relates: Option<String>,
}

impl FaultOut {
    fn new(reason: impl Into<String>, relates: Option<&str>) -> Self {
        Self {
            reason: reason.into(),
            relates: relates.map(str::to_string),
        }
    }
}

impl From<String> for FaultOut {
    fn from(s: String) -> Self {
        Self::new(s, None)
    }
}

impl IntoResponse for FaultOut {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "application/soap+xml; charset=utf-8")],
            soap::fault(&self.reason, self.relates.as_deref()),
        )
            .into_response()
    }
}

pub async fn print_service(
    State(world): State<SharedWorld>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, FaultOut> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let text = String::from_utf8_lossy(&body);
    let relates = soap::message_id(&text);
    if let Some(finding) = Envelope::parse(&text)
        .request_findings()
        .into_iter()
        .find(|finding| finding.reject)
    {
        warn!(field = finding.field, "WSD header rejected");
        return Err(FaultOut::new(finding.note, relates.as_deref()));
    }
    let action_uri = soap::action(&text).ok_or_else(|| {
        warn!(bytes = body.len(), "WSD request missing Action");
        FaultOut::new("missing Action", relates.as_deref())
    })?;
    let short = soap::short_action(&action_uri);
    {
        let mut w = world.write().await;
        w.note_wsd_action(short);
    }
    info!(short, action = %action_uri, bytes = body.len(), "WSD action");

    let out = dispatch(
        world,
        short,
        &text,
        content_type.as_deref(),
        &body,
        relates.as_deref(),
    )
    .await;
    match out {
        Ok(SoapOut::Xml(xml)) => Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/soap+xml; charset=utf-8")],
            xml,
        )
            .into_response()),
        Err(fault) => {
            warn!(short, "WSD action faulted");
            dump_fault_xml(short, &text, &fault.reason, fault.relates.as_deref());
            Err(fault)
        }
    }
}

/// Lab: write Fault responses (+ request body) under D:\temp\fakeprint-*.xml.
fn dump_fault_xml(short: &str, request: &str, reason: &str, relates: Option<&str>) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let safe: String = short
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let path = format!("D:\\temp\\fakeprint-fault-{safe}-{stamp}.xml");
    let fault_body = soap::fault(reason, relates);
    let blob = format!(
        "<!-- short={short} reason={reason} -->\n<!-- REQUEST -->\n{request}\n<!-- FAULT -->\n{fault_body}\n"
    );
    if let Err(e) = std::fs::write(&path, blob) {
        warn!(%path, error = %e, "failed to dump WSD fault xml");
    } else {
        warn!(%path, "dumped WSD fault xml");
    }
}

async fn dispatch(
    world: SharedWorld,
    short: &str,
    body: &str,
    content_type: Option<&str>,
    raw: &[u8],
    relates: Option<&str>,
) -> Result<SoapOut, FaultOut> {
    match short {
        "Get" => Ok(SoapOut::Xml(transfer_get(&world, relates).await)),
        "GetPrinterElements" => Ok(SoapOut::Xml(
            get_printer_elements(&world, body, relates).await,
        )),
        "CreatePrintJob" => Ok(SoapOut::Xml(create_print_job(&world, body, relates).await)),
        "SendDocument" => Ok(SoapOut::Xml(
            send_document(&world, body, content_type, raw, relates).await?,
        )),
        "AddDocument" => Ok(SoapOut::Xml(add_document(&world, body, relates).await)),
        "EndJob" => Ok(SoapOut::Xml(end_job(&world, body, relates).await)),
        "CancelJob" => Ok(SoapOut::Xml(cancel_job(&world, body, relates).await)),
        "GetActiveJobs" => Ok(SoapOut::Xml(get_active_jobs(&world, relates).await)),
        "GetJobHistory" => Ok(SoapOut::Xml(get_job_history(&world, relates).await)),
        "GetJobElements" => Ok(SoapOut::Xml(get_job_elements(&world, body, relates).await)),
        "ValidatePrintTicket" => Ok(SoapOut::Xml(validate_print_ticket(body, relates))),
        "SetEventRate" => Ok(SoapOut::Xml(set_event_rate(body, relates))),
        "SetPrinterElements" => Err(FaultOut::new(
            "WS-Print v1.2 is not advertised",
            relates,
        )),
        "GetPrintDeviceCapabilities"
        | "GetPrintDeviceResources"
        | "CreatePrintJob2"
        | "PrepareToPrint"
        | "GetBidiSchemaExtensions" => Err(FaultOut::new(
            "WS-Print v2.0 is not advertised",
            relates,
        )),
        "Subscribe" => Ok(SoapOut::Xml(subscribe(&world, body, relates).await)),
        "Unsubscribe" => {
            let inner = format!(
                r#"    <wse:UnsubscribeResponse xmlns:wse="{wse}"/>"#,
                wse = soap::WSE_NS,
            );
            Ok(SoapOut::Xml(soap::envelope(
                &soap::eventing_action("UnsubscribeResponse"),
                relates,
                &inner,
            )))
        }
        "Renew" | "GetStatus" => {
            let expires = soap::eventing_expires(body);
            let inner = format!(
                r#"    <wse:{short}Response xmlns:wse="{wse}">
      <wse:Expires>{expires}</wse:Expires>
    </wse:{short}Response>"#,
                wse = soap::WSE_NS,
            );
            Ok(SoapOut::Xml(soap::envelope(
                &soap::eventing_action(&format!("{short}Response")),
                relates,
                &inner,
            )))
        }
        other => {
            warn!(action = other, "unimplemented WSD action");
            Err(FaultOut::new(
                format!("action not implemented: {other}"),
                relates,
            ))
        }
    }
}

async fn transfer_get(world: &SharedWorld, relates: Option<&str>) -> String {
    let w = world.read().await;
    let service = soap::esc(w.service_endpoint());
    let uuid = w.device_uuid();
    let friendly = soap::esc(&w.profile().label);
    let manufacturer = soap::esc(&w.profile().manufacturer);
    let model = soap::esc(&w.profile().model);
    let compat = format!("{PRINT_NS}/PrinterServiceType");
    // Live WSDMON / Settings: PresentationUrl must be a reachable LAN URL (not
    // http://localhost/) or Add sticks on "Acquiring data".
    let base = presentation_base(w.device_endpoint());
    let base_esc = soap::esc(&base);
    // Keep DPWS Host (fakescan pattern) but bind xmlns:wsdp on Types itself —
    // some Windows QName readers do not inherit the parent Relationship xmlns.
    // Hosted block stays Brother-shaped (PrinterServiceType + CompatibleId URI).
    let inner = format!(
        r#"    <wsx:Metadata xmlns:wsx="http://schemas.xmlsoap.org/ws/2004/09/mex">
      <wsx:MetadataSection Dialect="http://schemas.xmlsoap.org/ws/2006/02/devprof/ThisModel">
        <wsdp:ThisModel xmlns:wsdp="http://schemas.xmlsoap.org/ws/2006/02/devprof"
                        xmlns:pnpx="http://schemas.microsoft.com/windows/pnpx/2005/10"
                        xmlns:df="http://schemas.microsoft.com/windows/2008/09/devicefoundation">
          <wsdp:Manufacturer xml:lang="en-US">{manufacturer}</wsdp:Manufacturer>
          <wsdp:ManufacturerUrl>{base_esc}</wsdp:ManufacturerUrl>
          <wsdp:ModelName xml:lang="en-US">{model}</wsdp:ModelName>
          <wsdp:ModelNumber>1</wsdp:ModelNumber>
          <wsdp:ModelUrl>{base_esc}</wsdp:ModelUrl>
          <wsdp:PresentationUrl>{base_esc}</wsdp:PresentationUrl>
          <pnpx:DeviceCategory>Printers</pnpx:DeviceCategory>
          <df:DeviceCategory>PrintFax.Printer</df:DeviceCategory>
        </wsdp:ThisModel>
      </wsx:MetadataSection>
      <wsx:MetadataSection Dialect="http://schemas.xmlsoap.org/ws/2006/02/devprof/ThisDevice">
        <wsdp:ThisDevice xmlns:wsdp="http://schemas.xmlsoap.org/ws/2006/02/devprof">
          <wsdp:FriendlyName xml:lang="en-US">{friendly}</wsdp:FriendlyName>
          <wsdp:FirmwareVersion>0.1.0</wsdp:FirmwareVersion>
          <wsdp:SerialNumber>FP-0001</wsdp:SerialNumber>
        </wsdp:ThisDevice>
      </wsx:MetadataSection>
      <wsx:MetadataSection Dialect="http://schemas.xmlsoap.org/ws/2006/02/devprof/Relationship">
        <wsdp:Relationship Type="http://schemas.xmlsoap.org/ws/2006/02/devprof/host"
                           xmlns:wsdp="http://schemas.xmlsoap.org/ws/2006/02/devprof"
                           xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing">
          <wsdp:Host>
            <wsa:EndpointReference>
              <wsa:Address>urn:uuid:{uuid}</wsa:Address>
            </wsa:EndpointReference>
            <wsdp:Types xmlns:wsdp="http://schemas.xmlsoap.org/ws/2006/02/devprof"
                        xmlns:wprt="{PRINT_NS}">wsdp:Device wprt:PrintDeviceType</wsdp:Types>
            <wsdp:ServiceId>uri:{uuid}</wsdp:ServiceId>
          </wsdp:Host>
          <wsdp:Hosted>
            <wsa:EndpointReference>
              <wsa:Address>{service}</wsa:Address>
            </wsa:EndpointReference>
            <wsdp:Types xmlns:wprt="{PRINT_NS}">wprt:PrinterServiceType</wsdp:Types>
            <wsdp:ServiceId>uri:{uuid}/PrinterService</wsdp:ServiceId>
            <pnpx:HardwareId xmlns:pnpx="http://schemas.microsoft.com/windows/pnpx/2005/10">VEN_FAKE&amp;DEV_WSDPRINT</pnpx:HardwareId>
            <pnpx:CompatibleId xmlns:pnpx="http://schemas.microsoft.com/windows/pnpx/2005/10">{compat}</pnpx:CompatibleId>
          </wsdp:Hosted>
        </wsdp:Relationship>
      </wsx:MetadataSection>
    </wsx:Metadata>"#
    );
    soap::envelope(
        "http://schemas.xmlsoap.org/ws/2004/09/transfer/GetResponse",
        relates,
        &inner,
    )
}

/// Origin for PresentationUrl / ManufacturerUrl (strip `/WebServices/Device`).
fn presentation_base(device_endpoint: &str) -> String {
    device_endpoint
        .trim_end_matches('/')
        .strip_suffix("/WebServices/Device")
        .map(|s| format!("{s}/"))
        .unwrap_or_else(|| {
            if device_endpoint.ends_with('/') {
                device_endpoint.to_string()
            } else {
                format!("{device_endpoint}/")
            }
        })
}

async fn subscribe(world: &SharedWorld, body: &str, relates: Option<&str>) -> String {
    let sub_id = uuid::Uuid::new_v4();
    let expires = soap::eventing_expires(body);
    let notify_to = soap::extract_tagged(body, "Address").unwrap_or_default();
    let filter_short =
        soap::subscribe_filter_short(body).unwrap_or_else(|| "PrinterStatusChanged".to_string());
    let kind = PrintEventKind::from_filter(&filter_short);

    let (mgr, service_ep) = {
        let mut w = world.write().await;
        let service_ep = w.service_endpoint().to_string();
        if !notify_to.is_empty() {
            w.register_event_sink(PrintEventSink {
                filter: filter_short.clone(),
                notify_to: notify_to.clone(),
                sub_id,
                service_ep: service_ep.clone(),
            });
        }
        (soap::esc(&service_ep), service_ep)
    };

    // Print Subscribe rarely carries scan-style DestinationResponses; still return a
    // SubscriptionManager that points at the print service (fakescan pattern).
    // Echo NotifyTo in logs — WSDMON delivery failures often mean a bad manager EPR.
    info!(%sub_id, %expires, %notify_to, %filter_short, "WS-Eventing Subscribe accepted (print)");
    info!(%body, "Subscribe request body");

    {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("fakeprint-subscribe-{stamp}.xml"));
        let _ = std::fs::write(&path, body);
        info!(path = %path.display(), "dumped Subscribe request body");
    }

    let inner = format!(
        r#"    <wse:SubscribeResponse xmlns:wse="{wse}">
      <wse:SubscriptionManager>
        <wsa:Address>{mgr}</wsa:Address>
        <wsa:ReferenceParameters>
          <wse:Identifier>uuid:{sub_id}</wse:Identifier>
        </wsa:ReferenceParameters>
      </wse:SubscriptionManager>
      <wse:Expires>{expires}</wse:Expires>
    </wse:SubscribeResponse>"#,
        wse = soap::WSE_NS,
    );
    let response = soap::envelope(&soap::eventing_action("SubscribeResponse"), relates, &inner);

    // WSDMON Subscribe is a triple-burst with distinct Action filters. Push the
    // matching event (PrinterStatusSummaryEvent / PrinterElementsChangeEvent).
    // JobEndStateEvent is stored and fired when a job actually ends — not now.
    if !notify_to.is_empty() && kind.notify_immediately() {
        let notify_to_clone = notify_to.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
            send_print_event(&notify_to_clone, sub_id, &service_ep, kind, None).await;
        });
    }

    response
}

async fn get_printer_elements(world: &SharedWorld, body: &str, relates: Option<&str>) -> String {
    let w = world.read().await;
    let inner = elements::get_printer_elements_body(w.profile(), body, w.status().active_jobs);
    info!(names = ?elements::requested_names(body), "GetPrinterElements");
    soap::envelope(
        &soap::print_action("GetPrinterElementsResponse"),
        relates,
        &inner,
    )
}

async fn create_print_job(world: &SharedWorld, body: &str, relates: Option<&str>) -> String {
    let name = soap::extract_tagged(body, "JobName").unwrap_or_else(|| "job".into());
    let user = soap::extract_tagged(body, "JobOriginatingUserName").unwrap_or_default();
    let fmt = soap::extract_tagged(body, "DocumentFormat")
        .unwrap_or_else(|| "application/octet-stream".into());
    let mut w = world.write().await;
    let job_id = w.create_job(&name, &fmt, &user);
    let token = w
        .job_token(job_id)
        .unwrap_or_else(|| format!("job-{job_id}"));
    info!(job_id, %fmt, "CreatePrintJob");
    // v1.0 CreatePrintJobResponse is JobId only; JobToken is kept as a lax extension
    // (WSD Scan clients / some stacks expect a correlator after create).
    let inner = format!(
        r#"    <wprt:CreatePrintJobResponse>
      <wprt:JobId>{job_id}</wprt:JobId>
      <wprt:JobToken>{token}</wprt:JobToken>
    </wprt:CreatePrintJobResponse>"#
    );
    soap::envelope(
        &soap::print_action("CreatePrintJobResponse"),
        relates,
        &inner,
    )
}

async fn send_document(
    world: &SharedWorld,
    body: &str,
    content_type: Option<&str>,
    raw: &[u8],
    relates: Option<&str>,
) -> Result<String, FaultOut> {
    let job_id = soap::extract_u32(body, "JobId").unwrap_or(0);
    let last = soap::extract_tagged(body, "LastDocument")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1"))
        .unwrap_or(true);
    let mut w = world.write().await;
    let fmt = w
        .get_job(job_id)
        .map(|j| j.document_format.clone())
        .unwrap_or_else(|| {
            soap::extract_tagged(body, "DocumentFormat")
                .unwrap_or_else(|| "application/octet-stream".into())
        });
    let payload = soap::extract_document_bytes(content_type, raw).unwrap_or_default();
    let peer = "wsd".to_string();
    let cap = w
        .capture_document(job_id, &payload, &fmt, &peer)
        .map_err(|e| FaultOut::new(e.to_string(), relates))?;
    if last {
        w.end_job(job_id);
    }
    info!(job_id, bytes = cap.bytes, path = %cap.path, last, "SendDocument captured");
    drop(w);
    if last {
        notify_job_end(world, job_id).await;
    }
    let inner = r#"    <wprt:SendDocumentResponse/>"#;
    Ok(soap::envelope(
        &soap::print_action("SendDocumentResponse"),
        relates,
        inner,
    ))
}

async fn add_document(world: &SharedWorld, body: &str, relates: Option<&str>) -> String {
    let job_id = soap::extract_u32(body, "JobId").unwrap_or(0);
    let url = soap::extract_tagged(body, "DocumentUrl").unwrap_or_default();
    let last = soap::extract_tagged(body, "LastDocument")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1"))
        .unwrap_or(true);
    let mut w = world.write().await;
    // Stub: accept URL reference without fetching (lab devices rarely hit AddDocument).
    if let Some(job) = w.get_job_mut(job_id) {
        job.documents = job.documents.saturating_add(1);
        job.state = crate::state::JobState::Processing;
    }
    if last {
        w.end_job(job_id);
    }
    info!(job_id, %url, last, "AddDocument accepted (no fetch)");
    drop(w);
    if last {
        notify_job_end(world, job_id).await;
    }
    soap::envelope(
        &soap::print_action("AddDocumentResponse"),
        relates,
        r#"    <wprt:AddDocumentResponse/>"#,
    )
}

async fn end_job(world: &SharedWorld, body: &str, relates: Option<&str>) -> String {
    let job_id = soap::extract_u32(body, "JobId").unwrap_or(0);
    {
        let mut w = world.write().await;
        w.end_job(job_id);
        info!(job_id, "EndJob");
    }
    notify_job_end(world, job_id).await;
    soap::envelope(
        &soap::print_action("EndJobResponse"),
        relates,
        r#"    <wprt:EndJobResponse/>"#,
    )
}

async fn cancel_job(world: &SharedWorld, body: &str, relates: Option<&str>) -> String {
    let job_id = soap::extract_u32(body, "JobId").unwrap_or(0);
    {
        let mut w = world.write().await;
        let _ = w.cancel_job(job_id);
        info!(job_id, "CancelJob");
    }
    notify_job_end(world, job_id).await;
    soap::envelope(
        &soap::print_action("CancelJobResponse"),
        relates,
        r#"    <wprt:CancelJobResponse/>"#,
    )
}

async fn get_active_jobs(world: &SharedWorld, relates: Option<&str>) -> String {
    let w = world.read().await;
    let mut summaries = String::new();
    for job in w.active_jobs() {
        let state = match job.state {
            crate::state::JobState::Pending => "Pending",
            crate::state::JobState::Processing => "Processing",
            crate::state::JobState::Completed => "Completed",
            crate::state::JobState::Canceled => "Canceled",
            crate::state::JobState::Aborted => "Aborted",
        };
        summaries.push_str(&format!(
            r#"
        <wprt:JobSummary>
          <wprt:JobId>{id}</wprt:JobId>
          <wprt:JobName>{name}</wprt:JobName>
          <wprt:JobOriginatingUserName>{user}</wprt:JobOriginatingUserName>
          <wprt:JobState>{state}</wprt:JobState>
        </wprt:JobSummary>"#,
            id = job.job_id,
            name = soap::esc(&job.name),
            user = soap::esc(&job.user),
        ));
    }
    let inner = format!(
        r#"    <wprt:GetActiveJobsResponse>
      <wprt:ActiveJobs>{summaries}
      </wprt:ActiveJobs>
    </wprt:GetActiveJobsResponse>"#
    );
    soap::envelope(
        &soap::print_action("GetActiveJobsResponse"),
        relates,
        &inner,
    )
}

async fn get_job_history(world: &SharedWorld, relates: Option<&str>) -> String {
    let _w = world.read().await;
    // Minimal empty history — WSDMON polls; an empty list is preferable to a fake job.
    let inner = r#"    <wprt:GetJobHistoryResponse>
      <wprt:JobHistory>
      </wprt:JobHistory>
    </wprt:GetJobHistoryResponse>"#;
    soap::envelope(&soap::print_action("GetJobHistoryResponse"), relates, inner)
}

async fn get_job_elements(world: &SharedWorld, body: &str, relates: Option<&str>) -> String {
    let job_id = soap::extract_u32(body, "JobId").unwrap_or(0);
    let w = world.read().await;
    let job = w.get_job(job_id);
    let inner = elements::get_job_elements_body(job, body, job_id);
    info!(job_id, found = job.is_some(), "GetJobElements");
    soap::envelope(
        &soap::print_action("GetJobElementsResponse"),
        relates,
        &inner,
    )
}

fn validate_print_ticket(body: &str, relates: Option<&str>) -> String {
    // Not in the v1.0 WSDL; Windows print-ticket stacks sometimes probe it.
    // Echo ValidTicket=true + a minimal DefaultPrintTicket-shaped ticket.
    let name = soap::extract_tagged(body, "JobName").unwrap_or_else(|| "DefaultJob".into());
    let fmt = soap::extract_tagged(body, "DocumentFormat")
        .unwrap_or_else(|| "application/octet-stream".into());
    info!(%name, %fmt, "ValidatePrintTicket");
    let inner = format!(
        r#"    <wprt:ValidatePrintTicketResponse>
      <wprt:ValidationInfo>
        <wprt:ValidTicket>true</wprt:ValidTicket>
      </wprt:ValidationInfo>
      <wprt:PrintTicket>
        <wprt:JobDescription>
          <wprt:JobName>{name}</wprt:JobName>
          <wprt:JobOriginatingUserName>DefaultUser</wprt:JobOriginatingUserName>
        </wprt:JobDescription>
      </wprt:PrintTicket>
    </wprt:ValidatePrintTicketResponse>"#,
        name = soap::esc(&name),
    );
    soap::envelope(
        &soap::print_action("ValidatePrintTicketResponse"),
        relates,
        &inner,
    )
}

fn set_event_rate(body: &str, relates: Option<&str>) -> String {
    let rate = soap::extract_tagged(body, "EventRate").unwrap_or_else(|| "30".into());
    info!(%rate, "SetEventRate");
    soap::envelope(
        &soap::print_action("SetEventRateResponse"),
        relates,
        r#"    <wprt:SetEventRateResponse/>"#,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrintEventKind {
    StatusSummary,
    ElementsChange,
    JobEndState,
    StatusChanged,
}

impl PrintEventKind {
    fn from_filter(short: &str) -> Self {
        match short {
            "PrinterStatusSummaryEvent" => Self::StatusSummary,
            "PrinterElementsChangeEvent" => Self::ElementsChange,
            "JobEndStateEvent" => Self::JobEndState,
            _ => Self::StatusChanged,
        }
    }

    fn notify_immediately(self) -> bool {
        !matches!(self, Self::JobEndState)
    }

    fn action_short(self) -> &'static str {
        match self {
            Self::StatusSummary => "PrinterStatusSummaryEvent",
            Self::ElementsChange => "PrinterElementsChangeEvent",
            Self::JobEndState => "JobEndStateEvent",
            Self::StatusChanged => "PrinterStatusChanged",
        }
    }
}

struct JobEndInfo {
    job_id: u32,
    name: String,
    user: String,
    completed: bool,
}

fn print_event_inner(kind: PrintEventKind, job: Option<&JobEndInfo>) -> String {
    match kind {
        PrintEventKind::StatusSummary => r#"    <wprt:PrinterStatusSummary>
      <wprt:PrinterState>Idle</wprt:PrinterState>
      <wprt:PrinterPrimaryStateReason>None</wprt:PrinterPrimaryStateReason>
    </wprt:PrinterStatusSummary>"#
            .to_string(),
        PrintEventKind::ElementsChange => r#"    <wprt:PrinterElementsChangeEvent>
      <wprt:PrinterElements>
        <wprt:ElementData Name="wprt:PrinterStatus" Valid="true">
          <wprt:PrinterStatus>
            <wprt:PrinterCurrentTime>2026-01-01T00:00:00Z</wprt:PrinterCurrentTime>
            <wprt:PrinterState>Idle</wprt:PrinterState>
            <wprt:PrinterStateReasons>
              <wprt:PrinterStateReason>None</wprt:PrinterStateReason>
            </wprt:PrinterStateReasons>
            <wprt:QueuedJobCount>0</wprt:QueuedJobCount>
          </wprt:PrinterStatus>
        </wprt:ElementData>
      </wprt:PrinterElements>
    </wprt:PrinterElementsChangeEvent>"#
            .to_string(),
        PrintEventKind::JobEndState => {
            let (id, name, user, state, reason) = match job {
                Some(j) => (
                    j.job_id,
                    soap::esc(&j.name),
                    soap::esc(&j.user),
                    if j.completed { "Completed" } else { "Canceled" },
                    if j.completed {
                        "JobCompletedSuccessfully"
                    } else {
                        "JobCanceled"
                    },
                ),
                None => (
                    0,
                    String::new(),
                    String::new(),
                    "Completed",
                    "JobCompletedSuccessfully",
                ),
            };
            format!(
                r#"    <wprt:JobEndStateEvent>
      <wprt:JobEndState>
        <wprt:JobName>{name}</wprt:JobName>
        <wprt:JobId>{id}</wprt:JobId>
        <wprt:JobOriginatingUserName>{user}</wprt:JobOriginatingUserName>
        <wprt:JobState>{state}</wprt:JobState>
        <wprt:JobStateReasons>
          <wprt:JobStateReason>{reason}</wprt:JobStateReason>
        </wprt:JobStateReasons>
      </wprt:JobEndState>
    </wprt:JobEndStateEvent>"#
            )
        }
        PrintEventKind::StatusChanged => r#"    <wprt:PrinterStatusChangedMessage>
      <wprt:PrinterStatus>
        <wprt:PrinterCurrentTime>2026-01-01T00:00:00Z</wprt:PrinterCurrentTime>
        <wprt:PrinterState>Idle</wprt:PrinterState>
        <wprt:PrinterStateReasons>
          <wprt:PrinterStateReason>None</wprt:PrinterStateReason>
        </wprt:PrinterStateReasons>
        <wprt:QueuedJobCount>0</wprt:QueuedJobCount>
        <wprt:PrinterLocalTime>2026-01-01T00:00:00Z</wprt:PrinterLocalTime>
      </wprt:PrinterStatus>
    </wprt:PrinterStatusChangedMessage>"#
            .to_string(),
    }
}

fn print_event_envelope(
    kind: PrintEventKind,
    notify_to: &str,
    sub_id: uuid::Uuid,
    service_ep: &str,
    job: Option<&JobEndInfo>,
) -> String {
    let msg_id = uuid::Uuid::new_v4();
    let notify_to_esc = soap::esc(notify_to);
    let service_esc = soap::esc(service_ep);
    let action = soap::print_action(kind.action_short());
    let inner = print_event_inner(kind, job);
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<soap:Envelope xmlns:soap="{SOAP_NS}" xmlns:wsa="{WSA_NS}" xmlns:wprt="{PRINT_NS}" xmlns:wse="{WSE_NS}">
  <soap:Header>
    <wsa:To>{notify_to_esc}</wsa:To>
    <wsa:Action>{action}</wsa:Action>
    <wsa:MessageID>urn:uuid:{msg_id}</wsa:MessageID>
    <wsa:From>
      <wsa:Address>{service_esc}</wsa:Address>
    </wsa:From>
    <wse:SubscriptionManager>
      <wsa:Address>{service_esc}</wsa:Address>
      <wsa:ReferenceParameters>
        <wse:Identifier>uuid:{sub_id}</wse:Identifier>
      </wsa:ReferenceParameters>
    </wse:SubscriptionManager>
  </soap:Header>
  <soap:Body>
{inner}
  </soap:Body>
</soap:Envelope>"#,
        SOAP_NS = soap::SOAP_NS,
        WSA_NS = soap::WSA_NS,
        PRINT_NS = soap::PRINT_NS,
        WSE_NS = soap::WSE_NS,
    )
}

async fn notify_job_end(world: &SharedWorld, job_id: u32) {
    let (sinks, info) = {
        let w = world.read().await;
        let job = w.get_job(job_id);
        let info = JobEndInfo {
            job_id,
            name: job.map(|j| j.name.clone()).unwrap_or_default(),
            user: job.map(|j| j.user.clone()).unwrap_or_default(),
            completed: job
                .map(|j| j.state == crate::state::JobState::Completed)
                .unwrap_or(true),
        };
        (w.event_sinks_matching("JobEndStateEvent"), info)
    };
    for sink in sinks {
        let job = JobEndInfo {
            job_id: info.job_id,
            name: info.name.clone(),
            user: info.user.clone(),
            completed: info.completed,
        };
        tokio::spawn(async move {
            send_print_event(
                &sink.notify_to,
                sink.sub_id,
                &sink.service_ep,
                PrintEventKind::JobEndState,
                Some(job),
            )
            .await;
        });
    }
}

/// Push a WS-Print event to WSDMON's NotifyTo. Fire-and-forget; raw TCP (no reqwest).
async fn send_print_event(
    notify_to: &str,
    sub_id: uuid::Uuid,
    service_ep: &str,
    kind: PrintEventKind,
    job: Option<JobEndInfo>,
) {
    use tokio::io::AsyncWriteExt;

    let url = notify_to.trim();
    let rest = match url.strip_prefix("http://") {
        Some(r) => r,
        None => {
            warn!(%notify_to, kind = kind.action_short(), "print event: NotifyTo is not http://; skipping");
            return;
        }
    };
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };

    let body_xml = print_event_envelope(kind, notify_to, sub_id, service_ep, job.as_ref());
    let content_len = body_xml.len();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\n\
         Content-Type: application/soap+xml; charset=utf-8\r\n\
         Content-Length: {content_len}\r\n\
         Connection: close\r\n\r\n{body_xml}"
    );

    match tokio::net::TcpStream::connect(host_port).await {
        Ok(mut stream) => match stream.write_all(request.as_bytes()).await {
            Ok(()) => {
                info!(%notify_to, %sub_id, kind = kind.action_short(), "print event notification sent");
            }
            Err(e) => {
                warn!(%notify_to, %sub_id, kind = kind.action_short(), error = %e, "print event write failed");
            }
        },
        Err(e) => {
            warn!(%notify_to, %sub_id, kind = kind.action_short(), error = %e, "print event connect failed");
        }
    }
}

// Re-export for unit tests that used the old free function.
pub use elements::printer_description_xml;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::DiscoveryStats;
    use crate::profile::Profile;
    use std::sync::Arc;

    fn world() -> SharedWorld {
        let dir = std::env::temp_dir().join(format!("fp-wsd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let profile = Profile {
            id: "generic-wsd".into(),
            label: "Fake WSD printer".into(),
            manufacturer: "fakeprint".into(),
            model: "Fake WSD Print".into(),
            device_id: "MFG:Fake;CMD:PJL,PCL,POSTSCRIPT;MDL:WSD Print;CLS:PRINTER;DES:Fake WSD printer;CID:MS_IPP;".into(),
            color_supported: true,
            pages_per_minute: 20,
            pages_per_minute_color: 12,
            document_formats: vec!["application/pdf".into()],
            multiple_document_jobs: true,
        };
        let mut w = World::new(
            dir.clone(),
            dir.join("inbox"),
            profile,
            Arc::new(DiscoveryStats::default()),
        )
        .unwrap();
        w.set_identity(
            "http://192.0.2.10:3921/WebServices/Device".into(),
            "http://192.0.2.10:3921/WebServices/PrinterService".into(),
            uuid::Uuid::parse_str("00000000-0000-4000-8000-0000000000f3").unwrap(),
        );
        Arc::new(RwLock::new(w))
    }

    #[tokio::test]
    async fn transfer_get_is_printers_category() {
        let xml = transfer_get(&world(), Some("urn:uuid:test")).await;
        assert!(xml.contains("Printers"));
        assert!(xml.contains("PrintFax.Printer"));
        assert!(xml.contains("<wsdp:Host>"));
        assert!(xml.contains("PrintDeviceType"));
        assert!(xml.contains(r#"xmlns:wsdp="http://schemas.xmlsoap.org/ws/2006/02/devprof""#));
        assert!(xml.contains("PrinterServiceType"));
        assert!(xml.contains("VEN_FAKE&amp;DEV_WSDPRINT"));
        assert!(xml.contains("/WebServices/PrinterService"));
        assert!(xml.contains("uri:00000000-0000-4000-8000-0000000000f3/PrinterService"));
        assert!(xml.contains("http://192.0.2.10:3921/"), "{xml}");
        assert!(!xml.contains("http://localhost/"), "{xml}");
    }

    #[test]
    fn presentation_base_strips_device_path() {
        assert_eq!(
            presentation_base("http://192.168.1.70:3921/WebServices/Device"),
            "http://192.168.1.70:3921/"
        );
    }

    #[tokio::test]
    async fn get_printer_elements_includes_description() {
        let body = r#"<wsa:Action>http://schemas.microsoft.com/windows/2006/08/wdp/print/GetPrinterElements</wsa:Action>
          <wprt:RequestedElements><wprt:Name>wprt:PrinterDescription</wprt:Name></wprt:RequestedElements>"#;
        let xml = get_printer_elements(&world(), body, None).await;
        assert!(xml.contains("PrinterDescription"));
        assert!(xml.contains("MFG:Fake"));
        assert!(xml.contains("CLS:PRINTER"));
        assert!(xml.contains("CID:MS_IPP"));
        assert!(xml.contains("GetPrinterElementsResponse"));
        assert!(xml.contains(r#"Name="wprt:PrinterDescription""#));
        assert!(xml.contains(r#"Valid="true""#));
    }

    #[tokio::test]
    async fn get_printer_elements_richer_capabilities_and_defaults() {
        let body = r#"<wprt:RequestedElements>
          <wprt:Name>wprt:PrinterCapabilities</wprt:Name>
          <wprt:Name>wprt:DefaultPrintTicket</wprt:Name>
          <wprt:Name>wprt:PrinterConfiguration</wprt:Name>
          <wprt:Name>wprt:PrinterStatus</wprt:Name>
        </wprt:RequestedElements>"#;
        let xml = get_printer_elements(&world(), body, None).await;
        assert!(xml.contains("JobValues"), "{xml}");
        assert!(xml.contains("DocumentValues"), "{xml}");
        assert!(xml.contains("DefaultPrintTicket"), "{xml}");
        assert!(xml.contains("PrinterConfiguration"), "{xml}");
        assert!(xml.contains("PrinterCurrentTime"), "{xml}");
        assert!(xml.contains("PrinterLocalTime"), "{xml}");
    }

    #[tokio::test]
    async fn get_printer_elements_unknown_name_valid_false() {
        let body = r#"<wprt:RequestedElements>
          <wprt:Name>wprt:NotARealElement</wprt:Name>
          <wprt:Name>wprt:PrinterStatus</wprt:Name>
        </wprt:RequestedElements>"#;
        let xml = get_printer_elements(&world(), body, None).await;
        assert!(
            xml.contains(r#"Name="wprt:NotARealElement" Valid="false""#),
            "{xml}"
        );
        assert!(
            xml.contains(r#"Name="wprt:PrinterStatus" Valid="true""#),
            "{xml}"
        );
    }

    #[tokio::test]
    async fn create_then_get_job_elements() {
        let w = world();
        let create = r#"<wprt:CreatePrintJobRequest>
          <wprt:PrintTicket>
            <wprt:JobDescription>
              <wprt:JobName>lab</wprt:JobName>
              <wprt:JobOriginatingUserName>tester</wprt:JobOriginatingUserName>
            </wprt:JobDescription>
            <wprt:DocumentDescription>
              <wprt:DocumentFormat>application/pdf</wprt:DocumentFormat>
            </wprt:DocumentDescription>
          </wprt:PrintTicket>
        </wprt:CreatePrintJobRequest>"#;
        let xml = create_print_job(&w, create, None).await;
        assert!(xml.contains("<wprt:JobId>1</wprt:JobId>"), "{xml}");
        assert!(
            xml.contains("<wprt:JobToken>job-1</wprt:JobToken>"),
            "{xml}"
        );

        let q = r#"<wprt:GetJobElementsRequest>
          <wprt:JobId>1</wprt:JobId>
          <wprt:RequestedElements>
            <wprt:Name>wprt:JobStatus</wprt:Name>
            <wprt:Name>wprt:PrintTicket</wprt:Name>
          </wprt:RequestedElements>
        </wprt:GetJobElementsRequest>"#;
        let xml = get_job_elements(&w, q, None).await;
        assert!(xml.contains("JobStatus"), "{xml}");
        assert!(
            xml.contains("<wprt:JobState>Pending</wprt:JobState>"),
            "{xml}"
        );
        assert!(xml.contains("tester"), "{xml}");
    }

    #[tokio::test]
    async fn validate_print_ticket_accepts() {
        let body = r#"<wprt:ValidatePrintTicketRequest>
          <wprt:PrintTicket>
            <wprt:JobDescription><wprt:JobName>x</wprt:JobName></wprt:JobDescription>
          </wprt:PrintTicket>
        </wprt:ValidatePrintTicketRequest>"#;
        let xml = validate_print_ticket(body, None);
        assert!(xml.contains("ValidTicket>true"), "{xml}");
        assert!(xml.contains("ValidatePrintTicketResponse"), "{xml}");
    }

    #[tokio::test]
    async fn subscribe_points_at_print_service() {
        let xml = subscribe(&world(), "<wse:Subscribe/>", None).await;
        assert!(xml.contains("SubscribeResponse"));
        assert!(xml.contains("192.0.2.10:3921/WebServices/PrinterService"));
        assert!(xml.contains("wse:Identifier"));
    }

    #[test]
    fn subscribe_filter_kinds_match_wsdmon_burst() {
        assert_eq!(
            PrintEventKind::from_filter("PrinterStatusSummaryEvent"),
            PrintEventKind::StatusSummary
        );
        assert_eq!(
            PrintEventKind::from_filter("PrinterElementsChangeEvent"),
            PrintEventKind::ElementsChange
        );
        assert_eq!(
            PrintEventKind::from_filter("JobEndStateEvent"),
            PrintEventKind::JobEndState
        );
        assert!(PrintEventKind::StatusSummary.notify_immediately());
        assert!(PrintEventKind::ElementsChange.notify_immediately());
        assert!(!PrintEventKind::JobEndState.notify_immediately());
    }

    #[test]
    fn status_summary_event_action_matches_filter() {
        let xml = print_event_envelope(
            PrintEventKind::StatusSummary,
            "http://192.0.2.1:5357/WsdPrint/x",
            uuid::Uuid::nil(),
            "http://192.0.2.10:3921/WebServices/PrinterService",
            None,
        );
        assert!(
            xml.contains("/wdp/print/PrinterStatusSummaryEvent"),
            "{xml}"
        );
        assert!(xml.contains("<wprt:PrinterStatusSummary>"), "{xml}");
        assert!(
            xml.contains("<wprt:PrinterState>Idle</wprt:PrinterState>"),
            "{xml}"
        );
        assert!(!xml.contains("PrinterStatusChanged"), "{xml}");
    }

    #[test]
    fn job_end_state_event_carries_job_id() {
        let job = JobEndInfo {
            job_id: 7,
            name: "testpage".into(),
            user: "lab".into(),
            completed: true,
        };
        let xml = print_event_envelope(
            PrintEventKind::JobEndState,
            "http://192.0.2.1:5357/WsdPrint/x",
            uuid::Uuid::nil(),
            "http://192.0.2.10:3921/WebServices/PrinterService",
            Some(&job),
        );
        assert!(xml.contains("/wdp/print/JobEndStateEvent"), "{xml}");
        assert!(xml.contains("<wprt:JobId>7</wprt:JobId>"), "{xml}");
        assert!(xml.contains("JobCompletedSuccessfully"), "{xml}");
    }

    #[tokio::test]
    async fn subscribe_stores_job_end_sink() {
        let w = world();
        let body = r#"<wse:Subscribe>
          <wse:Delivery><wse:NotifyTo>
            <wsa:Address>http://192.0.2.1:5357/WsdPrint/x</wsa:Address>
          </wse:NotifyTo></wse:Delivery>
          <wse:Filter Dialect="http://schemas.xmlsoap.org/ws/2006/02/devprof/Action">
            http://schemas.microsoft.com/windows/2006/08/wdp/print/JobEndStateEvent
          </wse:Filter>
        </wse:Subscribe>"#;
        let _ = subscribe(&w, body, None).await;
        let sinks = w.read().await.event_sinks_matching("JobEndStateEvent");
        assert_eq!(sinks.len(), 1);
        assert_eq!(sinks[0].notify_to, "http://192.0.2.1:5357/WsdPrint/x");
    }
}
