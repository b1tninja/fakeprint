# Lab fixtures

| File | Role |
|------|------|
| `sample.pdf` | Tiny **blank** PDF for byte-exact SOAP CI (`print-soap`) |
| `testpage.pdf` | Visible Letter page for lab proof (`print-test`) |
| `build_testpage.py` | Regenerates `testpage.pdf` (stdlib only) |

## Visible test page

`testpage.pdf` is what you want when a blank capture is not proof:

- Title banner: `fakeprint lab test page`
- Uncompressed magic: `FAKEPRINT-TESTPAGE-v1` (grep inbox `*.bin`)
- Gray wedge, R/G/B/K bars, corner registration marks, centre cross

```bash
python python/fixtures/build_testpage.py

# SOAP (exact bytes + magic):
fakeprint-lab print-test
# or:
.\scripts\fakeprint.ps1 lab print-test

# Spooler PrintTo (scores signature RGB fills even when magic is stripped):
.\scripts\fakeprint.ps1 print-test
.\scripts\fakeprint.ps1 print-test -Open   # also open a temp .pdf preview
.\scripts\fakeprint.ps1 open-job           # preview latest capture
```

Spooler proof: Microsoft IPP Class Driver re-encodes the page (FlateDecode,
no raw `FAKEPRINT-TESTPAGE-v1`), but the banner / paper / RGB bar fills survive.
`analyze_capture` requires ≥4 of those signature colors — a blank ~795-byte
PrintTo fails.

## Blank sample (CI only)

```bash
fakeprint-lab print-soap python/fixtures/sample.pdf
# or:
.\scripts\fakeprint.ps1 lab print-soap python/fixtures/sample.pdf
```
