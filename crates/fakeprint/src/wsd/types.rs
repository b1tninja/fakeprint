//! Shared vocabulary of the WSD print stack: operations and their actions, job and printer
//! state, events, and the traits the print stack is written against.
//!
//! Kept deliberately small. `PrintOp` is the one action table; `PrintJob`/`JobStore` describe
//! jobs; `PrintEvent`/`EventBus` carry the six WS-Print events; `PrinterModel` answers
//! `GetPrinterElements`-style questions. Nothing here does I/O.

use std::sync::Mutex;
use std::time::SystemTime;

use super::fault::PrintFault;
use super::xml::{ns, Element, QName};

// ---------------------------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------------------------

/// Which HTTP endpoint a message arrives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EndpointKind {
    /// The device (metadata) endpoint: WS-Transfer Get, WS-MetadataExchange.
    Device,
    /// The hosted print service: WS-Print operations and the WS-Eventing event source.
    PrintService,
    /// A WS-Discovery endpoint (directed Probe/Resolve).
    Discovery,
}

/// First protocol version that defines the operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PrintVersion {
    V10,
    V12,
    V20,
}

macro_rules! print_action {
    ($name:literal) => {
        concat!(
            "http://schemas.microsoft.com/windows/2006/08/wdp/print/",
            $name
        )
    };
}

/// Every operation the emulator can receive, with its `wsa:Action`.
///
/// Actions that exist only in prose (the v2.0 operations have a schema but no WSDL) have
/// no action here: [`PrintOp::action`] returns `None` and they can never be matched by
/// URI until the Word specification's values are confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrintOp {
    // WS-Print 1.0 (print service)
    CreatePrintJob,
    SendDocument,
    AddDocument,
    CancelJob,
    GetPrinterElements,
    GetJobElements,
    GetActiveJobs,
    GetJobHistory,
    SetEventRate,
    // WS-Print 1.2
    SetPrinterElements,
    // WS-Print 2.0: action URIs TBD
    CreatePrintJob2,
    GetPrintDeviceCapabilities,
    GetPrintDeviceResources,
    PrepareToPrint,
    GetBidiSchemaExtensions,
    // WS-Eventing (event source = print service)
    Subscribe,
    Renew,
    GetStatus,
    Unsubscribe,
    // Metadata
    TransferGet,
    GetMetadata,
    // WS-Discovery (directed)
    Probe,
    Resolve,
}

impl PrintOp {
    pub const ALL: [PrintOp; 23] = [
        PrintOp::CreatePrintJob,
        PrintOp::SendDocument,
        PrintOp::AddDocument,
        PrintOp::CancelJob,
        PrintOp::GetPrinterElements,
        PrintOp::GetJobElements,
        PrintOp::GetActiveJobs,
        PrintOp::GetJobHistory,
        PrintOp::SetEventRate,
        PrintOp::SetPrinterElements,
        PrintOp::CreatePrintJob2,
        PrintOp::GetPrintDeviceCapabilities,
        PrintOp::GetPrintDeviceResources,
        PrintOp::PrepareToPrint,
        PrintOp::GetBidiSchemaExtensions,
        PrintOp::Subscribe,
        PrintOp::Renew,
        PrintOp::GetStatus,
        PrintOp::Unsubscribe,
        PrintOp::TransferGet,
        PrintOp::GetMetadata,
        PrintOp::Probe,
        PrintOp::Resolve,
    ];

