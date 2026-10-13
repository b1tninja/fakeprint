//! SOAP 1.2 request and reply handling for WS-Print, WS-Transfer and WS-Eventing.
//!
//! * [`SoapRequest::parse`] takes the HTTP `Content-Type`, the `SOAPAction` header and the raw
//!   body. It handles plain `application/soap+xml` and MTOM/XOP `multipart/related`, parses
//!   the envelope strictly (see [`super::envelope`]), requires `wsa:Action`, and checks that
//!   every `xop:Include` names a part.
//! * [`SoapReply`] builds a reply, a fault or a notification: WS-Addressing headers in a fixed
//!   order (To, Action, MessageID, RelatesTo, reference blocks, extra blocks), a fresh
//!   `urn:uuid:` MessageID, optional MTOM attachments. [`SoapReply::into_http`] yields status,
//!   content type and body bytes.

use std::borrow::Cow;

use uuid::Uuid;

use super::envelope::{
    self, default_understood, Addressing, EndpointRef, SoapEnvelope, ANONYMOUS_ROLE,
};
use super::fault::{FaultKind, PrintFault};
use super::mtom::{self, ContentType, MtomError, MtomErrorKind, MtomLimits, Multipart, OutPart};
use super::xml::{self, ns, Element, PrefixMap, QName, XmlLimits};

/// Canonical WS-Print namespace (Windows WSDMON / WDP Print Service).
pub const PRINT_NS: &str = ns::PRINT;
pub const WSA_NS: &str = ns::WSA;
pub const SOAP_NS: &str = ns::SOAP12;
pub const WSE_NS: &str = ns::WSE;

/// `Content-Type` of a plain SOAP 1.2 reply.
pub const SOAP_CONTENT_TYPE: &str = "application/soap+xml; charset=utf-8";

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// Caps applied while reading a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestLimits {
    /// XML limits for an MTOM root part (the envelope without the document).
    pub xml_root: XmlLimits,
    /// XML limits for a single-part body (may carry an inline base64 document).
    pub xml_inline: XmlLimits,
    pub mtom: MtomLimits,
    /// Largest decoded `DocumentData`.
    pub max_document: usize,
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            xml_root: XmlLimits::SMALL,
            xml_inline: XmlLimits::INLINE_DOCUMENT,
            mtom: MtomLimits::default(),
            max_document: 256 << 20,
        }
    }
}

/// A parsed, validated SOAP 1.2 request.
#[derive(Debug, Clone)]
pub struct SoapRequest {
    pub envelope: SoapEnvelope,
    /// `wsa:Action` (always present).
    pub action: String,
    /// `SOAPAction` HTTP header, or the `action` parameter of the content type.
    pub soap_action: Option<String>,
    /// The MTOM package when the request was `multipart/related`.
    pub multipart: Option<Multipart>,
    limits: RequestLimits,
}

impl SoapRequest {
    /// Parse with the default limits.
    pub fn parse(
        content_type: Option<&str>,
        soap_action: Option<&str>,
        body: &[u8],
    ) -> Result<SoapRequest, PrintFault> {
        Self::parse_with(content_type, soap_action, body, &RequestLimits::default())
    }

    /// Parse with explicit limits. The error is the fault to send back.
    pub fn parse_with(
        content_type: Option<&str>,
        soap_action: Option<&str>,
        body: &[u8],
        limits: &RequestLimits,
    ) -> Result<SoapRequest, PrintFault> {
        let ct = match content_type.map(str::trim).filter(|c| !c.is_empty()) {
            Some(raw) => ContentType::parse(raw).map_err(mtom_fault)?,
            None => ContentType::parse("application/soap+xml").map_err(mtom_fault)?,
        };
        let mut multipart = None;
        let root = if ct.is_multipart_related() {
            let mp = Multipart::parse(&ct, body, &limits.mtom).map_err(mtom_fault)?;
            let part = mp.root_part();
            let charset = part.content_type.as_ref().and_then(|c| c.charset());
            let text = xml::decode_text(&part.data, charset).map_err(xml_fault)?;
            let root = xml::parse_str(&text, &limits.xml_root).map_err(xml_fault)?;
            multipart = Some(mp);
            root
        } else {
            match ct.essence.as_str() {
                "application/soap+xml" | "application/xop+xml" | "application/xml" | "text/xml" => {
                }
                other => {
                    return Err(PrintFault::malformed(format!(
                        "unsupported media type {other}; SOAP 1.2 needs application/soap+xml"
                    )))
                }
            }
            if body.len() > limits.xml_inline.max_input {
                return Err(PrintFault::malformed("request body too large"));
            }
            let text = xml::decode_text(body, ct.charset()).map_err(xml_fault)?;
            xml::parse_str(&text, &limits.xml_inline).map_err(xml_fault)?
        };
        let envelope = SoapEnvelope::from_element(&root)?;
        let action = envelope
            .addressing
            .action
            .clone()
            .ok_or_else(|| PrintFault::header_required("Action"))?;

        let header_action = soap_action
            .map(|s| s.trim().trim_matches('"').trim().to_string())
            .filter(|s| !s.is_empty());
        let soap_action = header_action.or_else(|| ct.param("action").map(str::to_string));

        let req = SoapRequest {
            envelope,
            action,
            soap_action,
            multipart,
            limits: *limits,
        };
        req.check_includes()?;
        Ok(req)
    }

