//! Minimal SOAP helpers for WS-Print / WS-Transfer / WS-Eventing.

use base64::Engine;
use uuid::Uuid;

/// Canonical WS-Print namespace (Windows WSDMON / WDP Print Service).
pub const PRINT_NS: &str = "http://schemas.microsoft.com/windows/2006/08/wdp/print";
pub const WSA_NS: &str = "http://schemas.xmlsoap.org/ws/2004/08/addressing";
pub const SOAP_NS: &str = "http://www.w3.org/2003/05/soap-envelope";
pub const WSE_NS: &str = "http://schemas.xmlsoap.org/ws/2004/08/eventing";

pub fn action(body: &str) -> Option<String> {
    if let Some(v) = extract_tagged(body, "Action") {
        return Some(v);
    }
    for marker in [
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
    ] {
        if body.contains(&format!("{marker}Request")) || body.contains(&format!("/{marker}")) {
            return Some(format!("{PRINT_NS}/{marker}"));
        }
    }
    if body.contains("transfer/Get") || (body.contains(":Get>") && body.contains("Transfer")) {
        return Some("http://schemas.xmlsoap.org/ws/2004/09/transfer/Get".into());
    }
    if body.contains("wse:Subscribe") || body.contains(":Subscribe>") {
        return Some(format!("{WSE_NS}/Subscribe"));
    }
    if body.contains("wse:Unsubscribe") || body.contains(":Unsubscribe>") {
        return Some(format!("{WSE_NS}/Unsubscribe"));
    }
    if body.contains("wse:Renew") || body.contains(":Renew>") {
        return Some(format!("{WSE_NS}/Renew"));
    }
    if body.contains("wse:GetStatus") || body.contains(":GetStatus>") {
        return Some(format!("{WSE_NS}/GetStatus"));
    }
    None
}

pub fn eventing_action(short: &str) -> String {
    format!("{WSE_NS}/{short}")
}

pub fn message_id(body: &str) -> Option<String> {
    extract_tagged(body, "MessageID").or_else(|| extract_tagged(body, "MessageId"))
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
    let relates = relates_to
        .map(|r| format!("    <wsa:RelatesTo>{}</wsa:RelatesTo>\n", esc(r.trim())))
        .unwrap_or_default();
    // Declare both wprt: and prt: — some clients use the short print prefix.
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<soap:Envelope xmlns:soap="{SOAP_NS}" xmlns:wsa="{WSA_NS}" xmlns:wprt="{PRINT_NS}" xmlns:prt="{PRINT_NS}">
  <soap:Header>
    <wsa:To>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</wsa:To>
    <wsa:Action>{action_uri}</wsa:Action>
    <wsa:MessageID>urn:uuid:{msg}</wsa:MessageID>
{relates}  </soap:Header>
  <soap:Body>
{body_xml}
  </soap:Body>
</soap:Envelope>"#
    )
}

pub fn fault(reason: &str) -> String {
    let reason = esc(reason);
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<soap:Envelope xmlns:soap="{SOAP_NS}" xmlns:wsa="{WSA_NS}">
  <soap:Header>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2004/08/addressing/fault</wsa:Action>
  </soap:Header>
  <soap:Body>
    <soap:Fault>
      <soap:Code><soap:Value>soap:Sender</soap:Value></soap:Code>
      <soap:Reason><soap:Text xml:lang="en">{reason}</soap:Text></soap:Reason>
    </soap:Fault>
  </soap:Body>
</soap:Envelope>"#
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
    if !body
        .windows(5)
        .any(|w| w == b"soap:" || w == b"SOAP:" || w == b"<?xml")
    {
        if body.len() > 16 {
            return Some(body.to_vec());
        }
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
            let end = rest
                .find(['"', '\'', ' ', '>'])
                .unwrap_or(rest.len());
            let cid = rest[..end].trim().trim_start_matches('<').trim_end_matches('>');
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
    fn detects_validate_print_ticket_without_action() {
        let body = r#"<wprt:ValidatePrintTicketRequest><wprt:PrintTicket/></wprt:ValidatePrintTicketRequest>"#;
        assert_eq!(
            action(body).as_deref(),
            Some("http://schemas.microsoft.com/windows/2006/08/wdp/print/ValidatePrintTicket")
        );
    }
}
