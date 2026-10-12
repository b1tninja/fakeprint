//! GetPrinterElements / GetJobElements builders (WS-Print v1.0 shapes).

use crate::profile::Profile;
use crate::state::{ActiveJob, JobState};
use crate::wsd::soap;

/// `<wprt:Name>` values from RequestedElements (not ElementData Name= attributes).
pub fn requested_names(body: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut idx = 0;
    while let Some(rel) = body[idx..].find("Name>") {
        let at = idx + rel;
        let before = body.as_bytes().get(at.wrapping_sub(1)).copied();
        if matches!(before, Some(b'<') | Some(b':')) {
            let after = &body[at + "Name>".len()..];
            if let Some(end) = after.find('<') {
                let v = after[..end].trim();
                if !v.is_empty() && !v.contains('=') {
                    names.push(v.to_string());
                }
            }
        }
        idx = at + 5;
    }
    names
}

fn client_name(names: &[String], suffix: &str) -> String {
    names
        .iter()
        .find(|n| n.ends_with(suffix) || *n == suffix || n.ends_with(&format!(":{suffix}")))
        .cloned()
        .unwrap_or_else(|| format!("wprt:{suffix}"))
}

fn want(names: &[String], suffix: &str) -> bool {
    names.is_empty()
        || names
            .iter()
            .any(|n| n.ends_with(suffix) || n == suffix || n.ends_with(&format!(":{suffix}")))
}

fn element_data(name: &str, inner: &str) -> String {
    format!(
        r#"
        <wprt:ElementData Name="{name}" Valid="true">
{inner}
        </wprt:ElementData>"#
    )
}

pub fn printer_description_xml(profile: &Profile) -> String {
    let color = if profile.color_supported {
        "true"
    } else {
        "false"
    };
    let multi = if profile.multiple_document_jobs {
        "true"
    } else {
        "false"
    };
    format!(
        r#"          <wprt:PrinterDescription>
            <wprt:ColorSupported>{color}</wprt:ColorSupported>
            <wprt:DeviceId>{device_id}</wprt:DeviceId>
            <wprt:MultipleDocumentJobsSupported>{multi}</wprt:MultipleDocumentJobsSupported>
            <wprt:PagesPerMinute>{ppm}</wprt:PagesPerMinute>
            <wprt:PagesPerMinuteColor>{ppm_c}</wprt:PagesPerMinuteColor>
            <wprt:PrinterName xml:lang="en-US">{name}</wprt:PrinterName>
            <wprt:PrinterInfo xml:lang="en-US">fakeprint WSD Phase 1</wprt:PrinterInfo>
            <wprt:PrinterLocation xml:lang="en-US">lab</wprt:PrinterLocation>
          </wprt:PrinterDescription>"#,
        device_id = soap::esc(&profile.device_id),
        ppm = profile.pages_per_minute,
        ppm_c = profile.pages_per_minute_color,
        name = soap::esc(&profile.label),
    )
}

pub fn printer_status_xml(queued: usize) -> String {
    // XSD: PrinterCurrentTime. Some MS samples / lab fixtures still show PrinterLocalTime —
    // emit both so either reader accepts Idle.
    format!(
        r#"          <wprt:PrinterStatus>
            <wprt:PrinterCurrentTime>2026-01-01T00:00:00Z</wprt:PrinterCurrentTime>
            <wprt:PrinterLocalTime>2026-01-01T00:00:00Z</wprt:PrinterLocalTime>
            <wprt:PrinterState>Idle</wprt:PrinterState>
            <wprt:PrinterStateReasons>
              <wprt:PrinterStateReason>None</wprt:PrinterStateReason>
            </wprt:PrinterStateReasons>
            <wprt:QueuedJobCount>{queued}</wprt:QueuedJobCount>
          </wprt:PrinterStatus>"#
    )
}

