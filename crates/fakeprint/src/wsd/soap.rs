//! Minimal SOAP helpers for WS-Print / WS-Transfer / WS-Eventing.

use std::io::{Cursor, Write};

use base64::Engine;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;
use uuid::Uuid;

pub use super::envelope::Envelope;

/// Canonical WS-Print namespace (Windows WSDMON / WDP Print Service).
pub const PRINT_NS: &str = "http://schemas.microsoft.com/windows/2006/08/wdp/print";
pub const WSA_NS: &str = "http://schemas.xmlsoap.org/ws/2004/08/addressing";
pub const SOAP_NS: &str = "http://www.w3.org/2003/05/soap-envelope";
pub const WSE_NS: &str = "http://schemas.xmlsoap.org/ws/2004/08/eventing";

pub fn action(body: &str) -> Option<String> {
    Envelope::parse(body).action_uri(print_ops(), WSE_NS, PRINT_NS)
}

const fn print_ops() -> &'static [&'static str] {
    &[
        "GetPrinterElements",
        "CreatePrintJob",
        "SendDocument",
        "AddDocument",
        "CancelJob",
        "EndJob",
        "GetActiveJobs",
        "GetJobHistory",
        "GetJobElements",
        "ValidatePrintTicket",
        "SetEventRate",
        "SetPrinterElements",
        "GetPrintDeviceCapabilities",
        "GetPrintDeviceResources",
        "CreatePrintJob2",
        "PrepareToPrint",
        "GetBidiSchemaExtensions",
    ]
}

pub fn eventing_action(short: &str) -> String {
    format!("{WSE_NS}/{short}")
}

pub fn message_id(body: &str) -> Option<String> {
    // RelatesTo is xs:anyURI. A bare UUID or a sentence is not one, so it is not copied.
    Envelope::parse(body)
        .message_id
        .filter(|id| super::envelope::absolute_iri(id))
}

/// Header problems in a reply this service wrote (SOAP 1.2 + WS-Addressing 2004/08).
pub fn reply_problems(xml: &str) -> Vec<String> {
    Envelope::parse(xml).reply_problems(super::envelope::ReplyRole::Reply)
}

/// Print-body problems against the vendored WDP print XSD.
pub fn body_problems(xml: &str) -> Vec<String> {
    super::print_schema::body_problems(xml)
}

pub fn extract_tagged(xml: &str, local: &str) -> Option<String> {
    let mut idx = 0;
    while let Some(rel) = xml[idx..].find(local) {
        let at = idx + rel;
        let before = xml.as_bytes().get(at.wrapping_sub(1)).copied();
        let after = xml.get(at + local.len()..).unwrap_or("");
        if matches!(before, Some(b'<') | Some(b':')) && after.starts_with('>') {
            let value_start = &after[1..];
            if let Some(end) = value_start.find('<') {
                let value = value_start[..end].trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
        idx = at + local.len();
    }
    None
}

pub fn extract_u32(xml: &str, local: &str) -> Option<u32> {
    extract_tagged(xml, local)?.parse().ok()
}

pub fn envelope(action_uri: &str, relates_to: Option<&str>, body_xml: &str) -> String {
    let msg = Uuid::new_v4();
    // Both wprt: and prt: — some clients use the short print prefix.
    write_envelope(
        &[("xmlns:wprt", PRINT_NS), ("xmlns:prt", PRINT_NS)],
        action_uri,
        &format!("urn:uuid:{msg}"),
        relates_to,
        body_xml,
    )
}

fn write_envelope(
    extra_xmlns: &[(&str, &str)],
    action_uri: &str,
    message_id: &str,
    relates_to: Option<&str>,
    body_xml: &str,
) -> String {
    let mut writer = Writer::new(Cursor::new(Vec::new()));
    let _ = writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("utf-8"), None)));
    let mut env = BytesStart::new("soap:Envelope");
    env.push_attribute(("xmlns:soap", SOAP_NS));
    env.push_attribute(("xmlns:wsa", WSA_NS));
    for (name, value) in extra_xmlns {
        env.push_attribute((*name, *value));
    }
    let _ = writer.write_event(Event::Start(env));
    let _ = writer.write_event(Event::Start(BytesStart::new("soap:Header")));
    write_text_el(
        &mut writer,
        "wsa:To",
        "http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous",
    );
    write_text_el(&mut writer, "wsa:Action", action_uri);
    write_text_el(&mut writer, "wsa:MessageID", message_id);
    if let Some(relates) = relates_to.map(str::trim).filter(|s| !s.is_empty()) {
        write_text_el(&mut writer, "wsa:RelatesTo", relates);
    }
    let _ = writer.write_event(Event::End(BytesEnd::new("soap:Header")));
    let _ = writer.write_event(Event::Start(BytesStart::new("soap:Body")));
    // write_all advances the Cursor. extend_from_slice on the inner Vec does not,
    // so the closing tags would overwrite the start of the payload.
    let _ = writer.get_mut().write_all(body_xml.as_bytes());
    let _ = writer.write_event(Event::End(BytesEnd::new("soap:Body")));
    let _ = writer.write_event(Event::End(BytesEnd::new("soap:Envelope")));
    let xml = String::from_utf8(writer.into_inner().into_inner()).unwrap_or_default();
    debug_assert!(
        Envelope::parse(&xml)
            .reply_problems(super::envelope::ReplyRole::Reply)
            .is_empty(),
        "{xml}"
    );
    xml
}