    /// Operation name (the body element is `<name>Request` for print operations).
    pub fn name(self) -> &'static str {
        use PrintOp::*;
        match self {
            CreatePrintJob => "CreatePrintJob",
            SendDocument => "SendDocument",
            AddDocument => "AddDocument",
            CancelJob => "CancelJob",
            GetPrinterElements => "GetPrinterElements",
            GetJobElements => "GetJobElements",
            GetActiveJobs => "GetActiveJobs",
            GetJobHistory => "GetJobHistory",
            SetEventRate => "SetEventRate",
            SetPrinterElements => "SetPrinterElements",
            CreatePrintJob2 => "CreatePrintJob2",
            GetPrintDeviceCapabilities => "GetPrintDeviceCapabilities",
            GetPrintDeviceResources => "GetPrintDeviceResources",
            PrepareToPrint => "PrepareToPrint",
            GetBidiSchemaExtensions => "GetBidiSchemaExtensions",
            Subscribe => "Subscribe",
            Renew => "Renew",
            GetStatus => "GetStatus",
            Unsubscribe => "Unsubscribe",
            TransferGet => "Get",
            GetMetadata => "GetMetadata",
            Probe => "Probe",
            Resolve => "Resolve",
        }
    }

    /// The primary endpoint that serves the operation.
    pub fn endpoint(self) -> EndpointKind {
        use PrintOp::*;
        match self {
            TransferGet | GetMetadata => EndpointKind::Device,
            Probe | Resolve => EndpointKind::Discovery,
            _ => EndpointKind::PrintService,
        }
    }

    /// Whether `kind` may serve the operation (Get and GetMetadata also work on the print
    /// service, which is a hosted service with its own metadata).
    pub fn allowed_on(self, kind: EndpointKind) -> bool {
        match self {
            PrintOp::TransferGet | PrintOp::GetMetadata => {
                matches!(kind, EndpointKind::Device | EndpointKind::PrintService)
            }
            _ => self.endpoint() == kind,
        }
    }

    /// Protocol version that introduced the operation.
    pub fn version(self) -> PrintVersion {
        use PrintOp::*;
        match self {
            SetPrinterElements => PrintVersion::V12,
            CreatePrintJob2
            | GetPrintDeviceCapabilities
            | GetPrintDeviceResources
            | PrepareToPrint
            | GetBidiSchemaExtensions => PrintVersion::V20,
            _ => PrintVersion::V10,
        }
    }

    /// The request `wsa:Action`; `None` when the package only defines it in prose.
    pub fn action(self) -> Option<&'static str> {
        use PrintOp::*;
        Some(match self {
            CreatePrintJob => print_action!("CreatePrintJob"),
            SendDocument => print_action!("SendDocument"),
            AddDocument => print_action!("AddDocument"),
            CancelJob => print_action!("CancelJob"),
            GetPrinterElements => print_action!("GetPrinterElements"),
            GetJobElements => print_action!("GetJobElements"),
            GetActiveJobs => print_action!("GetActiveJobs"),
            GetJobHistory => print_action!("GetJobHistory"),
            SetEventRate => print_action!("SetEventRate"),
            SetPrinterElements => {
                "http://schemas.microsoft.com/windows/2012/10/wdp/printV12/SetPrinterElements"
            }
            CreatePrintJob2
            | GetPrintDeviceCapabilities
            | GetPrintDeviceResources
            | PrepareToPrint
            | GetBidiSchemaExtensions => return None,
            Subscribe => "http://schemas.xmlsoap.org/ws/2004/08/eventing/Subscribe",
            Renew => "http://schemas.xmlsoap.org/ws/2004/08/eventing/Renew",
            GetStatus => "http://schemas.xmlsoap.org/ws/2004/08/eventing/GetStatus",
            Unsubscribe => "http://schemas.xmlsoap.org/ws/2004/08/eventing/Unsubscribe",
            TransferGet => "http://schemas.xmlsoap.org/ws/2004/09/transfer/Get",
            GetMetadata => "http://schemas.xmlsoap.org/ws/2004/09/mex/GetMetadata/Request",
            Probe => "http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe",
            Resolve => "http://schemas.xmlsoap.org/ws/2005/04/discovery/Resolve",
        })
    }

    /// The reply `wsa:Action`; `None` when the operation has no reply or the action is TBD.
    pub fn response_action(self) -> Option<&'static str> {
        use PrintOp::*;
        Some(match self {
            CreatePrintJob => print_action!("CreatePrintJobResponse"),
            SendDocument => print_action!("SendDocumentResponse"),
            AddDocument => print_action!("AddDocumentResponse"),
            CancelJob => print_action!("CancelJobResponse"),
            GetPrinterElements => print_action!("GetPrinterElementsResponse"),
            GetJobElements => print_action!("GetJobElementsResponse"),
            GetActiveJobs => print_action!("GetActiveJobsResponse"),
            GetJobHistory => print_action!("GetJobHistoryResponse"),
            SetEventRate => print_action!("SetEventRateResponse"),
            SetPrinterElements => {
                "http://schemas.microsoft.com/windows/2012/10/wdp/printV12/SetPrinterElementsResponse"
            }
            CreatePrintJob2 | GetPrintDeviceCapabilities | GetPrintDeviceResources
            | GetBidiSchemaExtensions | PrepareToPrint => return None,
            Subscribe => "http://schemas.xmlsoap.org/ws/2004/08/eventing/SubscribeResponse",
            Renew => "http://schemas.xmlsoap.org/ws/2004/08/eventing/RenewResponse",
            GetStatus => "http://schemas.xmlsoap.org/ws/2004/08/eventing/GetStatusResponse",
            Unsubscribe => "http://schemas.xmlsoap.org/ws/2004/08/eventing/UnsubscribeResponse",
            TransferGet => "http://schemas.xmlsoap.org/ws/2004/09/transfer/GetResponse",
            GetMetadata => "http://schemas.xmlsoap.org/ws/2004/09/mex/GetMetadata/Response",
            Probe => "http://schemas.xmlsoap.org/ws/2005/04/discovery/ProbeMatches",
            Resolve => "http://schemas.xmlsoap.org/ws/2005/04/discovery/ResolveMatches",
        })
    }

    /// Operation whose request action is exactly `uri`.
    pub fn from_action(uri: &str) -> Option<PrintOp> {
        let uri = uri.trim();
        PrintOp::ALL
            .iter()
            .copied()
            .find(|op| op.action() == Some(uri))
    }

    /// Like [`PrintOp::from_action`], but also accepts the print actions under the older
    /// `/2006/01/wdp/print/` base that some clients still send.
    pub fn from_action_lenient(uri: &str) -> Option<PrintOp> {
        let uri = uri.trim();
        if let Some(op) = Self::from_action(uri) {
            return Some(op);
        }
        let name = uri.strip_prefix(LEGACY_PRINT_ACTION_BASE)?;
        PrintOp::ALL.iter().copied().find(|op| {
            op.version() == PrintVersion::V10
                && op.endpoint() == EndpointKind::PrintService
                && !matches!(
                    op,
                    PrintOp::Subscribe | PrintOp::Renew | PrintOp::GetStatus | PrintOp::Unsubscribe
                )
                && op.name() == name
        })
    }

    /// Operations a request on `kind` can use, in table order.
    pub fn for_endpoint(kind: EndpointKind) -> Vec<PrintOp> {
        PrintOp::ALL
            .iter()
            .copied()
            .filter(|op| op.allowed_on(kind))
            .collect()
    }
}