pub fn printer_configuration_xml() -> String {
    r#"          <wprt:PrinterConfiguration>
            <wprt:PrinterEventRate>30</wprt:PrinterEventRate>
            <wprt:InputBins>
              <wprt:InputBinEntry Name="Tray1">
                <wprt:Installed>true</wprt:Installed>
                <wprt:MediaSizeName>na_letter_8.5x11in</wprt:MediaSizeName>
                <wprt:MediaType>stationery</wprt:MediaType>
                <wprt:MediaColor>white</wprt:MediaColor>
                <wprt:FeedDirection>LongEdgeFirst</wprt:FeedDirection>
                <wprt:Capacity>250</wprt:Capacity>
                <wprt:Level>80</wprt:Level>
              </wprt:InputBinEntry>
            </wprt:InputBins>
            <wprt:OutputBins>
              <wprt:OutputBinEntry Name="OutputBin1">
                <wprt:Installed>true</wprt:Installed>
                <wprt:Capacity>150</wprt:Capacity>
                <wprt:Level>0</wprt:Level>
              </wprt:OutputBinEntry>
            </wprt:OutputBins>
          </wprt:PrinterConfiguration>"#
    .into()
}

pub fn default_print_ticket_xml(profile: &Profile) -> String {
    let fmt = profile
        .document_formats
        .first()
        .map(|s| s.as_str())
        .unwrap_or("application/octet-stream");
    format!(
        r#"          <wprt:DefaultPrintTicket>
            <wprt:JobDescription>
              <wprt:JobName>DefaultJob</wprt:JobName>
              <wprt:JobOriginatingUserName>DefaultUser</wprt:JobOriginatingUserName>
            </wprt:JobDescription>
            <wprt:JobProcessing>
              <wprt:Copies>1</wprt:Copies>
              <wprt:Priority>50</wprt:Priority>
            </wprt:JobProcessing>
            <wprt:DocumentProcessing>
              <wprt:MediaSizeName>na_letter_8.5x11in</wprt:MediaSizeName>
              <wprt:MediaType>stationery</wprt:MediaType>
              <wprt:Orientation>Portrait</wprt:Orientation>
              <wprt:Sides>OneSided</wprt:Sides>
              <wprt:PrintQuality>Normal</wprt:PrintQuality>
              <wprt:Resolution>
                <wprt:Width>600</wprt:Width>
                <wprt:Height>600</wprt:Height>
              </wprt:Resolution>
            </wprt:DocumentProcessing>
            <wprt:DocumentDescription>
              <wprt:DocumentFormat>{fmt}</wprt:DocumentFormat>
            </wprt:DocumentDescription>
          </wprt:DefaultPrintTicket>"#,
        fmt = soap::esc(fmt),
    )
}

fn allowed_tokens(tag: &str, values: &[&str]) -> String {
    let mut s = format!("            <wprt:{tag}>");
    for v in values {
        s.push_str(&format!(
            "\n              <wprt:AllowedValue>{}</wprt:AllowedValue>",
            soap::esc(v)
        ));
    }
    s.push_str(&format!("\n            </wprt:{tag}>"));
    s
}

pub fn printer_capabilities_xml(profile: &Profile) -> String {
    let mut formats = String::new();
    for fmt in &profile.document_formats {
        formats.push_str(&format!(
            "\n              <wprt:AllowedValue>{}</wprt:AllowedValue>",
            soap::esc(fmt)
        ));
    }
    // WDP Print XSD: JobValues / DocumentValues (not invented DocumentFormatsSupported).
    format!(
        r#"          <wprt:PrinterCapabilities>
            <wprt:JobValues>
              <wprt:JobProcessing>
                <wprt:Copies>
                  <wprt:MinValue>1</wprt:MinValue>
                  <wprt:MaxValue>99</wprt:MaxValue>
                </wprt:Copies>
                <wprt:Priority>
                  <wprt:MinValue>1</wprt:MinValue>
                  <wprt:MaxValue>100</wprt:MaxValue>
                </wprt:Priority>
              </wprt:JobProcessing>
              <wprt:DocumentProcessing>
{media}
{media_type}
{orient}
{sides}
{quality}
                <wprt:Resolution>
                  <wprt:AllowedValue>
                    <wprt:Width>300</wprt:Width>
                    <wprt:Height>300</wprt:Height>
                  </wprt:AllowedValue>
                  <wprt:AllowedValue>
                    <wprt:Width>600</wprt:Width>
                    <wprt:Height>600</wprt:Height>
                  </wprt:AllowedValue>
                </wprt:Resolution>
                <wprt:NumberUp>
                  <wprt:PagesPerSheet>
                    <wprt:AllowedValue>1</wprt:AllowedValue>
                    <wprt:AllowedValue>2</wprt:AllowedValue>
                    <wprt:AllowedValue>4</wprt:AllowedValue>
                  </wprt:PagesPerSheet>
                  <wprt:Direction>
                    <wprt:AllowedValue>RightDown</wprt:AllowedValue>
                  </wprt:Direction>
                </wprt:NumberUp>
              </wprt:DocumentProcessing>
            </wprt:JobValues>
            <wprt:DocumentValues>
              <wprt:DocumentDescription>
                <wprt:Compression>
                  <wprt:AllowedValue>None</wprt:AllowedValue>
                </wprt:Compression>
                <wprt:Format>{formats}
                </wprt:Format>
              </wprt:DocumentDescription>
            </wprt:DocumentValues>
          </wprt:PrinterCapabilities>"#,
        media = allowed_tokens(
            "MediaSizeName",
            &["na_letter_8.5x11in", "na_legal_8.5x14in", "iso_a4_210x297mm"],
        ),
        media_type = allowed_tokens("MediaType", &["stationery", "photographic", "other"]),
        orient = allowed_tokens("Orientation", &["Portrait", "Landscape"]),
        sides = allowed_tokens("Sides", &["OneSided"]),
        quality = allowed_tokens("PrintQuality", &["Draft", "Normal", "High"]),
        formats = formats,
    )
}

