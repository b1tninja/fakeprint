//! SOAP 1.2 envelope and WS-Addressing 2004/08 header parsing.
//!
//! [`SoapEnvelope`] is the strict path: it works on a parsed [`Element`] tree, matches every
//! name by namespace and local name, enforces the envelope shape (optional `Header`, then one
//! `Body`), reads the addressing blocks (`To`, `Action`, `MessageID`, `RelatesTo`, `ReplyTo`,
//! `FaultTo`, `From`, with endpoint references and their reference parameters), keeps every
//! header block with its `mustUnderstand`/`role`, and reports mustUnderstand blocks nobody
//! handles. Problems come back as ready-made [`PrintFault`]s.

use super::fault::PrintFault;
use super::xml::{self, ns, Element, QName, XmlLimits};

/// Anonymous address / reply `wsa:To` (WS-Addressing 2004/08).
pub const ANONYMOUS_ROLE: &str = "http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous";
/// The 2005/08 spelling some stacks send; treated as anonymous too.
pub const ANONYMOUS_2005: &str = "http://www.w3.org/2005/08/addressing/anonymous";
/// Default `RelationshipType` of `RelatesTo`.
pub const REPLY_RELATIONSHIP: &str = "http://schemas.xmlsoap.org/ws/2004/08/addressing/reply";

const ROLE_NONE: &str = "http://www.w3.org/2003/05/soap-envelope/role/none";
const ROLE_NEXT: &str = "http://www.w3.org/2003/05/soap-envelope/role/next";
const ROLE_ULTIMATE: &str = "http://www.w3.org/2003/05/soap-envelope/role/ultimateReceiver";

/// A WS-Addressing endpoint reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointRef {
    pub address: String,
    pub reference_properties: Vec<Element>,
    pub reference_parameters: Vec<Element>,
}

impl EndpointRef {
    pub fn new(address: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            reference_properties: Vec::new(),
            reference_parameters: Vec::new(),
        }
    }

    pub fn anonymous() -> Self {
        Self::new(ANONYMOUS_ROLE)
    }

    pub fn is_anonymous(&self) -> bool {
        self.address == ANONYMOUS_ROLE || self.address == ANONYMOUS_2005
    }

    /// Reference properties and parameters, which a message to this endpoint carries as
    /// header blocks.
    pub fn header_blocks(&self) -> Vec<Element> {
        self.reference_properties
            .iter()
            .chain(self.reference_parameters.iter())
            .cloned()
            .collect()
    }

    /// Read an EPR-typed header or element (`ReplyTo`, `NotifyTo`, `EndTo`, ...).
    pub fn from_element(el: &Element) -> Result<EndpointRef, PrintFault> {
        let what = el.name.local.as_str();
        let mut address: Option<String> = None;
        let mut epr = EndpointRef::new("");
        for child in &el.children {
            if child.name.ns != ns::WSA {
                continue;
            }
            match child.name.local.as_str() {
                "Address" => {
                    if address.is_some() {
                        return Err(PrintFault::header_invalid(what, "more than one Address"));
                    }
                    address = Some(child.text_trimmed().to_string());
                }
                "ReferenceProperties" => epr.reference_properties.extend(child.children.clone()),
                "ReferenceParameters" => epr.reference_parameters.extend(child.children.clone()),
                _ => {}
            }
        }
        match address {
            Some(a) if !a.is_empty() => {
                epr.address = a;
                Ok(epr)
            }
            _ => Err(PrintFault::header_invalid(
                what,
                "endpoint reference without Address",
            )),
        }
    }

    /// Build an EPR-typed element named `name`.
    pub fn to_element(&self, name: QName) -> Element {
        let mut el = Element::from_qname(name).with_child(Element::text_el(
            ns::WSA,
            "Address",
            self.address.as_str(),
        ));
        if !self.reference_properties.is_empty() {
            el.children.push(
                Element::new(ns::WSA, "ReferenceProperties")
                    .with_children(self.reference_properties.clone()),
            );
        }
        if !self.reference_parameters.is_empty() {
            el.children.push(
                Element::new(ns::WSA, "ReferenceParameters")
                    .with_children(self.reference_parameters.clone()),
            );
        }
        el
    }
}

