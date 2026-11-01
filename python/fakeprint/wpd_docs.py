"""Generate markdown reference docs from the WSD print (WDP) XSD schemas.

Stdlib-only. Structure comes from :mod:`fakeprint.wpd_schema` (never from
hand-parsed ElementTree output): each ``*.xsd`` in the schema directory is
parsed once to learn which named types it declares, and the merged schema
(:func:`fakeprint.wpd_schema.load_schema`) supplies the type bodies.
"""

from __future__ import annotations

import argparse
from pathlib import Path

from . import wpd_schema as w

__all__ = ["generate_reference_docs", "main"]

_ROOT = Path(__file__).resolve().parents[2]
_DEFAULT_DIR = _ROOT / "docs" / "reference" / "schema"
_TARGET_NS = "http://schemas.microsoft.com/windows/2006/08/wdp/print"

#: (declaring xsd file, output page filename, page title)
_PAGES: tuple[tuple[str, str, str], ...] = (
    ("WellKnownValues.xsd", "01-WKVs-WellKnownValues.md", "WellKnownValues"),
    ("Common.xsd", "02-Common-Types.md", "Common"),
    ("Document.xsd", "03-Document.md", "Document"),
    ("Job.xsd", "04-Job.md", "Job"),
    ("Printer.xsd", "05-Printer.md", "Printer"),
    ("WDPPrint.xsd", "06-WDPPrint.md", "WDPPrint"),
    ("WDPPrintv11.xsd", "07-WDPPrintv11.md", "WDPPrint v1.1 extensions"),
    ("WDPPrintV12.xsd", "08-WDPPrintV12.md", "WDPPrint v1.2 extensions"),
)

_OVERVIEW = "00-Overview.md"


def _type_display(type_name: str, types: dict[str, w.TypeBody]) -> str:
    """Render a (possibly prefixed) type name for doc tables."""
    local = type_name.rsplit(":", 1)[-1]
    return local if local in types else type_name


def _bound(value: int | None) -> str:
    return "unbounded" if value is None else str(value)


def _particle_rows(
    particles: tuple[w.Particle, ...], schema: w.Schema
) -> list[tuple[str, str, int, int | None]]:
    """Flatten particles (including choice options) into table rows."""
    rows: list[tuple[str, str, int, int | None]] = []

    def row(particle: w.Particle) -> None:
        if particle.kind == "any":
            rows.append(("any", "any element", particle.min, particle.max))
            return
        type_name = (
            _type_display(particle.type_name, schema.types)
            if particle.type_name
            else "text"
        )
        rows.append((particle.name, type_name, particle.min, particle.max))

    for particle in particles:
        if particle.kind == "choice":
            for option in particle.options:
                row(option)
        else:
            row(particle)
    return rows


def _type_section(name: str, schema: w.Schema) -> list[str]:
    """Markdown lines for one ``## TypeName`` section."""
    body = schema.types[name]
    lines = [f"## {name}", ""]
    lines.append(f"- **Kind:** {body.kind}")
    if body.base:
        lines.append(f"- **Base:** {_type_display(body.base, schema.types)}")
    if body.kind == "simple" and body.simple is not None:
        simple = body.simple
        lines.append(f"- **Value:** `{simple.label()}`")
        if simple.values:
            values = ", ".join(f"`{v}`" for v in simple.values)
            lines.append(f"- **Allowed values:** {values}")
    if body.attrs:
        lines += [
            "",
            "**Attributes**",
            "",
            "| Name | Type | Required |",
            "| --- | --- | --- |",
        ]
        for attr in body.attrs:
            required = "yes" if attr.required else "no"
            lines.append(f"| {attr.name} | {attr.simple.label()} | {required} |")
    rows = _particle_rows(body.particles, schema)
    if rows:
        lines += [
            "",
            "**Child elements**",
            "",
            "| Name | Type | Min | Max |",
            "| --- | --- | --- | --- |",
        ]
        for name_r, type_name, min_v, max_v in rows:
            lines.append(
                f"| {name_r} | {type_name} | {_bound(min_v)} | {_bound(max_v)} |"
            )
    lines.append("")
    return lines


def _page_text(title: str, names: list[str], schema: w.Schema) -> str:
    parts: list[str] = [f"# {title}", ""]
    if names:
        parts += [
            f"{len(names)} types declared in this schema file, sorted by name.",
            "",
        ]
    for name in names:
        parts += _type_section(name, schema)
    return "\n".join(parts)