fn handled_printer_element(name: &str) -> bool {
    let one = [name.to_string()];
    want(&one, "PrinterDescription")
        || want(&one, "PrinterStatus")
        || want(&one, "PrinterConfiguration")
        || want(&one, "DefaultPrintTicket")
        || want(&one, "PrinterCapabilities")
        || want(&one, "Capabilities")
}

pub fn get_printer_elements_body(profile: &Profile, body: &str, queued: usize) -> String {
    let names = requested_names(body);
    let mut elements = String::new();

    if want(&names, "PrinterDescription") {
        elements.push_str(&element_data(
            &client_name(&names, "PrinterDescription"),
            &printer_description_xml(profile),
        ));
    }
    if want(&names, "PrinterStatus") {
        elements.push_str(&element_data(
            &client_name(&names, "PrinterStatus"),
            &printer_status_xml(queued),
        ));
    }
    if want(&names, "PrinterConfiguration") {
        elements.push_str(&element_data(
            &client_name(&names, "PrinterConfiguration"),
            &printer_configuration_xml(),
        ));
    }
    if want(&names, "DefaultPrintTicket") {
        elements.push_str(&element_data(
            &client_name(&names, "DefaultPrintTicket"),
            &default_print_ticket_xml(profile),
        ));
    }
    if want(&names, "PrinterCapabilities") || want(&names, "Capabilities") {
        elements.push_str(&element_data(
            &client_name(&names, "PrinterCapabilities"),
            &printer_capabilities_xml(profile),
        ));
    }

    // Unknown QNames → Valid=false so PrinterElements is never empty after a named ask.
    for n in &names {
        if !handled_printer_element(n) {
            elements.push_str(&format!(
                r#"
        <wprt:ElementData Name="{}" Valid="false"/>"#,
                soap::esc(n)
            ));
        }
    }
    if elements.is_empty() {
        elements.push_str(
            r#"
        <wprt:ElementData Name="wprt:Unknown" Valid="false"/>"#,
        );
    }

    format!(
        r#"    <wprt:GetPrinterElementsResponse>
      <wprt:PrinterElements>{elements}
      </wprt:PrinterElements>
    </wprt:GetPrinterElementsResponse>"#
    )
}

pub fn job_status_xml(job: &ActiveJob) -> String {
    let state = match job.state {
        JobState::Pending => "Pending",
        JobState::Processing => "Processing",
        JobState::Completed => "Completed",
        JobState::Canceled => "Canceled",
        JobState::Aborted => "Aborted",
    };
    format!(
        r#"          <wprt:JobStatus>
            <wprt:JobId>{id}</wprt:JobId>
            <wprt:JobState>{state}</wprt:JobState>
            <wprt:JobStateReasons>
              <wprt:JobStateReason>None</wprt:JobStateReason>
            </wprt:JobStateReasons>
            <wprt:KOctetsProcessed>{ko}</wprt:KOctetsProcessed>
            <wprt:MediaSheetsCompleted>{sheets}</wprt:MediaSheetsCompleted>
            <wprt:NumberOfDocuments>{docs}</wprt:NumberOfDocuments>
          </wprt:JobStatus>"#,
        id = job.job_id,
        ko = job.bytes_received / 1024,
        sheets = if job.state == JobState::Completed { 1 } else { 0 },
        docs = job.documents,
    )
}

