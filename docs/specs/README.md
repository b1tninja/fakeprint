# Specs index

| Spec | Notes |
|------|--------|
| WS-Print / Print Device Definition | [MS WS-Print](https://learn.microsoft.com/en-us/windows-hardware/drivers/print/ws-print-v1-1) — download Print Device Definition V1.0 (Word + WSDL/XSD) |
| WSDMON | [Port monitor](https://learn.microsoft.com/en-us/windows-hardware/drivers/print/wsdmon-port-monitor) |
| **Add-device SOAP deltas** | [ws-print-add-checklist.md](ws-print-add-checklist.md) — ranked Transfer Get / GetPrinterElements gaps vs MS samples + fixture |
| Fixture (target XML) | [fixtures/get-printer-elements-ms-shaped.xml](fixtures/get-printer-elements-ms-shaped.xml) |
| WS-Discovery / DPWS / Transfer | Same stack as fakescan; see fakescan `docs/specs/` |
| IPP `/ipp/print` | Get-Printer-Attributes on by default; IPP Everywhere (PWG 5100.14) is Phase 2 |

Phase 1 implements **WS-Print v1.0**-shaped elements only (PrinterDescription, Status, Capabilities, job ops). Do not claim v1.1/v1.2 until Add works.

## Implemented surface (code, 2026-10-11)

| Op / element | Status |
|--------------|--------|
| `GetPrinterElements` → Description / Status (`PrinterCurrentTime`+`PrinterLocalTime`) / Configuration / DefaultPrintTicket / Capabilities (`JobValues`+`DocumentValues`) | done; unknown QName → `Valid="false"` |
| IEEE-1284 `DeviceId` with `CLS:Printer` | done (`profiles/generic-wsd.json`) |
| `CreatePrintJob` (`JobId` + lax `JobToken`) / `SendDocument` (base64 + MTOM `xop:Include`) / `AddDocument` stub | done; `print_schema` allows lab `JobToken` / `PrinterLocalTime` (rejects scan `Images*`) |
| `GetJobElements` / `GetActiveJobs` / `GetJobHistory` / `SetEventRate` / `ValidatePrintTicket` soft stub | done |
| WS-Eventing Subscribe → PrintService `SubscriptionManager` | done (fakescan-shaped; no scan DestinationResponses) |

See [ws-print-add-checklist.md](ws-print-add-checklist.md) for remaining Add-device hypotheses.
