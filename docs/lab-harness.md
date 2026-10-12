# Lab harness (Python)

Optional print-lab client for fakeprint. Lives **in this repo**, not in pdfcraft or fakescan.

| Layer | Where | Role |
|-------|--------|------|
| Server | Rust `fakeprint` binary | WSD Print + capture to `inbox/` |
| Harness | `python/` (`pip install -e ./python`) | Control plane + SOAP print → assert capture |
| Windows install | `scripts/Fakeprint.psm1` | Add printer (person required) |

pdfcraft / apps under test must not take a fakeprint dependency for normal builds.

## Capture-assert (not fakescan loopback)

| Mode | How | Expect |
|------|-----|--------|
| **Capture-assert** | Client sends document (SOAP or Windows spooler) | Captured bytes in inbox / `GET /v1/jobs` match what was sent |
| Soft (future) | Faults / offline | Job fails or stalls — not shipped yet |

Print is **ingest**: you send bytes; fakeprint stores them. There is no page feeder and no `/v1/loopback`.

Do **not** copy from fakescan: `/v1/loopback`, pages, pattern, effects, scenario, WIA, or `pdfcraft-cli` scan glue. See [siblings.md](siblings.md).

```bash
# terminal 1
cargo run -p fakeprint -- --profiles profiles --profile generic-wsd

# terminal 2
pip install -e "./python[dev]"
fakeprint-lab print-soap python/fixtures/sample.pdf
# or:
.\scripts\fakeprint.ps1 lab print-soap python/fixtures/sample.pdf
pytest python/tests -m live
```

## Control plane used

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/health` | process up |
| `GET` | `/v1/status` | profile, active/captured job counts |
| `POST` | `/v1/reset` | clear in-memory jobs (inbox files kept) |
| `GET` | `/v1/jobs` | captured job metadata + paths |

Fault injection (`POST /v1/fault`) is **later** — [roadmap.md](roadmap.md).

## Python API

```python
from pathlib import Path
from fakeprint import Client
from fakeprint.soap import print_and_capture

path = Path("python/fixtures/sample.pdf")
payload = path.read_bytes()

with Client("http://127.0.0.1:3921") as c:
    c.reset()
    before = {j.job_id for j in c.jobs()}
    print_and_capture(c.base_url, payload, document_format="application/pdf")
    jobs = [j for j in c.jobs() if j.job_id not in before]
    assert jobs and jobs[-1].read_bytes() == payload
```

SOAP-only (CI, no spooler): `fakeprint-lab print-soap` — same path the live pytest uses.

## Windows Add printer

Person-gated; helpers in [windows.md](windows.md). After a test page, `fakeprint-lab jobs` or `GET /v1/jobs` should show a non-zero capture.
