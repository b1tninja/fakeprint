# fakeprint

<p align="center">
  <img src="docs/assets/fakeprint-logo.svg" alt="fakeprint" width="380" />
</p>

A **fake network printer** for testing Windows print queues and job capture — without a physical MFP.

Companion to **[fakescan](../fakescan)** (scan-only on `:3920`). Phase 1 is **WSD Print** (WS-Discovery → WSDMON Add printer → capture job bytes to `inbox/`). **IPP Everywhere** is stubbed for Phase 2. How the two labs differ: [docs/siblings.md](docs/siblings.md).

## Docs

| Doc | Start here for… |
|-----|-----------------|
| [docs/README.md](docs/README.md) | Full index |
| [docs/siblings.md](docs/siblings.md) | vs fakescan (ports, loopback ≠ capture) |
| [docs/windows.md](docs/windows.md) | Add printer (person clicks Add device) |
| [docs/lab-harness.md](docs/lab-harness.md) | Python lab client (SOAP capture expect) |
| [docs/architecture.md](docs/architecture.md) | Layout and ports |
| [docs/roadmap.md](docs/roadmap.md) | Shipped lab; IPP / faults later |
| [docs/prior-art.md](docs/prior-art.md) | Why not cupsd / fake-printer alone |
| [docs/e2e-wsd-status.md](docs/e2e-wsd-status.md) | Live Windows gate |

## Quick start

```bash
cargo run -p fakeprint -- --profiles profiles --profile generic-wsd
# LAN: --listen 0.0.0.0:3921 --advertise 192.168.1.10 --probe-listen -v
# Stable identity: --device-uuid 00000000-0000-4000-8000-0000000000f3
```

```bash
# PowerShell
Invoke-RestMethod http://127.0.0.1:3921/health
Invoke-RestMethod http://127.0.0.1:3921/v1/status
```

Default port **3921** (fakescan uses **3920**). Both can run on the same host with distinct UUIDs.

## Lab harness (Python, optional)

Print is **ingest**: send bytes, then assert the capture. Not fakescan’s page loopback.

```bash
pip install -e "./python[dev]"
fakeprint-lab print-soap python/fixtures/sample.pdf
# or: .\scripts\fakeprint.ps1 lab print-soap python/fixtures/sample.pdf
pytest python/tests -m live   # needs fakeprint running
```

See [docs/lab-harness.md](docs/lab-harness.md).

## Status

| Piece | State |
|-------|--------|
| Scaffold / `cargo test` | working |
| Control API `/health`, `/v1/status`, `/v1/reset`, `/v1/jobs` | working |
| Profile `generic-wsd` | working |
| WS-Discovery Hello (PrintDeviceType) | **live** — advertise + stable uuid (see e2e) |
| WS-Transfer Get (Printers / VEN_FAKE&DEV_WSDPRINT) | **live** |
| GetPrinterElements / CreatePrintJob / SendDocument / EndJob | working |
| Job capture to `inbox/` | working |
| Python lab client (`python/`, optional) | working — [lab-harness.md](docs/lab-harness.md) |
| Windows Add printer + test page | **person-blocked** — [e2e-wsd-status.md](docs/e2e-wsd-status.md) |
| IPP Everywhere | stub (`--ipp` → `/ipp/print` 501) |

## Windows helpers

```powershell
.\scripts\fakeprint.ps1 status
.\scripts\fakeprint.ps1 jobs
.\scripts\fakeprint.ps1 lab print-soap python/fixtures/sample.pdf
Import-Module .\scripts\Fakeprint.psm1
Open-FakeprintAddDevice   # ms-settings:printers only
```

See [docs/windows.md](docs/windows.md).

## License

MIT OR Apache-2.0