def _overview_text(
    schema: w.Schema, pages: list[tuple[str, str, str, list[str]]]
) -> str:
    elements = sorted(schema.elements.items())
    parts: list[str] = [
        "# Overview",
        "",
        "Reference for the WSD (Windows Device Driver) print XSD schema — "
        "the `Print` contract of the WSD Print Service. It defines the "
        "global request/response/event elements and the simple/complex "
        "types they use.",
        "",
        f"- **Target namespace:** `{_TARGET_NS}`",
        f"- **Global elements:** {len(elements)}",
        f"- **Named types:** {sum(len(names) for _, _, _, names in pages)}",
        "",
        "**Pages**",
        "",
        f"- [Overview]({_OVERVIEW})",
    ]
    for _, page_file, title, names in pages:
        parts.append(f"- [{title} ({len(names)} types)]({page_file})")
    parts += [
        "",
        "**Global elements**",
        "",
        "| Element | Type |",
        "| --- | --- |",
    ]
    for element, type_name in elements:
        parts.append(f"| {element} | {_type_display(type_name, schema.types)} |")
    parts.append("")
    return "\n".join(parts)


def _readme_text(pages: list[tuple[str, str, str, list[str]]]) -> str:
    parts: list[str] = [
        "# WSD Print XSD Schema Reference",
        "",
        "Official `*.xsd` / `*.wsdl` files are **not** checked in. Each Microsoft package "
        "is under the Non-Confidential Technical Documentation License Agreement; fetch "
        "your own copy with `scripts/Get-WsdSpecs.ps1` (the script prints the notice and "
        "waits for ACCEPT). See [Implementing Web Services on Devices for Printing]"
        "(https://learn.microsoft.com/en-us/windows-hardware/design/whitepapers/"
        "implementing-web-services-on-devices-for-printing#file-downloads).",
        "",
        "`print_schema` (Rust) and `wpd_schema` (Python) load every `*.xsd` here, plus "
        "`WDPPrintV20.xsd` from `docs/reference/upstream/print-v2/` when present. WSDL is "
        "not loaded. Regenerate the markdown pages below with `fakeprint-refs` after "
        "fetching XSDs (`pip install -e ./python`).",
        "",
        "| Page | Schema file | Types |",
        "| --- | --- | --- |",
        f"| [Overview]({_OVERVIEW}) | all files | — |",
    ]
    for xsd_file, page_file, title, names in pages:
        parts.append(f"| [{title}]({page_file}) | `{xsd_file}` | {len(names)} |")
    parts.append("")
    return "\n".join(parts)


def _types_by_file(
    schema_dir: Path, schema: w.Schema
) -> list[tuple[str, str, str, list[str]]]:
    """(xsd file, page file, title, sorted declared type names) per page."""
    pages: list[tuple[str, str, str, list[str]]] = []
    for xsd_file, page_file, title in _PAGES:
        path = schema_dir / xsd_file
        if not path.is_file():
            raise FileNotFoundError(f"schema file not found: {path}")
        declared = w.parse_schema(path).types
        names = sorted(n for n in declared if n[0].isalpha() and n in schema.types)
        pages.append((xsd_file, page_file, title, names))
    return pages


def generate_reference_docs(
    schema_dir: Path | str | None = None, out_dir: Path | str | None = None
) -> list[Path]:
    """Generate the markdown reference pages; return the written files."""
    schema_dir = Path(schema_dir) if schema_dir is not None else _DEFAULT_DIR
    out_dir = Path(out_dir) if out_dir is not None else _DEFAULT_DIR
    if not schema_dir.is_dir():
        raise FileNotFoundError(f"schema directory not found: {schema_dir}")
    out_dir.mkdir(parents=True, exist_ok=True)

    schema = w.load_schema(schema_dir)
    pages = _types_by_file(schema_dir, schema)

    contents = [
        ("README.md", _readme_text(pages)),
        (_OVERVIEW, _overview_text(schema, pages)),
    ]
    for _, page_file, title, names in pages:
        contents.append((page_file, _page_text(title, names, schema)))
    written: list[Path] = []
    for page_file, text in contents:
        path = out_dir / page_file
        path.write_text(text, encoding="utf-8")
        written.append(path)
    return written


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Generate markdown reference docs from the WSD print XSD schema."
    )
    parser.add_argument(
        "--schema-dir",
        default=None,
        help="directory containing the *.xsd files (default: docs/reference/schema)",
    )
    parser.add_argument(
        "--out-dir",
        default=None,
        help="directory to write the markdown pages into (default: docs/reference/schema)",
    )
    args = parser.parse_args(argv)
    for path in generate_reference_docs(args.schema_dir, args.out_dir):
        print(path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
