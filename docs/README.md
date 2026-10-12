# fakeprint docs

Root [README](../README.md) is the quick start.

| Doc | Topic |
|-----|--------|
| [siblings.md](siblings.md) | **vs fakescan** — ports, ground truth, what not to copy |
| [windows.md](windows.md) | Person Add printer, WSDMON notes |
| [lab-harness.md](lab-harness.md) | **Python client** + SOAP capture expect (optional) |
| [architecture.md](architecture.md) | Process layout, ports, modules |
| [roadmap.md](roadmap.md) | Lab harness shipped; Phase 2 IPP; faults later |
| [e2e-wsd-status.md](e2e-wsd-status.md) | Live Windows gate checklist |
| [prior-art.md](prior-art.md) | Related projects and why a sibling of fakescan |
| [specs/](specs/README.md) | WS-Print / DPWS pointers |
| [assets/](assets/) | Logo / mark |

## Lab entries (optional)

| Path | Role |
|------|------|
| `scripts/fakeprint.ps1` | CLI: status / health / jobs / reset / add / lab |
| `scripts/Fakeprint.psm1` | Importable helpers (`Open-FakeprintAddDevice`, …) |
| `python/` | `fakeprint-lab` control plane + SOAP print |
| `python/fixtures/sample.pdf` | Tiny PDF for `print-soap` |

```powershell
.\scripts\fakeprint.ps1 status
.\scripts\fakeprint.ps1 lab print-soap python/fixtures/sample.pdf
```

Scan lab lives in the sibling repo — [fakescan docs](../../fakescan/docs/README.md) / [siblings.md](siblings.md).