    /// Every `xop:Include` must have an `href` that names a part of this package.
    fn check_includes(&self) -> Result<(), PrintFault> {
        for body in &self.envelope.body {
            for el in body.descendants() {
                if el.is(ns::XOP, "Include") {
                    let href = el
                        .attr_local("href")
                        .ok_or_else(|| PrintFault::invalid_args("xop:Include without href"))?;
                    match &self.multipart {
                        Some(mp) => {
                            mp.resolve_href(href).map_err(mtom_fault)?;
                        }
                        None => {
                            return Err(PrintFault::invalid_args(
                                "xop:Include in a message that is not multipart/related",
                            ))
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn addressing(&self) -> &Addressing {
        &self.envelope.addressing
    }

    /// The request's `MessageID`, when it has a usable one.
    pub fn message_id(&self) -> Option<&str> {
        self.envelope.addressing.message_id.as_deref()
    }

    /// First `Body` child.
    pub fn body(&self) -> Option<&Element> {
        self.envelope.body_first()
    }

    /// Header block by name.
    pub fn header(&self, name: &QName) -> Option<&Element> {
        self.envelope.header(name)
    }

    /// Where the reply goes: `ReplyTo`, else anonymous.
    pub fn reply_destination(&self) -> EndpointRef {
        self.envelope
            .addressing
            .reply_to
            .clone()
            .unwrap_or_else(EndpointRef::anonymous)
    }

    /// Where a fault goes: `FaultTo`, else `ReplyTo`, else anonymous.
    pub fn fault_destination(&self) -> EndpointRef {
        self.envelope
            .addressing
            .fault_to
            .clone()
            .or_else(|| self.envelope.addressing.reply_to.clone())
            .unwrap_or_else(EndpointRef::anonymous)
    }

    /// `SOAPAction`/content-type action when it disagrees with `wsa:Action`.
    pub fn soap_action_mismatch(&self) -> Option<&str> {
        self.soap_action
            .as_deref()
            .filter(|s| *s != self.action.as_str())
    }

    /// Fault with `soap:MustUnderstand` when a mandatory header block is not in
    /// `extra_understood` or the defaults (WS-Addressing blocks, `wse:Identifier`).
    pub fn check_must_understand(&self, extra_understood: &[QName]) -> Result<(), PrintFault> {
        let mut known = default_understood();
        known.extend_from_slice(extra_understood);
        let bad = self.envelope.unhandled_must_understand(&known);
        if bad.is_empty() {
            Ok(())
        } else {
            Err(PrintFault::must_understand(&bad))
        }
    }

    /// Bytes of a `base64Binary` element: the target of its single `xop:Include`, or the
    /// decoded text. XOP content is borrowed from the package, untouched.
    pub fn binary_content<'a>(&'a self, el: &'a Element) -> Result<Cow<'a, [u8]>, PrintFault> {
        let includes: Vec<&Element> = el
            .children
            .iter()
            .filter(|c| c.is(ns::XOP, "Include"))
            .collect();
        if let Some(inc) = includes.first() {
            if includes.len() != 1 || el.children.len() != 1 || !el.text.trim().is_empty() {
                return Err(PrintFault::invalid_args(
                    "an element with xop:Include must contain only that one include",
                ));
            }
            let href = inc
                .attr_local("href")
                .ok_or_else(|| PrintFault::invalid_args("xop:Include without href"))?;
            let mp = self.multipart.as_ref().ok_or_else(|| {
                PrintFault::invalid_args("xop:Include in a message that is not multipart/related")
            })?;
            let part = mp.resolve_href(href).map_err(mtom_fault)?;
            if part.data.len() > self.limits.max_document {
                return Err(PrintFault::malformed("document too large"));
            }
            return Ok(Cow::Borrowed(part.data.as_slice()));
        }
        if !el.children.is_empty() {
            return Err(PrintFault::invalid_args(
                "binary element has child elements",
            ));
        }
        let data = mtom::decode_base64(&el.text, self.limits.max_document).map_err(mtom_fault)?;
        if data.is_empty() {
            return Err(PrintFault::invalid_args("empty binary content"));
        }
        Ok(Cow::Owned(data))
    }

    /// `DocumentData` of a `SendDocumentRequest`/`AddDocumentRequest`, as bytes.
    pub fn document_data(&self) -> Result<Cow<'_, [u8]>, PrintFault> {
        let body = self
            .body()
            .ok_or_else(|| PrintFault::invalid_args("empty body"))?;
        let el = body
            .child(ns::PRINT, "DocumentData")
            .ok_or_else(|| PrintFault::invalid_args("missing DocumentData"))?;
        self.binary_content(el)
    }

    /// A reply addressed per `ReplyTo`, correlated with this request.
    pub fn reply(&self, action: &str, body: Element) -> SoapReply {
        SoapReply::new(action, body)
            .relates_to(self.message_id())
            .to_endpoint(&self.reply_destination())
    }

    /// A reply with an empty body.
    pub fn reply_empty(&self, action: &str) -> SoapReply {
        SoapReply::empty(action)
            .relates_to(self.message_id())
            .to_endpoint(&self.reply_destination())
    }

    /// A fault reply, addressed per `FaultTo`.
    pub fn fault_reply(&self, fault: &PrintFault) -> SoapReply {
        fault
            .to_reply(self.message_id())
            .to_endpoint(&self.fault_destination())
    }
}

fn xml_fault(e: xml::XmlError) -> PrintFault {
    PrintFault::malformed(format!("not well-formed XML: {e}"))
}

fn mtom_fault(e: MtomError) -> PrintFault {
    match e.kind {
        MtomErrorKind::Cid => PrintFault::invalid_args(e.to_string()),
        _ => PrintFault::malformed(e.to_string()),
    }
}

// ---------------------------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------------------------

/// What goes on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpReply {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// An outgoing SOAP message: reply, fault or notification.
#[derive(Debug, Clone)]
pub struct SoapReply {
    pub status: u16,
    /// `wsa:To`; the anonymous role for ordinary replies.
    pub to: String,
    pub action: String,
    /// Fresh `urn:uuid:` unless overridden.
    pub message_id: String,
    pub relates_to: Option<String>,
    /// Header blocks after the addressing blocks (reference parameters, extra blocks).
    pub headers: Vec<Element>,
    pub body: Option<Element>,
    pub attachments: Vec<OutPart>,
}

impl SoapReply {
    /// 200 reply with a body element.
    pub fn new(action: &str, body: Element) -> Self {
        Self {
            status: 200,
            to: ANONYMOUS_ROLE.to_string(),
            action: action.to_string(),
            message_id: format!("urn:uuid:{}", Uuid::new_v4()),
            relates_to: None,
            headers: Vec::new(),
            body: Some(body),
            attachments: Vec::new(),
        }
    }

    /// 200 reply with an empty `soap:Body`.
    pub fn empty(action: &str) -> Self {
        let mut r = Self::new(action, Element::new("", "x"));
        r.body = None;
        r
    }

    /// A notification (event) for an endpoint such as a `wse:NotifyTo`: `To` is its address
    /// and its reference parameters become header blocks.
    pub fn notification(target: &EndpointRef, action: &str, body: Element) -> Self {
        Self::new(action, body).to_endpoint(target)
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    pub fn with_message_id(mut self, id: impl Into<String>) -> Self {
        self.message_id = id.into();
        self
    }

    /// Set `RelatesTo`; blank or non-IRI values are dropped.
    pub fn relates_to(mut self, id: Option<&str>) -> Self {
        self.relates_to = id
            .map(str::trim)
            .filter(|s| envelope::absolute_iri(s))
            .map(str::to_string);
        self
    }

    pub fn with_header(mut self, block: Element) -> Self {
        self.headers.push(block);
        self
    }

    /// Address the message to `target`: `To` and the endpoint's reference blocks.
    pub fn to_endpoint(mut self, target: &EndpointRef) -> Self {
        self.to = if target.is_anonymous() {
            ANONYMOUS_ROLE.to_string()
        } else {
            target.address.clone()
        };
        let blocks = target.header_blocks();
        // reference blocks come first among the non-addressing headers
        self.headers.splice(0..0, blocks);
        self
    }

    /// Attach binary content as an MTOM part and return the `xop:Include` element to put
    /// inside the (base64Binary) element that carries it.
    pub fn attach(&mut self, content_type: &str, data: Vec<u8>) -> Element {
        let cid = format!("{}@fakeprint", Uuid::new_v4());
        let href = mtom::cid_href(&cid);
        self.attachments.push(OutPart {
            content_id: cid,
            content_type: content_type.to_string(),
            data,
        });
        Element::new(ns::XOP, "Include").with_attr("href", href)
    }

    /// The `soap:Envelope` tree.
    pub fn to_element(&self) -> Element {
        let mut header = Element::new(ns::SOAP12, "Header")
            .with_child(Element::text_el(ns::WSA, "To", self.to.as_str()))
            .with_child(Element::text_el(ns::WSA, "Action", self.action.as_str()))
            .with_child(Element::text_el(
                ns::WSA,
                "MessageID",
                self.message_id.as_str(),
            ));
        if let Some(r) = &self.relates_to {
            header
                .children
                .push(Element::text_el(ns::WSA, "RelatesTo", r.as_str()));
        }
        header.children.extend(self.headers.iter().cloned());
        let mut body = Element::new(ns::SOAP12, "Body");
        if let Some(b) = &self.body {
            body.children.push(b.clone());
        }
        Element::new(ns::SOAP12, "Envelope")
            .with_child(header)
            .with_child(body)
    }

    /// The envelope as an XML document (attachments are not part of this text).
    pub fn to_xml(&self) -> String {
        self.to_element().to_document(&PrefixMap::wsd())
    }

    /// Status, content type and bytes. With attachments the body is an MTOM package.
    pub fn into_http(self) -> HttpReply {
        let xml = self.to_xml();
        if self.attachments.is_empty() {
            return HttpReply {
                status: self.status,
                content_type: SOAP_CONTENT_TYPE.to_string(),
                body: xml.into_bytes(),
            };
        }
        let root_cid = format!("{}@fakeprint", Uuid::new_v4());
        match mtom::build_multipart(
            xml.as_bytes(),
            &root_cid,
            Some(&self.action),
            &self.attachments,
        ) {
            Ok(built) => HttpReply {
                status: self.status,
                content_type: built.content_type,
                body: built.body,
            },
            Err(_) => {
                let fault = PrintFault::new(FaultKind::ServerErrorInternalError);
                HttpReply {
                    status: fault.http_status(),
                    content_type: SOAP_CONTENT_TYPE.to_string(),
                    body: fault.to_envelope(self.relates_to.as_deref()).into_bytes(),
                }
            }
        }
    }
}

/// Header problems in a reply this service wrote (SOAP 1.2 + WS-Addressing 2004/08).
/// Empty means the envelope is well formed and its headers are right.
pub fn reply_problems(xml_text: &str) -> Vec<String> {
    let root = match xml::parse_str(xml_text, &XmlLimits::INLINE_DOCUMENT) {
        Ok(r) => r,
        Err(e) => return vec![format!("not well-formed XML: {e}")],
    };
    let env = match SoapEnvelope::from_element(&root) {
        Ok(e) => e,
        Err(f) => return vec![format!("not a SOAP 1.2 envelope: {f}")],
    };
    let a = &env.addressing;
    let mut problems = Vec::new();
    if a.action
        .as_deref()
        .is_none_or(|v| !envelope::absolute_iri(v))
    {
        problems.push("Action must be an absolute IRI".into());
    }
    if a.message_id
        .as_deref()
        .is_none_or(|v| !envelope::reply_message_id(v))
    {
        problems.push(
            "MessageID must be a fresh uuid: or urn:uuid: IRI, not a reused fixed value".into(),
        );
    }
    if a.to.as_deref() != Some(ANONYMOUS_ROLE) {
        problems.push("reply To must be the anonymous role".into());
    }
    if a.relates_to.iter().any(|r| !envelope::absolute_iri(&r.id)) {
        problems.push("RelatesTo must be an absolute IRI".into());
    }
    let order: Vec<&str> = env
        .headers
        .iter()
        .filter(|h| h.name().ns == ns::WSA)
        .map(|h| h.name().local.as_str())
        .collect();
    let rank = |n: &str| match n {
        "To" => 0,
        "Action" => 1,
        "MessageID" => 2,
        "RelatesTo" => 3,
        _ => 4,
    };
    if order.windows(2).any(|w| rank(w[0]) > rank(w[1])) {
        problems.push("addressing headers are out of order".into());
    }
    problems
}

/// Document bytes of a SendDocument/AddDocument request, strictly (MTOM or inline base64).
/// `None` when the request is not a valid package or has no usable `DocumentData`.
pub fn document_bytes(content_type: Option<&str>, body: &[u8]) -> Option<Vec<u8>> {
    let req = SoapRequest::parse(content_type, None, body).ok()?;
    req.document_data().ok().map(Cow::into_owned)
}

/// The last path segment of an action URI (`GetPrinterElements`), for logs and counters.
pub fn short_action(action_uri: &str) -> &str {
    action_uri.rsplit('/').next().unwrap_or(action_uri)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wsd::fault::FaultCode;
    use crate::wsd::types::{EndpointKind, PrintEvent, PrintOp};

    const ENV_OPEN: &str = r#"<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope" xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:wprt="http://schemas.microsoft.com/windows/2006/08/wdp/print" xmlns:xop="http://www.w3.org/2004/08/xop/include">"#;

    fn request(header_extra: &str, body: &str) -> String {
        format!(
            "{ENV_OPEN}<soap:Header><wsa:To>urn:x</wsa:To><wsa:Action>http://schemas.microsoft.com/windows/2006/08/wdp/print/SendDocument</wsa:Action><wsa:MessageID>urn:uuid:11111111-1111-4111-8111-111111111111</wsa:MessageID>{header_extra}</soap:Header><soap:Body>{body}</soap:Body></soap:Envelope>"
        )
    }

    fn parse(xml: &str) -> Result<SoapRequest, PrintFault> {
        SoapRequest::parse(Some("application/soap+xml"), None, xml.as_bytes())
    }

    #[test]
    fn parses_headers_and_body() {
        let r = parse(&request(
            "<wsa:ReplyTo><wsa:Address>http://h/reply</wsa:Address><wsa:ReferenceParameters><x:Id xmlns:x=\"urn:x\">7</x:Id></wsa:ReferenceParameters></wsa:ReplyTo>",
            "<wprt:CancelJobRequest><wprt:JobId>3</wprt:JobId></wprt:CancelJobRequest>",
        ))
        .unwrap();
        assert_eq!(r.action, PrintOp::SendDocument.action().unwrap());
        assert_eq!(
            r.message_id(),
            Some("urn:uuid:11111111-1111-4111-8111-111111111111")
        );
        assert_eq!(r.body().unwrap().name.local, "CancelJobRequest");
        let reply = r.reply("urn:a", Element::new(ns::PRINT, "R"));
        assert_eq!(reply.to, "http://h/reply");
        assert!(reply.headers.iter().any(|h| h.name.local == "Id"));
        assert_eq!(
            reply.relates_to.as_deref(),
            Some("urn:uuid:11111111-1111-4111-8111-111111111111")
        );
    }

    #[test]
    fn fault_goes_to_fault_to_then_reply_to() {
        let r = parse(&request(
            "<wsa:ReplyTo><wsa:Address>http://h/r</wsa:Address></wsa:ReplyTo><wsa:FaultTo><wsa:Address>http://h/f</wsa:Address></wsa:FaultTo>",
            "<wprt:X/>",
        ))
        .unwrap();
        assert_eq!(
            r.fault_reply(&PrintFault::invalid_args("x")).to,
            "http://h/f"
        );
        assert_eq!(r.reply_empty("urn:a").to, "http://h/r");
    }

    #[test]
    fn strict_envelope_errors() {
        let kinds = |xml: &str| parse(xml).unwrap_err().kind();
        let soap11 = "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body/></s:Envelope>";
        assert_eq!(kinds(soap11), Some(FaultKind::VersionMismatch));
        assert_eq!(kinds("<a/>"), Some(FaultKind::Malformed));
        assert_eq!(kinds("<not xml"), Some(FaultKind::Malformed));
        assert_eq!(
            kinds(&format!("{ENV_OPEN}</soap:Envelope>")),
            Some(FaultKind::Malformed)
        );
        assert_eq!(
            kinds(&format!(
                "{ENV_OPEN}<soap:Body/><soap:Header/></soap:Envelope>"
            )),
            Some(FaultKind::Malformed)
        );
        // missing Action
        let no_action = format!("{ENV_OPEN}<soap:Header/><soap:Body/></soap:Envelope>");
        assert_eq!(
            kinds(&no_action),
            Some(FaultKind::MessageInformationHeaderRequired)
        );
        // wrong-namespace Action is not an Action
        let wrong_ns = no_action.replace(
            "<soap:Header/>",
            "<soap:Header><wsa:Action xmlns:wsa=\"urn:other\">u:a</wsa:Action></soap:Header>",
        );
        assert_eq!(
            kinds(&wrong_ns),
            Some(FaultKind::MessageInformationHeaderRequired)
        );
        // duplicate Action
        let dup = request("<wsa:Action>urn:b</wsa:Action>", "<wprt:X/>");
        assert_eq!(
            kinds(&dup),
            Some(FaultKind::InvalidMessageInformationHeader)
        );
        // EPR without Address
        let epr = request("<wsa:ReplyTo/>", "<wprt:X/>");
        assert_eq!(
            kinds(&epr),
            Some(FaultKind::InvalidMessageInformationHeader)
        );
        // bad mustUnderstand value
        let mu = request("<wprt:H soap:mustUnderstand=\"maybe\"/>", "<wprt:X/>");
        assert_eq!(kinds(&mu), Some(FaultKind::Malformed));
    }

    #[test]
    fn must_understand_handling() {
        let r = parse(&request(
            "<wprt:Odd soap:mustUnderstand=\"true\"/><wprt:Skip soap:mustUnderstand=\"true\" soap:role=\"http://www.w3.org/2003/05/soap-envelope/role/none\"/><wprt:Opt soap:mustUnderstand=\"false\"/>",
            "<wprt:X/>",
        ))
        .unwrap();
        let f = r.check_must_understand(&[]).unwrap_err();
        assert_eq!(f.kind(), Some(FaultKind::MustUnderstand));
        assert_eq!(f.http_status(), 500);
        assert_eq!(f.headers.len(), 1);
        assert!(r
            .check_must_understand(&[QName::new(ns::PRINT, "Odd")])
            .is_ok());
        let xml = f.to_envelope(None);
        assert!(xml.contains("NotUnderstood"), "{xml}");
        assert!(xml::parse_str(&xml, &XmlLimits::SMALL).is_ok());
    }

    #[test]
    fn non_iri_message_id_is_not_echoed() {
        let xml = request("", "<wprt:X/>").replace(
            "urn:uuid:11111111-1111-4111-8111-111111111111",
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
        );
        let r = parse(&xml).unwrap();
        assert!(r.message_id().is_none());
        assert!(!r.envelope.warnings.is_empty());
        assert!(r.reply_empty("urn:a").relates_to.is_none());
    }

    #[test]
    fn soap_action_header_and_media_types() {
        let xml = request("", "<wprt:X/>");
        let r = SoapRequest::parse(
            Some("application/soap+xml; charset=utf-8"),
            Some("\"urn:other\""),
            xml.as_bytes(),
        )
        .unwrap();
        assert_eq!(r.soap_action_mismatch(), Some("urn:other"));
        let ok = SoapRequest::parse(None, None, xml.as_bytes()).unwrap();
        assert!(ok.soap_action_mismatch().is_none());
        assert!(SoapRequest::parse(Some("text/html"), None, xml.as_bytes()).is_err());
        assert!(SoapRequest::parse(Some("garbage"), None, xml.as_bytes()).is_err());
    }

    #[test]
    fn inline_document_and_hostile_xml() {
        let r = parse(&request("", "<wprt:SendDocumentRequest><wprt:DocumentData>JVBERi0xLjQ=</wprt:DocumentData></wprt:SendDocumentRequest>")).unwrap();
        assert_eq!(r.document_data().unwrap().as_ref(), b"%PDF-1.4");
        let empty = parse(&request("", "<wprt:S><wprt:DocumentData/></wprt:S>")).unwrap();
        assert!(empty.document_data().is_err());
        let bomb = "<?xml version=\"1.0\"?><!DOCTYPE x [<!ENTITY a \"aaaa\">]><soap:Envelope xmlns:soap=\"http://www.w3.org/2003/05/soap-envelope\"><soap:Body>&a;</soap:Body></soap:Envelope>";
        assert_eq!(parse(bomb).unwrap_err().kind(), Some(FaultKind::Malformed));
        let deep = format!("{}{}", "<a>".repeat(500), "</a>".repeat(500));
        assert!(parse(&request("", &deep)).is_err());
        let attr = request("", &format!("<wprt:X a=\"{}\"/>", "z".repeat(100_000)));
        assert!(parse(&attr).is_err());
        let ent = request("", "<wprt:X>&bogus;</wprt:X>");
        assert!(parse(&ent).is_err());
    }

    fn mtom_request(include_href: &str, cid: &str, data: &[u8]) -> (String, Vec<u8>) {
        let xml = request(
            "",
            &format!("<wprt:SendDocumentRequest><wprt:DocumentData><xop:Include href=\"{include_href}\"/></wprt:DocumentData></wprt:SendDocumentRequest>"),
        );
        let mut b = Vec::new();
        b.extend_from_slice(b"--B\r\nContent-Type: application/xop+xml; charset=utf-8; type=\"application/soap+xml\"\r\nContent-ID: <env@x>\r\n\r\n");
        b.extend_from_slice(xml.as_bytes());
        b.extend_from_slice(
            format!(
                "\r\n--B\r\nContent-Type: application/octet-stream\r\nContent-ID: <{cid}>\r\n\r\n"
            )
            .as_bytes(),
        );
        b.extend_from_slice(data);
        b.extend_from_slice(b"\r\n--B--\r\n");
        (
            "multipart/related; type=\"application/xop+xml\"; start=\"<env@x>\"; boundary=B"
                .to_string(),
            b,
        )
    }

    #[test]
    fn mtom_document_resolved_by_cid() {
        let data: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
        let (ct, body) = mtom_request("cid:doc%401@x", "doc@1@x", &data);
        let r = SoapRequest::parse(Some(&ct), None, &body).unwrap();
        assert_eq!(r.document_data().unwrap().as_ref(), data.as_slice());
        assert_eq!(document_bytes(Some(&ct), &body).unwrap(), data);
    }

    #[test]
    fn mtom_cid_mismatch_is_invalid_args() {
        let (ct, body) = mtom_request("cid:missing@x", "doc@x", b"abc");
        let f = SoapRequest::parse(Some(&ct), None, &body).unwrap_err();
        assert_eq!(f.kind(), Some(FaultKind::InvalidArgs));
        // include in a non-multipart message
        let xml = request(
            "",
            "<wprt:S><wprt:DocumentData><xop:Include href=\"cid:a\"/></wprt:DocumentData></wprt:S>",
        );
        assert_eq!(
            parse(&xml).unwrap_err().kind(),
            Some(FaultKind::InvalidArgs)
        );
        // truncated package
        let (ct, body) = mtom_request("cid:doc@x", "doc@x", b"abc");
        assert!(SoapRequest::parse(Some(&ct), None, &body[..body.len() - 6]).is_err());
        assert!(document_bytes(Some(&ct), b"garbage").is_none());
    }

    #[test]
    fn reply_round_trip_and_validation() {
        let reply = SoapReply::new(
            PrintOp::CreatePrintJob.response_action().unwrap(),
            Element::new(ns::PRINT, "CreatePrintJobResponse").with_child(Element::text_el(
                ns::PRINT,
                "JobId",
                "5",
            )),
        )
        .relates_to(Some("urn:uuid:11111111-1111-4111-8111-111111111111"));
        let xml = reply.to_xml();
        assert!(
            reply_problems(&xml).is_empty(),
            "{:?}\n{xml}",
            reply_problems(&xml)
        );
        let back =
            SoapEnvelope::from_element(&xml::parse_str(&xml, &XmlLimits::SMALL).unwrap()).unwrap();
        assert_eq!(back.addressing.to.as_deref(), Some(ANONYMOUS_ROLE));
        assert_eq!(back.body[0].child(ns::PRINT, "JobId").unwrap().text, "5");
        // header order: To, Action, MessageID, RelatesTo
        let names: Vec<&str> = back
            .headers
            .iter()
            .map(|h| h.name().local.as_str())
            .collect();
        assert_eq!(names, ["To", "Action", "MessageID", "RelatesTo"]);
        let other = SoapReply::empty("urn:a").to_xml();
        assert!(!reply_problems(&other.replace("wsa:Action>urn:a", "wsa:Action>nope")).is_empty());
        let a = SoapReply::empty("urn:a");
        let b = SoapReply::empty("urn:a");
        assert_ne!(a.message_id, b.message_id);
        assert!(a.message_id.starts_with("urn:uuid:"));
    }

    #[test]
    fn mtom_reply_round_trip() {
        let mut reply = SoapReply::empty("urn:a");
        let inc = reply.attach("application/pdf", b"%PDF-1.4 reply".to_vec());
        reply.body = Some(
            Element::new(ns::PRINT, "R")
                .with_child(Element::new(ns::PRINT, "DocumentData").with_child(inc)),
        );
        let http = reply.into_http();
        assert!(http.content_type.starts_with("multipart/related"));
        let req = SoapRequest::parse(Some(&http.content_type), None, &http.body);
        // the reply has no Action-less problem: it parses as a request-shaped envelope
        let req = req.unwrap();
        let data = req
            .binary_content(
                req.body()
                    .unwrap()
                    .child(ns::PRINT, "DocumentData")
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(data.as_ref(), b"%PDF-1.4 reply");
    }

    #[test]
    fn notification_targets_notify_to_with_reference_parameters() {
        let mut epr = EndpointRef::new("http://127.0.0.1:9/notify");
        epr.reference_parameters
            .push(Element::text_el(ns::WSE, "Identifier", "urn:uuid:abc"));
        let ev = PrintEvent::JobEndState {
            job: crate::wsd::types::PrintJob::new(1, "j", "u"),
        };
        let msg = SoapReply::notification(
            &epr,
            ev.action(),
            Element::new(ns::PRINT, "JobEndStateEvent"),
        );
        let xml = msg.to_xml();
        let env =
            SoapEnvelope::from_element(&xml::parse_str(&xml, &XmlLimits::SMALL).unwrap()).unwrap();
        assert_eq!(
            env.addressing.to.as_deref(),
            Some("http://127.0.0.1:9/notify")
        );
        assert_eq!(
            env.header(&QName::new(ns::WSE, "Identifier")).unwrap().text,
            "urn:uuid:abc"
        );
    }

    // ----- faults -----

    #[test]
    fn every_fault_builds_a_valid_envelope() {
        for kind in FaultKind::ALL {
            let f = PrintFault::new(kind);
            let xml = f.to_envelope(Some("urn:uuid:11111111-1111-4111-8111-111111111111"));
            assert!(
                reply_problems(&xml).is_empty(),
                "{kind:?}: {:?}",
                reply_problems(&xml)
            );
            let root = xml::parse_str(&xml, &XmlLimits::SMALL).unwrap();
            let env = SoapEnvelope::from_element(&root).unwrap();
            assert_eq!(
                env.addressing.action.as_deref(),
                Some(crate::wsd::fault::FAULT_ACTION)
            );
            let fault = env.body[0].clone();
            assert!(fault.is(ns::SOAP12, "Fault"));
            let code = fault.child(ns::SOAP12, "Code").unwrap();
            assert_eq!(
                code.child(ns::SOAP12, "Value").unwrap().text_qname(),
                Some(kind.code().qname()),
                "{kind:?}"
            );
            let sub = code
                .child(ns::SOAP12, "Subcode")
                .and_then(|s| s.child(ns::SOAP12, "Value"))
                .and_then(|v| v.text_qname());
            assert_eq!(sub, kind.subcode(), "{kind:?}");
            assert_eq!(f.kind(), Some(kind), "{kind:?}");
            let text = fault
                .child(ns::SOAP12, "Reason")
                .unwrap()
                .child(ns::SOAP12, "Text")
                .unwrap();
            assert_eq!(text.attr(&QName::new(ns::XML, "lang")), Some("en"));
            assert_eq!(
                f.http_status(),
                if kind.code() == FaultCode::Sender {
                    400
                } else {
                    500
                }
            );
        }
    }

    #[test]
    fn fault_details() {
        let f = PrintFault::action_not_supported("urn:bogus");
        let xml = f.to_envelope(None);
        assert!(
            xml.contains("ProblemAction") && xml.contains("urn:bogus"),
            "{xml}"
        );
        let f = PrintFault::job_id_not_found(-4);
        assert!(f.to_envelope(None).contains("<wprt:JobId>-4</wprt:JobId>"));
        let f = PrintFault::format_not_supported(&["application/pdf".into()]);
        assert!(f
            .to_envelope(None)
            .contains("<wprt:Format><wprt:AllowedValue>application/pdf"));
        assert!(PrintFault::compression_not_supported(&[]).detail.is_empty());
        assert_eq!(PrintFault::version_mismatch().http_status(), 500);
        let hostile = PrintFault::invalid_args("<&\u{1}>").to_envelope(None);
        assert!(
            xml::parse_str(&hostile, &XmlLimits::SMALL).is_ok(),
            "{hostile}"
        );
    }

    // ----- cross-checks against the generated schema model (skipped when absent) -----

    fn model() -> Option<serde_json::Value> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/reference/schema/schema-model.json");
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    #[test]
    fn fault_enumeration_matches_schema_model() {
        let Some(m) = model() else { return };
        let listed: Vec<String> = m["faults"][0]["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                v["code"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches("wprt:")
                    .to_string()
            })
            .collect();
        let ours: Vec<&str> = FaultKind::SCHEMA_ENUMERATION
            .iter()
            .map(|k| k.subcode_local().unwrap())
            .collect();
        assert_eq!(listed.len(), 15);
        for l in &listed {
            assert!(ours.contains(&l.as_str()), "missing {l}");
        }
        assert_eq!(ours.len(), listed.len());
    }

    #[test]
    fn operation_actions_match_schema_model() {
        let Some(m) = model() else { return };
        for op in m["operations"].as_array().unwrap() {
            let name = op["name"].as_str().unwrap();
            if let Some(input) = op["input_action"].as_str() {
                let ours = PrintOp::ALL
                    .iter()
                    .find(|o| o.name() == name)
                    .unwrap_or_else(|| panic!("no op {name}"));
                assert_eq!(ours.action(), Some(input), "{name}");
                assert_eq!(
                    ours.response_action(),
                    op["output_action"].as_str(),
                    "{name}"
                );
                assert_eq!(PrintOp::from_action(input), Some(*ours));
            } else if let Some(action) = op["output_action"].as_str() {
                // event: must be one of ours
                let hit = [
                    "PrinterElementsChangeEvent",
                    "PrinterStatusSummaryEvent",
                    "PrinterStatusConditionEvent",
                    "PrinterStatusConditionClearedEvent",
                    "JobStatusEvent",
                    "JobEndStateEvent",
                ]
                .iter()
                .any(|e| action.ends_with(e));
                assert!(hit, "{action}");
            }
        }
    }

    #[test]
    fn op_table_basics() {
        assert_eq!(PrintOp::from_action("urn:nope"), None);
        assert_eq!(PrintOp::CreatePrintJob2.action(), None);
        assert_eq!(
            PrintOp::from_action_lenient(
                "http://schemas.microsoft.com/windows/2006/01/wdp/print/GetPrinterElements"
            ),
            Some(PrintOp::GetPrinterElements)
        );
        assert!(PrintOp::TransferGet.allowed_on(EndpointKind::Device));
        assert!(!PrintOp::CancelJob.allowed_on(EndpointKind::Device));
        assert!(PrintOp::for_endpoint(EndpointKind::PrintService).contains(&PrintOp::Subscribe));
        for op in PrintOp::ALL {
            if let Some(a) = op.action() {
                assert_eq!(PrintOp::from_action(a), Some(op));
            }
        }
    }
}