/// Older print-action base accepted by [`PrintOp::from_action_lenient`].
pub const LEGACY_PRINT_ACTION_BASE: &str =
    "http://schemas.microsoft.com/windows/2006/01/wdp/print/";

// ---------------------------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------------------------

/// `JobStateWKVType` (nine values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobState {
    Pending,
    PendingHeld,
    Processing,
    ProcessingStopped,
    Started,
    Terminating,
    Completed,
    Canceled,
    Aborted,
}

impl JobState {
    pub const ALL: [JobState; 9] = [
        JobState::Pending,
        JobState::PendingHeld,
        JobState::Processing,
        JobState::ProcessingStopped,
        JobState::Started,
        JobState::Terminating,
        JobState::Completed,
        JobState::Canceled,
        JobState::Aborted,
    ];

    /// Wire token.
    pub fn token(self) -> &'static str {
        match self {
            JobState::Pending => "Pending",
            JobState::PendingHeld => "Pending-Held",
            JobState::Processing => "Processing",
            JobState::ProcessingStopped => "ProcessingStopped",
            JobState::Started => "Started",
            JobState::Terminating => "Terminating",
            JobState::Completed => "Completed",
            JobState::Canceled => "Canceled",
            JobState::Aborted => "Aborted",
        }
    }

    pub fn from_token(s: &str) -> Option<JobState> {
        Self::ALL.iter().copied().find(|st| st.token() == s)
    }

    /// Completed, Canceled or Aborted: the job is over and belongs in the history.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Completed | JobState::Canceled | JobState::Aborted
        )
    }
}

