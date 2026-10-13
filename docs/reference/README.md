# WS-Print reference notes (fakeprint)

Our own short, factual notes on the Microsoft WS-Print service family, written for the people
implementing the emulator. They state facts (names, shapes, rules) in our words. The Microsoft
packages and everything mechanically generated from them are NOT in git (the license forbids
redistribution); regenerate them locally, see below.

| Doc | Use when... |
|-----|-------------|
| [sources.md](sources.md) | You need to know which package/version a fact came from |
| [operations.md](operations.md) | Operation set, actions, shapes, job lifecycle, minimal conforming printer |
| [faults.md](faults.md) | Which fault subcode to raise, and when |
| [format.md](format.md) | Document formats and compression values, what Windows asks for |
| [must-honor.md](must-honor.md) | `MustHonor` semantics and the attributes that exist in print |
| [validate-vs-create.md](validate-vs-create.md) | There is no ValidatePrintTicket in print: what to do instead |
| [../rewrite-guidance.md](../rewrite-guidance.md) | Gap analysis of the Rust code and the work packages for the rewrite |

## Regenerating the machine-derived reference (local only, ignored by git)

```powershell
# 1. fetch the Microsoft packages (prints the license notice, asks you to type ACCEPT)
.\scripts\Get-WsdSpecs.ps1                       # Print by default; -Service All for scan too

# 2. generate pages + schema-model.json next to the schemas
cd ..\fakeprint-python
$env:PYTHONPATH = "python"                       # or: pip install -e .  (console script fakeprint-refs)
python -m fakeprint.wsd_docs                     # -> fakeprint\docs\reference\schema\
python -m pytest                                 # parser/doc tests; package tests skip if the zips are absent
```

Output (all ignored): `schema/index.md`, per-file pages `NN-<File>.md`, `operations/*.md`,
`types/*.md` (one per named type), `concepts/*.md` (JobState, PrinterStatus, Resolution, ...),
`faults.md`, `enumerations.md`, `attributes.md`, `extension-points.md`, `ticket.md`, `package.md`,
`elements.md` and the machine-readable `schema-model.json` (the whole parsed graph, for code
generation and validation). The tracked `schema/README.md` is older prose and is not rewritten by
the generator.

## What is different from scan (read this first)

| Topic | WS-Scan | WS-Print |
|-------|---------|----------|
| Job model | `CreateScanJob` returns `JobId` + `JobToken`; client pulls with `RetrieveImage` | `CreatePrintJob` returns only `JobId`; client pushes with `SendDocument` (bytes) or `AddDocument` (URL) |
| Ticket check | `ValidateScanTicket` is a real operation | No validate operation in v1.0, v1.1, v1.2 or v2.0. `CreatePrintJob` is the only check ([validate-vs-create.md](validate-vs-create.md)) |
| Binary payload | Response side (images, MTOM) | Request side (`DocumentData`, MTOM/XOP request) |
| Pull vs push | Device-push scans exist (`ScanAvailableEvent`) | None. Print is always client-initiated |
| Timeout | 60 s to first `RetrieveImage` | 60 s to first document after `CreatePrintJob`, also between documents |
| Fault enum | `ScanServiceFaultValuesType` | `PrintServiceFaultValuesType` (15 subcodes, `wprt:` prefix) |
| Extension versions | One schema | v1.1 (driver config, color), v1.2 (`SetPrinterElements`), v2.0 (driverless: `CreatePrintJob2` etc.) each in its own namespace |

Everything here was derived from the 2006-2015 package contents. Windows behaviour that the
package does not state is marked **unverified** and should be confirmed with a capture.
