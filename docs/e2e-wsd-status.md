# E2E WSD Print status

**Lab log** (2026-10-11 evening). Host: Windows, advertise `192.168.1.70:3921`, uuid `00000000-0000-4000-8000-0000000000f3`, inbox `D:\temp\fakeprint-inbox`. Binary: `D:\temp\fakeprint-target\debug\fakeprint.exe` (`CARGO_TARGET_DIR=D:\temp\fakeprint-target`). fakescan on `:3920` left alone. Machine IP: `192.168.1.70` (single LAN interface).

## Gate (Phase 1) — updated 2026-10-12 01:15

| Step | State |
|------|--------|
| Hello / discovery (PrintDeviceType) | **live pass** — Hellos + ProbeMatches (`wsd_probes_answered` rises with `--probe-listen`) |
| Transfer Get → Printers / `VEN_FAKE&DEV_WSDPRINT` | **live pass** — `wsd_actions.Get` rises continuously; Hosted `…/WebServices/PrinterService`, CompatibleId URI, `PrintFax.Printer` |
| GetPrinterElements (JobValues / DocumentValues, DefaultPrintTicket, CLS:Printer) | **live pass** — `wsd_actions.GetPrinterElements` rose during Add; WSDMON read caps |
| IPP Get-Printer-Attributes at `/ipp/print` | **live pass** — `ipp_actions.GetPrinterAttributes` rises; valid IPP 2.0 response; `ipp_uuid` field in `/v1/status` carries the advertised `printer-uuid` |
| WS-Eventing Subscribe (triple-burst) | **live pass (IPP path)** — `wsd_actions.Subscribe=3`; WSDMON subscribes to `PrinterElementsChangeEvent` + `PrinterStatusSummaryEvent` + `JobEndStateEvent`; NotifyTo `192.168.1.70:5357` reachable; port committed via IPP UUID (see note below) |
| Windows PnP / WSDPrintDevice | **live pass** — `SWD\DAFWSDPROVIDER\…000f3` + hosted `…/PRINTERSERVICE` class `WSDPrintDevice`, `wsdprint.inf`, Status OK |
| WSD port in registry (`HKLM\…\WSD Port\Ports\WSD-dfcc9fab-…`) | **live pass** — `PrinterUUID = 00000000-0000-4000-8000-0000000000f4` (ipp_uuid), `Install Protocol = 1` (IPP) |
| Spooler queue (WSDMON) + test page | **live pass** — `fakeprint Fake WSD Print` queue, port `WSD-dfcc9fab-…`, `SWD\PRINTENUM\WSD-dfcc9fab-…` Status=Started, driver `prnms012.inf` |

## Current live status (2026-10-12 01:15)

```
wsd_actions:  { Get: 36 }   ← fakeprint restarted; counters reset; Windows side persists
ipp_actions:  { GetPrinterAttributes: 8 }
ipp_uuid:     00000000-0000-4000-8000-0000000000f4   ← IPP printer-uuid advertised

Spooler queue:  "fakeprint Fake WSD Print"  port=WSD-dfcc9fab-…  driver=Microsoft IPP Class Driver
PRINTENUM:      SWD\PRINTENUM\WSD-dfcc9fab-…  Status=Started
WSD port reg:   PrinterUUID=…000f4  Install Protocol=1 (IPP)  (no PortId — IPP path)
JD queue:       "Fake WSD printer"  port=Fakeprint-JD-default  (working; do not remove)
```

**Both queues are installed.** The WSD queue (`fakeprint Fake WSD Print`) was committed by WSDMON via the IPP path (Protocol=1) using the `ipp_uuid` from `/v1/status`. The JetDirect queue (`Fake WSD printer`) remains on `Fakeprint-JD-default`. Test page to the WSD queue is the next step.

### Subscribe triple-burst (new finding, 2026-10-12)

WSDMON sends **three Subscribe requests in rapid succession** (same millisecond burst, same `NotifyTo: http://192.168.1.70:5357/...`), each with a distinct filter:

| # | Filter (action URI suffix) |
|---|---------------------------|
| 1 | `PrinterElementsChangeEvent` |
| 2 | `PrinterStatusSummaryEvent` |
| 3 | `JobEndStateEvent` |

