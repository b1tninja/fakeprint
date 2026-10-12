# Architecture

## Goals

1. **Capture** — every completed SendDocument lands as bytes under `inbox/` (+ JSON sidecar).
2. **WSD first** — Windows Add printer via WSDMON, matching the fakescan discovery lab shape.
3. **Scriptable** — CI prints over SOAP and asserts capture without a person at the spooler.
4. **No app mocks** — the app under test talks a real print path; fakeprint is the other end.

Sibling split (scan vs print): [siblings.md](siblings.md).

## Process

One binary (`fakeprint`) serves:

1. **Control plane** — `GET /health`, `GET /v1/status`, `POST /v1/reset`, `GET /v1/jobs`
2. **WSD device metadata** — `POST /WebServices/Device` (WS-Transfer Get)
3. **WSD Print Service** — `POST /PrintService` (GetPrinterElements, job ops)
4. **WS-Discovery** — UDP Hello/Bye on 239.255.255.250:3702 (Hello always; Probe bind optional)
5. **IPP on `/ipp/print`** (default on; `--no-ipp` disables) — Get-Printer-Attributes and Print-Job. A Print-Job that carries a document is stored in `inbox/` the same way a WSD job is. Unknown operations return IPP status `0x0501`.
6. **JetDirect RAW** — TCP `:9100` by default (`--no-jetdirect` disables). Windows `Add-PrinterPort -PrinterHostAddress` always creates a RAW port on 9100.

Default TCP listen: **3921**. Job bytes land under `--inbox` (default `inbox/`).

```
fakeprint host (:3921)
  control API  --+
  job / inbox  --+--> print job FSM --> WSD Print + WS-Discovery
  profile JSON --+                  \-> IPP /ipp/print and JetDirect :9100
                        |
                        v
             WSDMON / Windows spooler
                        |
                        v
             apps under test / fakeprint-lab
```

**Lab harness** (optional Python client, not a Cargo dep): [lab-harness.md](lab-harness.md) / `python/`.

## Modules

| Module | Role |
|--------|------|
| `discovery` | WS-Discovery with `wprt:PrintDeviceType` |
| `wsd` | SOAP Print + Transfer Get |
| `profile` | JSON capability profiles |
| `state` | Jobs + inbox capture |
| `ipp` | Get-Printer-Attributes and Print-Job capture (default on); full IPP Everywhere later |
| `jetdirect` | RAW `:9100` capture |
| `lab` | Instance lock, registry path, lab UUID |

## Identity (Windows)

- PnP-X `DeviceCategory`: **Printers**
- HardwareId: `VEN_FAKE&DEV_WSDPRINT`
- CompatibleId: print NS `/PrinterServiceType`
- Friendly name from profile label (`Fake WSD printer`)

## Not in this process

- Scan / WIA / MTOM RetrieveImage — that is **fakescan** on `:3920` ([siblings.md](siblings.md))
- Full IPP Everywhere / mDNS — Phase 2 ([roadmap.md](roadmap.md))
