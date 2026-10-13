# Architecture

## Goals

1. **Capture** — every completed SendDocument lands as bytes under `inbox/` (+ JSON sidecar).
2. **WSD first** — Windows Add printer via WSDMON, matching the fakescan discovery lab shape.
3. **Scriptable** — CI prints over SOAP and asserts capture without a person at the spooler.
4. **No app mocks** — the app under test talks a real print path; fakeprint is the other end.

Sibling split (scan vs print): [siblings.md](siblings.md).

## Process

One binary (`fakeprint`) serves:

1. **Control plane** — `GET /health`, `GET /v1/status`, `POST /v1/reset`, `GET /v1/jobs`, `GET /v1/jobs/{id}`, `POST /v1/profile`, `GET|POST /v1/ctl`, `GET|POST|DELETE /v1/faults`
2. **WSD device metadata** — `POST /WebServices/Device` (WS-Transfer Get, WS-MetadataExchange GetMetadata)
3. **WSD Print Service** — `POST /WebServices/PrinterService` (print operations and the WS-Eventing event source; `/PrintService` is an older alias). An operation on the wrong endpoint is answered `wsa:ActionNotSupported`.
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

**Python (optional):** `wpd_schema` / `wpd_docs` mirror Rust `print_schema` and generate the markdown type reference under `docs/reference/schema/` (`fakeprint-refs`).

## Identity (Windows)

- PnP-X `DeviceCategory`: **Printers**
- HardwareId: `VEN_FAKE&DEV_WSDPRINT`
- CompatibleId: print NS `/PrinterServiceType`
- Friendly name from profile label (`Fake WSD printer`)

## Not in this process

- Scan / WIA / MTOM RetrieveImage — that is **fakescan** on `:3920` ([siblings.md](siblings.md))
- Full IPP Everywhere / mDNS — Phase 2 ([roadmap.md](roadmap.md))

## Library use and reply validation

`fakeprint::server` is the whole server as a library: `ServerConfig::embedded(inbox)` +
`serve(cfg).await` gives a `ServerHandle` on an ephemeral loopback port with no multicast,
discovery, JetDirect, named pipe, instance lock or Ctrl-C handler, and no file written outside
the inbox; `router(world, &RouterOptions)` is the same route table for hosts that bring their
own listener. `ServerHandle::shutdown()` ends every subscription (`SubscriptionEnd`) and stops
every task.

`FAKEPRINT_VALIDATE_REPLIES=1` (or `--validate-replies`, or `ServerConfig::validate_replies`)
runs every outgoing SOAP body (operation replies, fault structure, event notifications)
through the schema validator. A violation is logged at error level and counted:
`/v1/status` shows `validation.{enabled,checked,problems,recent}`, tests read
`PrintCtx::validation_problems()` / `ServerHandle::validation_problems()`.