**Implemented (needs fakeprint restart to pick up):** Subscribe Filter selects the Notify action. Immediate push for `PrinterStatusSummaryEvent` (Idle/None) and `PrinterElementsChangeEvent` (PrinterStatus stub). `JobEndStateEvent` is stored and fired when a job ends (SendDocument last / EndJob / CancelJob) — not on Subscribe. Unknown/empty Filter still pushes legacy `PrinterStatusChanged`.

**However, the WSD port was committed via the IPP path, not the Subscribe push.** WSDMON saw `CID:MS_IPP` in GetPrinterElements, probed `/ipp/print`, read `printer-uuid = 00000000-0000-4000-8000-0000000000f4` from the IPP GetPrinterAttributes response, and created an IPP-based WSD port (`Install Protocol=1`, `PrinterUUID = …000f4`). Matching events make a non-IPP / WSDMON-only path durable; they are not required for the current IPP queue.

## Root cause analysis (2026-10-11)

### What was confirmed

1. **`generic-wsd.json` has `CID:MS_IPP`** (updated in a prior session). WSDMON uses `prnms012.inf` (Microsoft IPP Class Driver) for this CID.
2. **IPP route (`/ipp/print`) was already registered by default** (`--ipp` default=true in `main.rs`). IPP Get-Printer-Attributes returns a valid binary IPP 2.0 response. `ipp_actions` in `/v1/status` now tracks WSDMON probes.
3. **WSDMON (APMon.dll) calls Subscribe on every Add Device**, then waits for a `PrinterStatusChanged` notification before creating the WSD port registry entry and `SWD\PRINTENUM` device. Without the push, Subscribe is retried ~3 times and the spooler queue is never created.
4. **setupapi.dev.log** shows full driver install for fakescan (`wsdscdrv.inf`) but **no** `[Device Install (Hardware initiated)]` section for fakeprint's PRINTERSERVICE — the WSDPrintDevice class install (wsdprint.inf) was cached from a prior session and not re-logged.
5. **WSD Port Monitors registry** (`HKLM:\...\WSD Port\Ports`) shows Brother's port (`WSD-43c6ea17-…`, Install Protocol=1, IPP PortId, WSD PortId) but nothing for fakeprint.
6. **Both adapters empty**: `Adapters\IPP` and `Adapters\WSPrint\OfflinePorts` are empty — WSDMON has not committed a port for fakeprint in either adapter.

### Primary blocker (updated 2026-10-12): Subscribe action URI mismatch

WSDMON calls Subscribe during Add Device in a rapid triple-burst, each call using a distinct filter:

1. `http://schemas.microsoft.com/windows/2006/08/wdp/print/PrinterElementsChangeEvent`
2. `http://schemas.microsoft.com/windows/2006/08/wdp/print/PrinterStatusSummaryEvent`
3. `http://schemas.microsoft.com/windows/2006/08/wdp/print/JobEndStateEvent`

fakeprint returns `SubscribeResponse` and then pushes a `PrinterStatusChanged` notification (SOAP Action `…/PrinterStatusChanged`) to the `NotifyTo` address. Port 5357 is reachable (`TcpTestSucceeded: True`), so the TCP POST succeeds. However, the Action header on the pushed notification does not match any of the three subscribed filters. WSDMON likely discards the notification, retries Subscribe, and eventually stops without committing `SWD\PRINTENUM\WSD-*`.

**Previous understanding (superseded):** The earlier analysis identified the missing push (no notification sent at all) as the blocker, and a `PrinterStatusChanged` push was added. That push is now being delivered, but its action URI is wrong for all three subscriptions.

**Fix (in tree; restart fakeprint to load):** Push the matching notification for each Subscribe, keyed on the `wse:Filter` value in the incoming body. Immediate `PrinterStatusSummaryEvent`:
```xml
<wprt:PrinterStatusSummary>
  <wprt:PrinterState>Idle</wprt:PrinterState>
  <wprt:PrinterPrimaryStateReason>None</wprt:PrinterPrimaryStateReason>
</wprt:PrinterStatusSummary>
```
and SOAP Action `http://schemas.microsoft.com/windows/2006/08/wdp/print/PrinterStatusSummaryEvent`. Same 150 ms delay + raw TCP POST. `PrinterElementsChangeEvent` is pushed on Subscribe; `JobEndStateEvent` is stored and fired when a job ends.

### Secondary observation: IPP probe URL

