# Sibling: fakescan

**fakeprint** and **[fakescan](../../fakescan)** are companion lab devices for Windows print and scan testing. Same host can run both; keep ports and UUIDs distinct.

| | **fakescan** | **fakeprint** |
|--|--------------|---------------|
| Role | Fake **scanner** | Fake **printer** |
| Default TCP | `:3920` | `:3921` |
| Wire (Phase 1) | WSD Scan + WIA | WSD Print + WSDMON |
| Lab ground truth | **Hard loopback** — upload JFIF → exact RetrieveImage echo | **Capture-assert** — SOAP/print → inbox / `GET /v1/jobs` |
| Python package | `python/` → `fakescan-lab` | `python/` → `fakeprint-lab` |
| PowerShell | `scripts/fakescan.ps1` | `scripts/fakeprint.ps1` |
| Docs | [fakescan docs](../../fakescan/docs/README.md) | this tree |

## Do not copy across

| Belongs in fakescan only | Belongs in fakeprint only |
|--------------------------|---------------------------|
| `/v1/loopback`, pages, pattern, effects, scenario | Job capture inbox + `/v1/jobs` |
| Feeder / jam / WIA acquire glue | Print job FSM / SendDocument |
| `pdfcraft-cli scan` lab path | Spooler / Add printer lab path |

Shared by design (copy patterns, not crates): WS-Discovery, Transfer Get shape, “person clicks Add device,” optional Python control client.

- Scan lab: [fakescan lab-harness](../../fakescan/docs/lab-harness.md)
- Print lab: [lab-harness.md](lab-harness.md)
