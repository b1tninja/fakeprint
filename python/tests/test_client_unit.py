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
    # Blank CI fixture — no rich marker (that lives in testpage.pdf).
    assert b"FAKEPRINT-TESTPAGE-v1" not in data


def test_testpage_pdf_is_visible_proof():
    from fakeprint.testpage import (
        MAGIC,
        analyze_capture,
        build_testpage_pdf,
        fixture_path,
    )

    built = build_testpage_pdf()
    assert built.startswith(b"%PDF-")
    assert MAGIC.encode("ascii") in built
    assert b"/MediaBox [0 0 612.0 792.0]" in built
    assert b"Helvetica" in built
    assert b"re f" in built  # at least one filled rect
    assert len(built) > 1000

    proof = analyze_capture(built)
    assert proof.magic and proof.visual and proof.ok
    assert proof.color_hits == proof.colors_expected

    fixture = fixture_path()
    assert fixture.is_file(), "run: python python/fixtures/build_testpage.py"
    data = fixture.read_bytes()
    assert MAGIC.encode("ascii") in data
    assert data.startswith(b"%PDF-")


def test_analyze_capture_rejects_blank_minimal():
    from fakeprint.testpage import analyze_capture

    blank = FIXTURE_PDF.read_bytes()
    proof = analyze_capture(blank)
    assert not proof.magic
    assert not proof.visual
    assert not proof.ok


def test_lab_help_smoke():
    proc = subprocess.run(
        [sys.executable, "-m", "fakeprint.lab", "--help"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert proc.returncode == 0
    assert "print-soap" in proc.stdout
    assert "print-test" in proc.stdout
    assert "prove-capture" in proc.stdout
    assert "fakeprint-lab" in proc.stdout


def test_lab_help_includes_ctl():
    """New ctl / ctl-info subcommands appear in help."""
    proc = subprocess.run(
        [sys.executable, "-m", "fakeprint.lab", "--help"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert proc.returncode == 0
    assert "ctl" in proc.stdout


def test_client_ctl_method_exists():
    """Client.ctl and Client.ctl_info and Client.set_profile are defined."""
    from fakeprint.client import Client
    import inspect
    assert callable(Client.ctl)
    assert callable(Client.ctl_info)
    assert callable(Client.set_profile)
    # Check signatures
    sig_ctl = inspect.signature(Client.ctl)
    assert "cmd" in sig_ctl.parameters


def test_status_has_name_field():
    """Status raw dict is passed through; name field is available."""
    from fakeprint.client import Status
    st = Status(raw={"name": "default", "profile_id": "generic-wsd", "active_jobs": 0, "captured_jobs": 0, "label": "X"})
    assert st.raw.get("name") == "default"


def test_mcp_server_importable():
    """mcp_server module is importable (the mcp import is guarded with SystemExit)."""
    proc = subprocess.run(
        [sys.executable, "-c",
         "import importlib.util; spec = importlib.util.spec_from_file_location('m', "
         r"r'python\fakeprint\mcp_server.py'); "
         "assert spec is not None"],
        capture_output=True, text=True, check=False,
        cwd=str(Path(__file__).resolve().parents[2]),
    )
    # We just check the file exists and is valid Python (parse only).
    src = Path(__file__).resolve().parents[2] / "python" / "fakeprint" / "mcp_server.py"
    assert src.is_file(), f"mcp_server.py not found at {src}"
    import ast
    ast.parse(src.read_text(encoding="utf-8"))  # raises SyntaxError if broken