/// `JobStateReasonsWKVType` (seventeen values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobStateReason {
    CompressionError,
    DocumentAccessError,
    DocumentFormatError,
    DocumentTimeoutError,
    DocumentTransferError,
    JobCanceledAtDevice,
    JobCanceledByUser,
    JobCompletedSuccessfully,
    JobCompletedWithErrors,
    JobCompletedWithWarnings,
    JobIncoming,
    JobPasswordWait,
    JobPrinting,
    None,
    PrinterStopped,
    UnsupportedCompression,
    UnsupportedDocumentFormat,
}

impl JobStateReason {
    pub const ALL: [JobStateReason; 17] = [
        JobStateReason::CompressionError,
        JobStateReason::DocumentAccessError,
        JobStateReason::DocumentFormatError,
        JobStateReason::DocumentTimeoutError,
        JobStateReason::DocumentTransferError,
        JobStateReason::JobCanceledAtDevice,
        JobStateReason::JobCanceledByUser,
        JobStateReason::JobCompletedSuccessfully,
        JobStateReason::JobCompletedWithErrors,
        JobStateReason::JobCompletedWithWarnings,
        JobStateReason::JobIncoming,
        JobStateReason::JobPasswordWait,
        JobStateReason::JobPrinting,
        JobStateReason::None,
        JobStateReason::PrinterStopped,
        JobStateReason::UnsupportedCompression,
        JobStateReason::UnsupportedDocumentFormat,
    ];

    pub fn token(self) -> &'static str {
        match self {
            JobStateReason::CompressionError => "CompressionError",
            JobStateReason::DocumentAccessError => "DocumentAccessError",
            JobStateReason::DocumentFormatError => "DocumentFormatError",
            JobStateReason::DocumentTimeoutError => "DocumentTimeoutError",
            JobStateReason::DocumentTransferError => "DocumentTransferError",
            JobStateReason::JobCanceledAtDevice => "JobCanceledAtDevice",
            JobStateReason::JobCanceledByUser => "JobCanceledByUser",
            JobStateReason::JobCompletedSuccessfully => "JobCompletedSuccessfully",
            JobStateReason::JobCompletedWithErrors => "JobCompletedWithErrors",
            JobStateReason::JobCompletedWithWarnings => "JobCompletedWithWarnings",
            JobStateReason::JobIncoming => "JobIncoming",
            JobStateReason::JobPasswordWait => "JobPasswordWait",
            JobStateReason::JobPrinting => "JobPrinting",
            JobStateReason::None => "None",
            JobStateReason::PrinterStopped => "PrinterStopped",
            JobStateReason::UnsupportedCompression => "UnsupportedCompression",
            JobStateReason::UnsupportedDocumentFormat => "UnsupportedDocumentFormat",
        }
    }

    pub fn from_token(s: &str) -> Option<JobStateReason> {
        Self::ALL.iter().copied().find(|r| r.token() == s)
    }
}

/// One document of a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobDocument {
    /// 1-based, per job.
    pub id: u32,
    pub name: Option<String>,
    /// `DocumentDescription/Format` as sent (a MIME type).
    pub format: String,
    /// `DocumentDescription/Compression` as sent.
    pub compression: String,
    /// Bytes received.
    pub size: u64,
    /// `DocumentUrl` for AddDocument.
    pub url: Option<String>,
    /// Where the capture was written, if anywhere.
    pub capture_path: Option<std::path::PathBuf>,
}