fn write_text_el(writer: &mut Writer<Cursor<Vec<u8>>>, name: &str, text: &str) {
    let _ = writer.write_event(Event::Start(BytesStart::new(name)));
    let _ = writer.write_event(Event::Text(BytesText::new(text)));
    let _ = writer.write_event(Event::End(BytesEnd::new(name)));
}

pub fn fault(reason: &str, relates_to: Option<&str>) -> String {
    let reason = esc(reason);
    let body = format!(
        r#"    <soap:Fault>
      <soap:Code><soap:Value>soap:Sender</soap:Value></soap:Code>
      <soap:Reason><soap:Text xml:lang="en">{reason}</soap:Text></soap:Reason>
    </soap:Fault>"#
    );
    envelope(
        "http://schemas.xmlsoap.org/ws/2004/08/addressing/fault",
        relates_to,
        &body,
    )
}

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn short_action(action_uri: &str) -> &str {
    action_uri.rsplit('/').next().unwrap_or(action_uri)
}

pub fn print_action(short: &str) -> String {
    format!("{PRINT_NS}/{short}")
}

/// WS-Eventing Filter text (Action dialect URI) from a Subscribe body.
///
/// WSDMON sends `<wse:Filter Dialect=".../Action">URI</wse:Filter>` — attributes
/// mean `extract_tagged` (which requires `Filter>`) misses it.
pub fn subscribe_filter(body: &str) -> Option<String> {
    let mut idx = 0;
    while let Some(rel) = body[idx..].find("Filter") {
        let at = idx + rel;
        let before = body.as_bytes().get(at.wrapping_sub(1)).copied();
        if matches!(before, Some(b'<') | Some(b':')) {
            let rest = &body[at + "Filter".len()..];
            if let Some(gt) = rest.find('>') {
                let value_start = &rest[gt + 1..];
                if let Some(end) = value_start.find('<') {
                    let value = value_start[..end].trim();
                    if !value.is_empty() && !value.starts_with('/') {
                        return Some(value.to_string());
                    }
                }
            }
        }
        idx = at + "Filter".len();
    }
    None
}

/// Last path segment of the Subscribe Filter (`PrinterStatusSummaryEvent`, …).
pub fn subscribe_filter_short(body: &str) -> Option<String> {
    subscribe_filter(body).map(|f| {
        f.rsplit('/')
            .next()
            .unwrap_or(f.as_str())
            .trim()
            .to_string()
    })
}

pub fn eventing_expires(body: &str) -> String {
    if let Some(raw) = extract_tagged(body, "Expires") {
        let t = raw.trim();
        if t.starts_with('P') || t.starts_with('p') {
            return esc(t);
        }
    }
    "PT8H".into()
}

/// Pull document bytes from SendDocument: base64 DocumentData, xop:Include, or MTOM part.
pub fn extract_document_bytes(content_type: Option<&str>, body: &[u8]) -> Option<Vec<u8>> {
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    if ct.contains("multipart") || looks_like_multipart(body) {
        if let Some(bytes) = extract_mtom_binary(content_type, body) {
            return Some(bytes);
        }
    }
    let text = String::from_utf8_lossy(body);
    if let Some(b64) = extract_tagged(&text, "DocumentData") {
        // Empty DocumentData with xop:Include is common in MTOM SOAP part.
        let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
        if !cleaned.is_empty() {
            if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(&cleaned) {
                return Some(decoded);
            }
            // Some stacks omit padding; try URL_SAFE / no-pad.
            if let Ok(decoded) = base64::engine::general_purpose::STANDARD_NO_PAD.decode(&cleaned) {
                return Some(decoded);
            }
        }
    }
    if text.contains("xop:Include") || text.contains("Include href=") {
        if let Some(cid) = xop_cid(&text) {
            if let Some(bytes) = extract_part_by_content_id(body, &cid) {
                return Some(bytes);
            }
        }
    }
    // Last resort: if the body looks binary (not SOAP), keep it.
    if body.len() > 16
        && !body
            .windows(5)
            .any(|w| w == b"soap:" || w == b"SOAP:" || w == b"<?xml")
    {
        return Some(body.to_vec());
    }
    Some(Vec::new())
}

