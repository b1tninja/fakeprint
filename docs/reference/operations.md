# WS-Print operations, events and job lifecycle

Facts from `WSDPrinterService.wsdl`, `WSDPrinterServiceV12.wsdl` and the schema; lifecycle rules
from the prose spec, restated. Per-operation generated pages (ignored) are in
`docs/reference/schema/operations/`.

All actions are `http://schemas.microsoft.com/windows/2006/08/wdp/print/<Name>`; the response is
`<Name>Response`. The binding is SOAP 1.2, document/literal, `soapActionRequired="true"`, the
soapAction equals the input Action. The portType is `PrinterServiceType` (an event source).
The device-level marker type `PrintDeviceType` has no operations.

## Request/response operations

| Operation | Needed? | Request body | Response body | Notes |
|-----------|---------|--------------|---------------|-------|
| CreatePrintJob | required | `PrintTicket` (JobDescription, JobProcessing?, DocumentProcessing?) | `JobId` | JobDescription is optional in the type but the prose makes it mandatory here |
| SendDocument | required | `JobId`, `DocumentDescription`, `DocumentProcessing?`, `LastDocument`, `DocumentData` (base64Binary) | empty (`xs:any ##other` only) | In practice MTOM/XOP, see below |
| AddDocument | optional | same as SendDocument but `DocumentUrl` instead of data | empty | Printer pulls the URL. Fault `ServerErrorAddDocumentNotSupported` when unsupported |
| CancelJob | required | `JobId` | empty | |
| GetPrinterElements | required | `RequestedElements/Name`* (QNames) | `PrinterElements/ElementData`* | `ElementData` has attributes `Name` (QName) and `Valid` (boolean) |
| GetJobElements | required | `JobId`, `RequestedElements/Name`* | `JobElements/ElementData`* | choice of JobStatus / PrintTicket / Documents per ElementData |
| GetActiveJobs | required | empty (wildcard) | `ActiveJobs/JobSummary`* | |
| GetJobHistory | required | empty (wildcard) | `JobHistory/JobSummary`* | |
| SetEventRate | required | `EventRate` 1..600 (seconds) | empty | Global minimum spacing for CompleteState events |
| SetPrinterElements (v1.2) | extension | `ElementsToSet/ElementToSet`+ (`ElementPath` + one of Boolean/Integer/String/Float/Binary) | `ElementsToSetResults/ElementToSetResult`+ (`ElementPath`, `Result`) | own namespace `wprt12`. Result tokens: Succeeded, ElementReadOnly, IncompatibleType, InvalidElementPath, UnknownElementPath, UnknownFailure |

Requested names for GetPrinterElements: `PrinterDescription`, `PrinterConfiguration`,
`PrinterStatus`, `DefaultPrintTicket`, `PrinterCapabilities`, or a vendor `ns:Section`.
GetJobElements: `JobStatus`, `PrintTicket`, `Documents` (or vendor). One `ElementData` per
requested name, same order; an unknown name yields `Valid="false"` with no content (not a fault).
The element for a valid name is a child of `ElementData` named like the request.

### Shapes worth memorising (required children)

- `PrinterDescription`: ColorSupported, DeviceId, PagesPerMinute, PrinterName+ (localized string); optional MultipleDocumentJobsSupported, PagesPerMinuteColor, PrinterInfo*, PrinterLocation*
- `PrinterStatus`: **PrinterCurrentTime**, PrinterState, QueuedJobCount; optional PrimaryStateReason, StateReasons, ActiveCondition, ConditionHistory
- `JobStatus`: JobId, JobState, KOctetsProcessed, MediaSheetsCompleted, NumberOfDocuments; JobStateReasons optional but when present it needs at least one reason
- `JobSummary`: JobId, JobState, JobName, JobOriginatingUserName and the three counters; JobStateReasons optional
- `DocumentDescription`: DocumentId (>=1), **Compression**, **Format** both required; DocumentName, DocumentUrl optional
- `PrinterCapabilities`: `JobValues` (JobProcessing/DocumentProcessing allowed values) and `DocumentValues/DocumentDescription/{Compression,Format}` lists of `AllowedValue`

JobId is an int in the schema (min 0) but the prose limits issued ids to 1 .. 2^31-1 and asks the
printer not to reuse recent ids (package inconsistency: schema allows 0).

## Events (outbound notifications, delivered through WS-Eventing)

| Event | Body | Category | Fires when |
|-------|------|----------|-----------|
| PrinterElementsChangeEvent | `ElementChanges` with only the changed pieces of Description/Configuration/DefaultPrintTicket/Capabilities | cumulative | any of those changes; a removed bin/consumable is sent as the entry with `Removed="true"` |
| PrinterStatusSummaryEvent | `StatusSummary` (PrinterState, reasons) | CompleteState | state or reasons change |
| PrinterStatusConditionEvent | `DeviceCondition` (@Id, Time, Name, Component, Severity) | cumulative | a condition is raised |
| PrinterStatusConditionClearedEvent | `DeviceConditionCleared` (ConditionId, ConditionClearTime) | cumulative | a condition clears |
| JobStatusEvent | `JobStatus` | CompleteState | job progress changes |
| JobEndStateEvent | `JobEndState` (JobId, JobCompletedState, JobCompletedStateReasons, JobName, JobOriginatingUserName, 3 counters) | cumulative | job ends |

CompleteState events may be coalesced: keep only the newest and send it once the event-rate
interval since the previous one has elapsed (send at once if the interval already passed).
Cumulative events are never buffered or dropped. Severity values: Informational, Warning, Critical.
Package inconsistency: the prose sample for JobEndState names the reason element
`JobCompletedStateReason`; the schema (via `JobStateReasonsType`) names it `JobStateReason`. Follow the schema.

