# WS-Print faults

## Envelope

SOAP 1.2 fault, reply action `http://schemas.xmlsoap.org/ws/2004/08/addressing/fault`, correlated
with `wsa:RelatesTo`. `Code/Value` is `soap:Sender` (the request was wrong) or `soap:Receiver`
(the printer failed); `Code/Subcode/Value` is the QName below; `Reason/Text` is English; `Detail`
is usually absent. Faults go to the request's FaultTo, else ReplyTo. Vendors may add private
subcodes. When several apply, return the most specific one.

## Subcodes (`PrintServiceFaultValuesType`, namespace `wprt`)

| Subcode | Code | Raise when |
|---------|------|-----------|
| `wsa:ActionNotSupported` | Sender | The Action is not an operation of this endpoint (not in the enum; Detail carries the offending action). |
| `wprt:InvalidArgs` | Sender | Missing/extra/mis-typed arguments (schema violation of the request body). Detail may name the argument. |
| `wprt:OperationFailed` | Receiver | The current state forbids the operation and no more specific subcode fits. |
| `wprt:ServerErrorTemporaryError` | Receiver | Transient internal problem; the same request may succeed later. Prefer a more specific code (disk full etc.) if one exists. |
| `wprt:ServerErrorInternalError` | Receiver | Permanent internal failure; retrying yields the same fault. |
| `wprt:ServerErrorNotAcceptingJobs` | Receiver | CreatePrintJob while in service mode, or user intervention is pending and buffers are exhausted. Client may retry later. |
| `wprt:ClientErrorJobIdNotFound` | Sender | JobId unknown, finished and forgotten, or outside 1..2^31-1. Detail echoes the id. Used by SendDocument, AddDocument, CancelJob, GetJobElements. |
| `wprt:ClientErrorMultipleDocumentsNotSupported` | Sender | The first document of the job does not carry `LastDocument=true` on a printer limited to one document. |
| `wprt:ClientErrorLastDocumentAlreadySent` | Sender | A further document arrives after one marked last. |
| `wprt:ClientErrorFormatNotSupported` | Sender | `Format` is not in the printer's allowed list. May list the supported formats in Detail (`ValueStringListType`). |
| `wprt:ClientErrorCompressionNotSupported` | Sender | `Compression` not supported; Detail may list the supported ones. |
| `wprt:ClientErrorDocumentURLSchemeNotSupported` | Sender | AddDocument URL scheme unsupported (e.g. https when only http works). |
| `wprt:ClientErrorDocumentURLNotFound` | Sender | AddDocument URL cannot be resolved or reached. |
| `wprt:ServerErrorAddDocumentNotSupported` | Receiver | AddDocument is not implemented (it is the one optional operation). |
| `wprt:ServerErrorDocumentActive` | Receiver | Another document of the same job is still being received/processed; the client should retry within the document timeout. |
| `wprt:ServerErrorJobCancelled` | Receiver | The job was cancelled while its document was being transferred. |

### Package inconsistencies (record, then follow the schema)

- The prose validation steps for SendDocument/AddDocument say "return the ClientErrorDocumentFormatNotSupported error", but the defined fault is `ClientErrorFormatNotSupported`. There is no `...DocumentFormatNotSupported` in the enum. (Scan has the same split; the scan notes in fakescan cover it.) Emit `wprt:ClientErrorFormatNotSupported`.
- `wprt:ClientErrorConflictingRequiredParameters` is documented as a CreatePrintJob/SendDocument/AddDocument error (conflicting MustHonor=true settings) but is **missing from the enumeration**; one sample even writes it with the scan prefix. Emit it with the `wprt` prefix anyway; it is the only sensible code.
- The faults are not declared in the WSDL (no `fault` messages), so the per-operation table above is from the prose, not from machine-readable data.

## Per operation (which faults can appear)

| Operation | Operation-specific | Plus common |
|-----------|--------------------|-------------|
| CreatePrintJob | `ServerErrorNotAcceptingJobs`, `ClientErrorConflictingRequiredParameters` | ActionNotSupported, InvalidArgs, OperationFailed, ServerError{Temporary,Internal}Error |
| SendDocument | `ServerErrorDocumentActive`, `ServerErrorJobCancelled`, `ClientErrorJobIdNotFound`, `...MultipleDocumentsNotSupported`, `...LastDocumentAlreadySent`, `...FormatNotSupported`, `...CompressionNotSupported`, `...ConflictingRequiredParameters` | same |
| AddDocument | all of SendDocument's plus `ServerErrorAddDocumentNotSupported`, `...DocumentURLSchemeNotSupported`, `...DocumentURLNotFound` | same |
| CancelJob, GetJobElements | `ClientErrorJobIdNotFound` | same |
| GetPrinterElements, GetActiveJobs, GetJobHistory, SetEventRate | none specific | same |

## Order of checks (CreatePrintJob/SendDocument)

1. Unknown action -> `wsa:ActionNotSupported`. Schema-invalid body -> `InvalidArgs`.
2. CreatePrintJob: printer state -> `ServerErrorNotAcceptingJobs`; ticket values vs allowed values with MustHonor -> reject / substitute / ignore ([must-honor.md](must-honor.md)); conflicts -> `ClientErrorConflictingRequiredParameters`.
3. SendDocument/AddDocument: job exists -> `ClientErrorJobIdNotFound`; job cancelled -> `ServerErrorJobCancelled`; last already sent; concurrent document -> `ServerErrorDocumentActive`; multi-document support; then **Format first, then Compression**; then (AddDocument) URL scheme and reachability.

Fault state effects: a rejected CreatePrintJob creates no job. A fault on SendDocument does not
end the job by itself; the 60-second document timeout still applies.