WSDMON probes `/ipp/print` on the **same host:port as WSD** (port 3921). The existing IPP handler already returned a correct response — the `ipp_actions` counter in `/v1/status` confirms WSDMON reached it. The `printer-uri-supported` in the response now uses the dynamic advertised IP (not hardcoded `192.168.1.70`).

## Fixes applied this gate (2026-10-11 evening)

1. **Filter-matching print events after Subscribe** (`wsd/mod.rs` `subscribe()`):
   - After `SubscribeResponse`, push the event named in `wse:Filter` (`PrinterStatusSummaryEvent` / `PrinterElementsChangeEvent`; `JobEndStateEvent` on job end).
   - Unknown Filter still pushes legacy `PrinterStatusChanged`.
   - Uses raw `tokio::net::TcpStream` — no new dependencies.
   - Subscribe body dumped under `%TEMP%\fakeprint-subscribe-{ms}.xml`.

2. **Dynamic IPP URI** (`ipp/mod.rs` + `state.rs`):
   - `World::ipp_uri()` derives `ipp://host:port/ipp/print` from `device_endpoint` at runtime (was hardcoded `192.168.1.70:3921`).
   - `print_handler` now takes `State<Arc<RwLock<World>>>` to read the live URI.
   - `ipp_actions` counter added to `/v1/status` (e.g. `GetPrinterAttributes`) so WSDMON probes are observable without stdout capture.

3. **IPP route always on with WSD** (`main.rs`): confirmed default `--ipp=true` already in place; also ensured `--no-ipp` disables it. Route uses `ipp::print_handler` (not the old stub).

4. **`tokio/io-util`** feature added to `Cargo.toml` for `AsyncWriteExt` in the TCP notification send.

5. **`CID:MS_IPP` profile** (`generic-wsd.json`): kept — matches `prnms012.inf` (Microsoft IPP Class Driver, `1284_CID_MS_IPP`); driver is in driver store.

## Live run args

```text
fakeprint.exe --listen 0.0.0.0:3921 --advertise 192.168.1.70
  --profiles D:\code\fakeprint\profiles --profile generic-wsd
  --device-uuid 00000000-0000-4000-8000-0000000000f3
  --inbox D:\temp\fakeprint-inbox --probe-listen -v
```

Start Hidden so shell abort does not kill it; do **not** `RedirectStandardOutput` (port bind race). Prefer `Invoke-RestMethod http://127.0.0.1:3921/v1/status` for `wsd_actions` + `ipp_actions`.

## Non-interactive spooler install (2026-10-12, updated)

`ensure` / `install-ipp` (admin or UAC one-shot):

1. `Add-Printer -IppURL http://<advertise>:3921/ipp/print` -- **now works** (Internet Port, `WSD Port Monitor`). Requires the new binary with `--ipp-uuid` support.
2. Fallback (old binary or if `-IppURL` fails): JetDirect TCPMON `:9100`.

```powershell
.\scripts\fakeprint.ps1 install-ipp -Force   # UAC if not elevated; creates Internet Port
.\scripts\fakeprint.ps1 ensure               # same path when added.pnp=false
.\scripts\fakeprint.ps1 ensure -WsdAdd       # skip; open Settings (WSDMON)
```

Live `PrintTo` via Internet Port (`WSD Port Monitor`, `peer=ipp`) confirmed 2026-10-12.

Prefer `.\scripts\fakeprint.ps1 print-test` (visible `testpage.pdf` with magic `FAKEPRINT-TESTPAGE-v1`) over a blank PDF or Settings “Print test page”. SOAP proof: `.\scripts\fakeprint.ps1 lab print-test`.

**Do not** pass an `http://` URL to `Add-PrinterPort` -- that creates TCPMON with
`PrinterHostAddress=http://...` and jobs stick at Printing/Retained.

---

## Root cause of `Add-Printer -IppURL` failure and fix (2026-10-12)

### Investigation

| Symptom | Observed before fix |
|---------|-------------------|
| Old error | "device was not found" (no printer-uuid in GPA) |
| New error (after printer-uuid added) | "The specified printer already exists" |
| `-Name "Fake-IPP-Test"` (different name) | Still "The specified printer already exists" |
| After removing JD queue | Still "The specified printer already exists" |
| `ipp_actions.GetPrinterAttributes` | +3 per `-IppURL` attempt -- server IS reached |