## v1.1, v1.2, v2.0 extensions (opt-in, advertised in PrinterDescription)

- v1.1: `SupportsWSPrintv11`, `DriverConfiguration` (GPD/PPD file names), `ColorRepresentation` (6 hex digits), `DeviceModelId` (36-char GUID string). No operations.
- v1.2: `SupportsWSPrintV12` plus `SetPrinterElements`. Not a standalone service: it extends a v1.0 device; settability is not advertised.
- v2.0 (driverless): `SupportsWSPrintV20`, `PrinterFormats` (PrintDeviceCapabilitiesFormats, PrintJobTicketFormats, PrintDeviceResourceFormats, each a list of MIME types) inside PrinterCapabilities, `PrintDeviceCapabilitiesChangeID` inside PrinterConfiguration, attribute `Volatile` (do not cache). Operations with schema but **no WSDL and no action in the package**: CreatePrintJob2 (JobDescription2 with optional user identity entries, optional ticket container = format string + base64 content; its response is the v1.0 CreatePrintJobResponse), GetPrintDeviceCapabilities, GetPrintDeviceResources, PrepareToPrint (no response), GetBidiSchemaExtensions. Their action URIs live in the Word spec (prefix `.../2014/04/wdp/printV20/`); do not hard-code them from the schema alone.
- The SupportsWSPrint flags do not imply each other; a device lists every one it supports.

## Print job lifecycle

States (`JobStateWKVType`): Pending, Pending-Held, Processing, ProcessingStopped, Started,
Terminating, Completed, Canceled, Aborted. Vendors may add or subset.

```text
CreatePrintJob
   -> Pending            reason JobIncoming while data arrives (may also pass through Started, transient)
   -> Processing         reason JobPrinting once the printer works on it
        -> ProcessingStopped   printer stopped / error (reason PrinterStopped); resumes to Processing
   -> Completed          JobCompletedSuccessfully | ...WithWarnings | ...WithErrors
CancelJob (Pending or Processing)
   -> Terminating (transient) -> Canceled      reason JobCanceledByUser (device panel: JobCanceledAtDevice)
failures
   -> Aborted            DocumentTimeoutError (no document within 60 s of CreatePrintJob, or between documents),
                         DocumentTransferError (transport broke mid-document; finish what printed, then abort),
                         DocumentFormatError, CompressionError, DocumentAccessError (AddDocument URL)
   Pending-Held          only reachable outside WSD (device-side hold)
```

Which operations are legal in which state:

| State | SendDocument/AddDocument | CancelJob | GetJobElements | in GetActiveJobs | in GetJobHistory |
|-------|--------------------------|-----------|----------------|------------------|------------------|
| Pending / Started / Processing (before LastDocument) | yes (one at a time; else `ServerErrorDocumentActive`) | yes -> Canceled | yes | yes | no |
| Pending/Processing after LastDocument received | `ClientErrorLastDocumentAlreadySent` | yes | yes | yes | no |
| ProcessingStopped | allowed if buffers permit | yes | yes | yes | no |
| Terminating | `ServerErrorJobCancelled` | idempotent success is the safest answer | yes | yes | no |
| Completed / Canceled / Aborted | `ClientErrorJobIdNotFound` once forgotten, `ClientErrorLastDocumentAlreadySent` while remembered | an error: nothing to cancel (use `ClientErrorJobIdNotFound` after forgetting) | known while in history, afterwards `ClientErrorJobIdNotFound` | no | yes (device-dependent depth) |
| unknown / out of range id | `ClientErrorJobIdNotFound` | same | same | n/a | n/a |

The prose says a job that has reached a terminating state and is no longer remembered is simply
"not found"; cancelling a completed or already-cancelled job is "an error" without naming the subcode
(unverified which subcode Windows tolerates; `ClientErrorJobIdNotFound` is the only defined candidate).
JobEndStateEvent should follow every transition into Completed, Canceled or Aborted.

Multi-document jobs: documents print in arrival order; `LastDocument=true` closes the job. A
printer without `MultipleDocumentJobsSupported` must accept exactly one document with
`LastDocument=true` and answer `ClientErrorMultipleDocumentsNotSupported` otherwise.

## SendDocument on the wire (MTOM)

`DocumentData` is `xs:base64Binary`. The spec's sample request is `multipart/related;
type="application/xop+xml"; start=...; start-info="application/soap+xml"` with a first
`application/xop+xml` part holding the envelope and an `xop:Include href="cid:..."` in
`DocumentData`, then one binary part (`application/octet-stream`, `Content-ID`). Inline base64
also validates against the schema. A server must resolve the include by Content-ID, not guess
the largest part. Content type of the document comes from `DocumentDescription/Format`, not from
the MIME part.

## What a minimal conforming printer implements

1. Discovery + metadata: Hello/Probe/Resolve matches with Types `wsdp:Device wprt:PrintDeviceType`; Transfer Get on the device endpoint returning ThisModel/ThisDevice/Relationship with a Hosted service of type `wprt:PrinterServiceType`; Windows additionally needs the PnP-X `CompatibleId` and `DeviceCategory` (see `docs/specs/ws-print-add-checklist.md`).
2. The nine v1.0 operations (AddDocument may fault `ServerErrorAddDocumentNotSupported`), the common faults (`wsa:ActionNotSupported`, `InvalidArgs`, `OperationFailed`, `ServerErrorTemporaryError`, `ServerErrorInternalError`) and the operation faults of [faults.md](faults.md).
3. WS-Eventing on the service endpoint with the six events; a client is expected to subscribe.
4. GetPrinterElements answers all five sections with schema-valid content (PrinterCapabilities must list allowed Format/Compression values).
5. A job table: ids 1..2^31-1 without quick reuse, JobStatus counters, active list and a bounded history.
