# Prior art

## fakescan

Sibling project: network **scanner** (WSD Scan + WIA). Explicitly out of scope for print. Ports, ground truth, and “do not copy” table: **[siblings.md](siblings.md)**. Patterns for WS-Discovery, Transfer Get, and Windows Add-device lab notes are reused by copy, not as a crate dependency.

## Microsoft WS-Print

- [WS-Print overview](https://learn.microsoft.com/en-us/windows-hardware/drivers/print/ws-print-v1-1)
- [WSDMON port monitor](https://learn.microsoft.com/en-us/windows-hardware/drivers/print/wsdmon-port-monitor)
- Print Device Definition downloads (same hub as Scan Service Definition)

## IPP / AirPrint fakes

- [flll/fake-printer](https://github.com/flll/fake-printer) — Windows IPP/AirPrint inbox (different stack)
- CUPS `ippeveprinter` — IPP Everywhere test printer

fakeprint Phase 1 is **WSD Print + WSDMON** so Windows “Add printer” discovery matches the fakescan lab. IPP is scaffolded for Phase 2.

## Naming

**fakeprint** = print companion. Not a printserv fork; not inside the fakescan crate tree.
