"""Live tests against a running fakeprint (pytest -m live)."""

from __future__ import annotations

import os

import pytest

from fakeprint import Client
from fakeprint.soap import print_and_capture

pytestmark = pytest.mark.live

BASE = os.environ.get("FAKEPRINT_BASE_URL", "http://127.0.0.1:3921")


@pytest.fixture
def client():
    c = Client(BASE)
    if not c.health():
        pytest.skip(f"fakeprint not up at {BASE}")
    c.reset()
    yield c
    c.reset()
    c.close()


def test_soap_print_capture_exact(client: Client):
    payload = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\nfakeprint-lab\n"
    before = len(client.jobs())
    job = print_and_capture(BASE, payload, document_format="application/pdf")
    rows = client.jobs()
    assert len(rows) == before + 1
    cap = next(j for j in rows if j.job_id == job.job_id)
    assert cap.bytes == len(payload)
    assert cap.read_bytes() == payload
