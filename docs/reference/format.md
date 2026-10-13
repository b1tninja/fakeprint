# Document formats and compression

## What the schema enumerates

`DocumentFormatWKVType` (the well-known values, strings up to 255 chars):

| Value | Meaning |
|-------|---------|
| `application/octet-stream` | opaque printer-specific data stream (driver-rendered PDL) |
| `application/postscript` | PostScript |
| `application/vnd.hp-PCL` | PCL |
| `application/vnd.ms-xpsdocument` | XPS (Microsoft XML Paper Specification) |
| `application/vnd.pwg-xhtml-print+xml` | PWG XHTML-Print |
| `image/g3fax`, `image/gif`, `image/jpeg`, `image/png` | raster images |
| `image/tiff`, `image/tiff-fx` | TIFF and TIFF-FX |
| `text/html`, `text/plain`, `text/plain;charset=utf-8` | text |
| `unknown` | format not stated |

`DocumentFormatExtType` is a union of that list and `MimeExtensionPatternType`
(`\w+[\w:/;_\-\\\+\.]*`), so **any MIME-looking string is schema-valid**. That is the legal route
for `application/pdf`, `image/pwg-raster`, `image/urf` and similar: they are *not* well-known
values but they are not invalid either. (The default fakeprint profile advertises both; that is
acceptable but should be labelled as extensions.)

Not present in the list: PDF, PWG raster, Apple raster, JPEG-2000, OpenXPS under its newer
`application/oxps` type. Nothing in the package says a client must pick from the well-known
subset when the printer advertises an extension value.

## Compression (`CompressionWKVType` + keyword extension `ns:keyword`)

`None`, `Gzip`, `Compress`, `Deflate`. Both `Compression` and `Format` are **required** children of
every `DocumentDescription` (so clients always send `<Compression>None</Compression>` at least).
Unsupported value -> `ClientErrorCompressionNotSupported`. The printer advertises what it accepts in
`PrinterCapabilities/DocumentValues/DocumentDescription/{Format,Compression}/AllowedValue`.
(Capabilities' `Format` list is typed as URI strings, `Compression` as NMTOKENS: a quirk of the
shared `ValueStringListType`/`ValueTokenListType` types.)

## How to validate

1. Compare `Format` to the profile's list case-sensitively as given (MIME types are
   case-insensitive in general but the schema uses exact strings; accept a case-insensitive
   match only in a lenient mode).
2. If it is outside the list: `ClientErrorFormatNotSupported`, ideally with the supported list in
   Detail. If `MustHonor` is absent/false the spec does not allow substituting a different document
   format in practice (the bytes are what they are), so treat Format as always strict.
3. Then Compression. Then decode/accept the bytes.

## What Windows' inbox WSD print path requests

The package says nothing about the client side. What we know or believe:

| Claim | Status |
|-------|--------|
| The inbox WSD port monitor (wsdmon) sends the driver's rendered output; for an inbox/class driver paired through the `PrinterServiceType` CompatibleId this is the driver's PDL, commonly `application/octet-stream` | **unverified** (confirm with a captured SendDocument) |
| XPS-driver queues can send `application/vnd.ms-xpsdocument` | **unverified**, plausible because XPS is in the WKV list |
| It reads `PrinterCapabilities` (Format AllowedValue) via GetPrinterElements before first job | documented flow in `docs/specs/ws-print-add-checklist.md`; exact read order **unverified** |
| It sends `Compression=None` | **unverified**; schema requires the element |
| It subscribes to events on the service endpoint at queue creation | believed, **unverified** |
| v2.0 / driverless: clients may choose among `PrinterFormats` MIME types; the sample uses `application/vnd.ms-PrintSchemaTicket+xml` for tickets | stated in the v2.0 prose (example), applies only to v2.0 devices |

Use the lab capture path (`inbox/` of captured jobs and the dumped requests) to turn each
unverified row into a recorded fact, and update this table with the capture date.
