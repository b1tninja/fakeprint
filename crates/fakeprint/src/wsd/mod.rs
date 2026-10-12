//! WSD Print Service + WS-Transfer Get handler.

pub mod elements;
pub mod soap;

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::state::World;
use crate::wsd::soap::PRINT_NS;

pub type SharedWorld = Arc<RwLock<World>>;

enum SoapOut {
    Xml(String),
}

pub struct FaultOut(String);

impl From<String> for FaultOut {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl IntoResponse for FaultOut {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "application/soap+xml; charset=utf-8")],
            soap::fault(&self.0),
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
    let action_uri = soap::action(&text).ok_or_else(|| {
        warn!(bytes = body.len(), "WSD request missing Action");
        FaultOut("missing Action".into())
    })?;
    let short = soap::short_action(&action_uri);
    let relates = soap::message_id(&text);
    {
        let mut w = world.write().await;
        w.note_wsd_action(short);
    }
    info!(short, action = %action_uri, bytes = body.len(), "WSD action");

    let out = dispatch(world, short, &text, content_type.as_deref(), &body, relates.as_deref()).await;
    match out {
        Ok(SoapOut::Xml(xml)) => Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/soap+xml; charset=utf-8")],
            xml,
        )
            .into_response()),
        Err(fault) => {
            warn!(short, "WSD action faulted");
            dump_fault_xml(short, &text, &fault.0);
            Err(fault)
        }
    }
}

/// Lab: write Fault responses (+ request body) under D:\temp\fakeprint-*.xml.
fn dump_fault_xml(short: &str, request: &str, reason: &str) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let safe: String = short
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let path = format!("D:\\temp\\fakeprint-fault-{safe}-{stamp}.xml");
    let fault_body = soap::fault(reason);
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
        "GetPrinterElements" => Ok(SoapOut::Xml(get_printer_elements(&world, body, relates).await)),
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
            Err(FaultOut(format!("action not implemented: {other}")))
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
    // Brother / live WSDMON: PresentationUrl must be a reachable LAN URL (not
    // http://localhost/) or Settings Add sticks on "Acquiring data".
    let base = presentation_base(w.device_endpoint());
    let base_esc = soap::esc(&base);
    // Brother Transfer Get Relationship is Hosted-only (no wsdp:Host). Keep
    // PrintDeviceType on Hello/ProbeMatch; declare xmlns on Types itself so a
    // non-inheriting QName reader still binds wprt:.
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
    let w = world.read().await;
    let mgr = soap::esc(w.service_endpoint());
    let sub_id = uuid::Uuid::new_v4();
    let expires = soap::eventing_expires(body);
    let notify_to = soap::extract_tagged(body, "Address").unwrap_or_default();
    // Print Subscribe rarely carries scan-style DestinationResponses; still return a
    // SubscriptionManager that points at the print service (fakescan pattern).
    // Echo NotifyTo in logs — WSDMON delivery failures often mean a bad manager EPR.
    info!(%sub_id, %expires, %notify_to, "WS-Eventing Subscribe accepted (print stub)");
    info!(%body, "Subscribe request body");
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
    soap::envelope(&soap::eventing_action("SubscribeResponse"), relates, &inner)
}

async fn get_printer_elements(world: &SharedWorld, body: &str, relates: Option<&str>) -> String {
    let w = world.read().await;
    let inner = elements::get_printer_elements_body(w.profile(), body, w.status().active_jobs);
    info!(names = ?elements::requested_names(body), "GetPrinterElements");
    soap::envelope(&soap::print_action("GetPrinterElementsResponse"), relates, &inner)
}

async fn create_print_job(world: &SharedWorld, body: &str, relates: Option<&str>) -> String {
    let name = soap::extract_tagged(body, "JobName").unwrap_or_else(|| "job".into());
    let user = soap::extract_tagged(body, "JobOriginatingUserName").unwrap_or_default();
    let fmt = soap::extract_tagged(body, "DocumentFormat")
        .unwrap_or_else(|| "application/octet-stream".into());
    let mut w = world.write().await;
    let job_id = w.create_job(&name, &fmt, &user);
    let token = w.job_token(job_id).unwrap_or_else(|| format!("job-{job_id}"));
    info!(job_id, %fmt, "CreatePrintJob");
    // v1.0 CreatePrintJobResponse is JobId only; JobToken is kept as a lax extension
    // (WSD Scan clients / some stacks expect a correlator after create).
    let inner = format!(
        r#"    <wprt:CreatePrintJobResponse>
      <wprt:JobId>{job_id}</wprt:JobId>
      <wprt:JobToken>{token}</wprt:JobToken>
    </wprt:CreatePrintJobResponse>"#
    );
    soap::envelope(&soap::print_action("CreatePrintJobResponse"), relates, &inner)
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
        .map_err(|e| FaultOut(e.to_string()))?;
    if last {
        w.end_job(job_id);
    }
    info!(job_id, bytes = cap.bytes, path = %cap.path, last, "SendDocument captured");
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
    soap::envelope(
        &soap::print_action("AddDocumentResponse"),
        relates,
        r#"    <wprt:AddDocumentResponse/>"#,
    )
}

async fn end_job(world: &SharedWorld, body: &str, relates: Option<&str>) -> String {
    let job_id = soap::extract_u32(body, "JobId").unwrap_or(0);
    let mut w = world.write().await;
    w.end_job(job_id);
    info!(job_id, "EndJob");
    soap::envelope(
        &soap::print_action("EndJobResponse"),
        relates,
        r#"    <wprt:EndJobResponse/>"#,
    )
}

async fn cancel_job(world: &SharedWorld, body: &str, relates: Option<&str>) -> String {
    let job_id = soap::extract_u32(body, "JobId").unwrap_or(0);
    let mut w = world.write().await;
    let _ = w.cancel_job(job_id);
    info!(job_id, "CancelJob");
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
    soap::envelope(&soap::print_action("GetActiveJobsResponse"), relates, &inner)
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
    soap::envelope(&soap::print_action("GetJobElementsResponse"), relates, &inner)
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
        <wprt:DocumentDescription>
          <wprt:DocumentFormat>{fmt}</wprt:DocumentFormat>
        </wprt:DocumentDescription>
      </wprt:PrintTicket>
    </wprt:ValidatePrintTicketResponse>"#,
        name = soap::esc(&name),
        fmt = soap::esc(&fmt),
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
            device_id: "MFG:Fake;MDL:WSD Print;CLS:Printer;".into(),
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
        // Brother-shaped: Hosted PrinterService only (PrintDeviceType stays on Hello).
        assert!(!xml.contains("<wsdp:Host>"), "{xml}");
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
        assert!(xml.contains("CLS:Printer"));
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
        assert!(xml.contains(r#"Name="wprt:NotARealElement" Valid="false""#), "{xml}");
        assert!(xml.contains(r#"Name="wprt:PrinterStatus" Valid="true""#), "{xml}");
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
        assert!(xml.contains("<wprt:JobToken>job-1</wprt:JobToken>"), "{xml}");

        let q = r#"<wprt:GetJobElementsRequest>
          <wprt:JobId>1</wprt:JobId>
          <wprt:RequestedElements>
            <wprt:Name>wprt:JobStatus</wprt:Name>
            <wprt:Name>wprt:PrintTicket</wprt:Name>
          </wprt:RequestedElements>
        </wprt:GetJobElementsRequest>"#;
        let xml = get_job_elements(&w, q, None).await;
        assert!(xml.contains("JobStatus"), "{xml}");
        assert!(xml.contains("<wprt:JobState>Pending</wprt:JobState>"), "{xml}");
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
}
