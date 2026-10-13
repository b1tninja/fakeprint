# MustHonor and the other print attributes

## Attributes that exist in the print schema

| Attribute | Type / default | Where | Meaning |
|-----------|----------------|-------|---------|
| `wprt:MustHonor` | boolean, default `false` | 17 simple-valued types (below) | The client insists on this value |
| `wprt:Removed` | boolean, default `false` | Storage, Consumable, InputBin, OutputBin entries | In a change event: this entry no longer exists |
| `Name` (unqualified) | required, string/keyword | the same four entry types | Identity of the entry; also the `Name` on ElementData |
| `Id` (unqualified) | required, int | `DeviceCondition` | Condition identity, matched by the cleared event |
| `Name`, `Valid` (unqualified) | QName / boolean, both required | `ElementData` of GetPrinterElements and GetJobElements | Which section, and whether the device knows it |
| `wprt20:Volatile` | boolean | v2.0, any element of a GetPrinterElements answer | Do not cache; changes often |
| `xs:anyAttribute ##other` | lax | almost every type | Vendor attributes are legal; ignore unknown foreign ones |

Qualified vs unqualified matters on the wire: `MustHonor`, `Removed`, `Volatile` are global
attributes (written `wprt:MustHonor="true"`), the others are local and unqualified.

Types that carry `MustHonor`: Compression, Format (DocumentFormat), MediaSizeName, MediaType,
MediaColor, Orientation, PrintQuality, Resolution, Sides, NumberUp/PagesPerSheet, NumberUp/Direction,
Copies, Priority, Collate and JogOffset (a boolean type), Staple Location and Angle, HolePunch details.
Some element annotations in the schema say `Attribute: MustHonor` as a hint; the attribute is
really declared on the type.

## Semantics (CreatePrintJob; the same elements appear per document in SendDocument/AddDocument)

For each element in the ticket, with the printer's allowed values from `PrinterCapabilities`:

| Element state | MustHonor | Result |
|---------------|-----------|--------|
| value supported | any | use it |
| value supported, MustHonor false | false/absent | printer may substitute another supported value (allowed, not required) |
| element unsupported or value unsupported | `true` (`1`) | reject the job (fault; see below) |
| element unsupported or value unsupported | false/absent (default) | ignore the element, or substitute a supported value |
| two or more elements conflict (the example given: media size vs media type) | `true` on all of them | reject with `ClientErrorConflictingRequiredParameters` |
| conflict but not all MustHonor=true | | the non-MustHonor ones yield |

The attribute is "in effect" only when written on that element. `true` and `1` both count.
The spec leaves open **which** subcode an unsupported MustHonor=true value gets, other than the
conflict case. Candidates: `InvalidArgs` for an unknown value, `ClientErrorConflictingRequiredParameters`
only for genuine conflicts. Unverified: what Windows does on seeing either.

Precedence between ticket and data stream: *Production* elements (JobName, JobOriginatingUserName,
Copies, Sides, NumberUp, PrintQuality) in the ticket override the PDL; *Layout* elements
(Orientation, MediaSize, MediaType) are controlled by the PDL and the ticket does not override them.

Per-document `DocumentProcessing` in SendDocument/AddDocument overrides the job-level
`DocumentProcessing` of CreatePrintJob for that document only.

## Differences from scan

- Scan's `Override` and `UsedDefault` (final-parameters echo) do not exist in print: CreatePrintJob answers with a bare `JobId`, so there is nowhere to report substitutions. The only place a client can see what was applied is `GetJobElements(PrintTicket)`, which a printer may fill with the effective ticket.
- Scan allows MustHonor on a defined allow-list in prose; in print it is defined by the types above, there is no separate prose list.
- Print has no Validate step in which to learn about substitutions in advance ([validate-vs-create.md](validate-vs-create.md)).
