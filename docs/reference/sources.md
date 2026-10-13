# Sources

Which package each fact came from. Nothing from the packages is stored in this repository; the
zips live (ignored) in `docs/reference/upstream/` and the extracted schemas in
`docs/reference/schema/`, both fetched by `scripts/Get-WsdSpecs.ps1`.

| Local zip | Contents (as inspected 2026-10-13) | Used for |
|-----------|------------------------------------|----------|
| `print-v1.2.zip` (WS-Print v1.2 `Printer.zip`) | `WebPackage/`: `WDPPrint.xsd`, `Common.xsd`, `Document.xsd`, `Job.xsd`, `Printer.xsd`, `WellKnownValues.xsd`, `WDPPrintv11.xsd`, `WDPPrintV12.xsd`, `WSDPrinterService.wsdl`, `WSDPrinterServiceV12.wsdl`, plus a Word spec of the service | Operations, actions, element shapes, enumerations, fault enum; the Word spec for lifecycle, fault situations, discovery/PnP-X notes |
| `print-device.zip` (Print Device Definition v1.0) | `WSDPrintDevice.wsdl` (an empty portType `PrintDeviceType` with a SOAP binding) and a Word doc | The discovery/device type marker only; it carries no operations |
| `print-v2.0.zip` (Print Service Definition v2.0) | `WDPPrintV20.xsd` and a **Word document only**: there is NO WSDL for v2.0 | v2.0 message shapes; v2.0 action URIs only exist in the prose |
| `scan-*.zip` | scanner counterparts | Contrast only |

## Namespaces

| Prefix used in our docs | URI | Defined by |
|-------------------------|-----|-----------|
| `wprt` | `http://schemas.microsoft.com/windows/2006/08/wdp/print` | service contract, all of v1.0 |
| `wprt11` | `http://schemas.microsoft.com/windows/2010/06/wdp/printv11` | `WDPPrintv11.xsd` (note the lower-case `v`) |
| `wprt12` | `http://schemas.microsoft.com/windows/2012/10/wdp/printV12` | `WDPPrintV12.xsd` + WSDL |
| `wprt20` | `http://schemas.microsoft.com/windows/2014/04/wdp/printV20` | `WDPPrintV20.xsd` |

The scanner family has two revisions of the namespace (`2006/01` in older samples, `2006/08` in the
package). The print package only ever uses `2006/08`; a `2006/01` print namespace appears nowhere
we looked, so the alias in `fakeprint.services` is a precaution, not a fact.

## Package defects worth knowing (found by the parser, `package.md` lists them)

- `WSDPrinterServiceV12.wsdl`: the binding's `type` is `wprt:PrinterServiceV12Type` but that portType is in the `wprt12` namespace, so the binding does not resolve to its portType. Consequently no soapAction can be derived mechanically for `SetPrinterElements`. The action URI on the portType is `.../2012/10/wdp/printV12/SetPrinterElements`.
- `WDPPrintV20.xsd` imports `WDPPrint.xsd` by a relative path that only works when both sit in one directory, but the two live in different folders of our checkout; the loader therefore searches extra directories.
- Documentation vs schema disagreements are listed in [faults.md](faults.md), [operations.md](operations.md) and [must-honor.md](must-honor.md) as "package inconsistency".

## Not in the package (do not assume)

- Windows Print Schema / PrintTicket keyword vocabularies (`psf:`, `psk:`): no schema references them. Only v2.0 carries an opaque ticket blob plus a MIME type string.
- Any statement about what Windows actually sends on the wire. The package describes the printer side only. See [format.md](format.md) for what we believe and what is unverified.
- Brother (or any vendor) behaviour. Vendor quirks are opt-in profile modes, never the reference.
