# Roadmap

Status markers: **shipped** | **next** | **later**.

## Phase 1 (current scaffold)

- [x] Workspace + profile + control plane
- [x] WS-Discovery PrintDeviceType
- [x] Transfer Get (Printers / WSDPRINT)
- [x] GetPrinterElements / CreatePrintJob / SendDocument / EndJob → inbox
- [x] Live Windows Add + visible test page (lab) — `print-test` / `lab print-test`

## Lab harness

| Item | Status |
|------|--------|
| Python control-plane + SOAP print client (`python/`) | **shipped** ([lab-harness.md](lab-harness.md)) |
| `scripts/fakeprint.ps1` (status / jobs / reset / add / lab) | **shipped** |
| Fixture `python/fixtures/sample.pdf` (blank CI) | **shipped** |
| Fixture `python/fixtures/testpage.pdf` + capture color proof | **shipped** (`print-test`, `open-job`) |
| Live `print-soap` capture expect | **shipped** (once; re-run with server up) |
| Fault injection (`POST /v1/fault`) | **later** (print is capture-in; no fakescan loopback) |
| IPP Get-Printer-Attributes and Print-Job on `/ipp/print` | **shipped** (default on; a document is stored in `inbox/`) |
| IPP Everywhere lab path | **later** (Phase 2) |

## Phase 2

- IPP Everywhere (mDNS, format negotiation). Basic Print-Job capture on `/ipp/print` is already on
- mDNS / Bonjour `_ipp._tcp` advertisement
- Document format negotiation that Windows IPP class driver accepts

## Later

- WS-Print v1.1 `SupportsWSPrintv11` + DriverConfiguration
- WS-Print v1.2 `SetPrinterElements`
- Fault injection (jam, out of paper, offline) via `/v1/*` — not `/v1/loopback`
- Docker image + GHCR
