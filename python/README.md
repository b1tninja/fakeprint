# fakeprint (Python)

Optional **control-plane + SOAP print** client for the fakeprint binary. Not a native extension — ground truth is bytes you **send**, expect is `/v1/jobs` capture (and inbox files).

```bash
# from fakeprint repo
pip install -e "./python[dev]"
# terminal 1
cargo run -p fakeprint -- --profiles profiles --profile generic-wsd
# terminal 2
fakeprint-lab print-soap fixtures/sample.pdf
# or: .\scripts\fakeprint.ps1 lab print-soap python/fixtures/sample.pdf
pytest -m live
```

## Why Python

- pytest for SOAP print → capture expect (byte-identical payload)
- Same shape as the fakescan lab client, but **ingest**, not loopback ([siblings](../docs/siblings.md))
- PowerShell stays for Windows Add printer; Python owns the harness

## Quick use

```python
from pathlib import Path
from fakeprint import Client
from fakeprint.soap import print_and_capture

payload = Path("fixtures/sample.pdf").read_bytes()
with Client("http://127.0.0.1:3921") as c:
    c.reset()
    print_and_capture(c.base_url, payload)
    assert c.jobs()[-1].read_bytes() == payload
```

See [docs/lab-harness.md](../docs/lab-harness.md).