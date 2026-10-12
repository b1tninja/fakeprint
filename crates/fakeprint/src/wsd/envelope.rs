//! SOAP 1.2 envelope. Header fields are parsed; the body markup is kept as it arrived.

use quick_xml::events::Event;
use quick_xml::reader::Reader;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Envelope {
    pub action: Option<String>,
    pub message_id: Option<String>,
    pub relates_to: Option<String>,
    pub to: Option<String>,
    /// `wse:Identifier` outside the body (header `ReferenceParameters`).
    pub identifier: Option<String>,
    /// Local name of the first `soap:Body` child (`CreateScanJobRequest`).
    pub body_name: Option<String>,
    /// Document element when the input is not wrapped in `soap:Envelope`.
    pub root: Option<String>,
    /// Markup inside `soap:Body`, unchanged.
    pub body: String,
    /// `xmlns` values declared on the envelope and its header.
    pub xmlns: Vec<String>,
    /// Direct children of `soap:Header`.
    pub headers: Vec<HeaderBlock>,
}

/// One header block. `must_understand` is the SOAP 1.2 attribute (`true` or `1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderBlock {
    pub name: String,
    pub must_understand: bool,
}

/// Who the envelope is speaking to. A reply uses the anonymous role; an event uses `NotifyTo`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReplyRole {
    Reply,
    Event,
}

/// What a header value does, and whether a bad value stops the exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub field: &'static str,
    /// `true` when SOAP says the receiver must fault.
    pub reject: bool,
    pub note: &'static str,
}

/// Anonymous role on a SOAP response (`wsa:To`).
pub const ANONYMOUS_ROLE: &str = "http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous";

const SOAP_NS: &str = "http://www.w3.org/2003/05/soap-envelope";
const WSA_NS: &str = "http://schemas.xmlsoap.org/ws/2004/08/addressing";