/// A print job as the WS-Print operations see it.
#[derive(Debug, Clone, PartialEq)]
pub struct PrintJob {
    /// 1 ..= 2^31-1.
    pub id: u32,
    pub name: String,
    pub user: String,
    pub state: JobState,
    pub reasons: Vec<JobStateReason>,
    pub documents: Vec<JobDocument>,
    /// A document with `LastDocument=true` has been accepted.
    pub last_document_received: bool,
    /// Bytes processed so far (the wire reports `KOctetsProcessed`).
    pub octets_processed: u64,
    pub media_sheets_completed: u32,
    /// `CreatePrintJob/PrintTicket` as received (kept for `GetJobElements`).
    pub ticket: Option<Element>,
    pub created: SystemTime,
    pub ended: Option<SystemTime>,
}

impl PrintJob {
    pub fn new(id: u32, name: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            user: user.into(),
            state: JobState::Pending,
            reasons: vec![JobStateReason::JobIncoming],
            documents: Vec::new(),
            last_document_received: false,
            octets_processed: 0,
            media_sheets_completed: 0,
            ticket: None,
            created: SystemTime::now(),
            ended: None,
        }
    }

    /// `KOctetsProcessed` on the wire: whole kilobytes, rounded up.
    pub fn k_octets_processed(&self) -> u64 {
        self.octets_processed.div_ceil(1024)
    }

    pub fn number_of_documents(&self) -> u32 {
        u32::try_from(self.documents.len()).unwrap_or(u32::MAX)
    }
}

/// Arguments of [`JobStore::create`].
#[derive(Debug, Clone, Default)]
pub struct NewJob {
    pub name: String,
    pub user: String,
    pub ticket: Option<Element>,
}

/// Job table. Implementations use interior mutability; callers never hold a lock across
/// `.await`. Errors are ready-made faults.
pub trait JobStore: Send + Sync {
    /// Allocate an id (1 ..= 2^31-1, not reused soon) and store the job in `Pending`.
    /// May fail with `ServerErrorNotAcceptingJobs` or a temporary error.
    fn create(&self, new: NewJob) -> Result<u32, PrintFault>;
    /// Snapshot of a job, active or remembered in the history.
    fn get(&self, id: u32) -> Option<PrintJob>;
    /// Apply `change` to the stored job and return the updated snapshot;
    /// `ClientErrorJobIdNotFound` when the id is unknown.
    fn update(
        &self,
        id: u32,
        change: &mut dyn FnMut(&mut PrintJob),
    ) -> Result<PrintJob, PrintFault>;
    /// Non-terminal jobs, ascending by id.
    fn active(&self) -> Vec<PrintJob>;
    /// Remembered terminal jobs, ascending by id (bounded).
    fn history(&self) -> Vec<PrintJob>;
}

// ---------------------------------------------------------------------------------------------
// Printer state and events
// ---------------------------------------------------------------------------------------------

/// `PrinterStateWKVType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrinterState {
    Idle,
    Processing,
    Stopped,
}

impl PrinterState {
    pub fn token(self) -> &'static str {
        match self {
            PrinterState::Idle => "Idle",
            PrinterState::Processing => "Processing",
            PrinterState::Stopped => "Stopped",
        }
    }
}

/// `ConditionSeverityType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Severity {
    Informational,
    Warning,
    Critical,
}

impl Severity {
    pub fn token(self) -> &'static str {
        match self {
            Severity::Informational => "Informational",
            Severity::Warning => "Warning",
            Severity::Critical => "Critical",
        }
    }
}

/// A raised printer condition (`DeviceCondition`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCondition {
    /// `Id` attribute, >= 1.
    pub id: u32,
    /// `xs:dateTime` text.
    pub time: String,
    pub name: String,
    pub component: String,
    pub severity: Severity,
}

/// Whether a later event of the same kind replaces an earlier one that has not been sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventCategory {
    /// Latest state wins; may be coalesced and throttled by the event rate.
    CompleteState,
    /// Never dropped or merged.
    Cumulative,
}

