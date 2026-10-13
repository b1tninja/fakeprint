# WSD Print XSD Schema Reference

Official `*.xsd` / `*.wsdl` files are **not** checked in. Each Microsoft package is under the Non-Confidential Technical Documentation License Agreement; fetch your own copy with `scripts/Get-WsdSpecs.ps1` (the script prints the notice and waits for ACCEPT). See [Implementing Web Services on Devices for Printing](https://learn.microsoft.com/en-us/windows-hardware/design/whitepapers/implementing-web-services-on-devices-for-printing#file-downloads).

`print_schema` (Rust) and `wpd_schema` (Python) load every `*.xsd` here, plus `WDPPrintV20.xsd` from `docs/reference/upstream/print-v2/` when present. WSDL is not loaded. Regenerate the markdown pages below with `fakeprint-refs` after fetching XSDs (`pip install -e ./python`).

| Page | Schema file | Types |
| --- | --- | --- |
| [Overview](00-Overview.md) | all files | — |
| [WellKnownValues](01-WKVs-WellKnownValues.md) | `WellKnownValues.xsd` | 25 |
| [Common](02-Common-Types.md) | `Common.xsd` | 58 |
| [Document](03-Document.md) | `Document.xsd` | 16 |
| [Job](04-Job.md) | `Job.xsd` | 43 |
| [Printer](05-Printer.md) | `Printer.xsd` | 37 |
| [WDPPrint](06-WDPPrint.md) | `WDPPrint.xsd` | 29 |
| [WDPPrint v1.1 extensions](07-WDPPrintv11.md) | `WDPPrintv11.xsd` | 3 |
| [WDPPrint v1.2 extensions](08-WDPPrintV12.md) | `WDPPrintV12.xsd` | 8 |
