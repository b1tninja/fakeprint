# WS-Print Add-device checklist (WSDMON)

Concrete SOAP / metadata deltas between **fakeprint today** (`transfer_get`, `get_printer_elements` in `crates/fakeprint/src/wsd/mod.rs`) and what Windows **WSDMON** + inbox **wsdprint.inf** expect for a successful **Add device** → print queue.

**Scope:** docs + fixture XML. Do not restart a live `:3921` lab from this note. Phase 1 stays on WS-Print **v1.0**-shaped elements; do **not** advertise `SupportsWSPrintv11` / `SupportsWSPrintV12` until Add works.

**Sources**

| Source | Role |
|--------|------|
| [WSDMON port monitor](https://learn.microsoft.com/en-us/windows-hardware/drivers/print/wsdmon-port-monitor) | Install + status + bidi path |
| [WS-Print v1.1](https://learn.microsoft.com/en-us/windows-hardware/drivers/print/ws-print-v1-1) | PrinterDescription sample; v1.1/v1.2 gates |
| [GetPrinterElementsRequest / Response](https://learn.microsoft.com/en-us/previous-versions/ff550514(v=vs.85)) | Requested QNames (Description, Status, Capabilities, DefaultPrintTicket, Configuration) |
| [PrinterCapabilities](https://learn.microsoft.com/en-us/previous-versions/ff560727(v=vs.85)) | Required `JobValues` / `DocumentValues` shape |
| [Enable WSPrint on a device](https://learn.microsoft.com/en-us/windows-hardware/drivers/3dprint/enabling-wsprint-on-a-device) | IEEE-1284 `DeviceId` with `CLS:Printer` |
| [WS-Print file downloads](https://learn.microsoft.com/en-us/windows-hardware/design/whitepapers/implementing-web-services-on-devices-for-printing#file-downloads) via `scripts/Get-WsdSpecs.ps1` | Official Printer.zip / PrintDevice.zip XSD and WSDL |
| fakescan `docs/specs/dpws.md` + `ws-transfer.md` | Working Transfer Get / PnP-X pattern (scan → print swap) |

Open-source **print servers** that speak full WS-Print on the wire are scarce; treat the MS samples + WSDL/XSD dump as the reference, not CUPS/raw-9100 projects.

## Install sequence (what must succeed)

```text
WS-Discovery Hello/ProbeMatch (PrintDeviceType)
        → Transfer Get on Device EPR (ThisModel / ThisDevice / Relationship)
        → UMBus FDO from Hosted HardwareId + CompatibleId
        → wsdprint.inf match (CompatibleId → …/PrinterServiceType)
        → WSDMON GetPrinterElements (DeviceId / name / caps)
        → Print PDO + spooler queue
```

Lab today ([e2e-wsd-status.md](../e2e-wsd-status.md)): discovery + Transfer Get **live pass**; Add still **person-blocked**.

## Ranked deltas (Add-device success)

Priority `P0` = likely blocks queue creation; `P1` = blocks first job / driver; `P2` = polish / later WS-Print versions.

### P0 — Replace stub `PrinterCapabilities` with schema `JobValues` / `DocumentValues`

| | |
|--|--|
| **MS / XSD** | `PrinterCapabilities` → `JobValues` (Copies, MediaSizeName, Orientation, …) + `DocumentValues` → `DocumentDescription` → `Format` / `Compression` with `AllowedValue` children. |
| **fakeprint now** | **Done:** `JobValues` / `DocumentValues` with AllowedValue lists from profile formats (see `wsd/elements.rs`). |
| **Why Add** | WSDMON GetMetadata after the service FDO expects schema-shaped capabilities; a stub that omits `Format` AllowedValues leaves the client with no legal SendDocument formats. |
| **Fix** | Emit minimal letter/stationery/OneSided/Portrait + Format list from `profile.document_formats`. Fixture: [fixtures/get-printer-elements-ms-shaped.xml](fixtures/get-printer-elements-ms-shaped.xml). |

### P0 — Answer `DefaultPrintTicket` (and never return an empty `PrinterElements`)

| | |
|--|--|
| **MS** | Clients request `wprt:DefaultPrintTicket` (often with Status / Capabilities). Response sample nests JobDescription / JobProcessing / DocumentProcessing under `ElementData Name="wprt:DefaultPrintTicket" Valid="true"`. Unknown QNames → `Valid="false"` empty `ElementData`. |
| **fakeprint now** | **Done:** DefaultPrintTicket + Configuration; unknown QNames → `Valid="false"`; empty request returns the common set. |
| **Why Add** | Empty response after association looks like a dead Print Service to WSDMON. |
| **Fix** | Always emit requested names; stub a minimal DefaultPrintTicket; for unknown names return `Valid="false"`. |

### P0 — IEEE-1284 `DeviceId` includes `CLS:Printer`

| | |
|--|--|
| **MS** | PrinterDescription sample: `MFG:Acme;MDL:PrintMaster 9020`. 3D/WSDPRINT INF pairing sample: `MFG:…; CMD:…; MDL:…; CLS:Printer; DES:…; CID:…`. WSDMON Xcv **DeviceID** feeds the print PDO. |
| **fakeprint now** | **Done:** Brother-shaped `CLS:PRINTER` + `CID:MS_IPP` → `1284_CID_MS_IPP` (inbox Microsoft IPP Class Driver; matches Brother’s working queue on this host). |
| **Why Add** | Class / INF selection for the print PDO. Pure WS-Print association (`WSDPrintDevice` + GetPrinterElements + Subscribe) does **not** create `PRINTENUM` here; Brother’s only WSD spooler queue used **IPP** (`Install Protocol=0x1`, `1284_CID_MS_IPP_PREF`). `CID:MS_PCL6` + installing PCL6 still produced no PRINTENUM attempt in setupapi. |
| **Fix** | DeviceId `…;CLS:PRINTER;…;CID:MS_IPP;` plus live `POST /ipp/print`. After DeviceId change: elevated remove `…f3` DAF nodes or `Disconnect-FakeprintAssociationEndpoint`, then person Add. |

### P1 — Transfer Get CompatibleId stays URI-shaped (already aligned)

| | |
|--|--|
| **Windows INF** | Inbox match is `UMB\http://schemas.microsoft.com/windows/2006/08/wdp/print/PrinterServiceType` (setupapi logs show UMBus `UMB\` prefix). |
| **fakeprint / fakescan** | Publish CompatibleId as `{PRINT_NS}/PrinterServiceType` **without** `UMB\` — same pattern that installed fakescan via `wsdscdrv.inf`. HardwareId `VEN_FAKE&DEV_WSDPRINT`, `pnpx:DeviceCategory` `Printers`, `df:DeviceCategory` `PrintFax.Printer`. |
| **Status** | Live Transfer Get already passes; do not “fix” by prepending `UMB\` unless a setupapi capture proves the wire form differs. |

### P1 — `PrinterConfiguration` when requested

| | |
|--|--|
| **MS** | Optional but commonly queried: Storage / Consumables / InputBins. |
| **fakeprint** | **Done:** minimal Tray1 + OutputBin1. |
| **Fix** | Minimal one tray (`Tray1`, `na_letter_8.5x11in`, Capacity/Level) after Description/Ticket/Capabilities work. |

### P2 — Do **not** claim WS-Print v1.1/v1.2 yet

| | |
|--|--|
| **MS** | `wprt11:SupportsWSPrintv11` / `wprtV12:SupportsWSPrintV12` gate further GetPrinterElements (DriverConfiguration, SetPrinterElements). |
| **fakeprint** | Correctly omitted. |
| **Rule** | Add v1.1 flags only after v1.0 Add + test page; otherwise WSDMON asks for DriverConfiguration you do not serve. |

### P2 — Job ops after queue exists

CreatePrintJob / SendDocument (incl. MTOM xop) / GetJobElements / ValidatePrintTicket stub / Subscribe are code-ready; they are **not** the Add listing gate. Rebuild live `:3921` only when free, then person Add.

## Side-by-side: PrinterDescription

| Element | MS sample | fakeprint |
|---------|-----------|-----------|
| ColorSupported | yes | yes (profile) |
| DeviceId | `MFG:…;MDL:…` (+ CLS in INF samples) | MFG/MDL/CMD/DES (+ CLS when profile updated) |
| MultipleDocumentJobsSupported | yes | yes |
| PagesPerMinute / Color | yes | yes |
| PrinterName / Info / Location | yes (`xml:lang`) | yes |
| SupportsWSPrintv11 | v1.1 only | omit (Phase 1) |

## Side-by-side: PrinterCapabilities

| Shape | MS / XSD | fakeprint |
|-------|----------|-----------|
| JobValues / DocumentProcessing | MediaSizeName, Orientation, Sides, … | **present** |
| DocumentValues / Format AllowedValue | MIME list | **present** (profile formats) |
| DocumentFormatsSupported | — | removed (non-schema) |
| JobIdSupported | — | removed (non-schema) |

## Transfer Get (Device EPR) — keep

Matches fakescan / DPWS host model:

- ThisModel: Manufacturer, ModelName, `pnpx:DeviceCategory=Printers`, `df:DeviceCategory=PrintFax.Printer`
- **PresentationUrl / ManufacturerUrl / ModelUrl** must be the LAN advertise origin (e.g. `http://192.168.1.70:3921/`), **not** `http://localhost/` — localhost stalls Settings on “Acquiring data”. Serve GET `/` with a tiny HTML page.
- ThisDevice: FriendlyName, FirmwareVersion, SerialNumber
- Relationship host: `wsdp:Device` + `wprt:PrintDeviceType` with **`xmlns:wsdp` declared on the Types element** (do not rely on parent-only xmlns)
- Hosted: PrinterService URL, `wprt:PrinterServiceType`, HardwareId, CompatibleId URI (no `UMB\` prefix on the wire)

## Fixture

See [fixtures/get-printer-elements-ms-shaped.xml](fixtures/get-printer-elements-ms-shaped.xml) — target response shape (P0 items now implemented in `wsd/elements.rs`).

## Handoff

P0 GetPrinterElements + PresentationUrl / Types-prefix fixes are in tree and on the lab binary. Live Add still loops Device `Get` without PrinterService SOAP / queue — see [e2e-wsd-status.md](../e2e-wsd-status.md). Next: person remove+re-Add or prove WSDMON vs IPP path. No IPP work in this track unless that proof says so.
