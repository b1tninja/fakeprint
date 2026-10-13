//! SOAP 1.2 faults for the WS-Print service.
//!
//! [`FaultKind`] catalogues what a printer can raise: the fifteen `wprt:` subcodes of the
//! schema's fault enumeration, one more the prose documents but the enumeration omits
//! (`ConflictingRequiredParameters`), the WS-Addressing faults, and the SOAP-level codes.
//! [`PrintFault`] is one concrete fault: code, optional subcode, reason, optional detail,
//! and any header blocks the fault needs (`soap:NotUnderstood`, `soap:Upgrade`).
//!
//! Mapping to HTTP follows the SOAP 1.2 HTTP binding: `Sender` faults are 400, everything
//! else (`Receiver`, `MustUnderstand`, `VersionMismatch`) is 500. Fault replies carry the
//! WS-Addressing fault action.

use super::soap::SoapReply;
use super::xml::{ns, Element, QName};

/// `wsa:Action` of every fault reply (WS-Addressing 2004/08).
pub const FAULT_ACTION: &str = "http://schemas.xmlsoap.org/ws/2004/08/addressing/fault";

/// Top-level `soap:Code/soap:Value`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultCode {
    /// The request was wrong.
    Sender,
    /// The service failed.
    Receiver,
    /// A mandatory header block was not understood.
    MustUnderstand,
    /// The envelope namespace is not SOAP 1.2.
    VersionMismatch,
}

impl FaultCode {
    pub fn local(self) -> &'static str {
        match self {
            FaultCode::Sender => "Sender",
            FaultCode::Receiver => "Receiver",
            FaultCode::MustUnderstand => "MustUnderstand",
            FaultCode::VersionMismatch => "VersionMismatch",
        }
    }

    pub fn qname(self) -> QName {
        QName::new(ns::SOAP12, self.local())
    }

    /// HTTP status of the fault response.
    pub fn http_status(self) -> u16 {
        match self {
            FaultCode::Sender => 400,
            _ => 500,
        }
    }
}

/// The catalogue of faults. `Malformed` and `Internal` have no subcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaultKind {
    // --- wprt: subcodes from the schema's enumeration (15) ---
    InvalidArgs,
    OperationFailed,
    ServerErrorTemporaryError,
    ServerErrorInternalError,
    ServerErrorNotAcceptingJobs,
    ClientErrorJobIdNotFound,
    ClientErrorMultipleDocumentsNotSupported,
    ClientErrorLastDocumentAlreadySent,
    ClientErrorFormatNotSupported,
    ClientErrorCompressionNotSupported,
    ClientErrorDocumentURLSchemeNotSupported,
    ClientErrorDocumentURLNotFound,
    ServerErrorAddDocumentNotSupported,
    ServerErrorDocumentActive,
    ServerErrorJobCancelled,
    // --- documented in prose but absent from the enumeration; emitted with the wprt prefix ---
    ClientErrorConflictingRequiredParameters,
    // --- WS-Addressing 2004/08 ---
    ActionNotSupported,
    MessageInformationHeaderRequired,
    InvalidMessageInformationHeader,
    // --- SOAP level ---
    MustUnderstand,
    VersionMismatch,
    /// `soap:Sender` without a subcode (unparseable XML, wrong envelope).
    Malformed,
    /// `soap:Receiver` without a subcode.
    Internal,
}

impl FaultKind {
    /// The fifteen subcodes of `PrintServiceFaultValuesType`, in schema order.
    pub const SCHEMA_ENUMERATION: [FaultKind; 15] = [
        FaultKind::InvalidArgs,
        FaultKind::OperationFailed,
        FaultKind::ClientErrorCompressionNotSupported,
        FaultKind::ClientErrorDocumentURLNotFound,
        FaultKind::ClientErrorDocumentURLSchemeNotSupported,
        FaultKind::ClientErrorFormatNotSupported,
        FaultKind::ClientErrorJobIdNotFound,
        FaultKind::ClientErrorLastDocumentAlreadySent,
        FaultKind::ClientErrorMultipleDocumentsNotSupported,
        FaultKind::ServerErrorAddDocumentNotSupported,
        FaultKind::ServerErrorDocumentActive,
        FaultKind::ServerErrorInternalError,
        FaultKind::ServerErrorJobCancelled,
        FaultKind::ServerErrorNotAcceptingJobs,
        FaultKind::ServerErrorTemporaryError,
    ];

