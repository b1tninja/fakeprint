"""Offline unit tests (no fakeprint process)."""

import subprocess
import sys
from pathlib import Path

from fakeprint.client import CapturedJob
from fakeprint.soap import guess_format

FIXTURE_PDF = Path(__file__).resolve().parents[1] / "fixtures" / "sample.pdf"


def test_captured_job_from_json():
    j = CapturedJob.from_json(
        {
            "job_id": 3,
            "path": "inbox/x.bin",
            "meta_path": "inbox/x.json",
            "bytes": 12,
            "document_format": "application/pdf",
            "peer": "127.0.0.1:1",
        }
    )
    assert j.job_id == 3
    assert j.bytes == 12


def test_guess_format():
    assert guess_format(Path("a.pdf")) == "application/pdf"
    assert guess_format(Path("b.JPG")) == "image/jpeg"
    assert guess_format(Path("c.jpeg")) == "image/jpeg"
    assert guess_format(Path("d.png")) == "image/png"
    assert guess_format(Path("e.tif")) == "image/tiff"
    assert guess_format(Path("f.tiff")) == "image/tiff"
    assert guess_format(Path("g.oxps")) == "application/oxps"
    assert guess_format(Path("h.xps")) == "application/vnd.ms-xpsdocument"
    assert guess_format(Path("i.bin")) == "application/octet-stream"


def test_sample_pdf_fixture_is_minimal_pdf():
    data = FIXTURE_PDF.read_bytes()
    assert data.startswith(b"%PDF-")
    assert b"%%EOF" in data
    assert b"/Type /Catalog" in data
    idx = data.rfind(b"startxref")
    assert idx >= 0
    off = int(data[idx:].splitlines()[1])
    assert data[off : off + 4] == b"xref"


def test_lab_help_smoke():
    proc = subprocess.run(
        [sys.executable, "-m", "fakeprint.lab", "--help"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert proc.returncode == 0
    assert "print-soap" in proc.stdout
    assert "fakeprint-lab" in proc.stdout