fn looks_like_multipart(body: &[u8]) -> bool {
    body.starts_with(b"--")
        || (find_slice(body, b"Content-Type:").is_some() && find_slice(body, b"\r\n--").is_some())
}

fn xop_cid(xml: &str) -> Option<String> {
    for marker in ["href=\"cid:", "href='cid:"] {
        if let Some(at) = xml.find(marker) {
            let rest = &xml[at + marker.len()..];
            let end = rest.find(['"', '\'', ' ', '>']).unwrap_or(rest.len());
            let cid = rest[..end]
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>');
            if !cid.is_empty() {
                return Some(cid.to_string());
            }
        }
    }
    None
}

fn extract_part_by_content_id(body: &[u8], cid: &str) -> Option<Vec<u8>> {
    let cid_l = cid.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(rel) = find_slice(&body[search_from..], b"\r\n\r\n") {
        let header_end = search_from + rel;
        let header = &body[search_from..header_end];
        let header_l = String::from_utf8_lossy(header).to_ascii_lowercase();
        let data_start = header_end + 4;
        let matches_cid = header_l.contains("content-id:")
            && (header_l.contains(&cid_l)
                || header_l.contains(&format!("<{cid_l}>"))
                || header_l.contains(&format!("cid:{cid_l}")));
        if matches_cid {
            let rest = &body[data_start..];
            if let Some(end) = find_slice(rest, b"\r\n--") {
                return Some(rest[..end].to_vec());
            }
            return Some(rest.to_vec());
        }
        search_from = data_start;
        if search_from >= body.len() {
            break;
        }
    }
    None
}

fn extract_mtom_binary(content_type: Option<&str>, body: &[u8]) -> Option<Vec<u8>> {
    // Prefer xop:Include cid when the SOAP XML part names it.
    if let Ok(text) = std::str::from_utf8(body) {
        // Multipart may have binary; search only the XML-looking prefix.
        let preview = if body.len() > 64 * 1024 {
            String::from_utf8_lossy(&body[..64 * 1024])
        } else {
            text.into()
        };
        if let Some(cid) = xop_cid(&preview) {
            if let Some(bytes) = extract_part_by_content_id(body, &cid) {
                return Some(bytes);
            }
        }
    }

    let boundary = multipart_boundary(content_type);
    let marker = b"\r\n\r\n";
    let mut search_from = 0;
    let mut found_xml = false;
    let mut best: Option<Vec<u8>> = None;

    while let Some(rel) = find_slice(&body[search_from..], marker) {
        let data_start = search_from + rel + marker.len();
        let header = &body[search_from..search_from + rel];
        let header_l = String::from_utf8_lossy(header).to_ascii_lowercase();
        if header_l.contains("application/xop+xml") || header_l.contains("application/soap") {
            found_xml = true;
            search_from = data_start;
            continue;
        }
        let binary_hint = header_l.contains("application/octet-stream")
            || header_l.contains("application/pdf")
            || header_l.contains("image/")
            || header_l.contains("application/postscript")
            || header_l.contains("application/vnd.hp")
            || header_l.contains("content-transfer-encoding: binary")
            || (found_xml && !header_l.contains("text/xml"));
        if binary_hint || found_xml {
            let rest = &body[data_start..];
            let chunk = if let Some(end) = find_slice(rest, b"\r\n--") {
                rest[..end].to_vec()
            } else if let Some(b) = boundary.as_ref() {
                let delim = format!("\r\n--{b}");
                find_slice(rest, delim.as_bytes())
                    .map(|end| rest[..end].to_vec())
                    .unwrap_or_else(|| rest.to_vec())
            } else {
                rest.to_vec()
            };
            if chunk.len() > best.as_ref().map(|b| b.len()).unwrap_or(0) {
                best = Some(chunk);
            }
        }
        search_from = data_start;
        if search_from >= body.len() {
            break;
        }
    }
    best
}

fn multipart_boundary(content_type: Option<&str>) -> Option<String> {
    let ct = content_type?;
    let lower = ct.to_ascii_lowercase();
    let key = "boundary=";
    let at = lower.find(key)?;
    let mut raw = ct[at + key.len()..].trim();
    if let Some(semi) = raw.find(';') {
        raw = &raw[..semi];
    }
    raw = raw.trim().trim_matches('"').trim_matches('\'');
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_string())
    }
}