/// Header blocks this service understands. `mustUnderstand="true"` on any other faults.
const UNDERSTOOD: &[&str] = &[
    "Action",
    "MessageID",
    "MessageId",
    "RelatesTo",
    "To",
    "ReplyTo",
    "From",
    "FaultTo",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Capture {
    Action,
    MessageId,
    RelatesTo,
    To,
    Identifier,
}

impl Envelope {
    /// Parse `input`, or the first SOAP envelope inside it (an MTOM part).
    /// A truncated or non-XML tail is ignored once the header fields are read.
    pub fn parse(input: &str) -> Self {
        let xml = envelope_slice(input);
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(false);
        let mut buf = Vec::new();
        let mut out = Envelope::default();
        let mut stack: Vec<String> = Vec::new();
        let mut capture: Option<Capture> = None;
        let mut text = String::new();
        let mut in_body = false;
        let mut body_at: Option<u64> = None;

        while let Ok(event) = reader.read_event_into(&mut buf) {
            match event {
                Event::Start(ref el) => {
                    let local = local_name(el.local_name().as_ref());
                    note_xmlns(&mut out, el);
                    if stack.is_empty() {
                        out.root = Some(local.clone());
                    }
                    if stack.len() == 2 && stack[0] == "Envelope" && stack[1] == "Header" {
                        out.headers.push(HeaderBlock {
                            name: local.clone(),
                            must_understand: must_understand(el),
                        });
                    }
                    if local == "Body" && stack.iter().any(|n| n == "Envelope") {
                        in_body = true;
                        body_at = Some(reader.buffer_position());
                    } else if in_body && out.body_name.is_none() {
                        out.body_name = Some(local.clone());
                    }
                    capture = begin_capture(&stack, in_body, &local, &mut text);
                    stack.push(local);
                }
                Event::Empty(ref el) => {
                    let local = local_name(el.local_name().as_ref());
                    note_xmlns(&mut out, el);
                    if stack.len() == 2 && stack[0] == "Envelope" && stack[1] == "Header" {
                        out.headers.push(HeaderBlock {
                            name: local.clone(),
                            must_understand: must_understand(el),
                        });
                    }
                    if local == "Body" && stack.iter().any(|n| n == "Envelope") {
                        in_body = false;
                        out.body.clear();
                    } else if in_body && out.body_name.is_none() {
                        out.body_name = Some(local);
                    }
                }
                Event::Text(ref el) => {
                    if capture.is_some() {
                        if let Ok(value) = el.unescape() {
                            text.push_str(&value);
                        }
                    }
                }
                Event::CData(ref el) => {
                    if capture.is_some() {
                        text.push_str(&String::from_utf8_lossy(el));
                    }
                }
                Event::End(ref el) => {
                    let local = local_name(el.local_name().as_ref());
                    if let Some(kind) = capture {
                        if stack.last().is_some_and(|top| top == &local) {
                            store(&mut out, kind, &text);
                            capture = None;
                            text.clear();
                        }
                    }
                    if local == "Body" {
                        if let Some(start) = body_at {
                            let end_pos = reader.buffer_position() as usize;
                            let start = start as usize;
                            let bytes = xml.as_bytes();
                            let tag = bytes[..end_pos.min(bytes.len())]
                                .iter()
                                .rposition(|b| *b == b'<')
                                .unwrap_or(end_pos);
                            if start <= tag && tag <= xml.len() {
                                out.body = xml[start..tag].to_string();
                            }
                        }
                        in_body = false;
                        body_at = None;
                    }
                    if stack.last().is_some_and(|top| top == &local) {
                        stack.pop();
                    }
                    if local == "Envelope" {
                        break;
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        out.action = out.action.filter(|s| !s.is_empty());
        out.message_id = out.message_id.filter(|s| !s.is_empty());
        out.relates_to = out.relates_to.filter(|s| !s.is_empty());
        out.to = out.to.filter(|s| !s.is_empty());
        out.identifier = out.identifier.filter(|s| !s.is_empty());
        out
    }

    /// Header `Action`, or the body element's operation when the header omitted it.
    pub fn action_uri(&self, ops: &[&str], event_ns: &str, default_ns: &str) -> Option<String> {
        // Action is an absolute IRI. A short name is not one, so the body element names the operation.
        if let Some(action) = &self.action {
            if absolute_iri(action) {
                return Some(action.clone());
            }
        }
        let name = self.body_name.as_deref().or_else(|| {
            self.root
                .as_deref()
                .filter(|n| !matches!(*n, "Envelope" | "Header" | "Body"))
        })?;
        let op = name.strip_suffix("Request").unwrap_or(name);
        if ops.contains(&op) {
            let ns = self
                .xmlns
                .iter()
                .find(|ns| ns.contains("/2006/08/wdp/"))
                .or_else(|| self.xmlns.iter().find(|ns| ns.contains("/2006/01/wdp/")))
                .map(String::as_str)
                .unwrap_or(default_ns);
            return Some(format!("{ns}/{op}"));
        }
        if op == "Get" {
            return Some("http://schemas.xmlsoap.org/ws/2004/09/transfer/Get".into());
        }
        if matches!(op, "Subscribe" | "Unsubscribe" | "Renew" | "GetStatus") {
            return Some(format!("{event_ns}/{op}"));
        }
        None
    }

    /// Inbound header notes. `reject` is only an unknown `mustUnderstand` block.
    pub fn request_findings(&self) -> Vec<Finding> {
        let mut out = Vec::new();
        match &self.action {
            Some(action) if !absolute_iri(action) => out.push(Finding {
                field: "Action",
                reject: false,
                note: "not an absolute IRI; the body element names the operation",
            }),
            None => out.push(Finding {
                field: "Action",
                reject: false,
                note: "absent; the body element names the operation",
            }),
            _ => {}
        }
        match &self.message_id {
            Some(id) if !absolute_iri(id) => out.push(Finding {
                field: "MessageID",
                reject: false,
                note: "not an absolute IRI; RelatesTo is omitted",
            }),
            None => out.push(Finding {
                field: "MessageID",
                reject: false,
                note: "absent; RelatesTo is omitted. The HTTP response is still this request's reply",
            }),
            _ => {}
        }
        if self.to.as_deref().is_some_and(|to| !absolute_iri(to)) {
            out.push(Finding {
                field: "To",
                reject: false,
                note: "not an absolute IRI; HTTP already delivered the request",
            });
        }
        if self.relates_to.is_some() {
            out.push(Finding {
                field: "RelatesTo",
                reject: false,
                note: "a request RelatesTo is not the id the reply uses",
            });
        }
        for header in &self.headers {
            if header.must_understand && !UNDERSTOOD.contains(&header.name.as_str()) {
                out.push(Finding {
                    field: "mustUnderstand",
                    reject: true,
                    note: "unknown header block with mustUnderstand",
                });
            }
        }
        out
    }

    /// Problems in an envelope this service wrote. Empty means the header matches the schema.
    pub fn reply_problems(&self, role: ReplyRole) -> Vec<String> {
        let mut problems = Vec::new();
        if self.root.as_deref() != Some("Envelope") {
            problems.push("document element is not soap:Envelope".into());
        }
        if !self.xmlns.iter().any(|ns| ns == SOAP_NS) {
            problems.push("missing xmlns for the SOAP 1.2 envelope".into());
        }
        if !self.xmlns.iter().any(|ns| ns == WSA_NS) {
            problems.push("missing xmlns for WS-Addressing 2004/08".into());
        }
        if self.action.as_deref().is_none_or(|action| !absolute_iri(action)) {
            problems.push("Action must be an absolute IRI".into());
        }
        if self.message_id.as_deref().is_none_or(|id| !reply_message_id(id)) {
            problems.push(
                "MessageID must be a fresh uuid: or urn:uuid: IRI, not a reused fixed value".into(),
            );
        }
        match role {
            ReplyRole::Reply => {
                if self.to.as_deref() != Some(ANONYMOUS_ROLE) {
                    problems.push("reply To must be the anonymous role".into());
                }
            }
            ReplyRole::Event => {
                if self.to.as_deref().is_none_or(|to| !absolute_iri(to)) {
                    problems.push("event To must be an absolute IRI (NotifyTo)".into());
                }
            }
        }
        if self.relates_to.as_deref().is_some_and(|id| !absolute_iri(id)) {
            problems.push("RelatesTo must be an absolute IRI".into());
        }
        problems
    }
}

/// `xs:anyURI` that is an absolute IRI: a scheme, no whitespace.
pub fn absolute_iri(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        return false;
    }
    let Some((scheme, rest)) = value.split_once(':') else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    let mut chars = scheme.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Message id this service emits: `uuid:{uuid}` or `urn:uuid:{uuid}`.
pub fn reply_message_id(value: &str) -> bool {
    let bare = value
        .strip_prefix("urn:uuid:")
        .or_else(|| value.strip_prefix("uuid:"));
    bare.is_some_and(is_uuid)
}

fn is_uuid(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(parts)
            .all(|(n, part)| part.len() == *n && part.chars().all(|c| c.is_ascii_hexdigit()))
}

fn must_understand(el: &quick_xml::events::BytesStart<'_>) -> bool {
    el.attributes().flatten().any(|attr| {
        if attr.key.local_name().as_ref() != b"mustUnderstand" {
            return false;
        }
        matches!(attr.unescape_value().as_deref(), Ok("true") | Ok("1"))
    })
}

fn begin_capture(
    stack: &[String],
    in_body: bool,
    local: &str,
    text: &mut String,
) -> Option<Capture> {
    if in_body {
        return None;
    }
    let kind = match local {
        "Action" => Capture::Action,
        "MessageID" | "MessageId" => Capture::MessageId,
        "RelatesTo" => Capture::RelatesTo,
        "To" => Capture::To,
        "Identifier" if stack.iter().any(|n| n == "ReferenceParameters") => Capture::Identifier,
        _ => return None,
    };
    text.clear();
    Some(kind)
}

fn store(out: &mut Envelope, kind: Capture, text: &str) {
    let value = text.trim().to_string();
    match kind {
        Capture::Action => out.action = Some(value),
        Capture::MessageId => out.message_id = Some(value),
        Capture::RelatesTo => out.relates_to = Some(value),
        Capture::To => out.to = Some(value),
        Capture::Identifier => out.identifier = Some(value),
    }
}

fn note_xmlns(out: &mut Envelope, el: &quick_xml::events::BytesStart<'_>) {
    for attr in el.attributes().flatten() {
        if !attr.key.as_ref().starts_with(b"xmlns") {
            continue;
        }
        if let Ok(value) = attr.unescape_value() {
            let value = value.into_owned();
            if !out.xmlns.iter().any(|v| v == &value) {
                out.xmlns.push(value);
            }
        }
    }
}

fn local_name(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).into_owned()
}

/// Byte offset of the first `<*Envelope` start tag, if one exists.
fn envelope_slice(input: &str) -> &str {
    let bytes = input.as_bytes();
    let mut idx = 0;
    while let Some(rel) = input[idx..].find("Envelope") {
        let at = idx + rel;
        let before = bytes.get(at.wrapping_sub(1)).copied();
        if matches!(before, Some(b'<') | Some(b':')) {
            if let Some(start) = input[..at].rfind('<') {
                return &input[start..];
            }
        }
        idx = at + "Envelope".len();
    }
    input
}
