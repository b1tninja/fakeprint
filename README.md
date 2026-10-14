# fakeprint

<p align="center">
  <img src="docs/assets/fakeprint-logo.svg" alt="fakeprint" width="380" />
</p>

<p align="center">
  <a href="https://github.com/b1tninja/fakeprint/actions/workflows/ci.yml"><img src="https://github.com/b1tninja/fakeprint/actions/workflows/ci.yml/badge.svg" alt="ci" /></a>
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
| [docs/multi-device.md](docs/multi-device.md) | Device manager: many printers, registry, install queue, tray, web UI |
| [docs/injection.md](docs/injection.md) | Fault injection rules and timing |
| [docs/skills/fake-device-platform-ports/SKILL.md](docs/skills/fake-device-platform-ports/SKILL.md) | Porting print/scan support to Linux, macOS or another stack with a fake device and a reference client |

## Quick start

```bash
cargo run -p fakeprint -- --profiles profiles --profile generic-wsd
# LAN: --listen 0.0.0.0:3921 --advertise 192.168.1.10 --probe-listen -v
# Stable identity: --device-uuid 00000000-0000-4000-8000-0000000000f3
```

```powershell
Invoke-RestMethod http://127.0.0.1:3921/health
Invoke-RestMethod http://127.0.0.1:3921/v1/status
```

Default port **3921** (fakescan uses **3920**). Both can run on the same host with distinct UUIDs.

## Lab harness (Python, optional)

Print is **ingest**: send bytes, then assert the capture. Not fakescan’s page loopback.

```bash
pip install -e "./python[dev]"
fakeprint-lab print-test                          # visible Letter page + magic
# blank CI only: fakeprint-lab print-soap python/fixtures/sample.pdf
pytest python/tests -m live   # needs fakeprint running
```

```powershell
.\scripts\fakeprint.ps1 print-test -Open   # spooler; scores signature RGB fills
.\scripts\fakeprint.ps1 lab prove-capture  # score latest inbox .bin
.\scripts\fakeprint.ps1 open-job
```

See [docs/lab-harness.md](docs/lab-harness.md).

## Status

| Piece | State |
|-------|--------|
| Scaffold / `cargo test` | working |
| Control API `/health`, `/v1/status`, `/v1/reset`, `/v1/jobs`, `/v1/ctl` | working |
| Profile `generic-wsd` | working |
| WS-Discovery Hello (PrintDeviceType) | **live** — advertise + stable uuid (see e2e) |
| WS-Transfer Get (Printers / VEN_FAKE&DEV_WSDPRINT) | **live** |
| GetPrinterElements / CreatePrintJob / SendDocument / EndJob | working |
| Job capture to `inbox/` | working |
| Lab spooler queue (IPP / WSDMON or JetDirect `:9100`) | **live** — `ensure` / `install-ipp` |
| Visible `print-test` (SOAP + spooler color proof) | **live** — [fixtures/README](python/fixtures/README.md) |
| Python lab client (`python/`, optional) | working — [lab-harness.md](docs/lab-harness.md) |
| IPP Get-Printer-Attributes + Print-Job | working on `/ipp/print` (`peer=ipp`) |
| IPP Everywhere (full) | **later** (Phase 2) |

## Windows helpers

```powershell
.\scripts\fakeprint.ps1 status
.\scripts\fakeprint.ps1 jobs
.\scripts\fakeprint.ps1 print-test -Open
.\scripts\fakeprint.ps1 ensure
Import-Module .\scripts\Fakeprint.psm1
Open-FakeprintAddDevice   # ms-settings:printers only (WSD path)
```

See [docs/windows.md](docs/windows.md).

## Why a fake device and a reference client

fakeprint exists so that printing code can be developed and tested without a real printer, and so that the same code can be
compared with a platform's own client:

* **A fake device is deterministic, safe and complete.** Real printers are slow, stateful and shared, and they will not give you a
  paper jam on page 3, a dropped connection or an HTML error page on demand. fakeprint speaks the protocols on loopback, validates
  every reply against the specification, and lets a test script capabilities, levels, conditions, timing and faults per printer.
* **A known-good reference client defines correct.** Each platform ships its own way to print (the Windows spooler, CUPS `lp` and
  `ipptool`, the macOS print stack). Driving that client at the same fake and comparing what arrives (document bytes, pixels,
  tickets, job outcomes and errors) shows where our client differs. The specification, not the reference and never a real device,
  decides which side is wrong.
* **Inject every fault.** With one rule format (`op`, `skip`, `count`, `delay_ms`, then pass, fault, HTTP status, or close the
  connection) a test can prove that each error is handled with a typed error and that "as good as the reference" holds under failure,
  not just on the happy path.

To repeat this for Linux or macOS (a CUPS/IPP fake and a `lp`/`ipptool` reference; eSCL and SANE or Image Capture for scanning) read
[docs/skills/fake-device-platform-ports/SKILL.md](docs/skills/fake-device-platform-ports/SKILL.md): the layering to copy, the build order,
what the fake must be able to do, the parity battery, the safety rules, and the definition of done. fakeprint already includes an
`ipp/` module and a raw-9100 listener to build on.

## Develop

```bash
cargo fmt --all -- --check
cargo clippy -p fakeprint --all-targets -- -D warnings
cargo test -p fakeprint
pip install -e "./python[dev]"
pytest python/tests -m "not live"
```

`.github/workflows/ci.yml` runs fmt, clippy, and `cargo test` on Ubuntu and Windows, then the offline Python tests. Live pytest needs a running server.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE)