fn find_slice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_keeps_body_inside_soap_body() {
        let xml = envelope(
            "http://schemas.microsoft.com/windows/2006/08/wdp/print/GetPrinterElementsResponse",
            Some("urn:uuid:abc"),
            "<wprt:PrinterElements/>",
        );
        assert!(
            xml.contains("<soap:Body><wprt:PrinterElements/></soap:Body>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wsa:RelatesTo>urn:uuid:abc</wsa:RelatesTo>"),
            "{xml}"
        );
        assert!(reply_problems(&xml).is_empty(), "{xml}");
    }

    #[test]
    fn replies_mint_a_fresh_message_id() {
        let one = envelope(
            "http://schemas.microsoft.com/windows/2006/08/wdp/print/GetPrinterElementsResponse",
            None,
            "<wprt:PrinterElements/>",
        );
        let two = envelope(
            "http://schemas.microsoft.com/windows/2006/08/wdp/print/GetPrinterElementsResponse",
            None,
            "<wprt:PrinterElements/>",
        );
        let id = |xml: &str| Envelope::parse(xml).message_id.unwrap();
        assert_ne!(id(&one), id(&two));
        assert!(id(&one).starts_with("urn:uuid:"));
        assert!(reply_problems(&one).is_empty(), "{one}");
    }

    #[test]
    fn bare_uuid_is_not_copied_to_relates_to() {
        let body = r#"<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope">
          <soap:Header><wsa:MessageID>bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb</wsa:MessageID></soap:Header>
          <soap:Body/>
        </soap:Envelope>"#;
        assert!(message_id(body).is_none());
    }

    #[test]
    fn detects_get_printer_elements() {
        let body = r#"<wsa:Action>http://schemas.microsoft.com/windows/2006/08/wdp/print/GetPrinterElements</wsa:Action>"#;
        assert_eq!(
            action(body).as_deref(),
            Some("http://schemas.microsoft.com/windows/2006/08/wdp/print/GetPrinterElements")
        );
    }

    #[test]
    fn extract_base64_document() {
        let body = r#"<wprt:DocumentData>JVBERi0=</wprt:DocumentData>"#; // "%PDF"
        let bytes = extract_document_bytes(Some("application/soap+xml"), body.as_bytes()).unwrap();
        assert_eq!(&bytes[..4], b"%PDF");
    }

    #[test]
    fn extract_mtom_with_xop_cid() {
        let pdf = b"%PDF-1.4 mtom";
        let mut body = Vec::new();
        body.extend_from_slice(b"--uuid:bound\r\n");
        body.extend_from_slice(
            b"Content-Type: application/xop+xml; charset=utf-8; type=\"application/soap+xml\"\r\n\r\n",
        );
        body.extend_from_slice(
            br#"<soap:Envelope><soap:Body><wprt:DocumentData>
            <xop:Include href="cid:doc-1@fakeprint"/>
            </wprt:DocumentData></soap:Body></soap:Envelope>"#,
        );
        body.extend_from_slice(b"\r\n--uuid:bound\r\n");
        body.extend_from_slice(b"Content-Type: application/octet-stream\r\n");
        body.extend_from_slice(b"Content-ID: <doc-1@fakeprint>\r\n");
        body.extend_from_slice(b"Content-Transfer-Encoding: binary\r\n\r\n");
        body.extend_from_slice(pdf);
        body.extend_from_slice(b"\r\n--uuid:bound--\r\n");

        let ct = "multipart/related; boundary=\"uuid:bound\"; type=\"application/xop+xml\"";
        let bytes = extract_document_bytes(Some(ct), &body).unwrap();
        assert_eq!(bytes, pdf);
    }

    #[test]
    fn subscribe_filter_short_from_action_dialect() {
        let body = r#"<wse:Subscribe>
          <wse:Delivery><wse:NotifyTo>
            <wsa:Address>http://192.0.2.1:5357/WsdPrint/x</wsa:Address>
          </wse:NotifyTo></wse:Delivery>
          <wse:Filter Dialect="http://schemas.xmlsoap.org/ws/2006/02/devprof/Action">
            http://schemas.microsoft.com/windows/2006/08/wdp/print/PrinterStatusSummaryEvent
          </wse:Filter>
        </wse:Subscribe>"#;
        assert_eq!(
            subscribe_filter_short(body).as_deref(),
            Some("PrinterStatusSummaryEvent")
        );
        assert_eq!(subscribe_filter_short("<wse:Subscribe/>"), None);
    }

    #[test]
    fn detects_validate_print_ticket_without_action() {
        let body = r#"<wprt:ValidatePrintTicketRequest><wprt:PrintTicket/></wprt:ValidatePrintTicketRequest>"#;
        assert_eq!(
            action(body).as_deref(),
            Some("http://schemas.microsoft.com/windows/2006/08/wdp/print/ValidatePrintTicket")
        );
    }
}