pub fn job_print_ticket_xml(job: &ActiveJob) -> String {
    format!(
        r#"          <wprt:PrintTicket>
            <wprt:JobDescription>
              <wprt:JobName>{name}</wprt:JobName>
              <wprt:JobOriginatingUserName>{user}</wprt:JobOriginatingUserName>
            </wprt:JobDescription>
            <wprt:DocumentDescription>
              <wprt:DocumentFormat>{fmt}</wprt:DocumentFormat>
            </wprt:DocumentDescription>
          </wprt:PrintTicket>"#,
        name = soap::esc(&job.name),
        user = soap::esc(&job.user),
        fmt = soap::esc(&job.document_format),
    )
}

pub fn get_job_elements_body(job: Option<&ActiveJob>, body: &str, job_id: u32) -> String {
    let names = requested_names(body);
    let mut elements = String::new();
    match job {
        Some(job) => {
            if want(&names, "JobStatus") {
                elements.push_str(&element_data(
                    &client_name(&names, "JobStatus"),
                    &job_status_xml(job),
                ));
            }
            if want(&names, "PrintTicket") {
                elements.push_str(&element_data(
                    &client_name(&names, "PrintTicket"),
                    &job_print_ticket_xml(job),
                ));
            }
        }
        None => {
            // Unknown job: Valid=false for each requested name (or JobStatus default).
            let report = if names.is_empty() {
                vec!["wprt:JobStatus".into()]
            } else {
                names.clone()
            };
            for n in report {
                elements.push_str(&format!(
                    r#"
        <wprt:ElementData Name="{n}" Valid="false"/>"#
                ));
            }
            let _ = job_id;
        }
    }
    format!(
        r#"    <wprt:GetJobElementsResponse>
      <wprt:JobElements>{elements}
      </wprt:JobElements>
    </wprt:GetJobElementsResponse>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_names_skips_attributes() {
        let body = r#"<wprt:RequestedElements>
          <wprt:Name>wprt:PrinterStatus</wprt:Name>
          <wprt:Name>wprt:DefaultPrintTicket</wprt:Name>
        </wprt:RequestedElements>
        <wprt:ElementData Name="wprt:PrinterStatus" Valid="true"/>"#;
        let names = requested_names(body);
        assert_eq!(names, vec!["wprt:PrinterStatus", "wprt:DefaultPrintTicket"]);
    }

    #[test]
    fn capabilities_use_job_and_document_values() {
        let p = Profile {
            id: "t".into(),
            label: "t".into(),
            manufacturer: "m".into(),
            model: "m".into(),
            device_id: "MFG:X;CLS:Printer;".into(),
            color_supported: true,
            pages_per_minute: 1,
            pages_per_minute_color: 1,
            document_formats: vec!["application/pdf".into()],
            multiple_document_jobs: false,
        };
        let xml = printer_capabilities_xml(&p);
        assert!(xml.contains("JobValues"));
        assert!(xml.contains("DocumentValues"));
        assert!(xml.contains("application/pdf"));
        assert!(xml.contains("na_letter_8.5x11in"));
        assert!(!xml.contains("DocumentFormatsSupported"));
        let ticket_only = get_printer_elements_body(
            &p,
            r#"<wprt:Name>wprt:DefaultPrintTicket</wprt:Name>"#,
            0,
        );
        assert!(ticket_only.contains("DefaultPrintTicket"));
        assert!(!ticket_only.contains("<wprt:PrinterElements>\n      </wprt:PrinterElements>"));
        let unknown = get_printer_elements_body(
            &p,
            r#"<wprt:Name>wprt:DriverConfiguration</wprt:Name>"#,
            0,
        );
        assert!(unknown.contains(r#"Name="wprt:DriverConfiguration" Valid="false""#));
        assert!(printer_description_xml(&p).contains("CLS:Printer"));
    }
}