Windows calls Get-Printer-Attributes, reads `printer-uuid = urn:uuid:...f3`, then checks
`SWD\DAFWSDProvider` for a device with that UUID. It finds the WSD device
`SWD\DAFWSDProvider\urn:uuid:00000000-0000-4000-8000-0000000000f3` and refuses to create
an Internet Port for a UUID already registered as a WSD device -- regardless of printer name.

### Fix (`lab.rs` + `state.rs` + `ipp/mod.rs`, 2026-10-12)

- **`LAB_IPP_UUID`** (`...f4`) -- stable companion to `LAB_DEVICE_UUID` (`...f3`).
- **`derive_ipp_uuid(device_uuid)`** -- last byte +1 (wrapping); stable + deterministic.
- **`--ipp-uuid` CLI flag** -- defaults to `LAB_IPP_UUID`; auto-derives when equal to `--device-uuid`.
- **`World::ipp_uuid()` / `World::set_ipp_uuid()`** -- stored in state, exposed in `/v1/status`.
- **`ipp/mod.rs`** -- `build_printer_attributes_response` uses `world.ipp_uuid()` (not `device_uuid`).

`/v1/status` now shows:
```json
{ "device_uuid": "00000000-0000-4000-8000-0000000000f3",
  "ipp_uuid":    "00000000-0000-4000-8000-0000000000f4" }
```

With `ipp_uuid = ...f4` not in `SWD\DAFWSDProvider`, `Add-Printer -IppURL` creates a
proper Internet Port (`WSD Port Monitor`, Description `IPP Port`, port name `WSD-{guid}`).

### Script changes (`Fakeprint.psm1`)

`Install-FakeprintIppQueue`:
- Passes `-Name $label` to `Add-Printer -IppURL`; falls back to auto-name on conflict.
- **Treats a JD queue the same as legacy TCPMON**: removes it before `-IppURL` so the label is free.
- `$created` detection finds the new Internet Port by `WSD Port Monitor` (not URL-as-port-name).

`Remove-FakeprintIppQueue`:
- Captures `WSD-*` Internet Port name before `Remove-Printer`; cleans it up after.

IPP GPA now advertises `printer-uuid = urn:uuid:...f4` (distinct from WSD uuid `...f3`),
http+ipp `printer-uri-supported`, Create-Job / Send-Document ops.
---

## Phase 1 complete — WSD queue installed (2026-10-12)

Both queues are now installed:

| Queue | Port | Path |
|-------|------|------|
| `fakeprint Fake WSD Print` | `WSD-dfcc9fab-…` (Protocol=1 IPP) | WSD → IPP |
| `Fake WSD printer` | `Fakeprint-JD-default` | JetDirect `:9100` |

`added.pnp=true` is set in the registry. Registry: `HKLM:\…\WSD Port\Ports\WSD-dfcc9fab-…` exists.

**Visible test page (2026-10-12):** done — do not use a blank PDF or Settings “Print test page” as proof.

```powershell
.\scripts\fakeprint.ps1 print-test -Open   # spooler; scores signature RGB fills
.\scripts\fakeprint.ps1 lab print-test     # SOAP; magic FAKEPRINT-TESTPAGE-v1
.\scripts\fakeprint.ps1 lab prove-capture  # score latest inbox .bin
.\scripts\fakeprint.ps1 open-job           # preview capture as temp .pdf
```

Live: PrintTo `Fake WSD printer` / WSD port → inbox ~46 KiB, `VisualProof` 5/5 colors (magic stripped by IPP Class Driver). Blank ~795-byte captures fail the color check.

---

## Historical: Add Device steps (for reference)

**WSDPrintDevice is already installed and OK.** If the WSD queue is ever removed
and needs to be re-created, the person step is:

**Open ms-settings:printers, click Add device:**

```powershell
# Recommended: opens Settings and starts the milestone watcher in one step.
.\scripts\fakeprint.ps1 add-device          # banner + beep + balloon + watcher

# Or split across two windows:
.\scripts\fakeprint.ps1 add-device -Quiet   # window 1: Settings only (no beep)
.\scripts\fakeprint.ps1 watch-wsd           # window 2: milestone watcher
```

`add-device` fires `Open-FakeprintAddDevice` (banner / beep / balloon) then runs
`Watch-FakeprintWsdAdd` which polls `/v1/status` and prints each milestone as it
arrives. Press Ctrl+C to abort the watcher early without killing fakeprint.