    /// Every kind.
    pub const ALL: [FaultKind; 23] = [
        FaultKind::InvalidArgs,
        FaultKind::OperationFailed,
        FaultKind::ServerErrorTemporaryError,
        FaultKind::ServerErrorInternalError,
        FaultKind::ServerErrorNotAcceptingJobs,
        FaultKind::ClientErrorJobIdNotFound,
        FaultKind::ClientErrorMultipleDocumentsNotSupported,
        FaultKind::ClientErrorLastDocumentAlreadySent,
        FaultKind::ClientErrorFormatNotSupported,
        FaultKind::ClientErrorCompressionNotSupported,
        FaultKind::ClientErrorDocumentURLSchemeNotSupported,
        FaultKind::ClientErrorDocumentURLNotFound,
        FaultKind::ServerErrorAddDocumentNotSupported,
        FaultKind::ServerErrorDocumentActive,
        FaultKind::ServerErrorJobCancelled,
        FaultKind::ClientErrorConflictingRequiredParameters,
        FaultKind::ActionNotSupported,
        FaultKind::MessageInformationHeaderRequired,
        FaultKind::InvalidMessageInformationHeader,
        FaultKind::MustUnderstand,
        FaultKind::VersionMismatch,
        FaultKind::Malformed,
        FaultKind::Internal,
    ];

    /// `soap:Code/soap:Value`.
    pub fn code(self) -> FaultCode {
        use FaultKind::*;
        match self {
            OperationFailed
            | ServerErrorTemporaryError
            | ServerErrorInternalError
            | ServerErrorNotAcceptingJobs
            | ServerErrorAddDocumentNotSupported
            | ServerErrorDocumentActive
            | ServerErrorJobCancelled
            | Internal => FaultCode::Receiver,
            MustUnderstand => FaultCode::MustUnderstand,
            VersionMismatch => FaultCode::VersionMismatch,
            _ => FaultCode::Sender,
        }
    }

    /// Local part of the subcode, `None` for the subcode-less kinds and the SOAP-level codes.
    pub fn subcode_local(self) -> Option<&'static str> {
        use FaultKind::*;
        Some(match self {
            InvalidArgs => "InvalidArgs",
            OperationFailed => "OperationFailed",
            ServerErrorTemporaryError => "ServerErrorTemporaryError",
            ServerErrorInternalError => "ServerErrorInternalError",
            ServerErrorNotAcceptingJobs => "ServerErrorNotAcceptingJobs",
            ClientErrorJobIdNotFound => "ClientErrorJobIdNotFound",
            ClientErrorMultipleDocumentsNotSupported => "ClientErrorMultipleDocumentsNotSupported",
            ClientErrorLastDocumentAlreadySent => "ClientErrorLastDocumentAlreadySent",
            ClientErrorFormatNotSupported => "ClientErrorFormatNotSupported",
            ClientErrorCompressionNotSupported => "ClientErrorCompressionNotSupported",
            ClientErrorDocumentURLSchemeNotSupported => "ClientErrorDocumentURLSchemeNotSupported",
            ClientErrorDocumentURLNotFound => "ClientErrorDocumentURLNotFound",
            ServerErrorAddDocumentNotSupported => "ServerErrorAddDocumentNotSupported",
            ServerErrorDocumentActive => "ServerErrorDocumentActive",
            ServerErrorJobCancelled => "ServerErrorJobCancelled",
            ClientErrorConflictingRequiredParameters => "ClientErrorConflictingRequiredParameters",
            ActionNotSupported => "ActionNotSupported",
            MessageInformationHeaderRequired => "MessageInformationHeaderRequired",
            InvalidMessageInformationHeader => "InvalidMessageInformationHeader",
            MustUnderstand | VersionMismatch | Malformed | Internal => return None,
        })
    }

    /// Full subcode name (`wprt:` or `wsa:` namespace).
    pub fn subcode(self) -> Option<QName> {
        use FaultKind::*;
        let local = self.subcode_local()?;
        let namespace = match self {
            ActionNotSupported
            | MessageInformationHeaderRequired
            | InvalidMessageInformationHeader => ns::WSA,
            _ => ns::PRINT,
        };
        Some(QName::new(namespace, local))
    }

    /// The kind whose subcode is `q`.
    pub fn from_subcode(q: &QName) -> Option<FaultKind> {
        Self::ALL
            .iter()
            .copied()
            .find(|k| k.subcode().as_ref() == Some(q))
    }

    /// English reason text used when the caller gives none.
    pub fn default_reason(self) -> &'static str {
        use FaultKind::*;
        match self {
            InvalidArgs => "The request has missing, extra or invalid arguments.",
            OperationFailed => "The operation cannot be performed in the current state.",
            ServerErrorTemporaryError => "A temporary error occurred; try again later.",
            ServerErrorInternalError => "An internal error occurred.",
            ServerErrorNotAcceptingJobs => "The printer is not accepting jobs.",
            ClientErrorJobIdNotFound => "The job id is not known.",
            ClientErrorMultipleDocumentsNotSupported => {
                "The printer accepts only a single document per job."
            }
            ClientErrorLastDocumentAlreadySent => "The last document of this job was already sent.",
            ClientErrorFormatNotSupported => "The document format is not supported.",
            ClientErrorCompressionNotSupported => "The document compression is not supported.",
            ClientErrorDocumentURLSchemeNotSupported => "The document URL scheme is not supported.",
            ClientErrorDocumentURLNotFound => "The document URL could not be reached.",
            ServerErrorAddDocumentNotSupported => "AddDocument is not supported.",
            ServerErrorDocumentActive => "Another document of this job is still being processed.",
            ServerErrorJobCancelled => "The job was cancelled.",
            ClientErrorConflictingRequiredParameters => {
                "The request has conflicting required parameters."
            }
            ActionNotSupported => "The action is not supported by this endpoint.",
            MessageInformationHeaderRequired => "A required message information header is missing.",
            InvalidMessageInformationHeader => "A message information header is invalid.",
            MustUnderstand => "A mandatory header block was not understood.",
            VersionMismatch => "Only SOAP 1.2 is supported.",
            Malformed => "The request is not a well-formed SOAP 1.2 message.",
            Internal => "The service failed to process the request.",
        }
    }
}

