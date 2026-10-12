"""Visible Letter test-page PDF for lab print proof (stdlib only).

``sample.pdf`` stays a blank minimal PDF for byte-exact SOAP CI.
This module builds a page you can see: title, magic marker, gray wedge,
RGB bars, and corner marks. The magic string is uncompressed so a SOAP
capture (and many spooler re-encodes) still grep cleanly.
"""

from __future__ import annotations

import re
import zlib
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

# Grep this in inbox *.bin after print-soap / print-test.
MAGIC = "FAKEPRINT-TESTPAGE-v1"

# Letter MediaBox in points.
PAGE_W = 612.0
PAGE_H = 792.0

# Distinctive fills written into the fixture. Spooler captures re-encode them
# as ~3-decimal floats (e.g. 0.97 -> 0.968627); we match rounded triples.
_SIG_COLORS = (
    (0.97, 0.97, 0.94),  # paper wash
    (0.12, 0.22, 0.45),  # banner
    (0.85, 0.12, 0.12),  # red bar
    (0.12, 0.65, 0.22),  # green bar
    (0.12, 0.28, 0.85),  # blue bar
)


@dataclass(frozen=True)
class CaptureProof:
    """Result of scanning an inbox capture for test-page proof."""

    magic: bool
    color_hits: int
    colors_expected: int
    flate_streams: int
    substantial: bool
    bytes: int

    @property
    def visual(self) -> bool:
        """True when enough signature fills survived (spooler-safe)."""
        return self.color_hits >= 4

    @property
    def ok(self) -> bool:
        return self.magic or self.visual or (self.substantial and self.color_hits >= 2)

    @property
    def note(self) -> str:
        if self.magic and self.visual:
            return f"magic + visual colors ({self.color_hits}/{self.colors_expected})"
        if self.magic:
            return "capture contains FAKEPRINT-TESTPAGE-v1"
        if self.visual:
            return (
                f"spooler visual proof: {self.color_hits}/{self.colors_expected} "
                "signature fills (magic stripped)"
            )
        if self.substantial:
            return "capture size non-blank but signature colors not found"
        return "capture still tiny -- likely a blank handler page"