/// One `wsa:RelatesTo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relationship {
    pub id: String,
    /// `RelationshipType`; `None` means the default (reply).
    pub kind: Option<String>,
}

/// The WS-Addressing message information headers of a message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Addressing {
    pub to: Option<String>,
    pub action: Option<String>,
    pub message_id: Option<String>,
    pub relates_to: Vec<Relationship>,
    pub reply_to: Option<EndpointRef>,
    pub fault_to: Option<EndpointRef>,
    pub from: Option<EndpointRef>,
}

/// One child of `soap:Header`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderBlock {
    pub element: Element,
    pub must_understand: bool,
    pub role: Option<String>,
}

impl HeaderBlock {
    pub fn name(&self) -> &QName {
        &self.element.name
    }

    /// Whether this node (an ultimate receiver) is a target of the block.
    pub fn targets_us(&self) -> bool {
        matches!(
            self.role.as_deref(),
            None | Some(ROLE_NEXT) | Some(ROLE_ULTIMATE)
        ) && self.role.as_deref() != Some(ROLE_NONE)
    }
}

/// Header blocks every WS-Print node understands.
pub fn default_understood() -> Vec<QName> {
    let mut v: Vec<QName> = [
        "To",
        "Action",
        "MessageID",
        "RelatesTo",
        "ReplyTo",
        "FaultTo",
        "From",
    ]
    .iter()
    .map(|l| QName::new(ns::WSA, l))
    .collect();
    v.push(QName::new(ns::WSE, "Identifier"));
    v
}

/// A parsed SOAP 1.2 envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct SoapEnvelope {
    pub headers: Vec<HeaderBlock>,
    pub addressing: Addressing,
    /// Children of `soap:Body`.
    pub body: Vec<Element>,
    /// Deviations that are tolerated (non-IRI values, missing optional headers).
    pub warnings: Vec<String>,
}

impl SoapEnvelope {
    /// Parse and check an envelope from bytes.
    pub fn parse(bytes: &[u8], limits: &XmlLimits) -> Result<SoapEnvelope, PrintFault> {
        let root = xml::parse(bytes, limits)
            .map_err(|e| PrintFault::malformed(format!("not well-formed XML: {e}")))?;
        Self::from_element(&root)
    }

    /// Check an already parsed envelope element.
    pub fn from_element(root: &Element) -> Result<SoapEnvelope, PrintFault> {
        if root.name.local != "Envelope" {
            return Err(PrintFault::malformed(
                "the document element is not soap:Envelope",
            ));
        }
        if root.name.ns == ns::SOAP11 {
            return Err(PrintFault::version_mismatch());
        }
        if root.name.ns != ns::SOAP12 {
            return Err(PrintFault::malformed(
                "the envelope is not in the SOAP 1.2 namespace",
            ));
        }
        let mut header_el: Option<&Element> = None;
        let mut body_el: Option<&Element> = None;
        for (i, child) in root.children.iter().enumerate() {
            if child.name.is(ns::SOAP12, "Header") {
                if i != 0 || header_el.is_some() {
                    return Err(PrintFault::malformed(
                        "soap:Header must be the first child, once",
                    ));
                }
                header_el = Some(child);
            } else if child.name.is(ns::SOAP12, "Body") {
                if body_el.is_some() {
                    return Err(PrintFault::malformed("more than one soap:Body"));
                }
                body_el = Some(child);
            } else {
                return Err(PrintFault::malformed(format!(
                    "unexpected envelope child {}",
                    child.name.local
                )));
            }
        }
        let body_el = body_el.ok_or_else(|| PrintFault::malformed("missing soap:Body"))?;

        let mut warnings = Vec::new();
        let mut headers = Vec::new();
        if let Some(h) = header_el {
            for block in &h.children {
                headers.push(read_block(block)?);
            }
        }
        let addressing = read_addressing(&headers, &mut warnings)?;
        Ok(SoapEnvelope {
            headers,
            addressing,
            body: body_el.children.clone(),
            warnings,
        })
    }