/// The six WS-Print events.
#[derive(Debug, Clone, PartialEq)]
pub enum PrintEvent {
    /// Changed pieces of Description/Configuration/DefaultPrintTicket/Capabilities (children
    /// of `ElementChanges`).
    PrinterElementsChange {
        changes: Vec<Element>,
    },
    /// `StatusSummary`.
    PrinterStatusSummary {
        state: PrinterState,
        primary_reason: Option<String>,
        reasons: Vec<String>,
    },
    PrinterStatusCondition {
        condition: DeviceCondition,
    },
    PrinterStatusConditionCleared {
        condition_id: u32,
        cleared: String,
    },
    /// Progress snapshot of a job.
    JobStatus {
        job: PrintJob,
    },
    /// Final snapshot of a job that reached a terminal state.
    JobEndState {
        job: PrintJob,
    },
}

impl PrintEvent {
    pub fn name(&self) -> &'static str {
        match self {
            PrintEvent::PrinterElementsChange { .. } => "PrinterElementsChangeEvent",
            PrintEvent::PrinterStatusSummary { .. } => "PrinterStatusSummaryEvent",
            PrintEvent::PrinterStatusCondition { .. } => "PrinterStatusConditionEvent",
            PrintEvent::PrinterStatusConditionCleared { .. } => {
                "PrinterStatusConditionClearedEvent"
            }
            PrintEvent::JobStatus { .. } => "JobStatusEvent",
            PrintEvent::JobEndState { .. } => "JobEndStateEvent",
        }
    }

    /// `wsa:Action` of the notification (also the WS-Eventing Action filter value).
    pub fn action(&self) -> &'static str {
        match self {
            PrintEvent::PrinterElementsChange { .. } => print_action!("PrinterElementsChangeEvent"),
            PrintEvent::PrinterStatusSummary { .. } => print_action!("PrinterStatusSummaryEvent"),
            PrintEvent::PrinterStatusCondition { .. } => {
                print_action!("PrinterStatusConditionEvent")
            }
            PrintEvent::PrinterStatusConditionCleared { .. } => {
                print_action!("PrinterStatusConditionClearedEvent")
            }
            PrintEvent::JobStatus { .. } => print_action!("JobStatusEvent"),
            PrintEvent::JobEndState { .. } => print_action!("JobEndStateEvent"),
        }
    }

    pub fn category(&self) -> EventCategory {
        match self {
            PrintEvent::PrinterStatusSummary { .. } | PrintEvent::JobStatus { .. } => {
                EventCategory::CompleteState
            }
            _ => EventCategory::Cumulative,
        }
    }
}

/// Sink for events. Publishing must not block; implementations queue.
pub trait EventBus: Send + Sync {
    fn publish(&self, event: PrintEvent);
}

/// Discards events.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullEventBus;

impl EventBus for NullEventBus {
    fn publish(&self, _event: PrintEvent) {}
}

/// Keeps every event, for tests.
#[derive(Debug, Default)]
pub struct RecordingEventBus {
    events: Mutex<Vec<PrintEvent>>,
}

impl RecordingEventBus {
    pub fn new() -> Self {
        Self::default()
    }
    /// Events published so far (a poisoned lock yields what is there).
    pub fn events(&self) -> Vec<PrintEvent> {
        match self.events.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }
}

impl EventBus for RecordingEventBus {
    fn publish(&self, event: PrintEvent) {
        match self.events.lock() {
            Ok(mut g) => g.push(event),
            Err(p) => p.into_inner().push(event),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Printer model
// ---------------------------------------------------------------------------------------------

/// Answers section questions for `GetPrinterElements`.
pub trait PrinterModel: Send + Sync {
    /// The element for a requested section name (`PrinterDescription`, `PrinterStatus`, ...,
    /// or a vendor section); `None` means the name is unknown (`Valid="false"`).
    fn element(&self, name: &QName) -> Option<Element>;

    /// Section names this model can answer, in the order they are normally listed.
    fn sections(&self) -> Vec<QName> {
        Vec::new()
    }
}

/// Qualified name of a WS-Print 1.0 element.
pub fn wprt(local: &str) -> QName {
    QName::new(ns::PRINT, local)
}
