# Windows lab

Person-gated Add printer for fakeprint. Scan install is the sibling — [fakescan windows.md](../../fakescan/docs/windows.md) / [siblings.md](siblings.md).

## CLI helpers

```powershell
.\scripts\fakeprint.ps1 status
.\scripts\fakeprint.ps1 jobs
.\scripts\fakeprint.ps1 lab print-soap python/fixtures/sample.pdf   # needs pip install -e ./python[dev]
.\scripts\fakeprint.ps1 add   # Printers & scanners UI
```

Python harness details: [lab-harness.md](lab-harness.md).

## Add printer (person required)

1. Start fakeprint with a LAN advertise address and stable UUID:

```powershell
cargo run -p fakeprint -- `
  --listen 0.0.0.0:3921 `
  --advertise 192.168.1.70 `
  --profiles profiles `
  --profile generic-wsd `
  --device-uuid 00000000-0000-4000-8000-0000000000f3 `
  --probe-listen -v
```

2. Open **only** Printers & scanners (never Bluetooth — steals focus):

```powershell
Import-Module .\scripts\Fakeprint.psm1
Open-FakeprintAddDevice
```

3. Run the combined open + watcher command (banner / beep / balloon; then polls milestones):

```powershell
.\scripts\fakeprint.ps1 add-device
```

   Under **Add a printer or scanner**, click **Add device** next to **Fake WSD printer**.
   The watcher prints each milestone as it arrives:

   | Milestone | Meaning |
   |-----------|---------|
   | `Subscribe` rises | WSDMON called Subscribe |
   | `GetPrinterElements` rises | WSDMON read caps |
   | Subscribe dump XML | `NotifyTo` address logged |
   | `PRINTENUM` device | WSD port committed |
   | WSD queue | Ready to print |

   Press Ctrl+C to abort the watcher without killing fakeprint. Run the watcher
   separately in a second window with `.\scripts\fakeprint.ps1 watch-wsd`.

4. Print a **visible** lab page (not a blank PDF / not only the Settings “Print test page”):
   ```powershell
   .\scripts\fakeprint.ps1 print-test
   # SOAP-only (no spooler): .\scripts\fakeprint.ps1 lab print-test
   ```
   Expect `VisualProof=True` (signature RGB fills) or magic on SOAP. Blank PrintTo was ~795 bytes and fails the color check. Optional: `-Open` or `.\scripts\fakeprint.ps1 open-job`.
5. Confirm: `Invoke-RestMethod http://127.0.0.1:3921/v1/jobs`

## Sticky offline

Similar to fakescan: after restart, Windows may keep a stale association. Keep fakeprint up, remove the printer queue if needed, re-Add while Hellos are flowing. Use a stable `--device-uuid` / `--advertise`.

## Coexistence with fakescan

| | fakescan | fakeprint |
|--|----------|-----------|
| Port | 3920 | 3921 |
| Category | Scanners | Printers |
| HardwareId | `DEV_WSDSCAN` | `DEV_WSDPRINT` |
| Example UUID | `…000f2` | `…000f3` |

Do **not** edit or remove fakescan’s firewall rules when setting up print. Both can listen on the same host (3920 + 3921) with distinct UUIDs.

## Firewall

Same-machine Windows **Add device** still needs an inbound allow on the **Private** profile when fakeprint listens on `0.0.0.0:3921` and advertises a LAN IP (lab: `192.168.1.70`).

Allow inbound:

- **TCP 3921** — control API (`/v1/*`) + WSD Print SOAP (`/PrintService`, device metadata)
- **UDP 3702** — WS-Discovery (shared with fakescan / Network Discovery; do not invent a second global “open everything” rule)

### Preferred lab rule (port-scoped, Private only)

Display name / name: **`Fakeprint-WSD-3921`**.

Elevated PowerShell (exact command):

```powershell
New-NetFirewallRule `
  -Name 'Fakeprint-WSD-3921' `
  -DisplayName 'Fakeprint-WSD-3921' `
  -Description 'Lab: inbound TCP 3921 for fakeprint WSD print SOAP/control (Private profile only).' `
  -Direction Inbound `
  -Action Allow `
  -Protocol TCP `
  -LocalPort 3921 `
  -Profile Private
```

Optional helper (prints the same command if not elevated; creates the rule when elevated):

```powershell
.\scripts\ensure-firewall.ps1
```

Check:

```powershell
Get-NetTCPConnection -LocalPort 3921 -State Listen -ErrorAction SilentlyContinue
Test-NetConnection -ComputerName 192.168.1.70 -Port 3921
Get-NetFirewallRule -DisplayName 'Fakeprint-WSD-3921' -ErrorAction SilentlyContinue |
  Format-List Name, Enabled, Direction, Action, Profile
```

### What you may already see

Windows often creates path-bound **Query User** allows named `fakeprint` the first time `fakeprint.exe` is prompted (TCP/UDP, program = that build path). Those cover the **current** binary under e.g. `D:\temp\fakeprint-target\debug\fakeprint.exe`, but:

- a new cargo/target path needs another prompt (or the port rule above);
- they are broader (Any local port; often Private+Public).

Prefer the named **`Fakeprint-WSD-3921`** rule for a stable lab. Leave fakescan’s Query User / 3920 rules alone.

Mirror for scan: fakescan `docs/windows.md` → Firewall (TCP 3920 + UDP 3702).

## Status

Live Add + capture: [e2e-wsd-status.md](e2e-wsd-status.md).
