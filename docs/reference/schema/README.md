# WDP print schema

Not checked in. Each package is under Microsoft's Non-Confidential Technical Documentation License Agreement: you may use it as a reference for your own implementation, and you may not duplicate the materials or give any part of them to anyone else. The fetch script prints that notice and waits until you type ACCEPT. Fetch your own copy with:

```powershell
.\scripts\Get-WsdSpecs.ps1
```

That reads the file-downloads list on [Implementing Web Services on Devices for Printing](https://learn.microsoft.com/en-us/windows-hardware/design/whitepapers/implementing-web-services-on-devices-for-printing#file-downloads) and extracts `*.xsd` and `*.wsdl` here.

| Package | What lands here |
|---------|-----------------|
| [WS-Print v1.2](https://download.microsoft.com/download/E/9/7/E974CFCB-4B3B-40CC-AF92-4F7F84477F0B/Printer.zip) (2013-09-16) | Print service XSD and WSDL |
| [Print Device Definition v1.0](https://download.microsoft.com/download/9/c/5/9c5b2167-8017-4bae-9fde-d599bac8184a/PrintDevice.zip) | `WSDPrintDevice.wsdl` |

[Print Service Definition v2.0](https://go.microsoft.com/fwlink/p/?LinkId=534008) is saved and extracted under `docs/reference/upstream/print-v2/` so it does not replace these v1.2 files. The Word documents stay inside the zip.

`print_schema` loads every `*.xsd` in this directory, including the v1.1 and v1.2 extension files, and `WDPPrintV20.xsd` from `docs/reference/upstream/print-v2/` when that file is present. The service does not advertise those versions. The WSDL is not loaded.
