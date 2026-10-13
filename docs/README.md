# fakeprint docs

Root [README](../README.md) is the quick start.

| Doc | What it is |
|-----|------------|
| [siblings.md](siblings.md) | **vs fakescan** — ports, ground truth, what not to copy |
| [lab-instance.md](lab-instance.md) | **Single-instance guard, registry, ctl, pipe, MCP** |
| [windows.md](windows.md) | Person Add printer, WSDMON notes |
| [lab-harness.md](lab-harness.md) | **Python client** + SOAP capture expect (optional) |
| [profiles.md](profiles.md) | Profile format, defaults, the smallest valid profile |
| [injection.md](injection.md) | Request fault injection (`/v1/faults` `inject`), runtime job timing |
| [architecture.md](architecture.md) | Process layout, ports, modules |
| [roadmap.md](roadmap.md) | Lab harness shipped; Phase 2 IPP; faults later |
| [e2e-wsd-status.md](e2e-wsd-status.md) | Live Windows gate checklist |
| [prior-art.md](prior-art.md) | Related projects and why a sibling of fakescan |
| [../CONTRIBUTING.md](../CONTRIBUTING.md) | fmt, clippy, tests, license |
| [specs/](specs/README.md) | WS-Print / DPWS pointers |
| [assets/](assets/) | Logo: beige printer + **Fp**; LCD **PC LOAD LETTER** |

## Lab entries (optional)

| Path | Role |
|------|------|
| `scripts/fakeprint.ps1` | CLI: ensure / ctl / print-test / open-job / list / add / remove / save / mark-added / status / lab |
| `scripts/Fakeprint.psm1` | Importable helpers — registry, ensure, ctl, discovery |
| `python/` | `fakeprint-lab` + `fakeprint-mcp` (optional `[mcp]`) |
| `python/fixtures/sample.pdf` | Blank tiny PDF for byte-exact `print-soap` CI |
| `python/fixtures/testpage.pdf` | Visible Letter page for `print-test` (magic `FAKEPRINT-TESTPAGE-v1`) |

```powershell
.\scripts\fakeprint.ps1 ensure            # start or attach
.\scripts\fakeprint.ps1 ctl status        # POST /v1/ctl
.\scripts\fakeprint.ps1 list              # registry rows
.\scripts\fakeprint.ps1 print-test
.\scripts\fakeprint.ps1 lab print-test
```

Scan lab lives in the sibling repo — [fakescan docs](../../fakescan/docs/README.md) / [siblings.md](siblings.md).