/// One fault, ready to be turned into a reply.
#[derive(Debug, Clone, PartialEq)]
pub struct PrintFault {
    pub code: FaultCode,
    pub subcode: Option<QName>,
    /// Boxed so that `Result<_, PrintFault>` stays small (clippy `result_large_err`).
    pub reason: Box<str>,
    /// Content of `soap:Detail` (each element becomes a child of `Detail`).
    pub detail: Vec<Element>,
    /// Extra SOAP header blocks of the fault message.
    pub headers: Vec<Element>,
}

impl PrintFault {
    /// A fault of `kind` with its default reason.
    pub fn new(kind: FaultKind) -> Self {
        Self {
            code: kind.code(),
            subcode: kind.subcode(),
            reason: kind.default_reason().into(),
            detail: Vec::new(),
            headers: Vec::new(),
        }
    }

    /// A fault outside the catalogue (vendor subcode, WS-Eventing fault, ...).
    pub fn custom(code: FaultCode, subcode: Option<QName>, reason: impl Into<String>) -> Self {
        Self {
            code,
            subcode,
            reason: reason.into().into_boxed_str(),
            detail: Vec::new(),
            headers: Vec::new(),
        }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = reason.into().into_boxed_str();
        self
    }

    pub fn with_detail(mut self, detail: Element) -> Self {
        self.detail.push(detail);
        self
    }

    /// The catalogue kind this fault corresponds to, when it has one.
    pub fn kind(&self) -> Option<FaultKind> {
        match &self.subcode {
            Some(q) => FaultKind::from_subcode(q),
            None => FaultKind::ALL.iter().copied().find(|k| {
                k.subcode_local().is_none()
                    && k.code() == self.code
                    && matches!(
                        (k, self.code),
                        (FaultKind::Malformed, FaultCode::Sender)
                            | (FaultKind::Internal, FaultCode::Receiver)
                            | (FaultKind::MustUnderstand, FaultCode::MustUnderstand)
                            | (FaultKind::VersionMismatch, FaultCode::VersionMismatch)
                    )
            }),
        }
    }

    pub fn http_status(&self) -> u16 {
        self.code.http_status()
    }

    // ----- shortcuts -----

    pub fn invalid_args(reason: impl Into<String>) -> Self {
        Self::new(FaultKind::InvalidArgs).with_reason(reason)
    }

    pub fn malformed(reason: impl Into<String>) -> Self {
        Self::new(FaultKind::Malformed).with_reason(reason)
    }

    pub fn internal(reason: impl Into<String>) -> Self {
        Self::new(FaultKind::Internal).with_reason(reason)
    }

    /// `wsa:ActionNotSupported`; the detail names the offending action.
    pub fn action_not_supported(action: &str) -> Self {
        Self::new(FaultKind::ActionNotSupported).with_detail(
            Element::new(ns::WSA, "ProblemAction").with_child(Element::text_el(
                ns::WSA,
                "Action",
                action,
            )),
        )
    }

    /// `wsa:MessageInformationHeaderRequired`; the detail names the missing header.
    pub fn header_required(header: &str) -> Self {
        Self::new(FaultKind::MessageInformationHeaderRequired)
            .with_reason(format!("The {header} header is required."))
            .with_detail(
                Element::text_el(ns::WSA, "ProblemHeaderQName", "")
                    .with_qtext(QName::new(ns::WSA, header)),
            )
    }