    /// First header block with this name.
    pub fn header(&self, name: &QName) -> Option<&Element> {
        self.headers
            .iter()
            .find(|h| h.name() == name)
            .map(|h| &h.element)
    }

    /// The first `Body` child (the operation element).
    pub fn body_first(&self) -> Option<&Element> {
        self.body.first()
    }

    /// Names of mandatory header blocks aimed at this node that are not in `understood`.
    pub fn unhandled_must_understand(&self, understood: &[QName]) -> Vec<QName> {
        self.headers
            .iter()
            .filter(|h| h.must_understand && h.targets_us() && !understood.contains(h.name()))
            .map(|h| h.name().clone())
            .collect()
    }
}

fn read_block(el: &Element) -> Result<HeaderBlock, PrintFault> {
    let mu = QName::new(ns::SOAP12, "mustUnderstand");
    let must_understand = match el.attr(&mu) {
        None => false,
        Some("true") | Some("1") => true,
        Some("false") | Some("0") => false,
        Some(other) => {
            return Err(PrintFault::malformed(format!(
                "soap:mustUnderstand has the invalid value {other:?}"
            )))
        }
    };
    let role = el
        .attr(&QName::new(ns::SOAP12, "role"))
        .map(|r| r.trim().to_string());
    Ok(HeaderBlock {
        element: el.clone(),
        must_understand,
        role,
    })
}

fn read_addressing(
    headers: &[HeaderBlock],
    warnings: &mut Vec<String>,
) -> Result<Addressing, PrintFault> {
    let mut a = Addressing::default();
    for block in headers {
        let el = &block.element;
        if el.name.ns != ns::WSA {
            continue;
        }
        let name = el.name.local.as_str();
        let dup = |name: &str| PrintFault::header_invalid(name, "appears more than once");
        match name {
            "To" => {
                if a.to.is_some() {
                    return Err(dup(name));
                }
                a.to = Some(el.text_trimmed().to_string());
            }
            "Action" => {
                if a.action.is_some() {
                    return Err(dup(name));
                }
                let v = el.text_trimmed();
                if v.is_empty() {
                    return Err(PrintFault::header_invalid(name, "empty"));
                }
                if !absolute_iri(v) {
                    warnings.push("Action is not an absolute IRI".into());
                }
                a.action = Some(v.to_string());
            }
            "MessageID" | "MessageId" => {
                if a.message_id.is_some() {
                    return Err(dup("MessageID"));
                }
                let v = el.text_trimmed();
                if absolute_iri(v) {
                    a.message_id = Some(v.to_string());
                } else {
                    warnings.push("MessageID is not an absolute IRI; it is not echoed".into());
                }
            }
            "RelatesTo" => {
                let v = el.text_trimmed();
                if v.is_empty() {
                    return Err(PrintFault::header_invalid(name, "empty"));
                }
                a.relates_to.push(Relationship {
                    id: v.to_string(),
                    kind: el
                        .attr_local("RelationshipType")
                        .map(|s| s.trim().to_string()),
                });
            }
            "ReplyTo" => {
                if a.reply_to.is_some() {
                    return Err(dup(name));
                }
                a.reply_to = Some(EndpointRef::from_element(el)?);
            }
            "FaultTo" => {
                if a.fault_to.is_some() {
                    return Err(dup(name));
                }
                a.fault_to = Some(EndpointRef::from_element(el)?);
            }
            "From" => {
                if a.from.is_some() {
                    return Err(dup(name));
                }
                a.from = Some(EndpointRef::from_element(el)?);
            }
            _ => {}
        }
    }
    if a.message_id.is_none() {
        warnings.push("no usable MessageID; RelatesTo will be omitted".into());
    }
    Ok(a)
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