def build_testpage_pdf(
    *,
    title: str = "fakeprint lab test page",
    when: datetime | None = None,
    extra_line: str = "",
) -> bytes:
    """Return a PDF-1.4 document with visible proof content."""
    ts = (when or datetime.now(timezone.utc)).strftime("%Y-%m-%d %H:%M:%SZ")
    lines = [
        title,
        MAGIC,
        f"generated {ts}",
    ]
    if extra_line:
        lines.append(extra_line)

    # Content stream: colored rects + Helvetica text. No FlateEncode.
    ops: list[str] = []
    # Light paper wash so a blank capture is obvious by contrast.
    ops.append("0.97 0.97 0.94 rg")
    ops.append(f"0 0 {PAGE_W:.1f} {PAGE_H:.1f} re f")

    # Top banner bar
    ops.append("0.12 0.22 0.45 rg")
    ops.append(f"36 {PAGE_H - 72:.1f} {PAGE_W - 72:.1f} 36 re f")

    # Gray wedge (left)
    wedge_x0, wedge_y0 = 36.0, 420.0
    wedge_w, wedge_h = 180.0, 200.0
    steps = 8
    step_h = wedge_h / steps
    for i in range(steps):
        g = i / max(steps - 1, 1)
        ops.append(f"{g:.3f} {g:.3f} {g:.3f} rg")
        y = wedge_y0 + (steps - 1 - i) * step_h
        ops.append(f"{wedge_x0:.1f} {y:.1f} {wedge_w:.1f} {step_h:.1f} re f")

    # RGB (+ black) bars (right of wedge)
    bar_x0 = 240.0
    bar_w = (PAGE_W - bar_x0 - 36.0) / 4.0
    colors = [
        (0.85, 0.12, 0.12),
        (0.12, 0.65, 0.22),
        (0.12, 0.28, 0.85),
        (0.08, 0.08, 0.08),
    ]
    for i, (r, g, b) in enumerate(colors):
        ops.append(f"{r:.3f} {g:.3f} {b:.3f} rg")
        x = bar_x0 + i * bar_w
        ops.append(f"{x:.1f} {wedge_y0:.1f} {bar_w:.1f} {wedge_h:.1f} re f")

    # Corner registration L-marks
    ops.append("0 0 0 rg")
    mark = 18.0
    thick = 2.0
    corners = [
        (36.0, 36.0, 1, 1),
        (PAGE_W - 36.0, 36.0, -1, 1),
        (36.0, PAGE_H - 36.0, 1, -1),
        (PAGE_W - 36.0, PAGE_H - 36.0, -1, -1),
    ]
    for cx, cy, sx, sy in corners:
        ops.append(
            f"{cx:.1f} {cy:.1f} {sx * mark:.1f} {sy * thick:.1f} re f"
        )
        ops.append(
            f"{cx:.1f} {cy:.1f} {sx * thick:.1f} {sy * mark:.1f} re f"
        )

    # Centre crosshair
    ops.append("0.2 0.2 0.2 rg")
    cx, cy = PAGE_W / 2.0, PAGE_H / 2.0 - 40.0
    ops.append(f"{cx - 20:.1f} {cy - 0.75:.1f} 40 1.5 re f")
    ops.append(f"{cx - 0.75:.1f} {cy - 20:.1f} 1.5 40 re f")

    # Text block (white on banner + black body)
    def pdf_str(s: str) -> str:
        # Escape PDF string specials; keep ASCII for greppable magic.
        out = []
        for ch in s:
            o = ord(ch)
            if ch in ("\\", "(", ")"):
                out.append("\\" + ch)
            elif 32 <= o < 127:
                out.append(ch)
            else:
                out.append("?")
        return "".join(out)

    ops.append("BT")
    ops.append("/F1 18 Tf")
    ops.append("1 1 1 rg")
    ops.append(f"48 {PAGE_H - 58:.1f} Td")
    ops.append(f"({pdf_str(lines[0])}) Tj")
    ops.append("0 -22 Td")
    ops.append("/F1 11 Tf")
    ops.append(f"({pdf_str(lines[1])}) Tj")
    ops.append("ET")

    ops.append("0 0 0 rg")
    ops.append("BT")
    ops.append("/F1 12 Tf")
    y = 360.0
    ops.append(f"36 {y:.1f} Td")
    for line in lines[2:]:
        ops.append(f"({pdf_str(line)}) Tj")
        ops.append("0 -16 Td")
    ops.append("(gray wedge | R G B K bars | corner marks) Tj")
    ops.append("0 -16 Td")
    ops.append("(open inbox capture as .pdf to verify content survived) Tj")
    ops.append("ET")

    stream = ("\n".join(ops) + "\n").encode("latin-1")

    objects: list[bytes] = []

    def add(body: bytes) -> int:
        objects.append(body)
        return len(objects)

    # 1 Catalog
    add(b"<< /Type /Catalog /Pages 2 0 R >>")
    # 2 Pages
    add(b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")
    # 3 Page
    add(
        (
            f"<< /Type /Page /Parent 2 0 R "
            f"/MediaBox [0 0 {PAGE_W:.1f} {PAGE_H:.1f}] "
            f"/Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>"
        ).encode("ascii")
    )
    # 4 Contents
    add(
        b"<< /Length %d >>\nstream\n" % len(stream)
        + stream
        + b"endstream"
    )
    # 5 Font
    add(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")
    # 6 Info
    info = (
        f"<< /Title ({pdf_str(title)}) "
        f"/Subject ({pdf_str(MAGIC)}) "
        f"/Creator (fakeprint.testpage) "
        f"/Producer (fakeprint) >>"
    ).encode("latin-1")
    add(info)

    # Assemble with xref
    out = bytearray()
    out.extend(b"%PDF-1.4\n")
    out.extend(f"% {MAGIC}\n".encode("ascii"))
    offsets = [0]  # object 0 free
    for i, body in enumerate(objects, start=1):
        offsets.append(len(out))
        out.extend(f"{i} 0 obj\n".encode("ascii"))
        out.extend(body)
        out.extend(b"\nendobj\n")
    xref_pos = len(out)
    out.extend(f"xref\n0 {len(offsets)}\n".encode("ascii"))
    out.extend(b"0000000000 65535 f \n")
    for off in offsets[1:]:
        out.extend(f"{off:010d} 00000 n \n".encode("ascii"))
    out.extend(
        (
            f"trailer<< /Size {len(offsets)} /Root 1 0 R /Info 6 0 R >>\n"
            f"startxref\n{xref_pos}\n%%EOF\n"
        ).encode("ascii")
    )
    return bytes(out)


def write_fixture(path: Path | None = None) -> Path:
    """Write ``fixtures/testpage.pdf`` next to this package's fixtures dir."""
    if path is None:
        path = Path(__file__).resolve().parents[1] / "fixtures" / "testpage.pdf"
    path.parent.mkdir(parents=True, exist_ok=True)
    data = build_testpage_pdf()
    path.write_bytes(data)
    return path


def fixture_path() -> Path:
    return Path(__file__).resolve().parents[1] / "fixtures" / "testpage.pdf"


def _iter_content_bodies(data: bytes) -> list[bytes]:
    """Yield PDF content-stream bodies (FlateDecode inflated, else raw)."""
    bodies: list[bytes] = []
    # Uncompressed streams (our fixture).
    for m in re.finditer(rb"<<\s*/Length\s+(\d+)\s*>>\s*stream\r?\n", data):
        n = int(m.group(1))
        body = data[m.end() : m.end() + n]
        if body:
            bodies.append(body)

    # Spooler / driver FlateDecode streams — read by declared Length only
    # (naive search for b'stream' hits noise inside compressed payloads).
    for m in re.finditer(rb"/Filter\s*/FlateDecode", data):
        window = data[m.start() : m.start() + 600]
        lm = re.search(rb"/Length\s+(\d+)", window)
        sm = re.search(rb"stream\r?\n", window)
        if not lm or not sm:
            continue
        n = int(lm.group(1))
        start = m.start() + sm.end()
        raw = data[start : start + n]
        try:
            bodies.append(zlib.decompress(raw))
        except zlib.error:
            continue
    return bodies


def _rg_triples(text: str) -> set[tuple[float, float, float]]:
    found: set[tuple[float, float, float]] = set()
    for a, b, c in re.findall(
        r"([0-9]*\.?[0-9]+)\s+([0-9]*\.?[0-9]+)\s+([0-9]*\.?[0-9]+)\s+rg",
        text,
        flags=re.IGNORECASE,
    ):
        found.add((round(float(a), 2), round(float(b), 2), round(float(c), 2)))
    return found


def analyze_capture(data: bytes, *, min_substantial: int = 1024) -> CaptureProof:
    """Score an inbox ``.bin`` for test-page proof (magic and/or colors)."""
    magic = MAGIC.encode("ascii") in data
    colors = set()
    flate = 0
    for body in _iter_content_bodies(data):
        if body[:2] in (b"x\x9c", b"x\xda", b"x\x01"):
            flate += 1  # shouldn't happen after inflate; count FlateDecode hits below
        text = body.decode("latin-1", errors="replace")
        colors |= _rg_triples(text)
    # Count FlateDecode filters separately for the report.
    flate = len(re.findall(rb"/Filter\s*/FlateDecode", data))

    sig = {(round(r, 2), round(g, 2), round(b, 2)) for r, g, b in _SIG_COLORS}
    hits = sum(1 for c in sig if c in colors)
    return CaptureProof(
        magic=magic,
        color_hits=hits,
        colors_expected=len(sig),
        flate_streams=flate,
        substantial=len(data) >= min_substantial,
        bytes=len(data),
    )


def analyze_capture_file(path: Path | str) -> CaptureProof:
    return analyze_capture(Path(path).read_bytes())
