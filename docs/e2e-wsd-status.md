# E2E WSD Print status

**Lab log** (2026-10-11). Host: Windows, advertise `192.168.1.70:3921`, uuid `00000000-0000-4000-8000-0000000000f3`, inbox `D:\temp\fakeprint-inbox`. Binary: `D:\temp\fakeprint-target\debug\fakeprint.exe`. fakescan on `:3920` left alone.

## Gate (Phase 1)

| Step | State |
|------|--------|
| Hello / discovery (PrintDeviceType) | **live pass** — `/v1/status` Hellos rise; Probes answered with `--probe-listen` |
| Transfer Get → Printers / `VEN_FAKE&DEV_WSDPRINT` | **live pass** — also `PrintFax.Printer`, Hosted `…/WebServices/PrinterService`, ServiceId `…/PrinterService` (Brother-shaped) |
| GetPrinterElements (JobValues / DocumentValues, DefaultPrintTicket, CLS:Printer) | **live pass** — Windows called it during association (`wsd_actions.GetPrinterElements`) |
| WS-Eventing Subscribe | **live pass** — Windows Subscribed during association |
| CreatePrintJob → SendDocument → inbox file | **lab SOAP pass** — simulated SendDocument wrote `inbox/*.bin` (not a WSDMON test page) |
| Windows PnP / WSDPrintDevice | **live pass** — `SWD\DAFWSDPROVIDER\…000f3` + hosted `…/PRINTERSERVICE` class `WSDPrintDevice`, `wsdprint.inf`, Status OK |
| Spooler queue (WSDMON) + test page | **blocked** — no `Get-Printer` queue / WSD port for fakeprint; person must still **Add** (or WSDMON must finish queue create) |

## Live run (this gate)

Running args (no `RedirectStandardOutput`):

```text
fakeprint.exe --listen 0.0.0.0:3921 --advertise 192.168.1.70
  --profiles …\profiles --profile generic-wsd
  --device-uuid 00000000-0000-4000-8000-0000000000f3
  --inbox D:\temp\fakeprint-inbox --probe-listen -v
```

| Check | Result |
|-------|--------|
| Hellos | rising |
| Transfer Get (lab IWR) | 200; Printers + PrintFax.Printer + PrinterService + `VEN_FAKE&DEV_WSDPRINT` |
| After `Open-FakeprintAddDevice` | Windows hit Device/PrintService: `Get`, `GetPrinterElements`, `Subscribe` (`/v1/status` → `wsd_actions`) |
| PnP | **Fake WSD printer** SoftwareDevice + **WSDPrintDevice** OK (uuid `…f3`) |
| `Get-Printer` Fake queue | **none** |
| inbox | SOAP sim capture present (`*.bin` + `.json`); **no** Windows test-page job |

**Code changes this gate (discovery / Transfer Get alignment):**

- `crates/fakeprint/src/main.rs` — advertise + route `/WebServices/PrinterService` (Brother path; keep legacy `/PrintService` aliases)
- `crates/fakeprint/src/wsd/mod.rs` — `df:DeviceCategory` → `PrintFax.Printer`; Hosted ServiceId `…/PrinterService`; log every SOAP short action; `World::note_wsd_action`
- `crates/fakeprint/src/state.rs` — `wsd_actions` on `/v1/status`

(Parallel work also landed richer `wsd/elements.rs` GetPrinterElements; do not regress that.)

## Exact next blocker

**WSDMON spooler queue not created** after a healthy `WSDPrintDevice` PnP install.

Windows already: Hello → Transfer Get → `wsdprint.inf` → GetPrinterElements + Subscribe. Missing: print queue + WSD port → person **Add device** in Printers & scanners (or diagnose why WSDMON stops after GetPrinterElements without creating a queue). Next SOAP to watch after person Add / test page: **CreatePrintJob → SendDocument** (MTOM) → non-zero `inbox/*.bin`.

Keep uuid/`--advertise` stable (`…000f3` / `192.168.1.70`). Do not restart with a random uuid (breaks the DAF association).

## Lab recipe

```powershell
# terminal A (no RedirectStandardOutput)
$env:CARGO_TARGET_DIR = 'D:\temp\fakeprint-target'
cargo run -p fakeprint --manifest-path D:\code\fakeprint\Cargo.toml -- `
  --listen 0.0.0.0:3921 --advertise 192.168.1.70 `
  --profiles D:\code\fakeprint\profiles --profile generic-wsd `
  --device-uuid 00000000-0000-4000-8000-0000000000f3 `
  --inbox D:\temp\fakeprint-inbox --probe-listen -v

Import-Module D:\code\fakeprint\scripts\Fakeprint.psm1
Open-FakeprintAddDevice
# person: Add device next to Fake WSD printer
# person: print test page
Invoke-RestMethod http://127.0.0.1:3921/v1/status   # watch wsd_actions
Invoke-RestMethod http://127.0.0.1:3921/v1/jobs
```

## Notes

- Do not open Bluetooth settings URI.
- Never kill fakescan (`:3920`) when restarting fakeprint.
- Prefer named firewall rule `Fakeprint-WSD-3921` (TCP 3921 Private); Query User `fakeprint` rules cover the current exe path only.
- Soft check: `Invoke-RestMethod http://127.0.0.1:3921/v1/status` — `wsd_actions` proves Windows SOAP without stdout capture.
