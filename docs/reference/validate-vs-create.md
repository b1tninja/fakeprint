# Validate vs create (print has no validate)

Scan splits ticket checking (`ValidateScanTicket`) from job creation. **Print has no
ValidatePrintTicket operation in any shipped version** (v1.0 WSDL, v1.1, v1.2, v2.0 schema and
prose). The parser's operation probe (`operations/index.md`, generated) confirms that
ValidatePrintTicket, PausePrinter, ResumePrinter, PurgeJob(s), PauseJob, ResumeJob, HoldJob and
ReleaseJob are all absent. Administrative control (pause, purge) is outside WS-Print; Windows
manages those on the host queue.

## What the spec gives instead

- The prose describes `CreatePrintJob` as the operation that "creates/validates" a job. It checks the ticket against the printer's allowed values at that point and either creates the job and returns a `JobId`, or faults. There is no way to dry-run.
- Document-level checks (Format, then Compression) happen in the first `SendDocument`/`AddDocument`, after the job already exists. A format rejection therefore leaves a created job behind; it ends as Aborted with `UnsupportedDocumentFormat`/`DocumentFormatError` (or by the document timeout) unless the client cancels it.
- Clients learn the legal values beforehand from `GetPrinterElements(PrinterCapabilities)` and the defaults from `DefaultPrintTicket`; those two sections are the "validation interface".

## Consequences for the emulator

| Situation | Behaviour |
|-----------|-----------|
| Request names `ValidatePrintTicket` | `wsa:ActionNotSupported` (current Rust invents an always-true ValidatePrintTicket; remove or keep only behind an explicit quirk, never as default) |
| Ticket has unsupported MustHonor=true value | fault at CreatePrintJob, no job created |
| Unsupported soft value | create the job, substitute or ignore, expose effective ticket via `GetJobElements(PrintTicket)` |
| Bad Format in SendDocument | `ClientErrorFormatNotSupported`; job stays alive until timeout/cancel |
| Capabilities must be honest | everything the printer would reject at create/send time must be absent from `AllowedValue` lists |

## Where the job is created vs where data arrives

CreatePrintJob: ticket only, no data, returns `JobId`. SendDocument/AddDocument: data, after.
The 60-second clock starts at the CreatePrintJob response. Contrast with scan, where the data
flows out of the device after create. The emulator should never invent a `JobToken` (the response
type has none).