    /// `wsa:InvalidMessageInformationHeader`; the detail names the offending header.
    pub fn header_invalid(header: &str, why: &str) -> Self {
        Self::new(FaultKind::InvalidMessageInformationHeader)
            .with_reason(format!("The {header} header is invalid: {why}"))
            .with_detail(
                Element::text_el(ns::WSA, "ProblemHeaderQName", "")
                    .with_qtext(QName::new(ns::WSA, header)),
            )
    }

    /// `wprt:ClientErrorJobIdNotFound`; the detail echoes the id as the client sent it.
    pub fn job_id_not_found(job_id: impl std::fmt::Display) -> Self {
        Self::new(FaultKind::ClientErrorJobIdNotFound).with_detail(Element::text_el(
            ns::PRINT,
            "JobId",
            job_id.to_string(),
        ))
    }

    /// `wprt:ClientErrorFormatNotSupported`, optionally listing the accepted formats.
    pub fn format_not_supported(supported: &[String]) -> Self {
        Self::allowed_list(
            FaultKind::ClientErrorFormatNotSupported,
            "Format",
            supported,
        )
    }

    /// `wprt:ClientErrorCompressionNotSupported`, optionally listing the accepted values.
    pub fn compression_not_supported(supported: &[String]) -> Self {
        Self::allowed_list(
            FaultKind::ClientErrorCompressionNotSupported,
            "Compression",
            supported,
        )
    }

    fn allowed_list(kind: FaultKind, container: &str, values: &[String]) -> Self {
        let mut fault = Self::new(kind);
        if !values.is_empty() {
            let list = values
                .iter()
                .fold(Element::new(ns::PRINT, container), |el, v| {
                    el.with_child(Element::text_el(ns::PRINT, "AllowedValue", v.as_str()))
                });
            fault.detail.push(list);
        }
        fault
    }

    /// `soap:MustUnderstand` naming the blocks that were not understood (one
    /// `soap:NotUnderstood` header block each).
    pub fn must_understand(blocks: &[QName]) -> Self {
        let mut fault = Self::new(FaultKind::MustUnderstand);
        for q in blocks {
            fault.headers.push(
                Element::new(ns::SOAP12, "NotUnderstood").with_attr_qname("qname", q.clone()),
            );
        }
        fault
    }

    /// `soap:VersionMismatch` advertising SOAP 1.2 in `soap:Upgrade`.
    pub fn version_mismatch() -> Self {
        let mut fault = Self::new(FaultKind::VersionMismatch);
        fault.headers.push(
            Element::new(ns::SOAP12, "Upgrade").with_child(
                Element::new(ns::SOAP12, "SupportedEnvelope")
                    .with_attr_qname("qname", QName::new(ns::SOAP12, "Envelope")),
            ),
        );
        fault
    }

    // ----- serialization -----

    /// The `soap:Fault` body element.
    pub fn fault_element(&self) -> Element {
        let mut code = Element::new(ns::SOAP12, "Code")
            .with_child(Element::new(ns::SOAP12, "Value").with_qtext(self.code.qname()));
        if let Some(sub) = &self.subcode {
            code.children.push(
                Element::new(ns::SOAP12, "Subcode")
                    .with_child(Element::new(ns::SOAP12, "Value").with_qtext(sub.clone())),
            );
        }
        let reason = Element::new(ns::SOAP12, "Reason").with_child(
            Element::text_el(ns::SOAP12, "Text", &*self.reason)
                .with_attr_ns(QName::new(ns::XML, "lang"), "en"),
        );
        let mut fault = Element::new(ns::SOAP12, "Fault")
            .with_child(code)
            .with_child(reason);
        if !self.detail.is_empty() {
            fault
                .children
                .push(Element::new(ns::SOAP12, "Detail").with_children(self.detail.clone()));
        }
        fault
    }

    /// A reply carrying this fault; `relates_to` is the request's MessageID.
    pub fn to_reply(&self, relates_to: Option<&str>) -> SoapReply {
        let mut reply = SoapReply::new(FAULT_ACTION, self.fault_element())
            .with_status(self.http_status())
            .relates_to(relates_to);
        for h in &self.headers {
            reply = reply.with_header(h.clone());
        }
        reply
    }

    /// The complete SOAP 1.2 envelope text of the fault reply.
    pub fn to_envelope(&self, relates_to: Option<&str>) -> String {
        self.to_reply(relates_to).to_xml()
    }
}

impl std::fmt::Display for PrintFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.subcode {
            Some(q) => write!(f, "{} / {}: {}", self.code.local(), q.local, self.reason),
            None => write!(f, "{}: {}", self.code.local(), self.reason),
        }
    }
}

impl std::error::Error for PrintFault {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fault_is_small_enough_to_travel_in_a_result() {
        assert!(
            std::mem::size_of::<PrintFault>() < 128,
            "{}",
            std::mem::size_of::<PrintFault>()
        );
    }
}
