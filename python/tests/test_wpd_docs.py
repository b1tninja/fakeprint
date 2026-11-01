"""Tests for fakeprint.wpd_docs (markdown reference doc generator)."""

from __future__ import annotations

import re
from pathlib import Path

import pytest

from fakeprint.wpd_docs import generate_reference_docs, main

ROOT = Path(__file__).resolve().parents[2]
SCHEMA_DIR = ROOT / "docs" / "reference" / "schema"

CANONICAL_PAGES = [
    "README.md",
    "00-Overview.md",
    "01-WKVs-WellKnownValues.md",
    "02-Common-Types.md",
    "03-Document.md",
    "04-Job.md",
    "05-Printer.md",
    "06-WDPPrint.md",
    "07-WDPPrintv11.md",
    "08-WDPPrintV12.md",
]


@pytest.fixture(scope="module", autouse=True)
def _require_schemas() -> None:
    if not SCHEMA_DIR.is_dir() or not (SCHEMA_DIR / "WDPPrint.xsd").is_file():
        pytest.skip("run scripts/Get-WsdSpecs.ps1 to fetch print XSDs")


@pytest.fixture()
def out_dir(tmp_path: Path) -> Path:
    return tmp_path / "docs_out"


def test_generate_returns_ten_paths(out_dir: Path) -> None:
    written = generate_reference_docs(schema_dir=SCHEMA_DIR, out_dir=out_dir)
    assert len(written) == 10


def test_canonical_pages_exist(out_dir: Path) -> None:
    generate_reference_docs(schema_dir=SCHEMA_DIR, out_dir=out_dir)
    for name in CANONICAL_PAGES:
        assert (out_dir / name).is_file(), f"missing generated page: {name}"


def test_readme_relative_links_resolve(out_dir: Path) -> None:
    generate_reference_docs(schema_dir=SCHEMA_DIR, out_dir=out_dir)
    readme = (out_dir / "README.md").read_text(encoding="utf-8")
    links = re.findall(r"\]\(([^)#\s]+\.md)\)", readme)
    assert links
    missing = [
        link
        for link in links
        if link.startswith(("http://", "https://", "/")) or not (out_dir / link).is_file()
    ]
    assert not missing, f"dangling README links: {missing}"


def test_main_out_dir_returns_zero(out_dir: Path, capsys: pytest.CaptureFixture) -> None:
    assert main(["--schema-dir", str(SCHEMA_DIR), "--out-dir", str(out_dir)]) == 0
    for name in CANONICAL_PAGES:
        assert (out_dir / name).is_file()