### Verify checklist (watcher surfaces these automatically)

| # | What to see | Meaning |
|---|-------------|---------|
| 1 | `wsd_actions.Subscribe` rises | WSDMON called Subscribe |
| 2 | `wsd_actions.GetPrinterElements` rises | WSDMON read caps |
| 3 | `D:\temp\fakeprint-subscribe-*.xml` appears | `NotifyTo` address logged |
| 4 | fakeprint log: `print event notification sent` with `kind=PrinterStatusSummaryEvent` | matching Filter push delivered (run foreground `-v`) |
| 5 | `SWD\PRINTENUM\WSD-*` in pnputil | WSD port committed |
| 6 | `Get-Printer \| Where-Object PortName -like '*WSD*'` | queue ready |
| 7 | Registry `HKLM:\…\WSD Port\Ports\WSD-{uuid}` — `Install Protocol = 1` | port monitor entry |

Manual checks (if not using the watcher):

```powershell
Invoke-RestMethod http://127.0.0.1:3921/v1/status | ConvertTo-Json
pnputil /enum-devices /class "{4d36e979-e325-11ce-bfc1-08002be10318}" | Select-String "WSD-0000"
Get-Printer | Where-Object PortName -like '*WSD*'
```

### If Subscribe is not called during Add Device

WSDMON may have cached the DeviceId from a prior session and skipped GPE entirely. Force a re-negotiation:

```powershell
# Full device remove: wipes WSDMON's cached state
pnputil /remove-device "SWD\DAFWSDProvider\urn:uuid:00000000-0000-4000-8000-0000000000f3/uri:00000000-0000-4000-8000-0000000000f3/PrinterService" /subtree
pnputil /remove-device "SWD\DAFWSDProvider\urn:uuid:00000000-0000-4000-8000-0000000000f3" /subtree
# Wait for fakeprint to Hello again, then Add Device
```

### If Subscribe IS called but no port appears

Check `D:\temp\fakeprint-subscribe-*.xml` for the `NotifyTo` address and the `wse:Filter` values.

**Observation (2026-10-12, then patched):** WSDMON subscribes to three filters in a burst; the old binary pushed `PrinterStatusChanged` (matched none). Port still committed via IPP. Matching Filter pushes are now in `wsd/mod.rs` (restart to load). Non-IPP WSDMON-only Add needs those events.

```powershell
# Read the three most recent Subscribe dumps and their filters:
Get-ChildItem D:\temp\fakeprint-subscribe-*.xml | Sort-Object LastWriteTime | Select-Object -Last 3 |
    ForEach-Object { Write-Host $_.Name; Select-Xml -Path $_.FullName -XPath '//*[local-name()="Filter"]' | ForEach-Object { $_.Node.InnerText } }

# Verify NotifyTo reachability:
Test-NetConnection -ComputerName 192.168.1.70 -Port 5357
```

## Lab recipe

```powershell
$env:CARGO_TARGET_DIR = 'D:\temp\fakeprint-target'
# start Hidden, no RedirectStandardOutput — see running args above

Import-Module D:\code\fakeprint\scripts\Fakeprint.psm1

# One-liner: opens Settings + starts milestone watcher (press Ctrl+C to abort watcher)
.\scripts\fakeprint.ps1 add-device
# → person: click "Add device" next to "Fake WSD printer"
# → watcher prints each milestone (Subscribe / GPE / PRINTENUM / WSD queue)

# Manual spot-checks (also available while watcher runs in another window):
Invoke-RestMethod http://127.0.0.1:3921/v1/status   # wsd_actions + ipp_actions
Get-ChildItem D:\temp\fakeprint-subscribe-*.xml      # NotifyTo address dumps
Get-ChildItem D:\temp\fakeprint-fault-*.xml          # fault body dumps
```

## Notes

- Do not open Bluetooth settings URI.
- Never kill fakescan (`:3920`) when restarting fakeprint.
- Prefer named firewall rule `Fakeprint-WSD-3921` (TCP 3921 Private).
- Soft check: `wsd_actions` + `ipp_actions` on `/v1/status` proves Windows SOAP/IPP without stdout capture.
- `D:\temp\fakeprint-subscribe-*.xml` dumps are written on every Subscribe call — review to confirm NotifyTo address is reachable.
